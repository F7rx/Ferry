//! Wire DTOs: LocalSend v2.2 shapes plus Ferry's optional extension members
//! (docs/05-protocol.md). Parsing is lenient (unknown device types, enum
//! case, unknown members); output matches what LocalSend expects exactly.

use crate::model::{DeviceKind, Protocol};
use indexmap::IndexMap;
pub use localsend::model::transfer::{FileDto, FileMetadata};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;

pub const PROTOCOL_VERSION: &str = "2.2";
pub const FERRY_PROTOCOL_VERSION: u32 = 1;

pub const API_V2: &str = "/api/localsend/v2";
pub const API_V1_INFO: &str = "/api/localsend/v1/info";
pub const API_FERRY: &str = "/api/ferry/v1";

/// Capabilities this build implements.
pub const FERRY_CAPS: &[&str] = &["resume", "verify", "status", "pair"];

/// Optional capability hint carried in every v2 DTO Ferry sends.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FerryHint {
    pub v: u32,
    #[serde(default)]
    pub caps: Vec<String>,
}

impl FerryHint {
    pub fn ours() -> Self {
        FerryHint { v: FERRY_PROTOCOL_VERSION, caps: FERRY_CAPS.iter().map(|c| c.to_string()).collect() }
    }

    pub fn has(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }
}

/// Device information as exchanged in register / info / prepare-upload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceDto {
    pub alias: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
    #[serde(default, with = "lenient_device_kind", skip_serializing_if = "Option::is_none")]
    pub device_type: Option<DeviceKind>,
    #[serde(default)]
    pub fingerprint: String,
    /// Present in requests (register / prepare-upload), absent in responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, with = "lenient_protocol", skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    #[serde(default)]
    pub download: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ferry: Option<FerryHint>,
}

impl DeviceDto {
    /// Checks peer-supplied fields against sane bounds.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.alias.trim().is_empty() || self.alias.len() > 256 {
            return Err("invalid alias");
        }
        if self.version.len() > 16 || self.fingerprint.len() > 256 {
            return Err("invalid field length");
        }
        if self.device_model.as_ref().is_some_and(|m| m.len() > 128) {
            return Err("invalid device model");
        }
        if let Some(ferry) = &self.ferry
            && (ferry.caps.len() > 32 || ferry.caps.iter().any(|c| c.len() > 32))
        {
            return Err("invalid capabilities");
        }
        Ok(())
    }

    /// Display name: see [`clean_alias`].
    pub fn display_alias(&self) -> String {
        clean_alias(&self.alias)
    }
}

