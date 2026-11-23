//! What to do when the main window cannot load Apple Music (`load-failure.mjs`): offline, DNS
//! failure, captive portal, bad certificate, proxy down. Without the offline page the window
//! stays a blank frame. Error codes are Chromium's net error codes.

/// Whether a failed load shows the offline page: main-frame failures only. Sub-resource failures
/// are normal on a flaky network, `-3` is an aborted load (a newer navigation replaced it), and a
/// `file:` URL is the offline page itself.
pub fn should_show_offline(error_code: i32, is_main_frame: bool, validated_url: &str) -> bool {
    is_main_frame && error_code != -3 && error_code != 0 && !validated_url.starts_with("file:")
}

fn friendly(error_code: i32) -> &'static str {
    match error_code {
        -106 => "You are offline",
        -105 => "Apple Music could not be found. Check your connection or DNS",
        -109 => "Apple Music is unreachable from this network",
        -102 => "The connection was refused",
        -118 | -7 => "The connection timed out",
        -130 => "The proxy server could not be reached",
        -299..=-200 => "A secure connection to Apple Music could not be made",
        _ => "Apple Music could not be loaded",
    }
}

/// The offline page's URL fragment: `m=<message>&d=<description (code)>`.
pub fn offline_hash(error_code: i32, description: &str) -> String {
    let detail = format!(
        "{} ({error_code})",
        if description.is_empty() {
            "error"
        } else {
            description
        }
    );
    format!(
        "m={}&d={}",
        encode_uri_component(friendly(error_code)),
        encode_uri_component(&detail)
    )
}

/// JavaScript's `encodeURIComponent`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::form_urlencoded;

    fn parse(hash: &str, key: &str) -> String {
        form_urlencoded::parse(hash.as_bytes())
            .find(|(k, _)| k == key)
            .unwrap()
            .1
            .into_owned()
    }

    #[test]
    fn main_frame_network_failures_show_the_offline_page() {
        for code in [-106, -105, -102, -118, -109, -7, -130, -200, -501] {
            assert!(
                should_show_offline(code, true, "https://music.apple.com/"),
                "{code}"
            );
        }
    }

    #[test]
    fn aborted_loads_sub_frames_and_the_offline_page_are_ignored() {
        assert!(!should_show_offline(-3, true, "https://music.apple.com/"));
        assert!(!should_show_offline(
            -106,
            false,
            "https://cdn.example/x.js"
        ));
        assert!(!should_show_offline(-106, true, "file:///app/offline.html"));
    }

    #[test]
    fn friendly_messages_round_trip() {
        let h = offline_hash(-106, "ERR_INTERNET_DISCONNECTED");
        assert_eq!(parse(&h, "m"), "You are offline");
        assert_eq!(parse(&h, "d"), "ERR_INTERNET_DISCONNECTED (-106)");
        assert!(parse(&offline_hash(-202, ""), "m").contains("secure connection"));
        assert!(parse(&offline_hash(-999, ""), "m").contains("could not be loaded"));
        assert_eq!(parse(&offline_hash(-1, ""), "d"), "error (-1)");
        assert_eq!(
            offline_hash(-106, "x"),
            "m=You%20are%20offline&d=x%20(-106)"
        );
    }
}
