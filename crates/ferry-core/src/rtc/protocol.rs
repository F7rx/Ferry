//! `ferry-dc/1` data-channel messages (docs/05-protocol.md §5.2), byte-for-byte
//! compatible with the TypeScript reference (`apps/app/src/lib/rtc/protocol.ts`).
//!
//! Text frames carry the JSON control messages below; binary frames carry the
//! bytes of the file currently being streamed. Every inbound control frame is
//! validated strictly (types, lengths, ranges, file names) before a session
//! acts on it; unknown members are dropped, unknown message types rejected.
//!
//! Lengths of strings follow the reference exactly: limits written as
//! "characters" count UTF-16 code units (JavaScript `string.length`), limits in
//! bytes count UTF-8 bytes. Outbound JSON is produced with the same member order
//! and escaping as `JSON.stringify`, so frames are identical on both sides.

use serde_json::Value;
use std::collections::HashSet;
use unicode_general_category::{GeneralCategory, get_general_category};

pub const DC_LABEL: &str = "ferry/1";
pub const DC_VERSION: u64 = 1;

/// Limits (05-protocol.md §5.2).
pub const MAX_CONTROL_BYTES: usize = 64 * 1024;
pub const MAX_FILES: usize = 10_000;
/// All frames of one (split) offer together.
pub const MAX_OFFER_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_NAME_LENGTH: usize = 1024;
pub const MAX_PATH_DEPTH: usize = 32;
pub const MAX_COMPONENT_BYTES: usize = 255;
pub const MAX_ID_LENGTH: usize = 256;
pub const MAX_MIME_LENGTH: usize = 255;
pub const MAX_REASON_LENGTH: usize = 1024;
/// Offer `text`, measured as its JSON string literal in UTF-8.
pub const MAX_TEXT_BYTES: usize = 60 * 1024;
pub const MAX_CODE_LENGTH: usize = 64;
pub const MAX_ALIAS_LENGTH: usize = 256;
pub const MAX_DEVICE_TYPE_LENGTH: usize = 32;
pub const MAX_PLATFORM_LENGTH: usize = 64;
pub const MAX_CAPS: usize = 64;
pub const MAX_CAP_LENGTH: usize = 64;
/// Largest binary frame accepted (Chrome's SCTP limit); senders use ≤ 64 KiB.
pub const MAX_BINARY_FRAME: usize = 256 * 1024;
pub const NONCE_LENGTH: usize = 32;
pub const MAC_LENGTH: usize = 32;
pub const SIGNATURE_LENGTH: usize = 64;

/// Chunking, backpressure and flow control.
pub const LARGE_CHUNK: usize = 64 * 1024;
pub const SMALL_CHUNK: usize = 16 * 1024;
pub const BUFFER_HIGH_WATER: usize = 1024 * 1024;
pub const BUFFER_LOW_WATER: usize = 256 * 1024;
/// File bytes a sender may have in flight beyond the receiver's last `progress`.
pub const RECV_WINDOW: u64 = 16 * 1024 * 1024;
/// Receivers report `progress` at least every this many processed bytes.
pub const PROGRESS_STEP: u64 = 1024 * 1024;

/// JavaScript's `Number.MAX_SAFE_INTEGER`.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// An error with a machine-readable code, as the reference's `RtcError`.
/// Codes: `protocol`, `auth`, `too-large`, `invalid`, `invalid-state`,
/// `cancelled`, `closed`, `timeout`, `source`, `no-sink`, `no-resume`,
/// `overrun`, `webrtc`, `rejected`, `internal`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcError {
    pub code: String,
    pub message: String,
}

impl RtcError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        RtcError { code: code.to_string(), message: message.into() }
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new("protocol", message)
    }
}

impl std::fmt::Display for RtcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RtcError {}

// ── Messages ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub alias: String,
    pub device_type: String,
    pub platform: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    pub id: String,
    /// Relative path, `/`-separated (`folder/sub/file.ext`); see [`file_name_problem`].
    pub name: String,
    pub size: u64,
    pub mime: String,
    /// Last modification time, milliseconds since the Unix epoch.
    pub modified: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alg {
    Ed25519,
    P256,
}

