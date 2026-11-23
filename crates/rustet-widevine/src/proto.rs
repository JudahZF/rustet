//! The Widevine protobuf messages this crate reads or writes, with only the fields it uses.
//!
//! Field numbers and types come from the Go `wv_proto2.pb.go`. The messages are proto2, so every
//! scalar is `optional`: a set field is always encoded, even when it holds the default value.
//! Where the CDM embeds a message it received as bytes (the client ID blob, the PSSH header), the
//! field is declared as `bytes`. That is the same wire format, and the original bytes go out
//! unchanged.

/// `SignedLicenseRequest.MessageType.LICENSE_REQUEST`.
pub const SIGNED_LICENSE_REQUEST: i32 = 1;
/// `LicenseType.DEFAULT`: a streaming licence.
pub const LICENSE_TYPE_DEFAULT: i32 = 1;
/// `LicenseRequest.RequestType.NEW`.
pub const REQUEST_TYPE_NEW: i32 = 1;
/// `ProtocolVersion.CURRENT`.
pub const PROTOCOL_VERSION_CURRENT: i32 = 21;
/// `WidevineCencHeader.Algorithm.AESCTR`.
pub const ALGORITHM_AESCTR: i32 = 1;
/// `ClientIdentification.TokenType.DEVICE_CERTIFICATE`.
pub const TOKEN_TYPE_DEVICE_CERTIFICATE: i32 = 1;
/// `DeviceCertificate.CertificateType.USER_DEVICE`.
pub const CERTIFICATE_TYPE_USER_DEVICE: i32 = 2;

#[derive(Clone, PartialEq, prost::Message)]
pub struct SignedLicenseRequest {
    #[prost(int32, optional, tag = "1")]
    pub r#type: Option<i32>,
    /// An encoded [`LicenseRequest`]: the signature covers exactly these bytes.
    #[prost(bytes = "vec", optional, tag = "2")]
    pub msg: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub signature: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LicenseRequest {
    /// An encoded `ClientIdentification` (the device's client ID blob).
    #[prost(bytes = "vec", optional, tag = "1")]
    pub client_id: Option<Vec<u8>>,
    #[prost(message, optional, tag = "2")]
    pub content_id: Option<ContentIdentification>,
    #[prost(int32, optional, tag = "3")]
    pub r#type: Option<i32>,
    #[prost(uint32, optional, tag = "4")]
    pub request_time: Option<u32>,
    #[prost(int32, optional, tag = "6")]
    pub protocol_version: Option<i32>,
    #[prost(uint32, optional, tag = "7")]
    pub key_control_nonce: Option<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ContentIdentification {
    #[prost(message, optional, tag = "1")]
    pub cenc_id: Option<Cenc>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Cenc {
    /// An encoded [`WidevineCencHeader`].
    #[prost(bytes = "vec", optional, tag = "1")]
    pub pssh: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "2")]
    pub license_type: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub request_id: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SignedLicense {
    #[prost(message, optional, tag = "2")]
    pub msg: Option<License>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub session_key: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct License {
    #[prost(message, repeated, tag = "3")]
    pub key: Vec<KeyContainer>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct KeyContainer {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub id: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub iv: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub key: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "4")]
    pub r#type: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WidevineCencHeader {
    #[prost(int32, optional, tag = "1")]
    pub algorithm: Option<i32>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub key_id: Vec<Vec<u8>>,
    #[prost(string, optional, tag = "3")]
    pub provider: Option<String>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub content_id: Option<Vec<u8>>,
    #[prost(string, optional, tag = "6")]
    pub policy: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ClientIdentification {
    /// Required in proto2.
    #[prost(int32, optional, tag = "1")]
    pub r#type: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub token: Option<SignedDeviceCertificate>,
    #[prost(message, repeated, tag = "3")]
    pub client_info: Vec<NameValue>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct NameValue {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub value: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SignedDeviceCertificate {
    #[prost(message, optional, tag = "1")]
    pub device_certificate: Option<DeviceCertificate>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub signature: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeviceCertificate {
    #[prost(int32, optional, tag = "1")]
    pub r#type: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub serial_number: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "3")]
    pub creation_time_seconds: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub public_key: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "5")]
    pub system_id: Option<u32>,
}
