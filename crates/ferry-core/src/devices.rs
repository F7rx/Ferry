//! The device directory: live discovery results merged with remembered
//! (trusted / favorite / mine) devices.
//!
//! Rules (fixing upstream's downgrade poisoning, docs/04-threat-model.md N3/N5):
//! - A verified device is keyed by its certificate fingerprint and only gains
//!   channels that answered a pinned HTTPS request.
//! - Plain-HTTP peers get their own `http:<ip>:<port>` entry. They can never be
//!   trusted and never merge into or shadow a verified device.
//! - The table is bounded; entries go offline and expire.

use crate::client::PeerAddress;
use crate::db::{Db, KnownDevice};
use crate::events::{EngineEvent, EventBus};
use crate::model::*;
use crate::proto::DeviceDto;
use crate::util::now_ms;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_LIVE: usize = 256;
const MAX_CHANNELS: usize = 8;
/// Without a confirmation for this long a device is shown as offline.
pub const ONLINE_TTL: Duration = Duration::from_secs(75);
/// Unremembered devices disappear after this long offline.
const FORGET_AFTER: Duration = Duration::from_secs(10 * 60);
/// Remembered devices' last address is written back at most this often.
const PERSIST_EVERY: Duration = Duration::from_secs(120);

#[derive(Clone, Debug)]
pub struct Observation {
    pub identity: PeerIdentity,
    pub addr: PeerAddress,
    pub dto: DeviceDto,
    pub rtt_ms: Option<u32>,
}

#[derive(Clone, Debug)]
struct Channel {
    addr: PeerAddress,
    rtt_ms: Option<u32>,
    last_ok: Instant,
    failures: u32,
}

#[derive(Clone, Debug)]
struct Live {
    verified: bool,
    dto: DeviceDto,
    channels: Vec<Channel>,
    last_seen: Instant,
    last_seen_ms: u64,
    ferry_confirmed: Option<bool>,
    persisted_at: Option<Instant>,
    reported_online: bool,
    /// Shown as the address of devices without LAN channels (WebRTC peers).
    label: Option<String>,
}

/// Trust flags for a verified fingerprint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trust {
    pub trusted: bool,
    pub favorite: bool,
    pub mine: bool,
}

pub struct DeviceDirectory {
    db: Arc<Db>,
    events: EventBus,
    state: Mutex<State>,
}

struct State {
    known: HashMap<String, KnownDevice>,
    live: HashMap<String, Live>,
}

pub fn plain_id(addr: &PeerAddress) -> String {
    format!("http:{}", addr.display())
}

impl DeviceDirectory {
    pub fn new(db: Arc<Db>, events: EventBus) -> crate::Result<Arc<Self>> {
        let known = db.known_devices()?.into_iter().map(|d| (d.fingerprint.clone(), d)).collect();
        Ok(Arc::new(Self { db, events, state: Mutex::new(State { known, live: HashMap::new() }) }))
    }

