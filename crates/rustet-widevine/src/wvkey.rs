//! The content-key request to Apple's Widevine licence endpoint.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{Cdm, Error, Identity, KeyType, pssh};

/// Apple's web playback licence endpoint.
pub const LICENSE_URL: &str =
    "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/acquireWebPlaybackLicense";

/// The per-track values of one licence request.
#[derive(Debug, Clone, Copy)]
pub struct LicenceRequest<'a> {
    /// The Apple Music asset ID.
    pub adam_id: &'a str,
    /// The key ID and URI prefix of the `EXT-X-KEY` URI, which reads `<uri_prefix>,<kid_base64>`.
    pub kid_base64: &'a str,
    pub uri_prefix: &'a str,
    /// The developer token, sent as `Authorization: Bearer`.
    pub token: &'a str,
    pub media_user_token: &'a str,
}

/// A content key and the IV it decrypts with.
#[derive(Clone, PartialEq, Eq)]
pub struct ContentKey {
    pub key: Vec<u8>,
    /// Apple Music's CBCS key contexts use a fixed zero IV.
    pub iv: [u8; 16],
}

/// The JSON body Apple's endpoint expects. Field order matches the Go engine (sorted keys).
#[derive(Serialize)]
struct RequestBody<'a> {
    #[serde(rename = "adamId")]
    adam_id: &'a str,
    challenge: String,
    #[serde(rename = "isLibrary")]
    is_library: bool,
    #[serde(rename = "key-system")]
    key_system: &'static str,
    uri: String,
    #[serde(rename = "user-initiated")]
    user_initiated: bool,
}

#[derive(Deserialize)]
struct PlaybackLicense {
    #[serde(default, rename = "errorCode")]
    error_code: i64,
    #[serde(default)]
    license: String,
    #[serde(default)]
    status: i64,
}

