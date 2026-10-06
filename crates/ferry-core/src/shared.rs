//! State shared by the engine's components.

use crate::db::Db;
use crate::devices::DeviceDirectory;
use crate::events::EventBus;
use crate::identity::Identity;
use crate::model::*;
use crate::proto::{DeviceDto, FERRY_PROTOCOL_VERSION, FerryHello, FerryHint, PROTOCOL_VERSION};
use crate::settings::SettingsStore;
use crate::transfer::TransferRegistry;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct NetState {
    pub port: u16,
    pub protocol: Protocol,
    pub addresses: Vec<String>,
}

pub struct Shared {
    pub identity: Identity,
    pub settings: SettingsStore,
    pub db: Arc<Db>,
    pub events: EventBus,
    pub transfers: Arc<TransferRegistry>,
    pub devices: Arc<DeviceDirectory>,
    pub net: RwLock<NetState>,
    pub shutdown: CancellationToken,
}

pub fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "ios") {
        "ios"
    } else if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

impl Shared {
    pub fn device_kind(&self) -> DeviceKind {
        self.settings.get().device_kind.unwrap_or(if cfg!(any(target_os = "android", target_os = "ios")) {
            DeviceKind::Mobile
        } else {
            DeviceKind::Desktop
        })
    }

    pub fn device_model(&self) -> Option<String> {
        self.settings.get().device_model.or_else(|| {
            Some(
                match platform_name() {
                    "windows" => "Windows",
                    "macos" => "macOS",
                    "ios" => "iOS",
                    "android" => "Android",
                    "linux" => "Linux",
                    _ => return None,
                }
                .to_string(),
            )
        })
    }

    /// What we send in register / prepare-upload requests.
    pub fn device_dto(&self) -> DeviceDto {
        let net = self.net.read().unwrap();
        DeviceDto {
            alias: self.settings.get().alias,
            version: PROTOCOL_VERSION.to_string(),
            device_model: self.device_model(),
            device_type: Some(self.device_kind()),
            fingerprint: self.identity.fingerprint.clone(),
            port: Some(net.port),
            protocol: Some(net.protocol),
            download: false,
            ferry: Some(FerryHint::ours()),
        }
    }

    /// What we answer to info / register.
    pub fn info_dto(&self) -> DeviceDto {
        DeviceDto { port: None, protocol: None, ..self.device_dto() }
    }

    pub fn register_response_dto(&self) -> DeviceDto {
        self.info_dto()
    }

    pub fn ferry_hello(&self) -> FerryHello {
        FerryHello {
            v: FERRY_PROTOCOL_VERSION,
            caps: FerryHint::ours().caps,
            device_id: self.identity.fingerprint.clone(),
            platform: platform_name().to_string(),
            app: APP_VERSION.to_string(),
        }
    }

    pub fn local_device(&self) -> LocalDevice {
        let net = self.net.read().unwrap();
        LocalDevice {
            alias: self.settings.get().alias,
            fingerprint: self.identity.fingerprint.clone(),
            device_kind: self.device_kind(),
            device_model: self.device_model(),
            port: net.port,
            protocol: net.protocol,
            addresses: net.addresses.clone(),
            short_id: crate::util::short_fingerprint(&self.identity.fingerprint),
            app_version: APP_VERSION.to_string(),
        }
    }

    /// How the UI should name a peer: the user's custom name wins.
    pub fn peer_ref(&self, identity: &PeerIdentity, dto: &DeviceDto, fallback_id: &str) -> PeerRef {
        let id = identity.fingerprint().map(str::to_string).unwrap_or_else(|| fallback_id.to_string());
        let custom = identity.fingerprint().and_then(|fp| self.devices.get(fp)).and_then(|d| d.custom_alias);
        PeerRef {
            id,
            alias: custom.unwrap_or_else(|| dto.display_alias()),
            device_kind: dto.device_type.unwrap_or_default(),
            device_model: dto.device_model.clone(),
            verified: identity.is_verified(),
        }
    }
}