impl Alg {
    pub fn as_str(self) -> &'static str {
        match self {
            Alg::Ed25519 => "ed25519",
            Alg::P256 => "p256",
        }
    }

    pub fn parse(s: &str) -> Option<Alg> {
        match s {
            "ed25519" => Some(Alg::Ed25519),
            "p256" => Some(Alg::P256),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub alg: Alg,
    /// Raw public key, base64url.
    pub key: String,
    /// 32 random bytes, base64url.
    pub nonce: String,
    pub device: DeviceInfo,
    pub caps: Vec<String>,
}

/// One frame of an offer (all but the last carry `more`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub transfer_id: String,
    pub files: Vec<FileMeta>,
    pub text: Option<String>,
    pub more: bool,
}

/// One frame of an answer. `offsets` only name ids accepted in the same frame,
/// in the order they were given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub transfer_id: String,
    pub accept: Vec<String>,
    pub offsets: Vec<(String, u64)>,
    pub declined: bool,
    pub more: bool,
}

impl Answer {
    pub fn offset_of(&self, id: &str) -> u64 {
        self.offsets.iter().rev().find(|(k, _)| k == id).map(|(_, v)| *v).unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    Hello(Hello),
    Auth { sig: String, mac: Option<String> },
    Offer(Offer),
    Answer(Answer),
    File { id: String, offset: u64 },
    FileEnd { id: String, sha256: String },
    FileAck { id: String, ok: bool, sha256: Option<String>, error: Option<String> },
    Progress { transfer_id: String, bytes: u64 },
    Done { transfer_id: String },
    Cancel { transfer_id: String, reason: Option<String> },
    Ping,
    Pong,
    Error { code: String, message: String },
}

impl Control {
    pub fn kind(&self) -> &'static str {
        match self {
            Control::Hello(_) => "hello",
            Control::Auth { .. } => "auth",
            Control::Offer(_) => "offer",
            Control::Answer(_) => "answer",
            Control::File { .. } => "file",
            Control::FileEnd { .. } => "file-end",
            Control::FileAck { .. } => "file-ack",
            Control::Progress { .. } => "progress",
            Control::Done { .. } => "done",
            Control::Cancel { .. } => "cancel",
            Control::Ping => "ping",
            Control::Pong => "pong",
            Control::Error { .. } => "error",
        }
    }

