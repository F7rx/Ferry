//! User settings: one JSON file, written atomically, never holding secrets.
//! Appearance settings live in the UI (they must apply before the engine runs).

use crate::error::Result;
use crate::model::DeviceKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub const SETTINGS_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 53317;

/// Who may skip the accept prompt. Only ever applies to *verified* peers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AutoAccept {
    /// Always ask.
    Off,
    /// Devices paired as "My devices" (default).
    MyDevices,
    /// Any device marked trusted.
    Trusted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub version: u32,

    // ── General ───────────────────────────────────────────────────────
    pub alias: String,
    /// Overrides the detected device kind (e.g. a headless server).
    pub device_kind: Option<DeviceKind>,
    pub device_model: Option<String>,

    // ── Receiving ─────────────────────────────────────────────────────
    /// Visible to nearby devices and accepting requests.
    pub receive_enabled: bool,
    /// `None` = the platform default (Downloads/Ferry).
    pub save_dir: Option<PathBuf>,
    pub auto_accept: AutoAccept,
    /// Required from senders that aren't trusted.
    pub pin: Option<String>,
    /// Seconds before an unanswered request is declined.
    pub decision_timeout_secs: u64,

    // ── History & privacy ─────────────────────────────────────────────
    pub history_enabled: bool,
    /// Store the body of received text messages in history.
    pub keep_message_text: bool,

    // ── Transfers ─────────────────────────────────────────────────────
    /// Hash files before sending to LocalSend devices so they can verify
    /// them (reads every file twice). Ferry devices verify without it.
    pub checksums_for_localsend: bool,
    /// Verify `sha256` that LocalSend senders provide.
    pub verify_incoming_checksums: bool,
    /// Concurrent file streams per transfer.
    pub parallel_files: u32,

    // ── Network ───────────────────────────────────────────────────────
    pub port: u16,
    /// HTTPS with mutual TLS. Turning it off is only for legacy peers.
    pub encryption: bool,
    pub multicast_group: String,
    pub ipv6: bool,
    /// Also use VPN / virtual-machine / container adapters.
    pub include_virtual_interfaces: bool,
    pub interface_whitelist: Option<Vec<String>>,
    pub interface_blacklist: Option<Vec<String>>,
    /// Fall back to probing the local subnet when multicast finds nothing.
    pub subnet_scan: bool,

    // ── WebRTC / remote ───────────────────────────────────────────────
    pub signaling_url: Option<String>,
    pub stun_servers: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: SETTINGS_VERSION,
            alias: default_alias(),
            device_kind: None,
            device_model: None,
            receive_enabled: true,
            save_dir: None,
            auto_accept: AutoAccept::MyDevices,
            pin: None,
            decision_timeout_secs: 300,
            history_enabled: true,
            keep_message_text: false,
            checksums_for_localsend: false,
            verify_incoming_checksums: true,
            parallel_files: 8,
            port: DEFAULT_PORT,
            encryption: true,
            multicast_group: "224.0.0.167".to_string(),
            ipv6: true,
            include_virtual_interfaces: false,
            interface_whitelist: None,
            interface_blacklist: None,
            subnet_scan: true,
            signaling_url: None,
            stun_servers: vec!["stun:stun.l.google.com:19302".to_string()],
        }
    }
}

impl Settings {
    /// Rejects values that would break the engine; returns a user-facing reason.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let alias = self.alias.trim();
        if alias.is_empty() {
            return Err("The device name can't be empty.".into());
        }
        if alias.chars().count() > 64 {
            return Err("The device name is too long (64 characters max).".into());
        }
        // 0 = any free port (tests, throwaway peers); the UI only offers ≥ 1024.
        if self.port != 0 && self.port < 1024 {
            return Err("Use a port between 1024 and 65535.".into());
        }
        if let Some(pin) = &self.pin
            && (pin.is_empty() || pin.len() > 32 || !pin.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            return Err("The PIN must be 1 to 32 letters or digits.".into());
        }
        if !(1..=32).contains(&self.parallel_files) {
            return Err("Parallel files must be between 1 and 32.".into());
        }
        if let Some(url) = self.signaling_url.as_deref().map(str::trim).filter(|u| !u.is_empty())
            && !((url.starts_with("ws://") || url.starts_with("wss://")) && url.len() > 6 && !url.contains(char::is_whitespace))
        {
            return Err("The signaling server address must start with ws:// or wss://.".into());
        }
        if self.multicast_group.parse::<std::net::Ipv4Addr>().map(|ip| !ip.is_multicast()).unwrap_or(true) {
            return Err("The multicast address must be an IPv4 multicast group (224.0.0.0/4).".into());
        }
        Ok(())
    }

    /// Effective save folder.
    pub fn save_dir(&self) -> PathBuf {
        self.save_dir.clone().unwrap_or_else(default_save_dir)
    }
}

