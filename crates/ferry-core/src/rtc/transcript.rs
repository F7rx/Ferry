//! The `ferry-dc/1` handshake transcript (05-protocol.md §5.2, threat model W3).
//! Each side hashes both DTLS fingerprints *as it observed them*, the session
//! id, both nonces and both identity keys. A signaling server that swaps SDPs
//! makes the two legs observe different fingerprints, so signatures fail.

use super::protocol::sdp_lines;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const DC_PROTOCOL: &str = "ferry-dc/1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Offerer,
    Answerer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Offerer => "offerer",
            Role::Answerer => "answerer",
        }
    }

    pub fn other(self) -> Role {
        match self {
            Role::Offerer => Role::Answerer,
            Role::Answerer => Role::Offerer,
        }
    }
}

pub struct TranscriptParts<'a> {
    pub session_id: &'a str,
    /// Normalized fingerprints (see [`extract_fingerprint`]).
    pub fp_offerer: &'a str,
    pub fp_answerer: &'a str,
    pub nonce_offerer: &'a [u8],
    pub nonce_answerer: &'a [u8],
    pub key_offerer: &'a [u8],
    pub key_answerer: &'a [u8],
}

/// `enc(x) = u32be(len(x)) ‖ x`.
fn enc(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u32).to_be_bytes());
    h.update(bytes);
}

/// `T = SHA-256(enc("ferry-dc/1") ‖ enc(sessionId) ‖ enc(fpO) ‖ enc(fpA) ‖ enc(nonceO) ‖ enc(nonceA) ‖ enc(keyO) ‖ enc(keyA))`
pub fn transcript_hash(p: &TranscriptParts<'_>) -> [u8; 32] {
    let mut h = Sha256::new();
    for part in [
        DC_PROTOCOL.as_bytes(),
        p.session_id.as_bytes(),
        p.fp_offerer.as_bytes(),
        p.fp_answerer.as_bytes(),
        p.nonce_offerer,
        p.nonce_answerer,
        p.key_offerer,
        p.key_answerer,
    ] {
        enc(&mut h, part);
    }
    h.finalize().into()
}

/// Signature input: UTF-8 `"ferry-dc/1 auth " + role` ‖ T.
pub fn auth_payload(role: Role, transcript: &[u8]) -> Vec<u8> {
    let mut out = format!("{DC_PROTOCOL} auth {}", role.as_str()).into_bytes();
    out.extend_from_slice(transcript);
    out
}

/// `roomKey = SHA-256("ferry-room-key/1" ‖ roomSecret)`
pub fn derive_room_key(secret: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"ferry-room-key/1");
    h.update(secret);
    h.finalize().into()
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(key) else {
        // HMAC accepts keys of any length.
        return [0; 32];
    };
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// `mac = HMAC-SHA256(roomKey, T)`, base64url.
pub fn room_mac(room_key: &[u8], transcript: &[u8]) -> String {
    super::b64::encode(&hmac(room_key, transcript))
}

pub fn verify_room_mac(room_key: &[u8], transcript: &[u8], mac_b64: &str) -> bool {
    use subtle::ConstantTimeEq;
    match super::b64::decode(mac_b64) {
        Some(mac) => mac.len() == 32 && bool::from(mac.ct_eq(&hmac(room_key, transcript))),
        None => false,
    }
}

/// Link/QR room id: `"r:" + base64url(SHA-256("ferry-room/1" ‖ secret))[0..22]`.
pub fn room_id_from_secret(secret: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"ferry-room/1");
    h.update(secret);
    let b64 = super::b64::encode(&h.finalize());
    format!("r:{}", &b64[..22])
}

/// Six-digit first-contact verification code: the first four bytes of T as a
/// big-endian u32, mod 1,000,000, zero-padded.
pub fn short_code(transcript: &[u8]) -> String {
    let n = match transcript.get(..4) {
        Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    };
    format!("{:06}", n % 1_000_000)
}

/// Extracts the DTLS certificate fingerprint(s) from an SDP, normalized to
/// `"<algo lowercase> <HEX:UPPER:WITH:COLONS>"`; several distinct ones are
/// deduplicated, sorted and joined with `,`. `None` when there is none.
pub fn extract_fingerprint(sdp: &str) -> Option<String> {
    let mut found = BTreeSet::new();
    for line in sdp_lines(sdp) {
        let Some(rest) = line.strip_prefix("a=fingerprint:") else { continue };
        let algo_end = rest.find([' ', '\t']).unwrap_or(rest.len());
        let algo = &rest[..algo_end];
        if algo.is_empty() || !algo.bytes().all(|b| (b'!'..=b'~').contains(&b)) {
            continue;
        }
        let value = rest[algo_end..].trim_start_matches([' ', '\t']);
        if value.len() == rest.len() - algo_end {
            continue; // no separating whitespace
        }
        let value = value.trim_end_matches([' ', '\t']);
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_hexdigit() || b == b':') {
            continue;
        }
        let hex: String = value.chars().filter(|c| *c != ':').collect::<String>().to_ascii_uppercase();
        if hex.len() < 32 || !hex.len().is_multiple_of(2) {
            continue;
        }
        let pairs: Vec<&str> = (0..hex.len()).step_by(2).map(|i| &hex[i..i + 2]).collect();
        found.insert(format!("{} {}", algo.to_ascii_lowercase(), pairs.join(":")));
    }
    if found.is_empty() {
        return None;
    }
    Some(found.into_iter().collect::<Vec<_>>().join(","))
}