    /// Records a confirmed contact. Returns the device id.
    pub fn observe(&self, obs: Observation) -> String {
        let id = match &obs.identity {
            PeerIdentity::Verified { fingerprint } => fingerprint.clone(),
            _ => plain_id(&obs.addr),
        };
        let verified = obs.identity.is_verified();
        let now = Instant::now();
        let summary = {
            let mut state = self.state.lock().unwrap();
            if !state.live.contains_key(&id) && state.live.len() >= MAX_LIVE {
                evict_oldest(&mut state.live);
            }
            let live = state.live.entry(id.clone()).or_insert_with(|| Live {
                verified,
                dto: obs.dto.clone(),
                channels: Vec::new(),
                last_seen: now,
                last_seen_ms: now_ms(),
                ferry_confirmed: None,
                persisted_at: None,
                reported_online: false,
                label: None,
            });
            live.dto = obs.dto.clone();
            live.last_seen = now;
            live.last_seen_ms = now_ms();
            match live.channels.iter_mut().find(|c| c.addr == obs.addr) {
                Some(channel) => {
                    channel.last_ok = now;
                    channel.failures = 0;
                    if obs.rtt_ms.is_some() {
                        channel.rtt_ms = obs.rtt_ms;
                    }
                }
                None => {
                    if live.channels.len() >= MAX_CHANNELS {
                        live.channels.sort_by_key(|c| std::cmp::Reverse(c.last_ok));
                        live.channels.truncate(MAX_CHANNELS - 1);
                    }
                    live.channels.push(Channel { addr: obs.addr.clone(), rtt_ms: obs.rtt_ms, last_ok: now, failures: 0 });
                }
            }
            live.reported_online = true;
            let persist = verified
                && state.known.contains_key(&id)
                && state.live.get(&id).is_some_and(|l| l.persisted_at.is_none_or(|t| t.elapsed() > PERSIST_EVERY));
            if persist {
                let addr = obs.addr.clone();
                let dto = obs.dto.clone();
                if let Some(known) = state.known.get_mut(&id) {
                    known.alias = dto.display_alias();
                    known.device_model = dto.device_model.clone();
                    known.device_kind = dto.device_type.unwrap_or_default();
                    known.last_address = Some(addr.host.clone());
                    known.last_port = Some(addr.port);
                    known.last_protocol = Some(addr.protocol.as_str().to_string());
                    known.last_seen_ms = now_ms();
                    known.is_ferry = known.is_ferry || dto.ferry.is_some();
                    let _ = self.db.upsert_device(known);
                }
                if let Some(live) = state.live.get_mut(&id) {
                    live.persisted_at = Some(now);
                }
            }
            summarize(&id, &state)
        };
        if let Some(device) = summary {
            self.events.emit(EngineEvent::DeviceUpdated { device });
        }
        id
    }

    /// Records a device reachable over WebRTC (seen on the signaling server).
    /// `id` is derived from its identity key, which every session pins, so the
    /// entry counts as verified; it has no LAN channels.
    pub fn observe_remote(&self, id: &str, dto: DeviceDto, label: Option<String>) {
        let now = Instant::now();
        let summary = {
            let mut state = self.state.lock().unwrap();
            if !state.live.contains_key(id) && state.live.len() >= MAX_LIVE {
                evict_oldest(&mut state.live);
            }
            let live = state.live.entry(id.to_string()).or_insert_with(|| Live {
                verified: true,
                dto: dto.clone(),
                channels: Vec::new(),
                last_seen: now,
                last_seen_ms: now_ms(),
                ferry_confirmed: Some(true),
                persisted_at: None,
                reported_online: false,
                label: None,
            });
            live.dto = dto.clone();
            live.last_seen = now;
            live.last_seen_ms = now_ms();
            live.label = label;
            live.reported_online = true;
            if let Some(known) = state.known.get_mut(id)
                && (known.alias != dto.display_alias() || now_ms().saturating_sub(known.last_seen_ms) > PERSIST_EVERY.as_millis() as u64)
            {
                known.alias = dto.display_alias();
                known.device_model = dto.device_model.clone();
                known.device_kind = dto.device_type.unwrap_or_default();
                known.last_seen_ms = now_ms();
                known.is_ferry = true;
                let _ = self.db.upsert_device(known);
            }
            summarize(id, &state)
        };
        if let Some(device) = summary {
            self.events.emit(EngineEvent::DeviceUpdated { device });
        }
    }

    /// A WebRTC device left the signaling server: offline (remembered) or gone.
    pub fn remote_gone(&self, id: &str) {
        let (summary, removed) = {
            let mut state = self.state.lock().unwrap();
            if state.live.remove(id).is_none() {
                return;
            }
            (summarize(id, &state), !state.known.contains_key(id))
        };
        match summary {
            Some(device) if !removed => self.events.emit(EngineEvent::DeviceUpdated { device }),
            _ => self.events.emit(EngineEvent::DeviceRemoved { id: id.to_string() }),
        }
    }

    /// Notes the result of a Ferry `hello` probe.
    pub fn set_ferry_confirmed(&self, id: &str, is_ferry: bool) {
        let mut state = self.state.lock().unwrap();
        if let Some(live) = state.live.get_mut(id) {
            live.ferry_confirmed = Some(is_ferry);
        }
    }

    pub fn ferry_confirmed(&self, id: &str) -> Option<bool> {
        self.state.lock().unwrap().live.get(id).and_then(|l| l.ferry_confirmed)
    }

