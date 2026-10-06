//! Finding devices: LocalSend-compatible multicast announcements answered over
//! pinned HTTPS, a netmask-aware subnet scan as fallback, periodic refresh of
//! remembered devices, and rebinding when the network changes.

use crate::client::{PeerAddress, PeerClient};
use crate::devices::Observation;
use crate::events::EngineEvent;
use crate::model::{DeviceSummary, PeerIdentity, Protocol};
use crate::net::interfaces::{self, LocalInterface};
use crate::net::limits::RateLimiter;
use crate::proto::{DeviceDto, FerryHint, PROTOCOL_VERSION};
use crate::shared::Shared;
use localsend::http::server::PeerIp;
use localsend::multicast::{self, MulticastConfig, MulticastDevice, MulticastEvent, MulticastHandle};
use localsend::util::interface::InterfaceFilter;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const ANSWER_TIMEOUT: Duration = Duration::from_millis(1500);
const SCAN_TIMEOUT: Duration = Duration::from_millis(600);
const SCAN_CONCURRENCY: usize = 64;
const DEDUP_WINDOW: Duration = Duration::from_secs(2);
const ANNOUNCE_EVERY: Duration = Duration::from_secs(30);
const PROBE_KNOWN_EVERY: Duration = Duration::from_secs(60);
const INTERFACE_POLL: Duration = Duration::from_secs(5);

struct Running {
    handle: Arc<MulticastHandle>,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

pub struct Discovery {
    shared: Arc<Shared>,
    running: tokio::sync::Mutex<Option<Running>>,
    interfaces: Mutex<Vec<LocalInterface>>,
    answer_rate: RateLimiter,
    recent: Mutex<HashMap<(String, IpAddr), Instant>>,
    last_udp_reply: Mutex<Option<Instant>>,
    probes: Arc<Semaphore>,
    scanning: AtomicBool,
    multicast_error: Mutex<Option<String>>,
}

impl Discovery {
    pub fn new(shared: Arc<Shared>) -> Arc<Self> {
        let include_virtual = shared.settings.get().include_virtual_interfaces;
        Arc::new(Self {
            shared,
            running: tokio::sync::Mutex::new(None),
            interfaces: Mutex::new(interfaces::list(include_virtual)),
            answer_rate: RateLimiter::new(4.0, 12.0),
            recent: Mutex::new(HashMap::new()),
            last_udp_reply: Mutex::new(None),
            probes: Arc::new(Semaphore::new(SCAN_CONCURRENCY)),
            scanning: AtomicBool::new(false),
            multicast_error: Mutex::new(None),
        })
    }

    pub fn interfaces(&self) -> Vec<LocalInterface> {
        self.interfaces.lock().unwrap().clone()
    }

    pub fn multicast_error(&self) -> Option<String> {
        self.multicast_error.lock().unwrap().clone()
    }

