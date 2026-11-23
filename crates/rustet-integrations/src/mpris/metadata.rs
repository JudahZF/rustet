//! Page metadata (JSON) to MPRIS metadata (D-Bus `a{sv}`).
//!
//! Types follow the MPRIS metadata specification: `mpris:trackid` is an object path,
//! `mpris:length` an int64, the counters int32; other values keep their JSON kind (string,
//! boolean, double, list of strings). The page is untrusted: an invalid track id becomes the
//! spec's "no track" path instead of a malformed message (which made the old D-Bus library drop
//! its connection), and oversized values are left out so a message stays far below the bus limit.

use serde_json::{Map, Value};
use zbus::zvariant::{ObjectPath, OwnedValue, Value as DValue};

use super::iface::Metadata;

/// The spec's "no track" object path.
pub(super) const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

const MAX_ENTRIES: usize = 64;
const MAX_KEY: usize = 64;
const MAX_STRING: usize = 4096;
const MAX_LIST: usize = 64;

fn integer(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite())
            .map(|f| f.round() as i64)
    })
}

fn short(s: &str) -> Option<String> {
    (s.len() <= MAX_STRING).then(|| s.to_owned())
}

fn value(key: &str, v: &Value) -> Option<DValue<'static>> {
    Some(match key {
        "mpris:trackid" => {
            let path = v
                .as_str()
                .and_then(|s| ObjectPath::try_from(s).ok())
                .filter(|p| p.len() <= MAX_STRING);
            DValue::from(
                path.unwrap_or_else(|| ObjectPath::from_static_str_unchecked(NO_TRACK))
                    .into_owned(),
            )
        }
        "mpris:length" => DValue::from(integer(v)?),
        "xesam:trackNumber" | "xesam:discNumber" | "xesam:audioBPM" | "xesam:useCount" => {
            DValue::from(i32::try_from(integer(v)?).ok()?)
        }
        _ => match v {
            Value::String(s) => DValue::from(short(s)?),
            Value::Bool(b) => DValue::from(*b),
            Value::Number(n) => DValue::from(n.as_f64()?),
            Value::Array(items) if items.len() <= MAX_LIST => {
                let strings: Option<Vec<String>> =
                    items.iter().map(|i| short(i.as_str()?)).collect();
                DValue::from(strings?)
            }
            _ => return None,
        },
    })
}

/// Converts the page's metadata. Keys must be namespaced (`mpris:…`, `xesam:…`).
pub(super) fn convert(map: &Map<String, Value>) -> Metadata {
    map.iter()
        .filter(|(k, _)| {
            k.len() <= MAX_KEY
                && k.split_once(':')
                    .is_some_and(|(ns, name)| !ns.is_empty() && !name.is_empty())
        })
        .filter_map(|(k, v)| Some((k.clone(), OwnedValue::try_from(value(k, v)?).ok()?)))
        .take(MAX_ENTRIES)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conv(v: Value) -> Metadata {
        convert(v.as_object().unwrap())
    }

    #[test]
    fn spec_types() {
        let m = conv(json!({
            "mpris:trackid": "/com/apple/music/track/1440857781",
            "mpris:length": 215_000_000.0,
            "mpris:artUrl": "https://is1-ssl.mzstatic.com/a/1000x1000bb.jpg",
            "xesam:title": "T",
            "xesam:artist": ["A"],
            "xesam:trackNumber": 3,
            "xesam:userRating": 0.8,
            "xesam:explicit": true,
        }));
        let sig = |k: &str| m[k].value_signature().to_string();
        assert_eq!(sig("mpris:trackid"), "o");
        assert_eq!(sig("mpris:length"), "x");
        assert_eq!(i64::try_from(&m["mpris:length"]).unwrap(), 215_000_000);
        assert_eq!(sig("mpris:artUrl"), "s");
        assert_eq!(sig("xesam:artist"), "as");
        assert_eq!(sig("xesam:trackNumber"), "i");
        assert_eq!(sig("xesam:userRating"), "d");
        assert_eq!(sig("xesam:explicit"), "b");
    }

    #[test]
    fn untrusted_values() {
        let m = conv(json!({
            "mpris:trackid": "/org/node/mediaplayer/rustet-player/1",
            "xesam:title": "x".repeat(MAX_STRING + 1),
            "xesam:genre": ["Pop", 5],
            "xesam:comment": { "nested": true },
            "nonamespace": "x",
            "xesam:album": "Al",
        }));
        assert_eq!(
            ObjectPath::try_from(m["mpris:trackid"].try_clone().unwrap())
                .unwrap()
                .as_str(),
            NO_TRACK
        );
        assert!(!m.contains_key("xesam:title"));
        assert!(!m.contains_key("xesam:genre"));
        assert!(!m.contains_key("xesam:comment"));
        assert!(!m.contains_key("nonamespace"));
        assert!(m.contains_key("xesam:album"));

        let many: Map<String, Value> = (0..200)
            .map(|i| (format!("xesam:k{i}"), json!("v")))
            .collect();
        assert_eq!(convert(&many).len(), MAX_ENTRIES);
    }
}