    /// A channel failed (connect error); it sinks in the ranking.
    pub fn channel_failed(&self, id: &str, addr: &PeerAddress) {
        let mut state = self.state.lock().unwrap();
        if let Some(channel) = state.live.get_mut(id).and_then(|l| l.channels.iter_mut().find(|c| &c.addr == addr)) {
            channel.failures += 1;
        }
    }

    /// Known addresses of a device, best first.
    pub fn channels(&self, id: &str) -> Vec<PeerAddress> {
        let state = self.state.lock().unwrap();
        let mut out: Vec<(u32, u32, Instant, PeerAddress)> = state
            .live
            .get(id)
            .map(|l| l.channels.iter().map(|c| (c.failures, c.rtt_ms.unwrap_or(500), c.last_ok, c.addr.clone())).collect())
            .unwrap_or_default();
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(b.2.cmp(&a.2)));
        let mut addrs: Vec<PeerAddress> = out.into_iter().map(|c| c.3).collect();
        // Remembered address of an offline favorite: still worth a try.
        if let Some(known) = state.known.get(id)
            && let (Some(host), Some(port)) = (&known.last_address, known.last_port)
        {
            let protocol = if known.last_protocol.as_deref() == Some("http") { Protocol::Http } else { Protocol::Https };
            let addr = PeerAddress { host: host.clone(), port, protocol };
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
        addrs
    }

    pub fn dto(&self, id: &str) -> Option<DeviceDto> {
        self.state.lock().unwrap().live.get(id).map(|l| l.dto.clone())
    }

    pub fn trust(&self, fingerprint: &str) -> Trust {
        self.state
            .lock()
            .unwrap()
            .known
            .get(fingerprint)
            .map(|k| Trust { trusted: k.trusted || k.mine, favorite: k.favorite, mine: k.mine })
            .unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<DeviceSummary> {
        summarize(id, &self.state.lock().unwrap())
    }

    pub fn list(&self) -> Vec<DeviceSummary> {
        let state = self.state.lock().unwrap();
        let mut ids: Vec<&String> = state.live.keys().collect();
        for id in state.known.keys() {
            if !state.live.contains_key(id) {
                ids.push(id);
            }
        }
        let mut out: Vec<DeviceSummary> = ids.into_iter().filter_map(|id| summarize(id, &state)).collect();
        out.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then(b.mine.cmp(&a.mine))
                .then(b.favorite.cmp(&a.favorite))
                .then(a.alias.to_lowercase().cmp(&b.alias.to_lowercase()))
        });
        out
    }

    /// Changes trust flags / the custom name of a verified device.
    pub fn update_flags(
        &self,
        id: &str,
        trusted: Option<bool>,
        favorite: Option<bool>,
        mine: Option<bool>,
        custom_alias: Option<Option<String>>,
    ) -> crate::Result<Option<DeviceSummary>> {
        let summary = {
            let mut state = self.state.lock().unwrap();
            let Some(live_or_known) =
                state.known.get(id).cloned().or_else(|| state.live.get(id).filter(|l| l.verified).map(|l| known_from_live(id, l)))
            else {
                return Err(crate::ErrorInfo::new("not_verified", "Only devices with a verified identity can be trusted.").into());
            };
            let mut known = live_or_known;
            if let Some(t) = trusted {
                known.trusted = t;
            }
            if let Some(f) = favorite {
                known.favorite = f;
            }
            if let Some(m) = mine {
                known.mine = m;
                if m {
                    known.trusted = true;
                }
            }
            if let Some(alias) = custom_alias {
                known.custom_alias = alias.map(|a| a.trim().chars().take(64).collect::<String>()).filter(|a| !a.is_empty());
            }
            if !known.trusted && !known.favorite && !known.mine && known.custom_alias.is_none() {
                self.db.forget_device(id)?;
                state.known.remove(id);
            } else {
                self.db.upsert_device(&known)?;
                state.known.insert(id.to_string(), known);
            }
            summarize(id, &state)
        };
        if let Some(device) = &summary {
            self.events.emit(EngineEvent::DeviceUpdated { device: device.clone() });
        } else {
            self.events.emit(EngineEvent::DeviceRemoved { id: id.to_string() });
        }
        Ok(summary)
    }

    /// Removes a device entirely (trust and live state).
    pub fn forget(&self, id: &str) -> crate::Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            state.known.remove(id);
            state.live.remove(id);
        }
        self.db.forget_device(id)?;
        self.events.emit(EngineEvent::DeviceRemoved { id: id.to_string() });
        Ok(())
    }

    /// Marks stale devices offline and forgets long-gone unremembered ones.
    pub fn sweep(&self) {
        let mut updates = Vec::new();
        let mut removed = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            let ids: Vec<String> = state.live.keys().cloned().collect();
            for id in ids {
                let (stale, gone) = {
                    let live = &state.live[&id];
                    (live.last_seen.elapsed() > ONLINE_TTL, live.last_seen.elapsed() > FORGET_AFTER)
                };
                if gone && !state.known.contains_key(&id) {
                    state.live.remove(&id);
                    removed.push(id);
                } else if stale && state.live[&id].reported_online {
                    state.live.get_mut(&id).unwrap().reported_online = false;
                    if let Some(s) = summarize(&id, &state) {
                        updates.push(s);
                    }
                }
            }
        }
        for device in updates {
            self.events.emit(EngineEvent::DeviceUpdated { device });
        }
        for id in removed {
            self.events.emit(EngineEvent::DeviceRemoved { id });
        }
    }

    /// Fingerprints of remembered devices (probed periodically).
    pub fn known_fingerprints(&self) -> Vec<String> {
        self.state.lock().unwrap().known.keys().cloned().collect()
    }
}

