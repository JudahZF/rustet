//! The Widevine CDM: builds and signs licence requests, and unwraps the content keys in a licence.

use std::time::{SystemTime, UNIX_EPOCH};

use aes::Aes128;
use cbc::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::NoPadding};
use cmac::{Cmac, KeyInit, Mac};
use prost::Message;
use rand::RngExt;
use sha1::Sha1;

use crate::Error;
use crate::identity::Identity;
use crate::proto;

/// What a key in a licence is for. Only [`KeyType::Content`] keys decrypt media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Signing,
    Content,
    KeyControl,
    OperatorSession,
    /// A value this CDM has no name for.
    Other(i32),
}

impl From<i32> for KeyType {
    fn from(value: i32) -> Self {
        match value {
            1 => Self::Signing,
            2 => Self::Content,
            3 => Self::KeyControl,
            4 => Self::OperatorSession,
            other => Self::Other(other),
        }
    }
}

/// A key unwrapped from a licence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub id: Vec<u8>,
    pub kind: KeyType,
    pub value: Vec<u8>,
}

/// One licence exchange: a session ID and the PSSH header it asks keys for.
pub struct Cdm<'a> {
    identity: &'a Identity,
    /// The encoded `WidevineCencHeader`, sent as it came.
    pssh: Vec<u8>,
    session_id: [u8; 32],
}

impl<'a> Cdm<'a> {
    /// Starts a session for `init_data`: a 32-byte box prefix followed by an encoded
    /// `WidevineCencHeader` (see [`crate::pssh::init_data`]).
    pub fn new(identity: &'a Identity, init_data: &[u8]) -> Result<Self, Error> {
        let pssh = init_data.get(32..).ok_or(Error::InitDataTooShort)?;
        proto::WidevineCencHeader::decode(pssh).map_err(Error::Pssh)?;
        Ok(Self {
            identity,
            pssh: pssh.to_vec(),
            session_id: new_session_id(),
        })
    }

    /// Builds the signed licence request. It is sent to the licence server, which answers with
    /// the licence that [`Cdm::license_keys`] unwraps.
    pub fn license_request(&self) -> Result<Vec<u8>, Error> {
        let request_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);
        let nonce = rand::rng().random_range(0..u32::MAX);
        let msg = self.request_message(request_time, nonce);
        let signature = self.identity.private_key.pss_sign::<Sha1>(&msg)?;
        Ok(proto::SignedLicenseRequest {
            r#type: Some(proto::SIGNED_LICENSE_REQUEST),
            msg: Some(msg),
            signature: Some(signature),
        }
        .encode_to_vec())
    }

    /// The encoded `LicenseRequest`, the part of the request that is signed.
    fn request_message(&self, request_time: u32, nonce: u32) -> Vec<u8> {
        proto::LicenseRequest {
            client_id: Some(self.identity.client_id.clone()),
            content_id: Some(proto::ContentIdentification {
                cenc_id: Some(proto::Cenc {
                    pssh: Some(self.pssh.clone()),
                    license_type: Some(proto::LICENSE_TYPE_DEFAULT),
                    request_id: Some(self.session_id.to_vec()),
                }),
            }),
            r#type: Some(proto::REQUEST_TYPE_NEW),
            request_time: Some(request_time),
            protocol_version: Some(proto::PROTOCOL_VERSION_CURRENT),
            key_control_nonce: Some(nonce),
        }
        .encode_to_vec()
    }

    /// Unwraps the keys in `license_response`. `license_request` is the request it answers: the
    /// key that wraps the content keys is derived from it.
    pub fn license_keys(
        &self,
        license_request: &[u8],
        license_response: &[u8],
    ) -> Result<Vec<Key>, Error> {
        let license = proto::SignedLicense::decode(license_response).map_err(Error::Decode)?;
        let request =
            proto::SignedLicenseRequest::decode(license_request).map_err(Error::Decode)?;
        let msg = request.msg.unwrap_or_default();

        let session_key = self
            .identity
            .private_key
            .oaep_decrypt::<Sha1>(license.session_key.as_deref().unwrap_or_default())?;

        let mut context = Vec::with_capacity(msg.len() + 16);
        context.extend_from_slice(b"\x01ENCRYPTION\x00");
        context.extend_from_slice(&msg);
        context.extend_from_slice(&[0, 0, 0, 0x80]);
        // Widevine session keys are AES-128 keys.
        let session_key: [u8; 16] = session_key
            .as_slice()
            .try_into()
            .map_err(|_| Error::SessionKeyLength(session_key.len()))?;
        let encryption_key = cmac(&session_key, &context);

        license
            .msg
            .ok_or(Error::MissingLicense)?
            .key
            .into_iter()
            .map(|container| {
                let kind = container.r#type.ok_or(Error::InvalidKeyContainer)?.into();
                let value = decrypt_key(
                    &encryption_key,
                    container.iv.as_deref().unwrap_or_default(),
                    container.key.as_deref().unwrap_or_default(),
                )?;
                Ok(Key {
                    id: container.id.unwrap_or_default(),
                    kind,
                    value,
                })
            })
            .collect()
    }
}

