//! Small helpers shared across modules.

use rand::Rng;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 128-bit random token, lowercase hex (session ids, file tokens, share tokens).
pub fn random_token() -> String {
    hex::encode(random_bytes::<16>())
}

/// Random bytes from the OS-seeded CSPRNG.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// Constant-time string comparison for secrets (tokens, PINs).
pub fn secret_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value >= 100.0 { format!("{value:.0} {}", UNITS[unit]) } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// Uppercase hex fingerprint, formatted in groups for humans: `AB12 CD34 …`.
pub fn short_fingerprint(fingerprint: &str) -> String {
    fingerprint.as_bytes().chunks(4).take(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join(" ")
}

/// MIME type from a file name, defaulting to `application/octet-stream`.
pub fn mime_for(name: &str) -> String {
    mime_guess::from_path(name).first_or_octet_stream().essence_str().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_humanely() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_000), "1.0 KB");
        assert_eq!(format_bytes(2_400_000_000), "2.4 GB");
        assert_eq!(format_bytes(512_000_000), "512 MB");
    }

    #[test]
    fn tokens_are_random_and_hex() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn secret_eq_requires_exact_match() {
        assert!(secret_eq("123456", "123456"));
        assert!(!secret_eq("123456", "123457"));
        assert!(!secret_eq("12345", "123456"));
    }
}