    /// Binds multicast and starts answering. Failure to bind (port in use, no
    /// network) is reported but not fatal: HTTP discovery keeps working.
    pub async fn start(self: &Arc<Self>) {
        let settings = self.shared.settings.get();
        let net_port = self.shared.net.read().unwrap().port;
        let protocol = self.shared.net.read().unwrap().protocol;
        let include_virtual = settings.include_virtual_interfaces;
        let ifaces = interfaces::list(include_virtual);
        *self.interfaces.lock().unwrap() = ifaces.clone();

        let mut blacklist = settings.interface_blacklist.clone().unwrap_or_default();
        if !include_virtual {
            // Exact-address patterns for virtual adapters (VMs, VPNs, containers).
            blacklist.extend(interfaces::list(true).into_iter().filter(|i| i.is_virtual).map(|i| i.addr.to_string()));
        }
        let mut extra = serde_json::Map::new();
        extra.insert("ferry".into(), serde_json::to_value(FerryHint::ours()).unwrap_or_default());
        let device = MulticastDevice {
            alias: settings.alias.clone(),
            version: PROTOCOL_VERSION.to_string(),
            device_model: self.shared.device_model(),
            device_type: Some(self.shared.device_kind().into()),
            fingerprint: self.shared.identity.fingerprint.clone(),
            port: net_port,
            protocol: protocol.into(),
            download: false,
            extra,
        };
        let group: Ipv4Addr = settings.multicast_group.parse().unwrap_or(multicast::DEFAULT_MULTICAST_GROUP);
        let (tx, rx) = mpsc::channel(256);
        let (stop_tx, stop_rx) = oneshot::channel();
        let config = MulticastConfig {
            group,
            group_v6: settings.ipv6.then_some(multicast::DEFAULT_MULTICAST_GROUP_V6),
            // The multicast port is the shared rendezvous (53317), even when our
            // own HTTP server had to fall back to another port.
            port: settings.port,
            interface_filter: InterfaceFilter { whitelist: settings.interface_whitelist.clone(), blacklist: Some(blacklist) },
            device,
            event_tx: tx,
        };
        match multicast::start(config, stop_rx).await {
            Ok(handle) => {
                *self.multicast_error.lock().unwrap() = None;
                let handle = Arc::new(handle);
                let task = tokio::spawn(self.clone().event_loop(rx, handle.clone()));
                *self.running.lock().await = Some(Running { handle: handle.clone(), stop: stop_tx, task });
                let h = handle.clone();
                tokio::spawn(async move { h.announce().await });
            }
            Err(err) => {
                tracing::warn!("multicast unavailable: {err:#}");
                *self.multicast_error.lock().unwrap() = Some(format!("{err:#}"));
            }
        }
        // Staged fallback: if nothing answered shortly, sweep the subnet.
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            this.probe_known().await;
            if this.shared.settings.get().subnet_scan && this.shared.devices.list().iter().all(|d| !d.online) {
                this.scan().await;
            }
        });
    }

    pub async fn stop(&self) {
        if let Some(running) = self.running.lock().await.take() {
            let _ = running.stop.send(());
            running.handle.wait_stopped().await;
            let _ = running.task.await;
        }
    }

    /// Boxed: `start` spawns the event loop, which may call `restart`, which
    /// calls `start`; the indirection breaks the future-type cycle.
    pub fn restart(self: &Arc<Self>) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        let this = self.clone();
        Box::pin(async move {
            this.stop().await;
            this.start().await;
        })
    }

    pub async fn announce(&self) {
        let handle = self.running.lock().await.as_ref().map(|r| r.handle.clone());
        if let Some(handle) = handle {
            tokio::spawn(async move { handle.announce().await });
        }
    }

    /// Manual refresh: announce, re-probe known devices, sweep the subnet.
    pub async fn refresh(self: &Arc<Self>) {
        self.announce().await;
        self.probe_known().await;
        if self.shared.settings.get().subnet_scan {
            let this = self.clone();
            tokio::spawn(async move { this.scan().await });
        }
    }

    async fn event_loop(self: Arc<Self>, mut rx: mpsc::Receiver<MulticastEvent>, handle: Arc<MulticastHandle>) {
        while let Some(event) = rx.recv().await {
            match event {
                MulticastEvent::Discovered { ip, scope_id, message } => {
                    if message.fingerprint.eq_ignore_ascii_case(&self.shared.identity.fingerprint) {
                        continue;
                    }
                    if !interfaces::is_local_peer(ip, &self.interfaces.lock().unwrap()) {
                        continue;
                    }
                    if !self.answer_rate.check(ip) || !self.first_sighting(&message.fingerprint, ip) {
                        continue;
                    }
                    let Ok(permit) = self.probes.clone().try_acquire_owned() else { continue };
                    let announce = message.extra.get("announce").and_then(|v| v.as_bool()).unwrap_or(true);
                    let protocol: Protocol = message.protocol.into();
                    let addr = PeerAddress { host: PeerIp { ip, scope_id }.to_string(), port: message.port, protocol };
                    let expected = (protocol == Protocol::Https).then(|| message.fingerprint.to_ascii_uppercase());
                    let this = self.clone();
                    let handle = handle.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        // Registering with the announcer *is* the protocol's answer.
                        if this.probe(addr, expected, ANSWER_TIMEOUT).await.is_err() && announce {
                            this.udp_reply(&handle).await;
                        }
                    });
                }
                MulticastEvent::SocketsFailed => {
                    tracing::warn!("multicast sockets failed; rebinding");
                    let this = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        this.restart().await;
                    });
                    return;
                }
            }
        }
    }

    fn first_sighting(&self, fingerprint: &str, ip: IpAddr) -> bool {
        let mut recent = self.recent.lock().unwrap();
        let now = Instant::now();
        if recent.len() > 1024 {
            recent.retain(|_, t| now.duration_since(*t) < DEDUP_WINDOW);
        }
        let key = (fingerprint.to_ascii_uppercase(), ip);
        match recent.get(&key) {
            Some(t) if now.duration_since(*t) < DEDUP_WINDOW => false,
            _ => {
                recent.insert(key, now);
                true
            }
        }
    }

    /// The protocol's UDP fallback answer when our HTTP answer failed (the
    /// announcer may block inbound TCP). Rate-limited globally.
    async fn udp_reply(&self, handle: &MulticastHandle) {
        {
            let mut last = self.last_udp_reply.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
                return;
            }
            *last = Some(Instant::now());
        }
        handle.reply().await;
    }

    /// Registers with `addr`; on success the device is in the directory.
    pub async fn probe(&self, addr: PeerAddress, expected: Option<String>, timeout: Duration) -> anyhow::Result<String> {
        let client = PeerClient::new(&self.shared.identity, addr.clone(), expected.clone())?;
        let started = Instant::now();
        let result = client.register(&self.shared.device_dto(), timeout).await?;
        let rtt = started.elapsed().as_millis() as u32;
        let identity = match (addr.protocol, result.cert_fingerprint) {
            (Protocol::Https, Some(fp)) => PeerIdentity::Verified { fingerprint: fp },
            (Protocol::Https, None) => anyhow::bail!("no certificate"),
            (Protocol::Http, _) => PeerIdentity::PlainHttp,
        };
        if identity.fingerprint().is_some_and(|fp| fp.eq_ignore_ascii_case(&self.shared.identity.fingerprint)) {
            anyhow::bail!("that's us");
        }
        let dto = sanitize_dto(result.device);
        Ok(self.shared.devices.observe(Observation { identity, addr, dto, rtt_ms: Some(rtt) }))
    }

    /// Someone registered with our server: confirm a channel back to them.
    pub fn handle_register(self: &Arc<Self>, identity: PeerIdentity, ip: PeerIp, dto: DeviceDto) {
        if identity.fingerprint().is_some_and(|fp| fp.eq_ignore_ascii_case(&self.shared.identity.fingerprint)) {
            return;
        }
        let (Some(port), Some(protocol)) = (dto.port, dto.protocol) else { return };
        if !self.first_sighting(identity.fingerprint().unwrap_or(&dto.fingerprint), ip.ip) {
            return;
        }
        let expected = match (&identity, protocol) {
            (PeerIdentity::Verified { fingerprint }, Protocol::Https) => Some(fingerprint.clone()),
            (_, Protocol::Https) => return, // must prove the identity it claims
            (_, Protocol::Http) => None,
        };
        let Ok(permit) = self.probes.clone().try_acquire_owned() else { return };
        let this = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let addr = PeerAddress { host: ip.to_string(), port, protocol };
            if let Err(err) = this.probe(addr, expected, ANSWER_TIMEOUT).await {
                tracing::debug!("could not reach {} back: {err:#}", ip);
            }
        });
    }

    /// Adds a device by address ("connect by IP").
    pub async fn add_manual(&self, host: &str, port: u16) -> crate::Result<DeviceSummary> {
        let host = host.trim().trim_start_matches('[').trim_end_matches(']').to_string();
        if host.parse::<IpAddr>().is_err() && !host.contains('%') {
            return Err(crate::ErrorInfo::new("invalid_address", "Enter an IP address like 192.168.1.20.").into());
        }
        for protocol in [Protocol::Https, Protocol::Http] {
            let addr = PeerAddress { host: host.clone(), port, protocol };
            if let Ok(id) = self.probe(addr, None, Duration::from_secs(3)).await
                && let Some(device) = self.shared.devices.get(&id)
            {
                return Ok(device);
            }
        }
        Err(crate::ErrorInfo::unreachable(&format!("{host}:{port}")).into())
    }

    /// Re-confirms remembered devices at their last known channels.
    pub async fn probe_known(self: &Arc<Self>) {
        for fp in self.shared.devices.known_fingerprints() {
            for addr in self.shared.devices.channels(&fp).into_iter().take(2) {
                let Ok(permit) = self.probes.clone().try_acquire_owned() else { return };
                let this = self.clone();
                let fp = fp.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = this.probe(addr, Some(fp), ANSWER_TIMEOUT).await;
                });
            }
        }
    }

    /// Legacy discovery: register with every host of our (physical) subnets.
    pub async fn scan(self: &Arc<Self>) {
        if self.scanning.swap(true, Ordering::SeqCst) {
            return;
        }
        let targets = interfaces::scan_targets(&self.interfaces.lock().unwrap());
        let port = self.shared.settings.get().port;
        tracing::debug!("scanning {} hosts", targets.len());
        let mut tasks = tokio::task::JoinSet::new();
        for ip in targets {
            let permit = self.probes.clone().acquire_owned().await.expect("semaphore");
            let this = self.clone();
            tasks.spawn(async move {
                let _permit = permit;
                let addr = PeerAddress { host: ip.to_string(), port, protocol: Protocol::Https };
                let _ = this.probe(addr, None, SCAN_TIMEOUT).await;
            });
        }
        while tasks.join_next().await.is_some() {}
        self.scanning.store(false, Ordering::SeqCst);
    }

    /// Background upkeep: online/offline sweep, periodic announcements,
    /// refreshing remembered devices, and noticing network changes.
    pub fn spawn_maintenance(self: &Arc<Self>, cancel: CancellationToken, lost_peer: Arc<tokio::sync::Notify>) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut sweep = tokio::time::interval(INTERFACE_POLL);
            let mut last_announce = Instant::now();
            let mut last_probe = Instant::now();
            loop {
                tokio::select! {
                    _ = sweep.tick() => {}
                    _ = lost_peer.notified() => {
                        this.announce().await;
                        this.probe_known().await;
                        continue;
                    }
                    _ = cancel.cancelled() => return,
                }
                this.shared.devices.sweep();
                let include_virtual = this.shared.settings.get().include_virtual_interfaces;
                let now = interfaces::list(include_virtual);
                let changed = {
                    let mut current = this.interfaces.lock().unwrap();
                    let changed = *current != now;
                    if changed {
                        *current = now.clone();
                    }
                    changed
                };
                if changed {
                    tracing::info!("network interfaces changed; rebinding discovery");
                    this.shared.net.write().unwrap().addresses = interfaces::shareable_addresses(&now);
                    this.shared.events.emit(EngineEvent::LocalDeviceChanged { device: this.shared.local_device() });
                    this.restart().await;
                    last_announce = Instant::now();
                    continue;
                }
                if last_announce.elapsed() > ANNOUNCE_EVERY {
                    last_announce = Instant::now();
                    this.announce().await;
                }
                if last_probe.elapsed() > PROBE_KNOWN_EVERY {
                    last_probe = Instant::now();
                    this.probe_known().await;
                }
            }
        });
    }
}

/// Peer-supplied display fields, bounded.
pub(crate) fn sanitize_dto(mut dto: DeviceDto) -> DeviceDto {
    dto.alias = dto.display_alias();
    if dto.device_model.as_ref().is_some_and(|m| m.len() > 64) {
        dto.device_model = dto.device_model.map(|m| m.chars().take(64).collect());
    }
    dto
}
