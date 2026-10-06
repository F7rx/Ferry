//! The persistent WebRTC device identity (threat model "Identity model").
//!
//! Native devices sign with Ed25519: the key is the engine identity's existing
//! signing key (`identity/signing-key.pem`, DPAPI-protected on Windows), so it
//! lives next to the TLS identity and survives restarts. Peers may use Ed25519
//! or ECDSA P-256 (browsers without WebCrypto Ed25519); both are verified.

use super::b64;
use super::protocol::{Alg, SIGNATURE_LENGTH};
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

/// Raw public key length: 32 bytes (Ed25519) or 65 (uncompressed P-256 point).
pub fn public_key_length(alg: Alg) -> usize {
    match alg {
        Alg::Ed25519 => 32,
        Alg::P256 => 65,
    }
}

/// Checks the length (and point-format prefix) of a raw public key.
pub fn is_valid_public_key(alg: Alg, raw: &[u8]) -> bool {
    raw.len() == public_key_length(alg) && (alg != Alg::P256 || raw[0] == 0x04)
}

#[derive(Clone)]
pub struct RtcIdentity {
    key: SigningKey,
    public_b64: String,
}

impl std::fmt::Debug for RtcIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtcIdentity").field("key", &self.public_b64).finish_non_exhaustive()
    }
}

impl RtcIdentity {
    /// The identity behind an Ed25519 PKCS#8 PEM private key.
    pub fn from_pkcs8_pem(pem: &str) -> Result<Self, String> {
        let key = SigningKey::from_pkcs8_pem(pem).map_err(|e| format!("invalid Ed25519 key: {e}"))?;
        Ok(Self::from_signing_key(key))
    }

    /// From a raw 32-byte Ed25519 seed (tests, vectors).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self::from_signing_key(SigningKey::from_bytes(seed))
    }

    pub fn generate() -> Self {
        Self::from_seed(&crate::util::random_bytes::<32>())
    }

    fn from_signing_key(key: SigningKey) -> Self {
        let public_b64 = b64::encode(key.verifying_key().as_bytes());
        RtcIdentity { key, public_b64 }
    }

    pub fn alg(&self) -> Alg {
        Alg::Ed25519
    }

    /// Raw public key, base64url without padding (the wire format and device key).
    pub fn public_key(&self) -> &str {
        &self.public_b64
    }

    pub fn public_key_raw(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// Signs `data`; the signature as base64url (deterministic, RFC 8032).
    pub fn sign(&self, data: &[u8]) -> String {
        b64::encode(&self.key.sign(data).to_bytes())
    }
}

/// Verifies a base64url signature against a base64url raw public key. Never panics.
pub fn verify(alg: Alg, public_key_b64: &str, data: &[u8], sig_b64: &str) -> bool {
    let (Some(raw), Some(sig)) = (b64::decode(public_key_b64), b64::decode(sig_b64)) else {
        return false;
    };
    if !is_valid_public_key(alg, &raw) || sig.len() != SIGNATURE_LENGTH {
        return false;
    }
    match alg {
        Alg::Ed25519 => {
            let (Ok(key), Ok(sig)) = (<[u8; 32]>::try_from(raw.as_slice()), <[u8; 64]>::try_from(sig.as_slice())) else {
                return false;
            };
            let Ok(key) = VerifyingKey::from_bytes(&key) else { return false };
            key.verify_strict(data, &ed25519_dalek::Signature::from_bytes(&sig)).is_ok()
        }
        Alg::P256 => ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &raw).verify(data, &sig).is_ok(),
    }
}

/// Parses an `ed25519`/`p256` name from signaling or the hello message.
pub fn parse_alg(s: &str) -> Option<Alg> {
    Alg::parse(s)
}