/// A peer-chosen name made safe to show: no control, bidi-override or
/// zero-width characters (they let one device dress up as another), at most
/// 64 characters, never empty.
pub fn clean_alias(alias: &str) -> String {
    let clean: String = alias.chars().filter(|c| !crate::fsutil::sanitize::is_invisible_or_control(*c)).take(64).collect();
    let clean = clean.trim();
    if clean.is_empty() { "Unknown device".to_string() } else { clean.to_string() }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareUploadRequest {
    pub info: DeviceDto,
    pub files: IndexMap<String, FileDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ferry: Option<FerryPrepareRequest>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FerryPrepareRequest {
    /// Stable across reconnects; identifies the transfer for resumption.
    pub transfer_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareUploadResponse {
    pub session_id: String,
    pub files: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ferry: Option<FerryPrepareResponse>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FerryPrepareResponse {
    pub resumable: bool,
    /// Confirmed bytes already received per unfinished file.
    #[serde(default)]
    pub offsets: HashMap<String, u64>,
}

/// Body of a successful upload response (LocalSend senders ignore it).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UploadResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifyRequest {
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifyResponse {
    pub ok: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FerryHello {
    pub v: u32,
    #[serde(default)]
    pub caps: Vec<String>,
    pub device_id: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub app: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TransferStatusResponse {
    pub files: HashMap<String, FileProgressDto>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileProgressDto {
    pub offset: u64,
    pub done: bool,
}

/// LocalSend error body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub message: String,
    /// 416 responses: the receiver's confirmed offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

pub mod lenient_device_kind {
    use super::*;

    pub fn serialize<S: Serializer>(value: &Option<DeviceKind>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(kind) => kind.serialize(s),
            None => s.serialize_none(),
        }
    }

    /// Case-insensitive; unknown values fall back to `desktop` (protocol §7.1).
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DeviceKind>, D::Error> {
        let value = Option::<serde_json::Value>::deserialize(d)?;
        Ok(value.and_then(|v| v.as_str().map(str::to_lowercase)).map(|v| match v.as_str() {
            "mobile" => DeviceKind::Mobile,
            "web" => DeviceKind::Web,
            "headless" => DeviceKind::Headless,
            "server" => DeviceKind::Server,
            _ => DeviceKind::Desktop,
        }))
    }
}

pub mod lenient_protocol {
    use super::*;

    pub fn serialize<S: Serializer>(value: &Option<Protocol>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(p) => s.serialize_str(p.as_str()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Protocol>, D::Error> {
        let value = Option::<serde_json::Value>::deserialize(d)?;
        Ok(value.and_then(|v| v.as_str().map(str::to_lowercase)).and_then(|v| match v.as_str() {
            "http" => Some(Protocol::Http),
            "https" => Some(Protocol::Https),
            _ => None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_lose_spoofing_characters() {
        // "Maya's iPhone" with a right-to-left override and zero-width joiners.
        assert_eq!(clean_alias("Maya\u{2019}s i\u{200D}Phone\u{202E}"), "Maya\u{2019}s iPhone");
        assert_eq!(clean_alias("\u{200B}\u{FEFF} \n"), "Unknown device");
        assert_eq!(clean_alias(&"x".repeat(100)).len(), 64);
        assert_eq!(clean_alias("  Studio PC  "), "Studio PC");
    }

    #[test]
    fn parses_localsend_register_and_tolerates_extensions() {
        let json = r#"{"alias":"Nice Orange","version":"2.0","deviceModel":"Samsung","deviceType":"MOBILE",
                       "fingerprint":"abc","port":53317,"protocol":"https","download":true,"announce":true,"somethingNew":1}"#;
        let dto: DeviceDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.device_type, Some(DeviceKind::Mobile));
        assert_eq!(dto.protocol, Some(Protocol::Https));
        assert_eq!(dto.port, Some(53317));
        assert!(dto.ferry.is_none());
    }

    #[test]
    fn unknown_device_type_falls_back_to_desktop() {
        let dto: DeviceDto = serde_json::from_str(r#"{"alias":"x","version":"2.1","deviceType":"toaster","fingerprint":"f"}"#).unwrap();
        assert_eq!(dto.device_type, Some(DeviceKind::Desktop));
    }

    #[test]
    fn serializes_exactly_like_localsend_plus_hint() {
        let dto = DeviceDto {
            alias: "Laptop".into(),
            version: PROTOCOL_VERSION.into(),
            device_model: None,
            device_type: Some(DeviceKind::Desktop),
            fingerprint: "FP".into(),
            port: Some(53317),
            protocol: Some(Protocol::Https),
            download: false,
            ferry: Some(FerryHint::ours()),
        };
        let v: serde_json::Value = serde_json::to_value(&dto).unwrap();
        assert_eq!(v["deviceType"], "desktop");
        assert_eq!(v["protocol"], "https");
        assert!(v.get("deviceModel").is_none());
        assert_eq!(v["ferry"]["v"], 1);
        // Upstream's strict DTO must accept what we send.
        let upstream: localsend::http::dto_v2::RegisterDtoV2 = serde_json::from_value(v).unwrap();
        assert_eq!(upstream.port, 53317);
    }

    #[test]
    fn prepare_upload_keeps_file_order() {
        let json = r#"{"info":{"alias":"a","version":"2.2","fingerprint":"f","port":1,"protocol":"https"},
            "files":{"z":{"id":"z","fileName":"z.txt","size":1,"fileType":"text/plain"},
                     "a":{"id":"a","fileName":"a.txt","size":2,"fileType":"text/plain"}}}"#;
        let req: PrepareUploadRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.files.keys().collect::<Vec<_>>(), ["z", "a"]);
    }
}