    /// The JSON text exactly as `JSON.stringify` writes the reference's object.
    pub fn to_json(&self) -> String {
        let mut o = String::with_capacity(64);
        o.push_str("{\"t\":");
        o.push_str(&q(self.kind()));
        match self {
            Control::Hello(h) => {
                o.push_str(",\"v\":1,\"alg\":");
                o.push_str(&q(h.alg.as_str()));
                field(&mut o, "key", &q(&h.key));
                field(&mut o, "nonce", &q(&h.nonce));
                o.push_str(",\"device\":{\"alias\":");
                o.push_str(&q(&h.device.alias));
                field(&mut o, "deviceType", &q(&h.device.device_type));
                field(&mut o, "platform", &q(&h.device.platform));
                o.push_str("},\"caps\":[");
                o.push_str(&h.caps.iter().map(|c| q(c)).collect::<Vec<_>>().join(","));
                o.push(']');
            }
            Control::Auth { sig, mac } => {
                field(&mut o, "sig", &q(sig));
                if let Some(mac) = mac {
                    field(&mut o, "mac", &q(mac));
                }
            }
            Control::Offer(offer) => {
                field(&mut o, "transferId", &q(&offer.transfer_id));
                o.push_str(",\"files\":[");
                o.push_str(&offer.files.iter().map(file_json).collect::<Vec<_>>().join(","));
                o.push(']');
                if let Some(text) = &offer.text {
                    field(&mut o, "text", &q(text));
                }
                if offer.more {
                    o.push_str(",\"more\":true");
                }
            }
            Control::Answer(a) => {
                field(&mut o, "transferId", &q(&a.transfer_id));
                o.push_str(",\"accept\":[");
                o.push_str(&a.accept.iter().map(|c| q(c)).collect::<Vec<_>>().join(","));
                o.push_str("],\"offsets\":");
                o.push_str(&js_object_numbers(&a.offsets));
                if a.declined {
                    o.push_str(",\"declined\":true");
                }
                if a.more {
                    o.push_str(",\"more\":true");
                }
            }
            Control::File { id, offset } => {
                field(&mut o, "id", &q(id));
                field(&mut o, "offset", &offset.to_string());
            }
            Control::FileEnd { id, sha256 } => {
                field(&mut o, "id", &q(id));
                field(&mut o, "sha256", &q(sha256));
            }
            Control::FileAck { id, ok, sha256, error } => {
                field(&mut o, "id", &q(id));
                field(&mut o, "ok", if *ok { "true" } else { "false" });
                if let Some(h) = sha256 {
                    field(&mut o, "sha256", &q(h));
                }
                if let Some(e) = error {
                    field(&mut o, "error", &q(e));
                }
            }
            Control::Progress { transfer_id, bytes } => {
                field(&mut o, "transferId", &q(transfer_id));
                field(&mut o, "bytes", &bytes.to_string());
            }
            Control::Done { transfer_id } => field(&mut o, "transferId", &q(transfer_id)),
            Control::Cancel { transfer_id, reason } => {
                field(&mut o, "transferId", &q(transfer_id));
                if let Some(r) = reason {
                    field(&mut o, "reason", &q(r));
                }
            }
            Control::Ping | Control::Pong => {}
            Control::Error { code, message } => {
                field(&mut o, "code", &q(code));
                field(&mut o, "message", &q(message));
            }
        }
        o.push('}');
        o
    }
}

/// Serializes a control message, enforcing the 64 KiB frame limit.
pub fn encode_control(msg: &Control) -> Result<String, RtcError> {
    let text = msg.to_json();
    if text.len() > MAX_CONTROL_BYTES {
        return Err(RtcError::new("too-large", format!("\"{}\" message exceeds {MAX_CONTROL_BYTES} bytes", msg.kind())));
    }
    Ok(text)
}

/// An `error` message with code and message clipped to the limits the peer enforces.
pub fn error_frame(code: &str, message: &str) -> Control {
    let code = clip_utf16(code, MAX_CODE_LENGTH);
    Control::Error { code: if code.is_empty() { "internal".to_string() } else { code }, message: clip_utf16(message, MAX_REASON_LENGTH) }
}

/// Encodes an offer as one or more frames of at most 64 KiB each: files are
/// packed greedily, `text` goes into the first frame, every frame but the last
/// carries `more`. Fails with `too-large` beyond [`MAX_OFFER_BYTES`].
pub fn encode_offer(transfer_id: &str, files: &[FileMeta], text: Option<&str>) -> Result<Vec<String>, RtcError> {
    let head = |first: bool, more: bool, items: Vec<FileMeta>| {
        Control::Offer(Offer {
            transfer_id: transfer_id.to_string(),
            files: items,
            text: if first { text.map(str::to_string) } else { None },
            more,
        })
    };
    let frames =
        pack(files, |f| file_json(f).len(), |first, more, items| head(first, more, items.to_vec()), |first| head(first, true, Vec::new()))?;
    check_total(&frames, "offer")?;
    Ok(frames)
}

