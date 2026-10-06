// Derived from LocalSend (https://github.com/localsend/localsend, Apache-2.0); modified by the Ferry authors.
//! Wire types of `/v1/ws` (LocalSend-compatible core plus Ferry extensions)
//! and their validation.

use axum::extract::ws::Utf8Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub(crate) const MAX_ALIAS_CHARS: usize = 64;
pub(crate) const MAX_VERSION_CHARS: usize = 32;
pub(crate) const MAX_TOKEN_CHARS: usize = 512;
pub(crate) const MAX_DEVICE_MODEL_CHARS: usize = 64;
pub(crate) const MAX_SESSION_ID_CHARS: usize = 64;
pub(crate) const MAX_SDP_BYTES: usize = 48 * 1024;
/// Serialized size of one ICE candidate (string or object).
pub(crate) const MAX_CANDIDATE_BYTES: usize = 4 * 1024;
pub(crate) const MAX_CAPS: usize = 16;
pub(crate) const MAX_CAP_CHARS: usize = 32;
pub(crate) const MAX_KEY_CHARS: usize = 128;

/// A rejected request: an `ERROR` code plus a short message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Violation {
    pub code: u16,
    pub message: &'static str,
}

impl Violation {
    pub(crate) const fn invalid(message: &'static str) -> Self {
        Self { code: 400, message }
    }

    pub(crate) const fn forbidden(message: &'static str) -> Self {
        Self { code: 403, message }
    }

    pub(crate) const fn too_large(message: &'static str) -> Self {
        Self { code: 413, message }
    }
}

/// `true` if `s` has more than `max` characters (without counting them all).
pub(crate) fn too_long(s: &str, max: usize) -> bool {
    s.len() > max && s.chars().nth(max).is_some()
}

/// Device type, serialized in UPPERCASE as LocalSend's signaling expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum DeviceType {
    Mobile,
    Desktop,
    Web,
    Headless,
    Server,
}

impl DeviceType {
    /// Case-insensitive; unknown values become `DESKTOP` (LocalSend's rule),
    /// so strict clients never see a value they cannot parse.
    fn parse_lenient(value: &str) -> Self {
        const ALL: [(&str, DeviceType); 5] = [
            ("MOBILE", DeviceType::Mobile),
            ("DESKTOP", DeviceType::Desktop),
            ("WEB", DeviceType::Web),
            ("HEADLESS", DeviceType::Headless),
            ("SERVER", DeviceType::Server),
        ];
        ALL.iter().find(|(name, _)| name.eq_ignore_ascii_case(value)).map_or(Self::Desktop, |(_, ty)| *ty)
    }
}

/// `ClientInfoWithoutId` as sent by clients (in `d` and in `UPDATE`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawClientInfo {
    alias: String,
    version: String,
    #[serde(default)]
    device_model: Option<String>,
    #[serde(default)]
    device_type: Option<String>,
    token: String,
    #[serde(default)]
    ext: Option<RawExt>,
}

#[derive(Debug, Deserialize)]
struct RawExt {
    #[serde(default = "default_ext_version")]
    v: u32,
    #[serde(default)]
    caps: Vec<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    nearby: Option<bool>,
}

fn default_ext_version() -> u32 {
    1
}

/// A validated client, as stored by the server and sent to peers.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientInfo {
    pub id: Uuid,
    pub alias: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_type: Option<DeviceType>,
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Ext>,
}

/// Ferry's capability object. Its presence opts a client into extensions.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Ext {
    pub v: u32,
    pub caps: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nearby: Option<bool>,
}

