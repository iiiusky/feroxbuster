use crate::config::{CONFIGURATION, PROGRESS_PRINTER};
use crate::filters::WildcardFilter;
use crate::scanner::should_filter_response;
use crate::utils::{
    ferox_print, format_url, get_url_path_length, make_request, module_colorizer, status_colorizer,
};
use crate::FeroxResponse;
use console::style;
use indicatif::ProgressBar;
use std::process;
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

/// length of a standard UUID, used when determining wildcard responses
const UUID_LENGTH: u64 = 32;

/// Simple helper to return a uuid, formatted as lowercase without hyphens
///
/// `length` determines the number of uuids to string together. Each uuid
/// is 32 characters long. So, a length of 1 return a 32 character string,
/// a length of 2 returns a 64 character string, and so on...
fn unique_string(length: usize) -> String {
    log::trace!("enter: unique_string({})", length);
    let mut ids = vec![];

    for _ in 0..length {
        ids.push(Uuid::new_v4().to_simple().to_string());
    }

    let unique_id = ids.join("");

    log::trace!("exit: unique_string -> {}", unique_id);
    unique_id
}

/// Tests the given url to see if it issues a wildcard response
///
/// In the event that url returns a wildcard response, a
/// [WildcardFilter](struct.WildcardFilter.html) is created and returned to the caller.
pub async fn wildcard_test(
    target_url: &str,
    bar: ProgressBar,
    tx_file: UnboundedSender<String>,
) -> Option<WildcardFilter> {
    log::trace!(
        "enter: wildcard_test({:?}, {:?}, {:?})",
        target_url,
        bar,
        tx_file
    );

    if CONFIGURATION.dont_filter {
        // early return, dont_filter scans don't need tested
        log::trace!("exit: wildcard_test -> None");
        return None;
    }

    let clone_req_one = tx_file.clone();
    let clone_req_two = tx_file.clone();

    if let Some(ferox_response) = make_wildcard_request(&target_url, 1, clone_req_one).await {
        bar.inc(1);

        // found a wildcard response
        let mut wildcard = WildcardFilter::default();

        let wc_length = ferox_response.content_length();

        if wc_length == 0 {
            log::trace!("exit: wildcard_test -> Some({:?})", wildcard);
            return Some(wildcard);
        }

        // content length of wildcard is non-zero, perform additional tests:
        //   make a second request, with a known-sized (64) longer request
        if let Some(resp_two) = make_wildcard_request(&target_url, 3, clone_req_two).await {
            bar.inc(1);

            let wc2_length = resp_two.content_length();

            if wc2_length == wc_length + (UUID_LENGTH * 2) {
                // second length is what we'd expect to see if the requested url is
                // reflected in the response along with some static content; aka custom 404
                let url_len = get_url_path_length(&ferox_response.url());

                wildcard.dynamic = wc_length - url_len;

                if !CONFIGURATION.quiet {
                    let msg = format!(
                            "{} {:>10} Wildcard response is dynamic; {} ({} + url length) responses; toggle this behavior by using {}\n",
                            status_colorizer("WLD"),
                            wildcard.dynamic,
                            style("auto-filtering").yellow(),
                            style(wc_length - url_len).cyan(),
                            style("--dont-filter").yellow()
                        );

                    ferox_print(&msg, &PROGRESS_PRINTER);

                    try_send_message_to_file(
                        &msg,
                        tx_file.clone(),
                        !CONFIGURATION.output.is_empty(),
                    );
                }
            } else if wc_length == wc2_length {
                wildcard.size = wc_length;

                if !CONFIGURATION.quiet {
                    let msg = format!(
                        "{} {:>10} Wildcard response is static; {} {} responses; toggle this behavior by using {}\n",
                        status_colorizer("WLD"),
                        wc_length,
                        style("auto-filtering").yellow(),
                        style(wc_length).cyan(),
                        style("--dont-filter").yellow()
                    );

                    ferox_print(&msg, &PROGRESS_PRINTER);

                    try_send_message_to_file(
                        &msg,
                        tx_file.clone(),
                        !CONFIGURATION.output.is_empty(),
                    );
                }
            }
        } else {
            bar.inc(2);
        }

        log::trace!("exit: wildcard_test -> Some({:?})", wildcard);
        return Some(wildcard);
    }

    log::trace!("exit: wildcard_test -> None");
    None
}