/// Encodes an answer (see [`encode_offer`]); each accepted id keeps its offset in its frame.
pub fn encode_answer(transfer_id: &str, accept: &[String], offsets: &[(String, u64)], declined: bool) -> Result<Vec<String>, RtcError> {
    if declined {
        let msg = Control::Answer(Answer {
            transfer_id: transfer_id.to_string(),
            accept: Vec::new(),
            offsets: Vec::new(),
            declined: true,
            more: false,
        });
        return Ok(vec![encode_control(&msg)?]);
    }
    let offset_of = |id: &str| offsets.iter().rev().find(|(k, _)| k == id).map(|(_, v)| *v).unwrap_or(0);
    let build = |more: bool, ids: &[String]| {
        Control::Answer(Answer {
            transfer_id: transfer_id.to_string(),
            accept: ids.to_vec(),
            offsets: ids.iter().filter(|id| offset_of(id) > 0).map(|id| (id.clone(), offset_of(id))).collect(),
            declined: false,
            more,
        })
    };
    let cost = |id: &String| {
        let n = q(id).len();
        let off = offset_of(id);
        if off > 0 { n + 1 + q(id).len() + 1 + off.to_string().len() } else { n }
    };
    let frames = pack(accept, cost, |_first, more, ids| build(more, ids), |_| build(true, &[]))?;
    check_total(&frames, "answer")?;
    Ok(frames)
}

/// Greedy packing of `items` into frames ≤ MAX_CONTROL_BYTES (the reference's `pack`).
fn pack<T>(
    items: &[T],
    cost: impl Fn(&T) -> usize,
    build: impl Fn(bool, bool, &[T]) -> Control,
    empty: impl Fn(bool) -> Control,
) -> Result<Vec<String>, RtcError> {
    let mut frames = Vec::new();
    let mut first = true;
    let empty_first = empty(true);
    let Some(mut budget) = MAX_CONTROL_BYTES.checked_sub(empty_first.to_json().len()) else {
        return Err(RtcError::new("too-large", format!("\"{}\" message exceeds {MAX_CONTROL_BYTES} bytes", empty_first.kind())));
    };
    let mut start = 0;
    let mut used = 0;
    for (i, item) in items.iter().enumerate() {
        let size = cost(item) + 1;
        if i > start && used + size > budget {
            frames.push(encode_control(&build(first, true, &items[start..i]))?);
            first = false;
            start = i;
            used = 0;
            budget = MAX_CONTROL_BYTES.saturating_sub(empty(false).to_json().len());
        }
        used += size;
    }
    frames.push(encode_control(&build(first, false, &items[start..]))?);
    Ok(frames)
}

fn check_total(frames: &[String], what: &str) -> Result<(), RtcError> {
    let total: usize = frames.iter().map(String::len).sum();
    if total > MAX_OFFER_BYTES {
        return Err(RtcError::new("too-large", format!("{what} exceeds {MAX_OFFER_BYTES} bytes")));
    }
    Ok(())
}

// ── Parsing ───────────────────────────────────────────────────────────────

/// Parses and validates an inbound control frame. Errors have code `protocol`.
pub fn parse_control(text: &str) -> Result<Control, RtcError> {
    if text.len() > MAX_CONTROL_BYTES {
        return Err(invalid("control message too large"));
    }
    let raw: Value = serde_json::from_str(text).map_err(|_| invalid("control message is not JSON"))?;
    let o = obj(&raw, "message")?;
    let t = o.get("t").and_then(Value::as_str).unwrap_or("");
    Ok(match t {
        "hello" => Control::Hello(parse_hello(o)?),
        "auth" => Control::Auth {
            sig: b64(o.get("sig"), "sig", SIGNATURE_LENGTH)?,
            mac: match o.get("mac") {
                None => None,
                Some(v) => Some(b64(Some(v), "mac", MAC_LENGTH)?),
            },
        },
        "offer" => Control::Offer(parse_offer(o)?),
        "answer" => Control::Answer(parse_answer(o)?),
        "file" => Control::File { id: id(o.get("id"), "id")?, offset: size(o.get("offset"), "offset")? },
        "file-end" => Control::FileEnd { id: id(o.get("id"), "id")?, sha256: hash(o.get("sha256"), "sha256")? },
        "file-ack" => {
            let Some(ok) = o.get("ok").and_then(Value::as_bool) else {
                return Err(invalid("ok must be a boolean"));
            };
            Control::FileAck {
                id: id(o.get("id"), "id")?,
                ok,
                sha256: match o.get("sha256") {
                    None => None,
                    v => Some(hash(v, "sha256")?),
                },
                error: match o.get("error") {
                    None => None,
                    v => Some(string(v, "error", 0, MAX_REASON_LENGTH)?),
                },
            }
        }
        "progress" => Control::Progress { transfer_id: id(o.get("transferId"), "transferId")?, bytes: size(o.get("bytes"), "bytes")? },
        "done" => Control::Done { transfer_id: id(o.get("transferId"), "transferId")? },
        "cancel" => Control::Cancel {
            transfer_id: id(o.get("transferId"), "transferId")?,
            reason: match o.get("reason") {
                None => None,
                v => Some(string(v, "reason", 0, MAX_REASON_LENGTH)?),
            },
        },
        "ping" => Control::Ping,
        "pong" => Control::Pong,
        "error" => Control::Error {
            code: string(o.get("code"), "code", 1, MAX_CODE_LENGTH)?,
            message: string(o.get("message"), "message", 0, MAX_REASON_LENGTH)?,
        },
        _ => {
            let shown = match o.get("t") {
                Some(Value::String(s)) => s.chars().take(38).collect::<String>(),
                Some(other) => other.to_string().chars().take(38).collect(),
                None => "undefined".into(),
            };
            return Err(invalid(format!("unknown message type \"{shown}\"")));
        }
    })
}

