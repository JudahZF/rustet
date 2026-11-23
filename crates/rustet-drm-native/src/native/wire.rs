//! Pure helpers with lasting wire formats (`native/music_token_request.h`,
//! `native/subscription_status.h`, adapted from PR #1).

use serde::Serialize;

/// The `createMusicToken` request body.
#[derive(Serialize)]
struct MusicTokenRequest<'a> {
    guid: &'a str,
    assertion: &'a str,
    #[serde(rename = "tcc-acceptance-date")]
    tcc_acceptance_date: String,
}

/// Serialises the whole request rather than interpolating it: the assertion is the developer
/// JWT and commonly exceeds any fixed buffer, and both strings must be escaped.
pub(crate) fn music_token_request(guid: &str, assertion: &str, acceptance_time_ms: i64) -> String {
    serde_json::to_string(&MusicTokenRequest {
        guid,
        assertion,
        tcc_acceptance_date: acceptance_time_ms.to_string(),
    })
    .expect("serialising strings cannot fail")
}

/// Whether `FairPlay::getSubscriptionStatus()` allows offline content: the records are 16
/// bytes, and the state is the `int` at offset 8 of the second record (2 =
/// kFPSubscriptionCanPlayContent, 3 = kFPSubscriptionCanStreamAndPlayContent). Empty or
/// partial status is common before sign-in and must never be read past.
///
/// # Safety
/// When `begin` and `end` are non-null and `begin <= end`, `[begin, end)` must be readable.
pub(crate) unsafe fn subscription_offline_available(begin: *const u8, end: *const u8) -> bool {
    if begin.is_null() || end.is_null() || end < begin || (end as usize - begin as usize) < 32 {
        return false;
    }
    // SAFETY: at least 32 readable bytes start at `begin`, per the caller.
    let state = unsafe { begin.add(24).cast::<i32>().read_unaligned() };
    state == 2 || state == 3
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from drm/tests/music_token_request_test.c.
    #[test]
    fn music_token_request_escapes_long_values() {
        let mut assertion = vec![b'x'; 4096];
        assertion[1500] = b'"';
        assertion[1501] = b'\\';
        assertion[1502] = b'\n';
        let assertion = String::from_utf8(assertion).unwrap();
        let guid = "device\"with\\escaped\ncharacters";
        let body = music_token_request(guid, &assertion, 1_791_292_345_678);
        assert!(body.len() > 4096);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["guid"], guid);
        assert_eq!(parsed["assertion"], assertion.as_str());
        assert_eq!(parsed["tcc-acceptance-date"], "1791292345678");

        let body = music_token_request("guid", "assertion", i64::MAX);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["tcc-acceptance-date"], "9223372036854775807");
    }

    // Ported from drm/tests/subscription_status_test.c.
    #[test]
    fn subscription_status_is_bounds_checked() {
        let mut records = [0u8; 32];
        let p = records.as_ptr();
        let null = std::ptr::null::<u8>();
        // SAFETY (all calls): every non-null range lies inside `records`.
        unsafe {
            assert!(!subscription_offline_available(null, null));
            assert!(!subscription_offline_available(null, p));
            assert!(!subscription_offline_available(p, null));
            assert!(!subscription_offline_available(p, p));
            assert!(!subscription_offline_available(p, p.add(16)));
            assert!(!subscription_offline_available(p, p.add(24)));
            assert!(!subscription_offline_available(p.add(16), p));
        }
        for (state, expected) in [(2i32, true), (3, true), (1, false)] {
            records[24..28].copy_from_slice(&state.to_ne_bytes());
            let p = records.as_ptr();
            // SAFETY: both ranges lie inside `records`.
            unsafe {
                assert!(!subscription_offline_available(p, p.add(31)));
                assert_eq!(subscription_offline_available(p, p.add(32)), expected);
            }
        }
    }
}
