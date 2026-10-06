//! The engine: the one object a shell (desktop app, CLI, tests) talks to.

use crate::browser::{BrowserServer, BrowserShareInfo};
use crate::db::Db;
use crate::devices::DeviceDirectory;
use crate::discovery::Discovery;
use crate::error::{ErrorInfo, Result};
use crate::events::{EngineEvent, EventBus, NoticeLevel};
use crate::identity::Identity;
use crate::model::*;
use crate::net::interfaces;
use crate::pairing::{OutgoingPairing, PairingManager, PairingOffer};
use crate::receive::ReceiveManager;
use crate::rtc::manager::{DEVICE_PREFIX, RtcManager};
use crate::send::{SendItem, SendManager, Target};
use crate::server::{self, Routes, ServerHandle, ServerSignal};
use crate::settings::{Settings, SettingsStore};
use crate::shared::{NetState, Shared};
use crate::transfer::TransferRegistry;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// How many ports after the configured one we try when it is taken.
const PORT_FALLBACKS: u16 = 10;

pub struct EngineConfig {
    /// Where identity, settings and the database live. `None` = in memory
    /// (fresh identity each run; tests and throwaway CLI peers).
    pub data_dir: Option<PathBuf>,
    /// Settings to use instead of the stored ones (merged on top in memory).
    pub settings_override: Option<Settings>,
    /// Multicast discovery (off for loopback tests).
    pub discovery: bool,
}

impl EngineConfig {
    pub fn persistent(data_dir: PathBuf) -> Self {
        EngineConfig { data_dir: Some(data_dir), settings_override: None, discovery: true }
    }

    pub fn ephemeral(settings: Settings) -> Self {
        EngineConfig { data_dir: None, settings_override: Some(settings), discovery: false }
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    receive: Arc<ReceiveManager>,
    send: Arc<SendManager>,
    discovery: Option<Arc<Discovery>>,
    routes: Arc<Routes>,
    server: tokio::sync::Mutex<Option<ServerHandle>>,
    browser: Arc<BrowserServer>,
    pairing: Arc<PairingManager>,
    rtc: Arc<RtcManager>,
}

impl Engine {
    pub async fn start(config: EngineConfig) -> Result<Arc<Engine>> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (identity, settings, db) = match &config.data_dir {
            Some(dir) => {
                let identity = Identity::load_or_create(&dir.join("identity"))?;
                let settings = SettingsStore::load(dir.join("settings.json"))?;
                if let Some(over) = config.settings_override.clone() {
                    settings.replace(over).map_err(|m| ErrorInfo::new("invalid_settings", m))?;
                }
                (identity, settings, Db::open(&dir.join("ferry.db"))?)
            }
            None => (
                Identity::generate()?,
                SettingsStore::ephemeral(config.settings_override.clone().unwrap_or_default()),
                Db::open_in_memory()?,
            ),
        };
        let db = Arc::new(db);
        let events = EventBus::new();
        let shutdown = CancellationToken::new();
        let transfers = TransferRegistry::new(events.clone());
        transfers.spawn_ticker(shutdown.clone());
        let devices = DeviceDirectory::new(db.clone(), events.clone())?;
        let s = settings.get();
        let shared = Arc::new(Shared {
            identity,
            settings,
            db,
            events,
            transfers,
            devices,
            net: RwLock::new(NetState {
                port: s.port,
                protocol: if s.encryption { Protocol::Https } else { Protocol::Http },
                addresses: interfaces::shareable_addresses(&interfaces::list(s.include_virtual_interfaces)),
            }),
            shutdown: shutdown.clone(),
        });

        let receive = ReceiveManager::new(shared.clone());
        let send = SendManager::new(shared.clone());
        let (signal_tx, signal_rx) = mpsc::channel(256);
        let pairing = PairingManager::new(shared.clone());
        let routes = Routes::new(shared.clone(), receive.clone(), pairing.clone(), signal_tx);
        let discovery = config.discovery.then(|| Discovery::new(shared.clone()));