impl RawClientInfo {
    /// Applies the field limits (`413` for oversized fields) and normalizes
    /// `deviceType`. Unknown members are dropped.
    pub(crate) fn validate(self, id: Uuid) -> Result<ClientInfo, Violation> {
        if too_long(&self.alias, MAX_ALIAS_CHARS) {
            return Err(Violation::too_large("alias too long"));
        }
        if too_long(&self.version, MAX_VERSION_CHARS) {
            return Err(Violation::too_large("version too long"));
        }
        if too_long(&self.token, MAX_TOKEN_CHARS) {
            return Err(Violation::too_large("token too long"));
        }
        if self.device_model.as_deref().is_some_and(|m| too_long(m, MAX_DEVICE_MODEL_CHARS)) {
            return Err(Violation::too_large("deviceModel too long"));
        }
        let ext = match self.ext {
            None => None,
            Some(ext) => {
                if ext.caps.len() > MAX_CAPS {
                    return Err(Violation::too_large("too many caps"));
                }
                if ext.caps.iter().any(|c| too_long(c, MAX_CAP_CHARS)) {
                    return Err(Violation::too_large("cap too long"));
                }
                if ext.key.as_deref().is_some_and(|k| too_long(k, MAX_KEY_CHARS)) {
                    return Err(Violation::too_large("key too long"));
                }
                Some(Ext { v: ext.v, caps: ext.caps, key: ext.key, nearby: ext.nearby })
            }
        };
        Ok(ClientInfo {
            id,
            alias: self.alias,
            version: self.version,
            device_model: self.device_model,
            device_type: self.device_type.as_deref().map(DeviceType::parse_lenient),
            token: self.token,
            ext,
        })
    }
}

/// Messages accepted from clients. Unknown members are ignored.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE", rename_all_fields = "camelCase")]
pub(crate) enum ClientMessage {
    Update {
        info: RawClientInfo,
    },
    Offer {
        session_id: String,
        target: Uuid,
        sdp: String,
    },
    Answer {
        session_id: String,
        target: Uuid,
        sdp: String,
    },
    // Ferry extensions below.
    Ice {
        target: Uuid,
        session_id: String,
        /// `None` (`null`) = end of candidates. See [`check_candidate`].
        #[serde(default)]
        candidate: Option<Value>,
    },
    Cancel {
        target: Uuid,
        session_id: String,
    },
    RoomJoin {
        room: String,
    },
    RoomLeave {
        room: String,
    },
    Ping,
}

const KNOWN_TYPES: [&str; 8] = ["UPDATE", "OFFER", "ANSWER", "ICE", "CANCEL", "ROOM_JOIN", "ROOM_LEAVE", "PING"];

impl ClientMessage {
    /// Messages only clients that sent `ext` may use.
    pub(crate) fn is_extension(&self) -> bool {
        !matches!(self, Self::Update { .. } | Self::Offer { .. } | Self::Answer { .. })
    }
}

pub(crate) fn parse_client_message(text: &str) -> Result<ClientMessage, Violation> {
    serde_json::from_str(text).map_err(|_| classify_invalid(text))
}

/// Explains why `text` is not a valid client message (error path only).
fn classify_invalid(text: &str) -> Violation {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Violation::invalid("invalid JSON");
    };
    match value.get("type").and_then(serde_json::Value::as_str) {
        None => Violation::invalid("missing message type"),
        Some(ty) if KNOWN_TYPES.contains(&ty) => Violation::invalid("invalid message"),
        Some(_) => Violation::invalid("unknown message type"),
    }
}

/// An ICE candidate is relayed verbatim when it is a string (the SDP
/// `candidate:` line) or a flat object (`RTCIceCandidateInit`: `candidate`,
/// `sdpMid`, `sdpMLineIndex`, `usernameFragment`) of at most
/// [`MAX_CANDIDATE_BYTES`] serialized.
pub(crate) fn check_candidate(candidate: &Value) -> Result<(), Violation> {
    let shape_ok = match candidate {
        Value::String(_) => true,
        Value::Object(fields) => fields.values().all(|v| !v.is_array() && !v.is_object()),
        _ => false,
    };
    if !shape_ok {
        return Err(Violation::invalid("invalid candidate"));
    }
    // Bounded by the frame size limit.
    if serde_json::to_string(candidate).map_or(true, |s| s.len() > MAX_CANDIDATE_BYTES) {
        return Err(Violation::too_large("candidate too large"));
    }
    Ok(())
}

/// `r:` + 16 to 64 base64url characters (link/QR rooms) or `c:` + 6 digits
/// (short codes).
pub(crate) fn is_valid_room_id(room: &str) -> bool {
    if let Some(rest) = room.strip_prefix("r:") {
        (16..=64).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    } else if let Some(rest) = room.strip_prefix("c:") {
        rest.len() == 6 && rest.bytes().all(|b| b.is_ascii_digit())
    } else {
        false
    }
}

/// `HELLO.server` for clients that sent `ext`.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ServerInfo {
    pub v: u32,
    pub caps: Vec<&'static str>,
}

