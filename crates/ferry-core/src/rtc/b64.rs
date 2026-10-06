//! base64url without padding (RFC 4648 §5), decoded strictly like the
//! reference: no padding, no foreign characters, canonical trailing bits.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}