        let browser = BrowserServer::new(shared.clone(), receive.clone(), routes.clone());
        let rtc = RtcManager::new(shared.clone())?;
        let engine = Arc::new(Engine {
            shared: shared.clone(),
            receive,
            send,
            discovery,
            routes,
            server: tokio::sync::Mutex::new(None),
            browser,
            pairing,
            rtc,
        });
        engine.start_server().await;
        // WebRTC through the signaling server, when one is configured.
        engine.rtc.start();
        if let Some(discovery) = &engine.discovery {
            discovery.start().await;
            discovery.spawn_maintenance(shutdown.clone(), engine.send.lost_peer.clone());
        }
        engine.spawn_signal_loop(signal_rx);
        engine.spawn_housekeeping();
        shared.events.emit(EngineEvent::LocalDeviceChanged { device: shared.local_device() });
        tracing::info!("Ferry engine started as {} ({})", shared.settings.get().alias, shared.identity.fingerprint);
        Ok(engine)
    }

    async fn start_server(&self) {
        let settings = self.shared.settings.get();
        let tls = settings.encryption;
        let mut last_err = None;
        for port in settings.port..=settings.port.saturating_add(PORT_FALLBACKS) {
            match server::start(self.routes.clone(), port, tls).await {
                Ok(handle) => {
                    {
                        let mut net = self.shared.net.write().unwrap();
                        net.port = handle.port;
                        net.protocol = handle.protocol;
                    }
                    if handle.port != settings.port {
                        self.shared.events.emit(EngineEvent::Notice {
                            level: NoticeLevel::Warning,
                            code: "port_fallback".into(),
                            message: format!(
                                "Port {} is used by another app, so Ferry is receiving on {}. LocalSend devices still find it automatically.",
                                settings.port, handle.port
                            ),
                        });
                    }
                    self.shared.events.emit(EngineEvent::ServerStatus { running: true, port: handle.port, error: None });
                    *self.server.lock().await = Some(handle);
                    return;
                }
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => last_err = Some(err),
                Err(err) => {
                    last_err = Some(err);
                    break;
                }
            }
        }
        let message = format!("Can't receive: {}", last_err.map(|e| e.to_string()).unwrap_or_default());
        tracing::error!("{message}");
        self.shared.events.emit(EngineEvent::ServerStatus { running: false, port: settings.port, error: Some(message) });
    }