/// Messages sent to clients.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE", rename_all_fields = "camelCase")]
pub(crate) enum ServerMessage<'a> {
    Hello {
        client: &'a ClientInfo,
        peers: Vec<&'a ClientInfo>,
        #[serde(skip_serializing_if = "Option::is_none")]
        server: Option<&'a ServerInfo>,
    },
    Join {
        peer: &'a ClientInfo,
    },
    Update {
        peer: &'a ClientInfo,
    },
    Left {
        peer_id: Uuid,
    },
    Offer {
        peer: &'a ClientInfo,
        session_id: &'a str,
        sdp: &'a str,
    },
    Answer {
        peer: &'a ClientInfo,
        session_id: &'a str,
        sdp: &'a str,
    },
    Error {
        code: u16,
        message: &'a str,
        /// Correlation for errors caused by a relay message.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<&'a str>,
        /// Correlation for errors caused by a room message.
        #[serde(skip_serializing_if = "Option::is_none")]
        room: Option<&'a str>,
    },
    // Ferry extensions below; only ever sent to clients that sent `ext`.
    RoomHello {
        room: &'a str,
        peers: Vec<&'a ClientInfo>,
    },
    RoomPeerJoined {
        room: &'a str,
        peer: &'a ClientInfo,
    },
    RoomPeerLeft {
        room: &'a str,
        peer_id: Uuid,
    },
    Ice {
        peer: &'a ClientInfo,
        session_id: &'a str,
        candidate: Option<&'a Value>,
    },
    Cancel {
        peer: &'a ClientInfo,
        session_id: &'a str,
    },
    Pong,
}

/// Serializes a server message into a WebSocket text payload.
pub(crate) fn encode(message: &ServerMessage<'_>) -> Utf8Bytes {
    // These types contain only strings, numbers and sequences: infallible.
    Utf8Bytes::from(serde_json::to_string(message).expect("server message serializes"))
}