/// Acquires the content key for one track: builds a Widevine challenge with `identity`, posts it
/// to `url` (normally [`LICENSE_URL`]) and unwraps the key from the licence. When the licence
/// holds several content keys, the last one wins.
///
/// There is no overall timeout: the caller's `client` sets connection timeouts and `cancel`
/// stops the request.
pub async fn acquire_key(
    client: &reqwest::Client,
    url: &str,
    identity: &Identity,
    request: &LicenceRequest<'_>,
    cancel: &CancellationToken,
) -> Result<ContentKey, Error> {
    let init_data = pssh::init_data(request.kid_base64)?;
    let cdm = Cdm::new(identity, &init_data)?;
    let challenge = cdm.license_request()?;

    let body = RequestBody {
        adam_id: request.adam_id,
        challenge: STANDARD.encode(&challenge),
        is_library: false,
        key_system: "com.widevine.alpha",
        uri: format!("{},{}", request.uri_prefix, request.kid_base64),
        user_initiated: true,
    };
    let send = async {
        let response = client
            .post(url)
            .header("authorization", format!("Bearer {}", request.token))
            .header("x-apple-music-user-token", request.media_user_token)
            .header("accept", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        Ok::<_, reqwest::Error>((status, response.bytes().await?))
    };
    let (status, response) = tokio::select! {
        result = send => result?,
        () = cancel.cancelled() => return Err(Error::Cancelled),
    };

    let license = parse_license(status, &response)?;
    cdm.license_keys(&challenge, &license)?
        .into_iter()
        .rfind(|k| k.kind == KeyType::Content)
        .map(|k| ContentKey {
            key: k.value,
            iv: [0; 16],
        })
        .ok_or(Error::NoContentKey)
}

/// Extracts the Widevine licence from the endpoint's JSON answer.
fn parse_license(status: reqwest::StatusCode, body: &[u8]) -> Result<Vec<u8>, Error> {
    let answer: PlaybackLicense = serde_json::from_slice(body).map_err(Error::ResponseJson)?;
    if answer.error_code != 0 || answer.status != 0 {
        // An error response carries no licence, so its body is safe to log and often says why.
        let text = String::from_utf8_lossy(body);
        let shown = match text.char_indices().nth(400) {
            Some((end, _)) => format!("{}…", &text[..end]),
            None => text.into_owned(),
        };
        warn!(status = status.as_u16(), body = %shown, "licence refused");
        return Err(Error::LicenceRefused {
            code: answer.error_code,
            status: answer.status,
        });
    }
    STANDARD
        .decode(answer.license)
        .map_err(Error::LicenceBase64)
}

#[cfg(test)]
mod tests {
    use prost::Message;
    use rsa::traits::PublicKeyParts;
    use sha1::Sha1;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::proto;

    const TRACK: LicenceRequest<'static> = LicenceRequest {
        adam_id: "1440833098",
        kid_base64: "MDEyMzQ1Njc4OWFiY2RlZg==",
        uri_prefix: "skd://itunes",
        token: "dev-token",
        media_user_token: "user-token",
    };

    /// Plays the licence server: wraps `content_key` for the request in `challenge`.
    fn license_for(identity: &Identity, challenge: &[u8], content_key: &[u8; 16]) -> Vec<u8> {
        use aes::Aes128;
        use cbc::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
        use cmac::{Cmac, KeyInit, Mac};

        let session_key = [0x5A; 16];
        let msg = proto::SignedLicenseRequest::decode(challenge)
            .unwrap()
            .msg
            .unwrap();
        let mut mac = <Cmac<Aes128> as KeyInit>::new_from_slice(&session_key).unwrap();
        mac.update(b"\x01ENCRYPTION\x00");
        mac.update(&msg);
        mac.update(&[0, 0, 0, 0x80]);
        let wrapping_key = mac.finalize().into_bytes();

        let iv = [3; 16];
        let wrapped = cbc::Encryptor::<Aes128>::new_from_slices(&wrapping_key, &iv)
            .unwrap()
            .encrypt_padded_vec::<Pkcs7>(content_key);
        let public = identity.private_key.to_public_key();
        assert_eq!(public.size(), 256);
        proto::SignedLicense {
            msg: Some(proto::License {
                key: vec![proto::KeyContainer {
                    id: Some(b"0123456789abcdef".to_vec()),
                    iv: Some(iv.to_vec()),
                    key: Some(wrapped),
                    r#type: Some(2),
                }],
            }),
            session_key: Some(public.oaep_encrypt::<Sha1>(&session_key).unwrap()),
        }
        .encode_to_vec()
    }

    #[tokio::test]
    async fn sends_the_track_parameters_and_unwraps_the_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "Bearer dev-token"))
            .and(header("x-apple-music-user-token", "user-token"))
            .and(header("content-type", "application/json"))
            .respond_with(|req: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                let challenge = STANDARD
                    .decode(body["challenge"].as_str().unwrap())
                    .unwrap();
                let license = license_for(&Identity::embedded(), &challenge, b"content-key-0001");
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "license": STANDARD.encode(license) }))
            })
            .mount(&server)
            .await;

        let key = acquire_key(
            &reqwest::Client::new(),
            &server.uri(),
            &Identity::embedded(),
            &TRACK,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(key.key, b"content-key-0001");
        assert_eq!(key.iv, [0; 16]);

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["uri"], "skd://itunes,MDEyMzQ1Njc4OWFiY2RlZg==");
        assert_eq!(body["adamId"], "1440833098");
        assert_eq!(body["key-system"], "com.widevine.alpha");
        assert_eq!(body["isLibrary"], false);
        assert_eq!(body["user-initiated"], true);
    }

    #[tokio::test]
    async fn cancellation_stops_the_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)))
            .mount(&server)
            .await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = acquire_key(
            &reqwest::Client::new(),
            &server.uri(),
            &Identity::embedded(),
            &TRACK,
            &cancel,
        )
        .await;
        assert!(matches!(result, Err(Error::Cancelled)));
    }

    #[test]
    fn a_refused_licence_is_reported_with_its_code() {
        let err = parse_license(
            reqwest::StatusCode::OK,
            br#"{"errorCode":-42604,"status":-1021}"#,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            Error::LicenceRefused {
                code: -42604,
                status: -1021
            }
        ));
        assert_eq!(
            err.to_string(),
            "error in license response, code: -42604, status: -1021"
        );

        let licence = STANDARD.encode([0; 64]);
        let body = format!(r#"{{"errorCode":42,"license":"{licence}"}}"#);
        assert!(matches!(
            parse_license(reqwest::StatusCode::OK, body.as_bytes()),
            Err(Error::LicenceRefused {
                code: 42,
                status: 0
            })
        ));
    }

    #[test]
    fn a_valid_answer_yields_the_licence() {
        let body = format!(
            r#"{{"license":"{}","renew-after":0}}"#,
            STANDARD.encode([0; 64])
        );
        assert_eq!(
            parse_license(reqwest::StatusCode::OK, body.as_bytes()).unwrap(),
            [0; 64]
        );
        assert!(matches!(
            parse_license(reqwest::StatusCode::OK, b"<html>"),
            Err(Error::ResponseJson(_))
        ));
    }
}