/// Validates the files of an outgoing or incoming offer (count, ids, names, sizes).
pub fn validate_files(files: &[Value]) -> Result<Vec<FileMeta>, RtcError> {
    if files.len() > MAX_FILES {
        return Err(invalid(format!("too many files (max {MAX_FILES})")));
    }
    let mut seen = HashSet::new();
    let mut total: u64 = 0;
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let o = obj(f, "file")?;
        let name = string(o.get("name"), "file name", 1, MAX_NAME_LENGTH)?;
        if let Some(problem) = file_name_problem(&name) {
            return Err(invalid(format!("file name {problem}")));
        }
        let mime = string(o.get("mime"), "mime", 0, MAX_MIME_LENGTH)?;
        if mime.chars().any(is_c0_c1) {
            return Err(invalid("mime contains control characters"));
        }
        let meta = FileMeta {
            id: id(o.get("id"), "file id")?,
            name,
            size: size(o.get("size"), "file size")?,
            mime,
            modified: match o.get("modified") {
                None => None,
                Some(v) => Some(safe_integer(v).ok_or_else(|| invalid("modified must be an integer"))?),
            },
        };
        if !seen.insert(meta.id.clone()) {
            return Err(invalid(format!("duplicate file id {}", q(&meta.id))));
        }
        total += meta.size;
        if total > MAX_SAFE_INTEGER {
            return Err(invalid("total size too large"));
        }
        out.push(meta);
    }
    Ok(out)
}

/// Checks the outgoing side of an offer (the same rules a receiver applies).
pub fn validate_metas(files: &[FileMeta]) -> Result<(), RtcError> {
    let values: Vec<Value> = files.iter().map(|f| serde_json::from_str(&file_json(f)).unwrap_or(Value::Null)).collect();
    validate_files(&values).map(|_| ())
}

/// Why `name` is not an acceptable relative path, or `None` when it is.
/// Rejects (threat model F1/F2): control characters including NUL, backslashes,
/// absolute paths, drive prefixes, empty / `.` / `..` / all-dot components
/// (judged after removing invisible characters), more than [`MAX_PATH_DEPTH`]
/// components and components over 255 UTF-8 bytes. Rust strings cannot hold
/// lone surrogates; frames containing them fail JSON parsing instead.
pub fn file_name_problem(name: &str) -> Option<&'static str> {
    let len = js_len(name);
    if !(1..=MAX_NAME_LENGTH).contains(&len) {
        return Some("length out of range");
    }
    if name.chars().any(is_c0_c1) {
        return Some("contains control characters");
    }
    if name.contains('\\') {
        return Some("contains a backslash");
    }
    if name.starts_with('/') {
        return Some("is an absolute path");
    }
    let b = name.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Some("has a drive prefix");
    }
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() > MAX_PATH_DEPTH {
        return Some("is nested too deeply");
    }
    for part in parts {
        let visible: String = part.chars().filter(|c| !is_invisible(*c)).collect();
        let visible = visible.trim_matches(is_js_space);
        if visible.is_empty() {
            return Some("has an empty path component");
        }
        if visible.chars().all(|c| c == '.') {
            return Some("has a '.' or '..' component");
        }
        if part.len() > MAX_COMPONENT_BYTES {
            return Some("has a component longer than 255 bytes");
        }
    }
    None
}