    fn spawn_signal_loop(self: &Arc<Self>, mut rx: mpsc::Receiver<ServerSignal>) {
        let engine = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(signal) = rx.recv().await {
                let Some(engine) = engine.upgrade() else { return };
                match signal {
                    ServerSignal::Registered { identity, ip, dto } => {
                        if let Some(d) = &engine.discovery {
                            d.handle_register(identity, ip, dto);
                        }
                    }
                    ServerSignal::CancelReceived { identity, session_id, .. } => {
                        engine.send.peer_cancelled(&identity, session_id.as_deref());
                    }
                }
            }
        });
    }

    fn spawn_housekeeping(self: &Arc<Self>) {
        let receive = self.receive.clone();
        let browser = self.browser.clone();
        let pairing = self.pairing.clone();
        let cancel = self.shared.shutdown.clone();
        let devices = self.shared.devices.clone();
        let has_discovery = self.discovery.is_some();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        receive.housekeeping();
                        browser.prune();
                        pairing.prune();
                        if !has_discovery {
                            devices.sweep();
                        }
                    }
                    _ = cancel.cancelled() => return,
                }
            }
        });
    }

    // ── Queries ──────────────────────────────────────────────────────────

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.shared.events.subscribe()
    }

    pub fn local_device(&self) -> LocalDevice {
        self.shared.local_device()
    }

    pub fn devices(&self) -> Vec<DeviceSummary> {
        self.shared.devices.list()
    }

    pub fn transfers(&self) -> Vec<TransferSummary> {
        self.shared.transfers.list()
    }

    pub fn transfer_files(&self, id: &str) -> Option<Vec<TransferFile>> {
        self.shared.transfers.get(id).map(|e| e.files())
    }

    pub fn history(&self, limit: u32, before_id: Option<i64>, direction: Option<Direction>) -> Result<Vec<HistoryEntry>> {
        self.shared.db.history(limit.min(500), before_id, direction)
    }

    pub fn delete_history(&self, id: i64) -> Result<bool> {
        self.shared.db.delete_history(id)
    }

    pub fn clear_history(&self) -> Result<()> {
        self.shared.db.clear_history()
    }

    pub fn settings(&self) -> Settings {
        self.shared.settings.get()
    }

    pub fn port(&self) -> u16 {
        self.shared.net.read().unwrap().port
    }

    pub fn fingerprint(&self) -> String {
        self.shared.identity.fingerprint.clone()
    }

    pub fn multicast_error(&self) -> Option<String> {
        self.discovery.as_ref().and_then(|d| d.multicast_error())
    }

    pub fn interfaces(&self) -> Vec<interfaces::LocalInterface> {
        match &self.discovery {
            Some(d) => d.interfaces(),
            None => interfaces::list(self.shared.settings.get().include_virtual_interfaces),
        }
    }

    // ── Actions ──────────────────────────────────────────────────────────

    /// Sends to LAN devices and/or WebRTC devices (`rtc:` ids, reached
    /// through the signaling server); returns one transfer id per transfer.
    pub async fn send(&self, targets: Vec<Target>, items: Vec<SendItem>) -> Result<Vec<String>> {
        let (rtc, lan): (Vec<Target>, Vec<Target>) =
            targets.into_iter().partition(|t| matches!(t, Target::Device { id } if id.starts_with(DEVICE_PREFIX)));
        if rtc.is_empty() {
            return self.send.send(lan, items).await;
        }
        if items.is_empty() {
            return Err(ErrorInfo::new("nothing_to_send", "Choose something to send and a device to send it to.").into());
        }
        let drop_id = (rtc.len() + lan.len() > 1).then(|| uuid::Uuid::new_v4().to_string());
        let mut ids = Vec::new();
        if !lan.is_empty() {
            ids.extend(self.send.send_with_drop(lan, items.clone(), drop_id.clone()).await?);
        }
        let rtc_ids: Vec<String> = rtc.into_iter().filter_map(|t| if let Target::Device { id } = t { Some(id) } else { None }).collect();
        ids.extend(self.rtc.send(rtc_ids, items, drop_id).await?);
        Ok(ids)
    }

    pub fn respond(&self, request_id: &str, decision: Decision) -> bool {
        let decision2 = decision.clone();
        self.receive.respond(request_id, decision) || self.rtc.respond(request_id, decision2)
    }

    /// Cancels a transfer in either direction.
    pub fn cancel(&self, id: &str) -> bool {
        self.send.cancel(id) || self.receive.cancel_transfer(id) || self.rtc.cancel(id)
    }

    pub fn pause(&self, id: &str) -> bool {
        self.send.pause(id)
    }

    pub fn resume(&self, id: &str) -> bool {
        self.send.resume(id)
    }

    pub fn submit_pin(&self, id: &str, pin: Option<String>) -> bool {
        self.send.submit_pin(id, pin)
    }

    /// Removes a finished transfer from the list.
    pub fn dismiss(&self, id: &str) -> bool {
        self.shared.transfers.remove(id)
    }

    pub async fn refresh_devices(&self) {
        if let Some(d) = &self.discovery {
            d.refresh().await;
        }
    }

    pub async fn add_device(&self, host: &str, port: u16) -> Result<DeviceSummary> {
        match &self.discovery {
            Some(d) => d.add_manual(host, port).await,
            None => Discovery::new(self.shared.clone()).add_manual(host, port).await,
        }
    }

    pub fn set_device_flags(
        &self,
        id: &str,
        trusted: Option<bool>,
        favorite: Option<bool>,
        mine: Option<bool>,
        custom_alias: Option<Option<String>>,
    ) -> Result<Option<DeviceSummary>> {
        self.shared.devices.update_flags(id, trusted, favorite, mine, custom_alias)
    }

    // ── Pairing ("my devices") ────────────────────────────────────────────

    /// A single-use QR code / link another device can scan to pair with us.
    pub fn create_pairing_offer(&self) -> Result<PairingOffer> {
        self.pairing.create_offer()
    }

    pub fn cancel_pairing_offer(&self, id: &str) -> bool {
        self.pairing.cancel_offer(id)
    }

    /// Pairs with the device showing `uri`.
    pub async fn pair_with_uri(&self, uri: &str) -> Result<DeviceSummary> {
        self.pairing.pair_with_uri(uri).await
    }

    /// Asks a nearby device to pair by code comparison (result: `PairingFinished`).
    pub async fn start_code_pairing(&self, device_id: &str) -> Result<OutgoingPairing> {
        self.pairing.start_code_pairing(device_id).await
    }

    pub fn cancel_code_pairing(&self, id: &str) -> bool {
        self.pairing.cancel_code_pairing(id)
    }

    /// Answers a `PairingRequest`: true when the codes match.
    pub fn respond_pairing(&self, request_id: &str, accept: bool) -> bool {
        self.pairing.respond(request_id, accept)
    }

    /// Removes a device from "my devices" here and tells it (best effort).
    pub fn unpair_device(&self, id: &str) -> Result<Option<DeviceSummary>> {
        self.pairing.unpair(id)
    }

    pub fn forget_device(&self, id: &str) -> Result<()> {
        self.shared.devices.forget(id)
    }

    /// Validates, stores and applies settings; returns the stored settings.
    pub async fn update_settings(&self, settings: Settings) -> Result<Settings> {
        let previous = self.shared.settings.replace(settings.clone()).map_err(|m| ErrorInfo::new("invalid_settings", m))?;
        let network_changed = previous.port != settings.port || previous.encryption != settings.encryption;
        let identity_changed = previous.alias != settings.alias
            || previous.device_kind != settings.device_kind
            || previous.device_model != settings.device_model;
        let discovery_changed = previous.multicast_group != settings.multicast_group
            || previous.ipv6 != settings.ipv6
            || previous.include_virtual_interfaces != settings.include_virtual_interfaces
            || previous.interface_whitelist != settings.interface_whitelist
            || previous.interface_blacklist != settings.interface_blacklist;
        if network_changed {
            if let Some(handle) = self.server.lock().await.take() {
                handle.stop().await;
            }
            self.start_server().await;
        }
        if (network_changed || identity_changed || discovery_changed)
            && let Some(d) = &self.discovery
        {
            d.restart().await;
        }
        self.rtc.settings_changed(&previous);
        self.shared.events.emit(EngineEvent::LocalDeviceChanged { device: self.shared.local_device() });
        Ok(self.shared.settings.get())
    }

    /// A link any browser on this network can open to download `items`.
    pub async fn share_with_browsers(&self, items: Vec<SendItem>, pin: Option<String>) -> Result<BrowserShareInfo> {
        self.browser.share_files(items, pin, None).await
    }

    /// A link any browser on this network can open to send files here.
    pub async fn receive_from_browsers(&self, pin: Option<String>) -> Result<BrowserShareInfo> {
        self.browser.receive_from_browsers(pin, None).await
    }

    pub fn stop_browser_link(&self, id: &str) -> bool {
        self.browser.stop(id)
    }

    pub fn browser_links(&self) -> Vec<BrowserShareInfo> {
        self.browser.list()
    }

    pub async fn diagnostics(&self) -> Vec<crate::diagnostics::DiagnosticCheck> {
        crate::diagnostics::run(self).await
    }

    /// Whether `path` is something Ferry handled (a received or sent file, or
    /// inside the save folder); shells only open/reveal such paths.
    pub fn is_known_path(&self, path: &std::path::Path) -> bool {
        let save = self.shared.settings.get().save_dir();
        if let (Ok(p), Ok(s)) = (std::fs::canonicalize(path), std::fs::canonicalize(&save))
            && p.starts_with(s)
        {
            return true;
        }
        self.shared.db.history_has_path(&path.display().to_string()).unwrap_or(false)
    }

    // ── WebRTC (signaling server, private links) ─────────────────────────

    /// The signaling connection behind WebRTC transfers.
    pub fn signaling_status(&self) -> SignalingStatus {
        self.rtc.status()
    }

    /// Creates a private link: devices that open it see this device on any
    /// network (requires a signaling server).
    pub fn create_room(&self) -> RoomInfo {
        self.rtc.create_room()
    }

    /// Joins a private link (`…#room=<secret>`, or just the secret).
    pub fn join_room(&self, link: &str) -> Result<RoomInfo> {
        self.rtc.join_room(link)
    }

    pub fn leave_room(&self, id: &str) -> bool {
        self.rtc.leave_room(id)
    }

    pub fn rooms(&self) -> Vec<RoomInfo> {
        self.rtc.rooms()
    }

    /// Whether peers on the same network see this device on the signaling
    /// server (`false`: only devices that open one of its private links).
    /// Applies on the next (re)connection.
    pub fn set_signaling_nearby(&self, nearby: bool) {
        self.rtc.set_nearby(nearby);
        self.rtc.start();
    }

    /// Gathers loopback ICE candidates too (same-machine setups without a
    /// network). Applies on the next (re)connection.
    pub fn set_webrtc_loopback(&self, include: bool) {
        self.rtc.set_include_loopback(include);
        self.rtc.start();
    }

    pub async fn shutdown(&self) {
        self.rtc.stop();
        self.shared.shutdown.cancel();
        if let Some(d) = &self.discovery {
            d.stop().await;
        }
        if let Some(handle) = self.server.lock().await.take() {
            handle.stop().await;
        }
    }
}