/// The computer's name, the most recognizable default for other people.
pub fn default_alias() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(hostname)
        .unwrap_or_else(|| "Ferry device".to_string());
    let trimmed = raw.trim().trim_end_matches(".local");
    if trimmed.is_empty() { "Ferry device".to_string() } else { trimmed.chars().take(64).collect() }
}

#[cfg(unix)]
fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

#[cfg(not(unix))]
fn hostname() -> Option<String> {
    None
}

pub fn default_save_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|d| d.download_dir().map(Path::to_path_buf))
        .unwrap_or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join("Downloads")).unwrap_or_else(|| PathBuf::from(".")))
        .join("Ferry")
}

/// Thread-safe settings with atomic persistence.
#[derive(Clone)]
pub struct SettingsStore {
    path: PathBuf,
    inner: Arc<RwLock<Settings>>,
}

impl SettingsStore {
    pub fn load(path: PathBuf) -> Result<SettingsStore> {
        let settings = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                Ok(settings) => migrate(settings),
                Err(err) => {
                    // Keep the broken file for inspection instead of silently
                    // discarding the user's configuration.
                    let backup = path.with_extension("json.corrupt");
                    let _ = std::fs::rename(&path, &backup);
                    tracing::warn!("Settings were unreadable ({err}); saved a copy to {} and started fresh", backup.display());
                    Settings::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(err) => return Err(err.into()),
        };
        let store = SettingsStore { path, inner: Arc::new(RwLock::new(settings)) };
        store.save()?;
        Ok(store)
    }

    /// In-memory settings (tests, ephemeral CLI runs).
    pub fn ephemeral(settings: Settings) -> SettingsStore {
        SettingsStore { path: PathBuf::new(), inner: Arc::new(RwLock::new(settings)) }
    }

    pub fn get(&self) -> Settings {
        self.inner.read().unwrap().clone()
    }

    /// Validates and stores new settings; returns the previous ones.
    pub fn replace(&self, settings: Settings) -> std::result::Result<Settings, String> {
        settings.validate()?;
        let previous = std::mem::replace(&mut *self.inner.write().unwrap(), settings);
        if let Err(err) = self.save() {
            tracing::error!("Could not save settings: {err}");
        }
        Ok(previous)
    }

    fn save(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let json = serde_json::to_vec_pretty(&*self.inner.read().unwrap())?;
        write_atomic(&self.path, &json)
    }
}

fn migrate(mut settings: Settings) -> Settings {
    if settings.version < SETTINGS_VERSION {
        settings.version = SETTINGS_VERSION;
    }
    if settings.validate().is_err() {
        // Repair individual fields rather than losing everything.
        let defaults = Settings::default();
        if settings.alias.trim().is_empty() {
            settings.alias = defaults.alias;
        }
        if settings.port < 1024 {
            settings.port = defaults.port;
        }
        if !(1..=32).contains(&settings.parallel_files) {
            settings.parallel_files = defaults.parallel_files;
        }
    }
    settings
}

/// Writes `bytes` to `path` so that readers see either the old or the new
/// content, never a mix: temp file in the same folder, fsync, rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("file"), crate::util::random_token()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Settings::default().validate().unwrap();
    }

    #[test]
    fn persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let store = SettingsStore::load(path.clone()).unwrap();
        let mut s = store.get();
        s.alias = "Studio iMac".into();
        s.pin = Some("4711".into());
        store.replace(s).unwrap();
        let reloaded = SettingsStore::load(path).unwrap().get();
        assert_eq!(reloaded.alias, "Studio iMac");
        assert_eq!(reloaded.pin.as_deref(), Some("4711"));
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let s: Settings = serde_json::from_str(r#"{"alias":"X","someFutureField":1}"#).unwrap();
        assert_eq!(s.alias, "X");
        assert_eq!(s.port, DEFAULT_PORT);
    }

    #[test]
    fn corrupt_file_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let store = SettingsStore::load(path.clone()).unwrap();
        assert_eq!(store.get().port, DEFAULT_PORT);
        assert!(dir.path().join("settings.json.corrupt").exists());
    }

    #[test]
    fn rejects_invalid_values() {
        let store = SettingsStore::ephemeral(Settings::default());
        let mut s = store.get();
        s.pin = Some("12 34".into());
        assert!(store.replace(s).is_err());
        let mut s = store.get();
        s.port = 80;
        assert!(store.replace(s).is_err());
    }
}