/// Transfer and file ids: 1 to 256 printable ASCII characters, no spaces.
pub fn is_valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_LENGTH && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Remote `a=max-message-size` as the reference reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaxMessageSize {
    Limit(u64),
    Unlimited,
}

/// Chunk size for binary frames: 64 KiB when the remote `max-message-size`
/// allows it (unlimited counts), otherwise 16 KiB. Unknown → 16 KiB.
pub fn chunk_size_for(max: Option<MaxMessageSize>) -> usize {
    match max {
        None => SMALL_CHUNK,
        Some(MaxMessageSize::Unlimited) => LARGE_CHUNK,
        Some(MaxMessageSize::Limit(0)) => LARGE_CHUNK,
        Some(MaxMessageSize::Limit(n)) if n >= LARGE_CHUNK as u64 => LARGE_CHUNK,
        Some(_) => SMALL_CHUNK,
    }
}

/// Reads `a=max-message-size` from an SDP (first well-formed line). Per RFC
/// 8841 an absent attribute means 65536 and 0 means "no limit".
pub fn parse_max_message_size(sdp: &str) -> MaxMessageSize {
    for line in sdp_lines(sdp) {
        let Some(rest) = line.strip_prefix("a=max-message-size:") else { continue };
        let digits_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if digits_end == 0 || !rest[digits_end..].chars().all(is_js_space) {
            continue;
        }
        let n = rest[..digits_end].bytes().fold(0u64, |n, d| n.saturating_mul(10).saturating_add(u64::from(d - b'0')));
        return if n == 0 { MaxMessageSize::Unlimited } else { MaxMessageSize::Limit(n) };
    }
    MaxMessageSize::Limit(65536)
}

/// Lines of an SDP the way a JavaScript `/m` regex sees them (`\r`, `\n`,
/// U+2028 and U+2029 all end a line).
pub(crate) fn sdp_lines(sdp: &str) -> impl Iterator<Item = &str> {
    sdp.split(['\r', '\n', '\u{2028}', '\u{2029}'])
}

// ── JSON helpers (JSON.stringify-compatible) ──────────────────────────────

/// A JSON string literal (same escaping as `JSON.stringify`).
pub(crate) fn q(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

fn field(o: &mut String, name: &str, value: &str) {
    o.push_str(",\"");
    o.push_str(name);
    o.push_str("\":");
    o.push_str(value);
}

pub(crate) fn file_json(f: &FileMeta) -> String {
    let mut o = String::with_capacity(64 + f.name.len());
    o.push_str("{\"id\":");
    o.push_str(&q(&f.id));
    field(&mut o, "name", &q(&f.name));
    field(&mut o, "size", &f.size.to_string());
    field(&mut o, "mime", &q(&f.mime));
    if let Some(m) = f.modified {
        field(&mut o, "modified", &m.to_string());
    }
    o.push('}');
    o
}

/// A JavaScript object literal of numbers, with JavaScript's own-key order:
/// array-index keys ascending first, then the others in insertion order.
fn js_object_numbers(entries: &[(String, u64)]) -> String {
    let mut seen = HashSet::new();
    let mut unique: Vec<&(String, u64)> = Vec::new();
    for e in entries.iter().rev() {
        if seen.insert(&e.0) {
            unique.push(e);
        }
    }
    unique.reverse();
    let (mut index, other): (Vec<_>, Vec<_>) = unique.into_iter().partition(|(k, _)| array_index(k).is_some());
    index.sort_by_key(|(k, _)| array_index(k));
    let body: Vec<String> = index.into_iter().chain(other).map(|(k, v)| format!("{}:{v}", q(k))).collect();
    format!("{{{}}}", body.join(","))
}

fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse::<u32>().ok().filter(|n| *n != u32::MAX)
}

