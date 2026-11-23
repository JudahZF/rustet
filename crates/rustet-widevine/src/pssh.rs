//! Builds the CDM init data for the key ID in an Apple HLS `EXT-X-KEY` URI.

use base64::Engine;
use base64::alphabet::{STANDARD, URL_SAFE};
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use prost::Message;
use tracing::debug;

use crate::{Error, proto};

/// The key ID of a CBCS playlist that carries no real one. It is only a cache key: the real key
/// comes from the licence.
const PLACEHOLDER_KID: &str = "AAAAAAAAAAAAAAAAAAAAAA";

/// The 32 bytes in front of the header. The CDM skips them, as it skips a real `pssh` box header.
const PREFIX: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

/// Builds init data for [`crate::Cdm::new`] from the base64 key ID of an `EXT-X-KEY` URI.
///
/// An empty or placeholder key ID becomes 16 zero bytes. A longer value keeps its first 16
/// bytes: Apple music-video playlists put a 56-byte value there whose first 16 bytes are the key ID.
pub fn init_data(kid_base64: &str) -> Result<Vec<u8>, Error> {
    let mut kid = if kid_base64.is_empty() || kid_base64 == PLACEHOLDER_KID {
        debug!("using placeholder KID (zero bytes)");
        vec![0; 16]
    } else {
        decode_kid(kid_base64)?
    };
    kid.truncate(16);
    if kid.len() != 16 {
        return Err(Error::KidLength(kid.len()));
    }

    let header = proto::WidevineCencHeader {
        algorithm: Some(proto::ALGORITHM_AESCTR),
        key_id: vec![kid],
        provider: Some(String::new()),
        // The base64 of an empty content ID.
        content_id: Some(Vec::new()),
        policy: Some(String::new()),
    };
    let mut data = PREFIX.to_vec();
    header.encode(&mut data).expect("a Vec grows as needed");
    Ok(data)
}

/// Accepts the padded and unpadded forms Apple HLS emits. Key IDs are opaque bytes, so URL-safe
/// alphabets are accepted too. Like Go's decoder, non-zero trailing bits are ignored.
fn decode_kid(value: &str) -> Result<Vec<u8>, Error> {
    const fn engine(alphabet: &base64::alphabet::Alphabet, padded: bool) -> GeneralPurpose {
        let padding = if padded {
            DecodePaddingMode::RequireCanonical
        } else {
            DecodePaddingMode::RequireNone
        };
        GeneralPurpose::new(
            alphabet,
            GeneralPurposeConfig::new()
                .with_decode_padding_mode(padding)
                .with_decode_allow_trailing_bits(true),
        )
    }
    const ENGINES: [GeneralPurpose; 4] = [
        engine(&STANDARD, true),
        engine(&STANDARD, false),
        engine(&URL_SAFE, true),
        engine(&URL_SAFE, false),
    ];
    ENGINES
        .iter()
        .find_map(|e| e.decode(value).ok())
        .ok_or(Error::KidEncoding)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the Go `getPSSH("", base64("0123456789abcdef"))` returned.
    const GO_INIT_DATA: &[u8] = include_bytes!("../tests/data/init_data.bin");

    #[test]
    fn matches_go_for_padded_and_unpadded_kids() {
        // base64("0123456789abcdef"), with and without padding.
        for kid in ["MDEyMzQ1Njc4OWFiY2RlZg==", "MDEyMzQ1Njc4OWFiY2RlZg"] {
            assert_eq!(init_data(kid).unwrap(), GO_INIT_DATA, "{kid}");
        }
    }

    #[test]
    fn placeholder_and_empty_kids_become_zero() {
        for kid in ["", PLACEHOLDER_KID] {
            let data = init_data(kid).unwrap();
            let header = proto::WidevineCencHeader::decode(&data[32..]).unwrap();
            assert_eq!(header.key_id, [vec![0; 16]]);
        }
    }

    #[test]
    fn long_kids_keep_their_first_16_bytes() {
        let kid: Vec<u8> = (0..56).collect();
        let data =
            init_data(&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&kid)).unwrap();
        let header = proto::WidevineCencHeader::decode(&data[32..]).unwrap();
        assert_eq!(header.key_id, [kid[..16].to_vec()]);
    }

    #[test]
    fn rejects_short_or_invalid_kids() {
        assert!(matches!(init_data("c2hvcnQ="), Err(Error::KidLength(5))));
        assert!(matches!(init_data("not base64!"), Err(Error::KidEncoding)));
    }
}