fn evict_oldest(live: &mut HashMap<String, Live>) {
    if let Some(oldest) = live.iter().min_by_key(|(_, l)| l.last_seen).map(|(id, _)| id.clone()) {
        live.remove(&oldest);
    }
}

fn known_from_live(id: &str, live: &Live) -> KnownDevice {
    let addr = live.channels.first().map(|c| c.addr.clone());
    KnownDevice {
        fingerprint: id.to_string(),
        alias: live.dto.display_alias(),
        custom_alias: None,
        device_model: live.dto.device_model.clone(),
        device_kind: live.dto.device_type.unwrap_or_default(),
        trusted: false,
        favorite: false,
        mine: false,
        last_address: addr.as_ref().map(|a| a.host.clone()),
        last_port: addr.as_ref().map(|a| a.port),
        last_protocol: addr.as_ref().map(|a| a.protocol.as_str().to_string()),
        last_seen_ms: live.last_seen_ms,
        is_ferry: live.dto.ferry.is_some(),
    }
}

fn summarize(id: &str, state: &State) -> Option<DeviceSummary> {
    let known = state.known.get(id);
    let live = state.live.get(id);
    if known.is_none() && live.is_none() {
        return None;
    }
    let best = live.and_then(|l| {
        l.channels.iter().min_by(|a, b| a.failures.cmp(&b.failures).then(a.rtt_ms.unwrap_or(500).cmp(&b.rtt_ms.unwrap_or(500))))
    });
    let online = live.is_some_and(|l| l.last_seen.elapsed() <= ONLINE_TTL);
    let (alias, device_model, device_kind, protocol, download, is_ferry) = match (live, known) {
        (Some(l), _) => (
            l.dto.display_alias(),
            l.dto.device_model.clone(),
            l.dto.device_type.unwrap_or_default(),
            best.map(|c| c.addr.protocol).unwrap_or(Protocol::Https),
            l.dto.download,
            l.ferry_confirmed.unwrap_or(l.dto.ferry.is_some()),
        ),
        (None, Some(k)) => (
            k.alias.clone(),
            k.device_model.clone(),
            k.device_kind,
            if k.last_protocol.as_deref() == Some("http") { Protocol::Http } else { Protocol::Https },
            false,
            k.is_ferry,
        ),
        (None, None) => unreachable!(),
    };
    Some(DeviceSummary {
        id: id.to_string(),
        alias,
        device_model,
        device_kind,
        verified: live.map(|l| l.verified).unwrap_or(true),
        protocol,
        is_ferry,
        trusted: known.is_some_and(|k| k.trusted || k.mine),
        favorite: known.is_some_and(|k| k.favorite),
        mine: known.is_some_and(|k| k.mine),
        online,
        last_seen_ms: live.map(|l| l.last_seen_ms).or(known.map(|k| k.last_seen_ms)).unwrap_or(0),
        address: best
            .map(|c| c.addr.display())
            .or_else(|| live.and_then(|l| l.label.clone()))
            .or_else(|| known.and_then(|k| Some(format!("{}:{}", k.last_address.as_ref()?, k.last_port?)))),
        ip_version: best.map(|c| c.addr.ip_version()),
        rtt_ms: best.and_then(|c| c.rtt_ms),
        custom_alias: known.and_then(|k| k.custom_alias.clone()),
        download,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::FerryHint;

    fn dto(alias: &str, fp: &str) -> DeviceDto {
        DeviceDto {
            alias: alias.into(),
            version: "2.2".into(),
            device_model: None,
            device_type: Some(DeviceKind::Mobile),
            fingerprint: fp.into(),
            port: Some(53317),
            protocol: Some(Protocol::Https),
            download: false,
            ferry: Some(FerryHint::ours()),
        }
    }

    fn addr(host: &str, protocol: Protocol) -> PeerAddress {
        PeerAddress { host: host.into(), port: 53317, protocol }
    }

    fn dir() -> Arc<DeviceDirectory> {
        DeviceDirectory::new(Arc::new(Db::open_in_memory().unwrap()), EventBus::new()).unwrap()
    }

    #[test]
    fn plain_http_claims_cannot_poison_a_verified_device() {
        let d = dir();
        let id = d.observe(Observation {
            identity: PeerIdentity::Verified { fingerprint: "AAAA".into() },
            addr: addr("192.168.1.10", Protocol::Https),
            dto: dto("Laptop", "AAAA"),
            rtt_ms: Some(5),
        });
        // An attacker announces the victim's fingerprint over plain HTTP.
        let attacker = d.observe(Observation {
            identity: PeerIdentity::PlainHttp,
            addr: addr("192.168.1.66", Protocol::Http),
            dto: dto("Laptop", "AAAA"),
            rtt_ms: Some(1),
        });
        assert_ne!(id, attacker);
        assert_eq!(d.channels(&id), vec![addr("192.168.1.10", Protocol::Https)]);
        assert!(!d.get(&attacker).unwrap().verified);
        // ...and can never be trusted.
        assert!(d.update_flags(&attacker, Some(true), None, None, None).is_err());
    }

    #[test]
    fn channels_rank_by_failures_then_rtt() {
        let d = dir();
        let fp = PeerIdentity::Verified { fingerprint: "BB".into() };
        for (host, rtt) in [("10.0.0.2", 40), ("10.0.0.3", 3)] {
            d.observe(Observation { identity: fp.clone(), addr: addr(host, Protocol::Https), dto: dto("Tab", "BB"), rtt_ms: Some(rtt) });
        }
        assert_eq!(d.channels("BB")[0].host, "10.0.0.3");
        d.channel_failed("BB", &addr("10.0.0.3", Protocol::Https));
        assert_eq!(d.channels("BB")[0].host, "10.0.0.2");
    }

    #[test]
    fn trust_persists_and_survives_restart() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let d = DeviceDirectory::new(db.clone(), EventBus::new()).unwrap();
        d.observe(Observation {
            identity: PeerIdentity::Verified { fingerprint: "CC".into() },
            addr: addr("10.0.0.9", Protocol::Https),
            dto: dto("Phone", "CC"),
            rtt_ms: None,
        });
        d.update_flags("CC", Some(true), Some(true), None, Some(Some("My Phone".into()))).unwrap();
        let again = DeviceDirectory::new(db, EventBus::new()).unwrap();
        let trust = again.trust("CC");
        assert!(trust.trusted && trust.favorite && !trust.mine);
        let listed = again.get("CC").unwrap();
        assert!(!listed.online);
        assert_eq!(listed.custom_alias.as_deref(), Some("My Phone"));
        assert_eq!(again.channels("CC")[0].host, "10.0.0.9");
    }

    #[test]
    fn table_is_bounded() {
        let d = dir();
        for i in 0..(MAX_LIVE + 20) {
            d.observe(Observation {
                identity: PeerIdentity::Verified { fingerprint: format!("FP{i}") },
                addr: addr(&format!("10.0.{}.{}", i / 250, i % 250 + 1), Protocol::Https),
                dto: dto("x", ""),
                rtt_ms: None,
            });
        }
        assert!(d.list().len() <= MAX_LIVE);
    }
}