/// 16 random hex digits, then `01`, then zeros: the shape of a real CDM's session ID.
fn new_session_id() -> [u8; 32] {
    const DIGITS: &[u8; 16] = b"ABCDEF0123456789";
    let mut rng = rand::rng();
    let mut id = [b'0'; 32];
    for byte in &mut id[..16] {
        *byte = DIGITS[rng.random_range(0..DIGITS.len())];
    }
    id[17] = b'1';
    id
}

/// AES-128-CMAC of `data`.
fn cmac(key: &[u8; 16], data: &[u8]) -> [u8; 16] {
    let mut mac = <Cmac<Aes128> as KeyInit>::new(key.into());
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// AES-128-CBC decrypts a wrapped key and strips its PKCS#7 padding. Like the reference CDM, the
/// padding bytes are not checked: the last byte says how many to drop.
fn decrypt_key(encryption_key: &[u8; 16], iv: &[u8], wrapped: &[u8]) -> Result<Vec<u8>, Error> {
    let mut value = cbc::Decryptor::<Aes128>::new_from_slices(encryption_key, iv)
        .map_err(|_| Error::InvalidKeyContainer)?
        .decrypt_padded_vec::<NoPadding>(wrapped)
        .map_err(|_| Error::InvalidKeyContainer)?;
    if let Some(&pad) = value.last() {
        let len = value
            .len()
            .checked_sub(pad.into())
            .ok_or(Error::InvalidKeyContainer)?;
        value.truncate(len);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INIT_DATA: &[u8] = include_bytes!("../tests/data/init_data.bin");
    /// A request the Go CDM signed with the embedded identity.
    const GO_REQUEST: &[u8] = include_bytes!("../tests/data/license_request.bin");
    /// The `LicenseRequest` inside [`GO_REQUEST`], as the Go CDM re-encodes it for key derivation.
    const GO_REQUEST_MSG: &[u8] = include_bytes!("../tests/data/license_request_msg.bin");
    /// A licence answering [`GO_REQUEST`]: session key `sessionkey-16byt`, a 32-byte signing key
    /// and the content key `content-key-0001`. Go derived the wrapping key
    /// `4005f23ff531934e95662e8f75c491af` from it.
    const GO_RESPONSE: &[u8] = include_bytes!("../tests/data/license_response.bin");

    fn go_request() -> proto::LicenseRequest {
        let signed = proto::SignedLicenseRequest::decode(GO_REQUEST).unwrap();
        proto::LicenseRequest::decode(signed.msg.as_deref().unwrap()).unwrap()
    }

    #[test]
    fn request_message_matches_go_byte_for_byte() {
        let identity = Identity::embedded();
        let go = go_request();
        let mut cdm = Cdm::new(&identity, INIT_DATA).unwrap();
        cdm.session_id = go
            .content_id
            .unwrap()
            .cenc_id
            .unwrap()
            .request_id
            .unwrap()
            .try_into()
            .unwrap();

        let msg = cdm.request_message(go.request_time.unwrap(), go.key_control_nonce.unwrap());
        assert_eq!(msg, GO_REQUEST_MSG);
    }

    #[test]
    fn signed_request_verifies_under_the_device_key() {
        let identity = Identity::embedded();
        let public = identity.private_key.to_public_key();

        let go = proto::SignedLicenseRequest::decode(GO_REQUEST).unwrap();
        assert_eq!(
            go.msg.as_deref(),
            Some(GO_REQUEST_MSG),
            "Go re-encodes the message unchanged"
        );
        public
            .pss_verify::<Sha1>(GO_REQUEST_MSG, &go.signature.unwrap())
            .unwrap();

        let ours = Cdm::new(&identity, INIT_DATA)
            .unwrap()
            .license_request()
            .unwrap();
        let ours = proto::SignedLicenseRequest::decode(ours.as_slice()).unwrap();
        assert_eq!(ours.r#type, Some(proto::SIGNED_LICENSE_REQUEST));
        public
            .pss_verify::<Sha1>(&ours.msg.unwrap(), &ours.signature.unwrap())
            .unwrap();
    }

    #[test]
    fn license_keys_match_go() {
        let identity = Identity::embedded();
        let cdm = Cdm::new(&identity, INIT_DATA).unwrap();
        let keys = cdm.license_keys(GO_REQUEST, GO_RESPONSE).unwrap();
        assert_eq!(
            keys,
            [
                Key {
                    id: b"sign-id".to_vec(),
                    kind: KeyType::Signing,
                    value: vec![0xAA; 32]
                },
                Key {
                    id: b"0123456789abcdef".to_vec(),
                    kind: KeyType::Content,
                    value: b"content-key-0001".to_vec()
                },
            ]
        );
    }

    #[test]
    fn derived_encryption_key_matches_go() {
        let mut context = b"\x01ENCRYPTION\x00".to_vec();
        context.extend_from_slice(GO_REQUEST_MSG);
        context.extend_from_slice(&[0, 0, 0, 0x80]);
        let key = cmac(b"sessionkey-16byt", &context);
        assert_eq!(
            key,
            *b"\x40\x05\xf2\x3f\xf5\x31\x93\x4e\x95\x66\x2e\x8f\x75\xc4\x91\xaf"
        );
    }

    #[test]
    fn session_id_has_the_cdm_shape() {
        let id = new_session_id();
        assert!(id[..16].iter().all(|b| b"ABCDEF0123456789".contains(b)));
        assert_eq!(&id[16..], b"0100000000000000");
    }

    #[test]
    fn rejects_short_or_malformed_init_data() {
        let identity = Identity::embedded();
        assert!(matches!(
            Cdm::new(&identity, &[0; 31]),
            Err(Error::InitDataTooShort)
        ));
        let mut bad = vec![0; 32];
        bad.push(0xFF);
        assert!(matches!(Cdm::new(&identity, &bad), Err(Error::Pssh(_))));
    }

    #[test]
    fn padding_longer_than_the_key_is_an_error() {
        // One block that decrypts to a last byte of 0xFF must not underflow.
        let key = [7; 16];
        let iv = [0; 16];
        let mut block = [0xFF; 16];
        use cbc::cipher::BlockModeEncrypt;
        let wrapped = cbc::Encryptor::<Aes128>::new(&key.into(), &iv.into())
            .encrypt_padded::<NoPadding>(&mut block, 16)
            .unwrap()
            .to_vec();
        assert!(matches!(
            decrypt_key(&key, &iv, &wrapped),
            Err(Error::InvalidKeyContainer)
        ));
        assert!(matches!(
            decrypt_key(&key, &iv, &wrapped[..15]),
            Err(Error::InvalidKeyContainer)
        ));
    }
}