/// Generates a uuid and appends it to the given target url. The reasoning is that the randomly
/// generated unique string should not exist on and be served by the target web server.
///
/// Once the unique url is created, the request is sent to the server. If the server responds
/// back with a valid status code, the response is considered to be a wildcard response. If that
/// wildcard response has a 3xx status code, that redirection location is displayed to the user.
async fn make_wildcard_request(
    target_url: &str,
    length: usize,
    tx_file: UnboundedSender<String>,
) -> Option<FeroxResponse> {
    log::trace!(
        "enter: make_wildcard_request({}, {}, {:?})",
        target_url,
        length,
        tx_file
    );

    let unique_str = unique_string(length);

    let nonexistent = match format_url(
        target_url,
        &unique_str,
        CONFIGURATION.add_slash,
        &CONFIGURATION.queries,
        None,
    ) {
        Ok(url) => url,
        Err(e) => {
            log::error!("{}", e);
            log::trace!("exit: make_wildcard_request -> None");
            return None;
        }
    };

    let wildcard = status_colorizer("WLD");

    match make_request(&CONFIGURATION.client, &nonexistent.to_owned()).await {
        Ok(response) => {
            if CONFIGURATION
                .status_codes
                .contains(&response.status().as_u16())
            {
                // found a wildcard response
                let ferox_response = FeroxResponse::from(response, false).await;
                let url_len = get_url_path_length(&ferox_response.url());
                let content_len = ferox_response.content_length();

                if !CONFIGURATION.quiet && !should_filter_response(&ferox_response) {
                    let msg = format!(
                        "{} {:>10} Got {} for {} (url length: {})\n",
                        wildcard,
                        content_len,
                        status_colorizer(&ferox_response.status().as_str()),
                        ferox_response.url(),
                        url_len
                    );

                    ferox_print(&msg, &PROGRESS_PRINTER);

                    try_send_message_to_file(
                        &msg,
                        tx_file.clone(),
                        !CONFIGURATION.output.is_empty(),
                    );
                }

                if ferox_response.status().is_redirection() {
                    // show where it goes, if possible
                    if let Some(next_loc) = ferox_response.headers().get("Location") {
                        let next_loc_str = next_loc.to_str().unwrap_or("Unknown");
                        if !CONFIGURATION.quiet && !should_filter_response(&ferox_response) {
                            let msg = format!(
                                "{} {:>10} {} redirects to => {}\n",
                                wildcard,
                                content_len,
                                ferox_response.url(),
                                next_loc_str
                            );

                            ferox_print(&msg, &PROGRESS_PRINTER);

                            try_send_message_to_file(
                                &msg,
                                tx_file.clone(),
                                !CONFIGURATION.output.is_empty(),
                            );
                        }
                    }
                }
                log::trace!("exit: make_wildcard_request -> {:?}", ferox_response);
                return Some(ferox_response);
            }
        }
        Err(e) => {
            log::warn!("{}", e);
            log::trace!("exit: make_wildcard_request -> None");
            return None;
        }
    }
    log::trace!("exit: make_wildcard_request -> None");
    None
}