/// An `ERROR` message without correlation fields.
pub(crate) fn error_json(code: u16, message: &str) -> Utf8Bytes {
    encode(&ServerMessage::Error { code, message, session_id: None, room: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(value: serde_json::Value) -> RawClientInfo {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn device_type_is_normalized() {
        let info =
            |ty: &str| raw(json!({"alias":"a","version":"2.1","token":"t","deviceType":ty})).validate(Uuid::nil()).unwrap().device_type;
        assert_eq!(info("MOBILE"), Some(DeviceType::Mobile));
        assert_eq!(info("mobile"), Some(DeviceType::Mobile));
        assert_eq!(info("Headless"), Some(DeviceType::Headless));
        assert_eq!(info("TABLET"), Some(DeviceType::Desktop));
        let none = raw(json!({"alias":"a","version":"2.1","token":"t"})).validate(Uuid::nil()).unwrap();
        assert_eq!(none.device_type, None);
    }

    #[test]
    fn client_info_serializes_like_localsend() {
        let info = raw(json!({
            "alias": "Cute Apple", "version": "2.3", "deviceModel": "Dell",
            "deviceType": "DESKTOP", "token": "123", "unknown": 1
        }))
        .validate(Uuid::nil())
        .unwrap();
        assert_eq!(
            serde_json::to_string(&ServerMessage::Hello { client: &info, peers: vec![], server: None }).unwrap(),
            r#"{"type":"HELLO","client":{"id":"00000000-0000-0000-0000-000000000000","alias":"Cute Apple","version":"2.3","deviceModel":"Dell","deviceType":"DESKTOP","token":"123"},"peers":[]}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMessage::Left { peer_id: Uuid::nil() }).unwrap(),
            r#"{"type":"LEFT","peerId":"00000000-0000-0000-0000-000000000000"}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMessage::RoomPeerLeft { room: "c:123456", peer_id: Uuid::nil() }).unwrap(),
            r#"{"type":"ROOM_PEER_LEFT","room":"c:123456","peerId":"00000000-0000-0000-0000-000000000000"}"#
        );
        assert!(
            serde_json::to_string(&ServerMessage::Ice { peer: &info, session_id: "s", candidate: None })
                .unwrap()
                .ends_with(r#""sessionId":"s","candidate":null}"#)
        );
        assert_eq!(error_json(404, "unknown target").as_str(), r#"{"type":"ERROR","code":404,"message":"unknown target"}"#);
    }

    #[test]
    fn ext_is_kept_and_limited() {
        let info = raw(json!({
            "alias": "a", "version": "2.1", "token": "t",
            "ext": {"v": 1, "caps": ["rooms", "trickle"], "key": "abc", "nearby": false, "x": 1}
        }))
        .validate(Uuid::nil())
        .unwrap();
        let ext = info.ext.unwrap();
        assert_eq!(ext.caps, ["rooms", "trickle"]);
        assert_eq!(ext.nearby, Some(false));

        let too_many = raw(json!({
            "alias": "a", "version": "2.1", "token": "t",
            "ext": {"v": 1, "caps": vec!["c"; 17]}
        }));
        assert_eq!(too_many.validate(Uuid::nil()).unwrap_err().code, 413);
        let long_alias = raw(json!({"alias": "é".repeat(65), "version": "2.1", "token": "t"}));
        assert_eq!(long_alias.validate(Uuid::nil()).unwrap_err().code, 413);
        let ok_alias = raw(json!({"alias": "é".repeat(64), "version": "2.1", "token": "t"}));
        assert!(ok_alias.validate(Uuid::nil()).is_ok());
    }

    #[test]
    fn parses_client_messages() {
        let target = Uuid::new_v4();
        let msg = parse_client_message(&format!(r#"{{"type":"ICE","target":"{target}","sessionId":"s","candidate":null}}"#)).unwrap();
        assert!(matches!(msg, ClientMessage::Ice { candidate: None, .. }));
        assert!(msg.is_extension());
        let msg = parse_client_message(&format!(r#"{{"type":"OFFER","target":"{target}","sessionId":"s","sdp":"x"}}"#)).unwrap();
        assert!(!msg.is_extension());
        assert!(matches!(parse_client_message(r#"{"type":"PING"}"#), Ok(ClientMessage::Ping)));

        assert_eq!(parse_client_message("nope").unwrap_err().message, "invalid JSON");
        assert_eq!(parse_client_message("{}").unwrap_err().message, "missing message type");
        assert_eq!(parse_client_message(r#"{"type":"NOPE"}"#).unwrap_err().message, "unknown message type");
        assert_eq!(parse_client_message(r#"{"type":"OFFER","target":"x"}"#).unwrap_err().message, "invalid message");
    }

    #[test]
    fn candidates() {
        let init = json!({
            "candidate": "candidate:1 1 udp 2122260223 192.0.2.1 54321 typ host",
            "sdpMid": "0", "sdpMLineIndex": 0, "usernameFragment": null
        });
        assert!(check_candidate(&init).is_ok());
        assert!(check_candidate(&json!("candidate:1 1 udp 1 192.0.2.1 1 typ host")).is_ok());
        assert_eq!(check_candidate(&json!(42)).unwrap_err().code, 400);
        assert_eq!(check_candidate(&json!(["a"])).unwrap_err().code, 400);
        assert_eq!(check_candidate(&json!({"candidate": {"x": 1}})).unwrap_err().code, 400);
        let big = json!({ "candidate": "a".repeat(MAX_CANDIDATE_BYTES) });
        assert_eq!(check_candidate(&big).unwrap_err().code, 413);

        // Objects are relayed as objects.
        let msg =
            parse_client_message(&format!(r#"{{"type":"ICE","target":"{}","sessionId":"s","candidate":{init}}}"#, Uuid::nil())).unwrap();
        let ClientMessage::Ice { candidate: Some(c), .. } = msg else {
            panic!("not an ICE message with a candidate");
        };
        assert_eq!(c, init);
    }

    #[test]
    fn room_ids() {
        assert!(is_valid_room_id("r:abcdefghijklmnop"));
        assert!(is_valid_room_id(&format!("r:{}", "A-_9".repeat(16))));
        assert!(!is_valid_room_id(&format!("r:{}", "a".repeat(65))));
        assert!(!is_valid_room_id("r:abcdefghijklmno"));
        assert!(!is_valid_room_id("r:abcdefghijklmno="));
        assert!(is_valid_room_id("c:012345"));
        assert!(!is_valid_room_id("c:12345"));
        assert!(!is_valid_room_id("c:1234567"));
        assert!(!is_valid_room_id("c:12345a"));
        assert!(!is_valid_room_id("x:012345"));
        assert!(!is_valid_room_id(""));
    }
}