/// JavaScript `string.length`: UTF-16 code units.
pub fn js_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.slice(0, max)` in UTF-16 units, never splitting a surrogate pair.
pub fn clip_utf16(s: &str, max: usize) -> String {
    let mut n = 0;
    let mut out = String::new();
    for c in s.chars() {
        n += c.len_utf16();
        if n > max {
            break;
        }
        out.push(c);
    }
    out
}

/// JavaScript `\s` (also what `String.prototype.trim` strips).
pub(crate) fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

fn is_c0_c1(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1F}' | '\u{7F}'..='\u{9F}')
}

/// `\p{Cc}\p{Cf}\p{Zl}\p{Zp}`: characters a user cannot see.
fn is_invisible(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::Control | GeneralCategory::Format | GeneralCategory::LineSeparator | GeneralCategory::ParagraphSeparator
    )
}

// ── Validation helpers ────────────────────────────────────────────────────

type Obj = serde_json::Map<String, Value>;

fn invalid(message: impl Into<String>) -> RtcError {
    RtcError::protocol(message)
}

fn obj<'a>(value: &'a Value, what: &str) -> Result<&'a Obj, RtcError> {
    value.as_object().ok_or_else(|| invalid(format!("{what} must be an object")))
}

fn string(value: Option<&Value>, what: &str, min: usize, max: usize) -> Result<String, RtcError> {
    let Some(Value::String(s)) = value else {
        return Err(invalid(format!("{what} must be a string")));
    };
    let len = js_len(s);
    if len < min || len > max {
        return Err(invalid(format!("{what} length out of range")));
    }
    Ok(s.clone())
}

fn id(value: Option<&Value>, what: &str) -> Result<String, RtcError> {
    match value {
        Some(Value::String(s)) if is_valid_id(s) => Ok(s.clone()),
        _ => Err(invalid(format!("{what} must be 1-{MAX_ID_LENGTH} printable ASCII characters"))),
    }
}

/// `Number.isSafeInteger` for a JSON number.
fn safe_integer(v: &Value) -> Option<i64> {
    let n = v.as_number()?;
    if let Some(i) = n.as_i64() {
        return (i.unsigned_abs() <= MAX_SAFE_INTEGER).then_some(i);
    }
    if let Some(u) = n.as_u64() {
        return (u <= MAX_SAFE_INTEGER).then_some(u as i64);
    }
    let f = n.as_f64()?;
    (f.is_finite() && f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64).then_some(f as i64)
}

fn size(value: Option<&Value>, what: &str) -> Result<u64, RtcError> {
    match value.and_then(safe_integer) {
        Some(n) if n >= 0 => Ok(n as u64),
        _ => Err(invalid(format!("{what} must be a non-negative safe integer"))),
    }
}

fn hash(value: Option<&Value>, what: &str) -> Result<String, RtcError> {
    match value {
        Some(Value::String(s)) if s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) => Ok(s.clone()),
        _ => Err(invalid(format!("{what} must be lowercase hex SHA-256"))),
    }
}

fn b64(value: Option<&Value>, what: &str, length: usize) -> Result<String, RtcError> {
    match value {
        Some(Value::String(s)) if super::b64::decode(s).is_some_and(|b| b.len() == length) => Ok(s.clone()),
        _ => Err(invalid(format!("{what} must be {length} bytes of base64url"))),
    }
}

fn strings(value: Option<&Value>, what: &str, max_items: usize, max_len: usize) -> Result<Vec<String>, RtcError> {
    match value {
        Some(Value::Array(items)) if items.len() <= max_items => items.iter().map(|v| string(Some(v), what, 0, max_len)).collect(),
        _ => Err(invalid(format!("{what} must be an array of at most {max_items} items"))),
    }
}

fn more(o: &Obj) -> Result<bool, RtcError> {
    match o.get("more") {
        None => Ok(false),
        Some(Value::Bool(true)) => Ok(true),
        Some(_) => Err(invalid("more must be true when present")),
    }
}

fn parse_hello(o: &Obj) -> Result<Hello, RtcError> {
    let v = o.get("v").and_then(safe_integer);
    if v != Some(DC_VERSION as i64) || !o.get("v").is_some_and(Value::is_number) {
        return Err(invalid("unsupported protocol version"));
    }
    let Some(alg) = o.get("alg").and_then(Value::as_str).and_then(Alg::parse) else {
        return Err(invalid("unsupported key algorithm"));
    };
    let key = match o.get("key") {
        Some(Value::String(k)) if super::b64::decode(k).is_some_and(|raw| super::identity::is_valid_public_key(alg, &raw)) => k.clone(),
        _ => return Err(invalid("invalid public key")),
    };
    let d = obj(o.get("device").unwrap_or(&Value::Null), "device")?;
    Ok(Hello {
        alg,
        key,
        nonce: b64(o.get("nonce"), "nonce", NONCE_LENGTH)?,
        device: DeviceInfo {
            alias: string(d.get("alias"), "alias", 0, MAX_ALIAS_LENGTH)?,
            device_type: string(d.get("deviceType"), "deviceType", 0, MAX_DEVICE_TYPE_LENGTH)?,
            platform: string(d.get("platform"), "platform", 0, MAX_PLATFORM_LENGTH)?,
        },
        caps: strings(o.get("caps"), "caps", MAX_CAPS, MAX_CAP_LENGTH)?,
    })
}

fn parse_offer(o: &Obj) -> Result<Offer, RtcError> {
    let Some(Value::Array(files)) = o.get("files") else {
        return Err(invalid("files must be an array"));
    };
    Ok(Offer {
        transfer_id: id(o.get("transferId"), "transferId")?,
        files: validate_files(files)?,
        text: match o.get("text") {
            None => None,
            v => Some(string(v, "text", 0, MAX_CONTROL_BYTES)?),
        },
        more: more(o)?,
    })
}

fn parse_answer(o: &Obj) -> Result<Answer, RtcError> {
    let accept = match o.get("accept") {
        Some(Value::Array(items)) if items.len() <= MAX_FILES => {
            items.iter().map(|v| id(Some(v), "accepted id")).collect::<Result<Vec<_>, _>>()?
        }
        _ => return Err(invalid(format!("accept must be an array of at most {MAX_FILES} ids"))),
    };
    let ids: HashSet<&String> = accept.iter().collect();
    if ids.len() != accept.len() {
        return Err(invalid("duplicate id in accept"));
    }
    let raw_offsets = obj(o.get("offsets").unwrap_or(&Value::Null), "offsets")?;
    let mut offsets = Vec::new();
    for (k, v) in raw_offsets {
        if !ids.contains(k) {
            return Err(invalid("offset for an id that is not accepted in this frame"));
        }
        offsets.push((k.clone(), size(Some(v), "offset")?));
    }
    // Keep the frame's id order (serde_json's map is sorted).
    offsets.sort_by_key(|(k, _)| accept.iter().position(|a| a == k));
    let mut answer = Answer { transfer_id: id(o.get("transferId"), "transferId")?, accept, offsets, declined: false, more: false };
    match o.get("declined") {
        None => {}
        Some(Value::Bool(true)) => {
            if !answer.accept.is_empty() || o.contains_key("more") {
                return Err(invalid("a declining answer accepts nothing and is a single frame"));
            }
            answer.declined = true;
        }
        Some(_) => return Err(invalid("declined must be true when present")),
    }
    answer.more = more(o)?;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_key_order() {
        let e = |k: &str, v: u64| (k.to_string(), v);
        assert_eq!(js_object_numbers(&[e("b", 1), e("10", 2), e("a", 3), e("2", 4), e("01", 5)]), r#"{"2":4,"10":2,"b":1,"a":3,"01":5}"#);
    }

    #[test]
    fn utf16_clipping_keeps_pairs_whole() {
        assert_eq!(clip_utf16("a😀b", 2), "a");
        assert_eq!(clip_utf16("a😀b", 3), "a😀");
        assert_eq!(js_len("😀"), 2);
    }
}