/// Simply tries to connect to all given sites before starting to scan
///
/// In the event that no sites can be reached, the program will exit.
///
/// Any urls that are found to be alive are returned to the caller.
pub async fn connectivity_test(target_urls: &[String]) -> Vec<String> {
    log::trace!("enter: connectivity_test({:?})", target_urls);

    let mut good_urls = vec![];

    for target_url in target_urls {
        let request = match format_url(
            target_url,
            "",
            CONFIGURATION.add_slash,
            &CONFIGURATION.queries,
            None,
        ) {
            Ok(url) => url,
            Err(e) => {
                log::error!("{}", e);
                continue;
            }
        };

        match make_request(&CONFIGURATION.client, &request).await {
            Ok(_) => {
                good_urls.push(target_url.to_owned());
            }
            Err(e) => {
                if !CONFIGURATION.quiet {
                    ferox_print(
                        &format!("Could not connect to {}, skipping...", target_url),
                        &PROGRESS_PRINTER,
                    );
                }
                log::error!("{}", e);
            }
        }
    }

    if good_urls.is_empty() {
        log::error!("Could not connect to any target provided, exiting.");
        log::trace!("exit: connectivity_test");
        eprintln!(
            "{} {} Could not connect to any target provided",
            status_colorizer("ERROR"),
            module_colorizer("heuristics::connectivity_test"),
        );

        process::exit(1);
    }

    log::trace!("exit: connectivity_test -> {:?}", good_urls);

    good_urls
}

/// simple helper to keep DRY; sends a message using the transmitter side of the given mpsc channel
/// the receiver is expected to be the side that saves the message to CONFIGURATION.output.
fn try_send_message_to_file(msg: &str, tx_file: UnboundedSender<String>, save_output: bool) {
    log::trace!("enter: try_send_message_to_file({}, {:?})", msg, tx_file);

    if save_output {
        match tx_file.send(msg.to_string()) {
            Ok(_) => {
                log::trace!(
                    "sent message from heuristics::try_send_message_to_file to file handler"
                );
            }
            Err(e) => {
                log::error!(
                    "{} {} {}",
                    status_colorizer("ERROR"),
                    module_colorizer("heuristics::try_send_message_to_file"),
                    e
                );
            }
        }
    }
    log::trace!("exit: try_send_message_to_file");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FeroxChannel;
    use tokio::sync::mpsc;

    #[test]
    /// request a unique string of 32bytes * a value returns correct result
    fn heuristics_unique_string_returns_correct_length() {
        for i in 0..10 {
            assert_eq!(unique_string(i).len(), i * 32);
        }
    }

    #[test]
    /// simply test the default values for wildcardfilter, expect 0, 0
    fn heuristics_wildcardfilter_dafaults() {
        let wcf = WildcardFilter::default();
        assert_eq!(wcf.size, 0);
        assert_eq!(wcf.dynamic, 0);
    }

    #[tokio::test(core_threads = 1)]
    /// tests that given a message and transmitter, the function sends the message across the
    /// channel
    async fn heuristics_try_send_message_to_file_sends_when_true() {
        let (tx, mut rx): FeroxChannel<String> = mpsc::unbounded_channel();
        let msg = "It really tied the room together.";
        let should_save = true;
        try_send_message_to_file(&msg, tx, should_save);

        assert_eq!(rx.recv().await.unwrap(), msg);
    }

    #[tokio::test(core_threads = 1)]
    #[should_panic]
    /// tests that when save_output is false, nothing is sent to the receiver
    async fn heuristics_try_send_message_to_file_sends_when_false() {
        let (tx, mut rx): FeroxChannel<String> = mpsc::unbounded_channel();
        let msg = "I'm the Dude, so that's what you call me.";
        let should_save = false;
        try_send_message_to_file(&msg, tx, should_save);

        assert_ne!(rx.recv().await.unwrap(), msg);
    }

    #[tokio::test(core_threads = 1)]
    /// tests that when save_output is true, but the receiver is closed, nothing is sent to the receiver
    /// this test doesn't assert anything, but reaches the error block of the given function and
    /// can be verified with --nocapture and RUST_LOG being set
    async fn heuristics_try_send_message_to_file_sends_with_closed_receiver() {
        env_logger::init();
        let (tx, mut rx): FeroxChannel<String> = mpsc::unbounded_channel();
        let msg = "Hey, nice marmot.";
        let should_save = true;
        rx.close();
        try_send_message_to_file(&msg, tx, should_save);
    }
}
