//! The device identity: the TLS certificate whose fingerprint *is* the device
//! on the LAN, and an Ed25519 key for WebRTC/pairing signatures.
//!
//! Stored apart from settings (a corrupt settings file must never cost the
//! identity). Private keys are encrypted with DPAPI (user scope) on Windows and
//! written with 0600 permissions elsewhere.

use crate::error::Result;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Identity {
    pub cert_pem: String,
    pub key_pem: String,
    /// Uppercase hex SHA-256 of the certificate DER.
    pub fingerprint: String,
    /// Ed25519 private key (PKCS#8 PEM) for signatures outside TLS.
    pub signing_key_pem: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").field("fingerprint", &self.fingerprint).finish_non_exhaustive()
    }
}

const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";
const SIGNING_FILE: &str = "signing-key.pem";

impl Identity {
    /// Loads the identity from `dir`, creating it on first run.
    pub fn load_or_create(dir: &Path) -> Result<Identity> {
        std::fs::create_dir_all(dir)?;
        if let Some(identity) = Self::load(dir)? {
            return Ok(identity);
        }
        let identity = Self::generate()?;
        identity.store(dir)?;
        tracing::info!("Created new device identity {}", identity.fingerprint);
        Ok(identity)
    }

    /// A fresh, unsaved identity (tests, ephemeral CLI instances).
    pub fn generate() -> Result<Identity> {
        let cert = localsend::crypto::cert::generate_self_signed_named("Ferry Device")?;
        let signing = localsend::crypto::token::generate_key();
        let signing_key_pem = localsend::crypto::token::export_private_key(&signing)?.to_string();
        Ok(Identity { cert_pem: cert.certificate_pem, key_pem: cert.private_key_pem, fingerprint: cert.fingerprint, signing_key_pem })
    }

    fn load(dir: &Path) -> Result<Option<Identity>> {
        let cert_path = dir.join(CERT_FILE);
        if !cert_path.exists() {
            return Ok(None);
        }
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_pem = read_secret(&dir.join(KEY_FILE))?;
        let signing_key_pem = match read_secret(&dir.join(SIGNING_FILE)) {
            Ok(pem) => pem,
            Err(_) => {
                // Older stores without a signing key: add one.
                let key = localsend::crypto::token::generate_key();
                let pem = localsend::crypto::token::export_private_key(&key)?.to_string();
                write_secret(&dir.join(SIGNING_FILE), &pem)?;
                pem
            }
        };
        let der = pem_to_der(&cert_pem)?;
        let fingerprint = localsend::crypto::cert::fingerprint_from_cert_der(&der);
        Ok(Some(Identity { cert_pem, key_pem, fingerprint, signing_key_pem }))
    }

    fn store(&self, dir: &Path) -> Result<()> {
        write_secret(&dir.join(KEY_FILE), &self.key_pem)?;
        write_secret(&dir.join(SIGNING_FILE), &self.signing_key_pem)?;
        // The certificate is public; written last so a half-written store is
        // detected as "no identity" and regenerated.
        crate::settings::write_atomic(&dir.join(CERT_FILE), self.cert_pem.as_bytes())?;
        Ok(())
    }
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    use rustls::pki_types::pem::PemObject;
    let der = rustls::pki_types::CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| anyhow::anyhow!("invalid certificate: {e}"))?;
    Ok(der.to_vec())
}

fn secret_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        let mut p = path.as_os_str().to_owned();
        p.push(".dpapi");
        PathBuf::from(p)
    } else {
        path.to_path_buf()
    }
}

fn write_secret(path: &Path, value: &str) -> Result<()> {
    let path = secret_path(path);
    let bytes = protect(value.as_bytes())?;
    crate::settings::write_atomic(&path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn read_secret(path: &Path) -> Result<String> {
    let bytes = std::fs::read(secret_path(path))?;
    let plain = unprotect(&bytes)?;
    String::from_utf8(plain).map_err(|e| anyhow::anyhow!("corrupt key file: {e}").into())
}

#[cfg(windows)]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi::protect(data).map_err(Into::into)
}

#[cfg(windows)]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi::unprotect(data).map_err(Into::into)
}

#[cfg(not(windows))]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(not(windows))]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(windows)]
mod dpapi {
    use std::io;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData};

    /// Binds the ciphertext to this purpose so other DPAPI blobs can't be swapped in.
    const ENTROPY: &[u8] = b"ferry-identity-v1";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    fn run(
        data: &[u8],
        f: unsafe fn(*const CRYPT_INTEGER_BLOB, *const CRYPT_INTEGER_BLOB, *mut CRYPT_INTEGER_BLOB) -> i32,
    ) -> io::Result<Vec<u8>> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = unsafe { f(&input, &entropy, &mut output) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe { LocalFree(output.pbData as _) };
        Ok(result)
    }

    unsafe fn protect_raw(input: *const CRYPT_INTEGER_BLOB, entropy: *const CRYPT_INTEGER_BLOB, output: *mut CRYPT_INTEGER_BLOB) -> i32 {
        unsafe { CryptProtectData(input, std::ptr::null(), entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, output) }
    }

    unsafe fn unprotect_raw(input: *const CRYPT_INTEGER_BLOB, entropy: *const CRYPT_INTEGER_BLOB, output: *mut CRYPT_INTEGER_BLOB) -> i32 {
        unsafe {
            CryptUnprotectData(input, std::ptr::null_mut(), entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, output)
        }
    }

    pub fn protect(data: &[u8]) -> io::Result<Vec<u8>> {
        run(data, protect_raw)
    }

    pub fn unprotect(data: &[u8]) -> io::Result<Vec<u8>> {
        run(data, unprotect_raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trips_and_keeps_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let created = Identity::load_or_create(dir.path()).unwrap();
        let loaded = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(created.fingerprint, loaded.fingerprint);
        assert_eq!(created.key_pem, loaded.key_pem);
        assert_eq!(created.fingerprint.len(), 64);
        assert!(created.fingerprint.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    }

    #[test]
    fn private_key_is_not_stored_in_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity::load_or_create(dir.path()).unwrap();
        if cfg!(windows) {
            let raw = std::fs::read(dir.path().join("key.pem.dpapi")).unwrap();
            let needle = identity.key_pem.as_bytes();
            assert!(!raw.windows(32).any(|w| needle.windows(32).next() == Some(w)));
            assert!(!dir.path().join("key.pem").exists());
        }
    }
}
