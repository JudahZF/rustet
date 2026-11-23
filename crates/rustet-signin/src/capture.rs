//! What the sign-in window looks for, and where it may go. Pure functions, no web view.

use serde::Deserialize;
use url::Url;

/// The origin whose cookies and `localStorage` hold the music user token.
pub(crate) const MUSIC_ORIGIN: &str = "https://music.apple.com";
/// The cookie the web player sets after sign-in.
pub(crate) const TOKEN_COOKIE: &str = "media-user-token";
/// Apple's storefront hint cookie (a two-letter country code).
pub(crate) const STOREFRONT_COOKIE: &str = "itua";

/// Hosts a frame may navigate to: the web player and Apple ID's sign-in frame.
/// Found by logging every navigation of the sign-in flow up to the credentials form.
const ALLOWED_HOSTS: [&str; 2] = ["music.apple.com", "idmsa.apple.com"];

/// Whether a frame (main frame or iframe) may navigate to `url`. Everything else is dropped:
/// other links open nowhere.
pub(crate) fn allow_navigation(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    match url.scheme() {
        // Empty and `srcdoc` iframes the pages create themselves.
        "about" => matches!(url.path(), "blank" | "srcdoc"),
        // No user info: `https://x@evil.example@music.apple.com` is Apple's host, but no
        // sign-in URL carries credentials.
        "https" => {
            url.username().is_empty()
                && url.password().is_none()
                && url
                    .host_str()
                    .is_some_and(|host| ALLOWED_HOSTS.contains(&host))
        }
        _ => false,
    }
}

/// `scheme://host/path` of a URL, for logs: queries and fragments of the sign-in flow can carry
/// session state and are never logged.
pub(crate) fn loggable(url: &str) -> String {
    match Url::parse(url) {
        Ok(url) => format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or(""),
            url.path()
        ),
        Err(_) => "<invalid url>".to_owned(),
    }
}

/// A storefront code: two ASCII letters, lowercased (`US` → `us`).
pub(crate) fn storefront(value: &str) -> Option<String> {
    (value.len() == 2 && value.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| value.to_ascii_lowercase())
}

/// Script run in the main frame: the names of the `music.apple.com` `localStorage` keys, and
/// the value of MusicKit's user-token key `music.<id>.u` if there is one. Returns `null` on any
/// other origin.
pub(crate) const LOCAL_STORAGE_SCRIPT: &str = r#"(() => {
  if (location.origin !== "https://music.apple.com") return null;
  const keys = [];
  let token = null;
  for (let i = 0; i < localStorage.length; i++) {
    const key = localStorage.key(i);
    keys.push(key);
    if (/^music\.[^.]+\.u$/.test(key)) token = localStorage.getItem(key);
  }
  return { keys, token };
})()"#;

/// What [`LOCAL_STORAGE_SCRIPT`] returns.
#[derive(Deserialize, Default)]
pub(crate) struct LocalStorage {
    pub keys: Vec<String>,
    token: Option<String>,
}

impl LocalStorage {
    /// Parses the script result as the web view reports it (JSON). Anything unexpected,
    /// including `null` from another origin, reads as empty.
    pub(crate) fn parse(json: &str) -> Self {
        serde_json::from_str::<Option<Self>>(json)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// The user token, if MusicKit stored one. MusicKit may store it JSON-quoted.
    pub(crate) fn token(&self) -> Option<String> {
        let raw = self.token.as_deref()?.trim();
        let token = serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_owned());
        (!token.is_empty()).then_some(token)
    }
}

/// The token and storefront in a set of cookies for [`MUSIC_ORIGIN`]. An empty
/// `media-user-token` (the web player writes one when signed out) is not a token.
pub(crate) fn from_cookies<'a>(
    cookies: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> (Option<String>, Option<String>) {
    let (mut token, mut storefront_hint) = (None, None);
    for (name, value) in cookies {
        match name {
            TOKEN_COOKIE if !value.is_empty() => token = Some(value.to_owned()),
            STOREFRONT_COOKIE => storefront_hint = storefront(value),
            _ => {}
        }
    }
    (token, storefront_hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_is_limited_to_the_sign_in_hosts() {
        for url in [
            "https://music.apple.com/us/login",
            "https://idmsa.apple.com/appleauth/auth/authorize/signin?frame_id=x",
            "about:blank",
            "about:srcdoc",
        ] {
            assert!(allow_navigation(url), "{url}");
        }
        for url in [
            "http://music.apple.com/us/login",
            "https://music.apple.com.evil.example/",
            "https://evil.example/?u=https://music.apple.com",
            "https://www.apple.com/legal/",
            "https://apps.apple.com/app/apple-music/id1108187390",
            "https://user@evil.example@music.apple.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,hi",
            "about:config",
            "not a url",
        ] {
            assert!(!allow_navigation(url), "{url}");
        }
    }

    #[test]
    fn logs_drop_query_and_fragment() {
        assert_eq!(
            loggable("https://idmsa.apple.com/appleauth/auth?state=secret#x"),
            "https://idmsa.apple.com/appleauth/auth"
        );
    }

    #[test]
    fn storefront_is_two_letters() {
        assert_eq!(storefront("US").as_deref(), Some("us"));
        assert_eq!(storefront("gb").as_deref(), Some("gb"));
        assert_eq!(storefront(""), None);
        assert_eq!(storefront("usa"), None);
        assert_eq!(storefront("1;"), None);
    }

    #[test]
    fn cookies_give_token_and_storefront() {
        let cookies = [("geo", "US"), ("media-user-token", "abc"), ("itua", "DE")];
        assert_eq!(
            from_cookies(cookies),
            (Some("abc".to_owned()), Some("de".to_owned()))
        );
        assert_eq!(from_cookies([("media-user-token", "")]), (None, None));
    }

    #[test]
    fn local_storage_result() {
        let found = LocalStorage::parse(r#"{"keys":["a","music.x.u"],"token":"tok"}"#);
        assert_eq!(found.keys, ["a", "music.x.u"]);
        assert_eq!(found.token().as_deref(), Some("tok"));
        let quoted = LocalStorage::parse(r#"{"keys":[],"token":"\"tok\""}"#);
        assert_eq!(quoted.token().as_deref(), Some("tok"));
        assert_eq!(LocalStorage::parse("null").token(), None);
        assert_eq!(
            LocalStorage::parse(r#"{"keys":[],"token":""}"#).token(),
            None
        );
        assert_eq!(LocalStorage::parse("garbage").token(), None);
    }
}
