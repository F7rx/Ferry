//! Pairing: making another device one of "my devices" (trusted both ways,
//! auto-accept), with an out-of-band check that defeats a machine in the
//! middle. Two ways (docs/05-protocol.md §3.4):
//!
//! - **QR / link.** We show a single-use secret; the other device proves it
//!   saw it with an HMAC bound to both TLS fingerprints. Showing it is the
//!   consent, so there is no prompt here.
//! - **Code comparison.** Both screens show a 6-digit code derived from both
//!   fingerprints and a nonce from each side; the person at the receiving
//!   device confirms they match. The asker commits to its nonce before it
//!   sees the other one, so nobody can grind certificates or nonces until a
//!   code happens to match (a relay gets one 1-in-a-million guess per prompt
//!   the person confirms).

use crate::client::{PeerAddress, PeerClient};
use crate::devices::Observation;
use crate::error::{ErrorInfo, Result};
use crate::events::{EngineEvent, NoticeLevel};
use crate::model::{DeviceSummary, PeerIdentity, PeerRef, Protocol};
use crate::net::limits::{Attempt, FailureTracker, peer_key};
use crate::proto::DeviceDto;
use crate::server::{self, PeerContext, Resp, Routes, error, json};
use crate::shared::Shared;
use crate::util::{now_ms, random_bytes, random_token};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use hyper::StatusCode;
use hyper::body::Incoming;
use localsend::reqwest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub const PAIR_PATH: &str = "/api/ferry/v1/pair";
pub const UNPAIR_PATH: &str = "/api/ferry/v1/unpair";
pub const REVEAL_PATH: &str = "/api/ferry/v1/pair/reveal";
/// Between the commitment and the reveal (both are machine steps).
const COMMIT_TTL: Duration = Duration::from_secs(30);
const COMMIT_DOMAIN: &[u8] = b"ferry-pair-commit/1";
const URI_PREFIX: &str = "ferry://pair?";
/// How long a shown QR code stays valid.
const OFFER_TTL: Duration = Duration::from_secs(5 * 60);
/// How long a code-comparison prompt waits for an answer.
const CODE_TTL: Duration = Duration::from_secs(2 * 60);
const MAX_OFFERS: usize = 4;
const MAX_PROMPTS: usize = 4;
const MAX_URI_ADDRESSES: usize = 8;
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// A QR code / link this device is showing.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingOffer {
    pub id: String,
    pub uri: String,
    pub expires_at_ms: u64,
}

/// Someone asked to pair by code comparison; the person here decides.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingRequest {
    pub id: String,
    pub peer: PeerRef,
    /// "123 456"; must match the code on the other screen.
    pub code: String,
    pub expires_at_ms: u64,
}

/// A code-comparison request we sent; finishes with `PairingFinished`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutgoingPairing {
    pub id: String,
    pub peer: PeerRef,
    pub code: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingOutcome {
    Paired,
    Declined,
    Cancelled,
    Failed,
}

#[derive(Serialize, Deserialize)]
struct PairBody {
    /// base64url HMAC for the QR flow; absent for code comparison.
    #[serde(default)]
    proof: Option<String>,
    /// Code comparison: base64url SHA-256("ferry-pair-commit/1" ‖ asker's nonce).
    #[serde(default)]
    commit: Option<String>,
    device: DeviceDto,
}

/// Answer to a commitment: where to reveal, and the responder's nonce.
#[derive(Serialize, Deserialize)]
struct CommitReply {
    session: String,
    nonce: String,
}

#[derive(Serialize, Deserialize)]
struct RevealBody {
    session: String,
    nonce: String,
}

/// A code-comparison request waiting for the asker's reveal.
struct Commitment {
    fingerprint: String,
    group: IpAddr,
    commit: [u8; 32],
    nonce: [u8; 32],
    dto: DeviceDto,
    expires_at: Instant,
}

struct Offer {
    id: String,
    secret: [u8; 16],
    expires_at: Instant,
}

struct Prompt {
    fingerprint: String,
    /// The asker's IP group (IPv4 address or IPv6 /64): one open request each.
    group: IpAddr,
    decide: Option<oneshot::Sender<bool>>,
}

pub struct PairingManager {
    shared: Arc<Shared>,
    offers: Mutex<Vec<Offer>>,
    prompts: Mutex<HashMap<String, Prompt>>,
    commitments: Mutex<HashMap<String, Commitment>>,
    outgoing: Mutex<HashMap<String, CancellationToken>>,
    failures: FailureTracker,
}

impl PairingManager {
    pub fn new(shared: Arc<Shared>) -> Arc<Self> {
        Arc::new(Self {
            shared,
            offers: Mutex::new(Vec::new()),
            prompts: Mutex::new(HashMap::new()),
            commitments: Mutex::new(HashMap::new()),
            outgoing: Mutex::new(HashMap::new()),
            failures: FailureTracker::new(5, 50, Duration::from_secs(5 * 60)),
        })
    }

    // ── Showing a QR code ─────────────────────────────────────────────────

    pub fn create_offer(&self) -> Result<PairingOffer> {
        let (addresses, port, protocol) = {
            let net = self.shared.net.read().unwrap();
            (net.addresses.clone(), net.port, net.protocol)
        };
        if protocol != Protocol::Https {
            return Err(ErrorInfo::new("encryption_off", "Turn on encryption to pair devices.").into());
        }
        if addresses.is_empty() {
            return Err(ErrorInfo::new("no_network", "Connect to a network first.").into());
        }
        let secret = random_bytes::<16>();
        let uri = pair_uri(&self.shared.identity.fingerprint, &addresses, port, &secret);
        let offer = Offer { id: random_token(), secret, expires_at: Instant::now() + OFFER_TTL };
        let info = PairingOffer { id: offer.id.clone(), uri, expires_at_ms: now_ms() + OFFER_TTL.as_millis() as u64 };
        let evicted = {
            let mut offers = self.offers.lock().unwrap();
            let mut evicted = take_expired(&mut offers);
            if offers.len() >= MAX_OFFERS {
                evicted.push(offers.remove(0).id);
            }
            offers.push(offer);
            evicted
        };
        for id in evicted {
            self.shared.events.emit(EngineEvent::PairingOfferClosed { id, device: None });
        }
        Ok(info)
    }

    pub fn cancel_offer(&self, id: &str) -> bool {
        let removed = {
            let mut offers = self.offers.lock().unwrap();
            let before = offers.len();
            offers.retain(|o| o.id != id);
            offers.len() != before
        };
        if removed {
            self.shared.events.emit(EngineEvent::PairingOfferClosed { id: id.to_string(), device: None });
        }
        removed
    }

    /// Drops expired offers (housekeeping).
    pub fn prune(&self) {
        let expired = take_expired(&mut self.offers.lock().unwrap());
        for id in expired {
            self.shared.events.emit(EngineEvent::PairingOfferClosed { id, device: None });
        }
    }

    // ── Scanning a QR code ────────────────────────────────────────────────

    /// Pairs with the device that shows `uri` (scanned or pasted).
    pub async fn pair_with_uri(&self, uri: &str) -> Result<DeviceSummary> {
        let parsed = ParsedUri::parse(uri)?;
        let ours = self.shared.identity.fingerprint.clone();
        if parsed.fingerprint.eq_ignore_ascii_case(&ours) {
            return Err(ErrorInfo::new("pair_self", "That code belongs to this device. Scan it with the other one.").into());
        }
        let (client, id) = self.reach(&parsed).await?;
        let proof = URL_SAFE_NO_PAD.encode(proof(&parsed.secret, &parsed.fingerprint, &ours));
        let body = PairBody { proof: Some(proof), commit: None, device: self.shared.device_dto() };
        let resp = client
            .http_client()
            .post(format!("{}{PAIR_PATH}", client_base(&client)))
            .json(&body)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ErrorInfo::new("pair_failed", "Couldn't reach the other device.").with_hint(e.to_string()))?;
        match resp.status().as_u16() {
            200 => self.mark_mine(&id),
            403 => {
                Err(ErrorInfo::new("pair_expired", "This code has expired or was already used. Show a new one on the other device.").into())
            }
            429 => Err(ErrorInfo::new("pair_locked", "Too many attempts. Wait a few minutes and try again.").into()),
            404 => {
                Err(ErrorInfo::new("pair_unsupported", "The other device doesn't support pairing. Update it to the latest Ferry.").into())
            }
            s => Err(ErrorInfo::new("pair_failed", format!("Pairing failed (HTTP {s}).")).into()),
        }
    }

    /// Connects to the first address in the URI that answers with the pinned
    /// certificate, and registers it in the device directory.
    async fn reach(&self, parsed: &ParsedUri) -> Result<(PeerClient, String)> {
        let mut last = None;
        for host in &parsed.addresses {
            let addr = PeerAddress { host: host.clone(), port: parsed.port, protocol: Protocol::Https };
            let client = PeerClient::new(&self.shared.identity, addr.clone(), Some(parsed.fingerprint.clone()))
                .map_err(|e| ErrorInfo::internal(&e))?;
            match client.register(&self.shared.device_dto(), PROBE_TIMEOUT).await {
                Ok(result) if result.cert_fingerprint.as_deref().is_some_and(|fp| fp.eq_ignore_ascii_case(&parsed.fingerprint)) => {
                    let identity = PeerIdentity::Verified { fingerprint: parsed.fingerprint.clone() };
                    let dto = crate::discovery::sanitize_dto(result.device);
                    let id = self.shared.devices.observe(Observation { identity, addr, dto, rtt_ms: None });
                    return Ok((client, id));
                }
                Ok(_) => last = Some("certificate mismatch".to_string()),
                Err(e) => last = Some(e.to_string()),
            }
        }
        let mut err = ErrorInfo::new("pair_unreachable", "Couldn't reach the other device. Make sure both are on the same network.");
        if let Some(detail) = last {
            err = err.with_hint(detail);
        }
        Err(err.into())
    }

    // ── Code comparison, asking side ──────────────────────────────────────

    /// Asks `device_id` to pair. Returns once both nonces are exchanged and
    /// the code is known; the result arrives as `PairingFinished`.
    pub async fn start_code_pairing(self: &Arc<Self>, device_id: &str) -> Result<OutgoingPairing> {
        let device =
            self.shared.devices.get(device_id).ok_or_else(|| ErrorInfo::new("unknown_device", "That device is no longer around."))?;
        if !device.verified {
            return Err(ErrorInfo::new("not_verified", "Only devices with a verified identity can be paired.").into());
        }
        let addr = self
            .shared
            .devices
            .channels(device_id)
            .into_iter()
            .find(|a| a.protocol == Protocol::Https)
            .ok_or_else(|| ErrorInfo::new("offline", format!("{} is offline.", device.alias)))?;
        let fingerprint = device_id.to_string();
        let ours = self.shared.identity.fingerprint.clone();
        let client = PeerClient::new(&self.shared.identity, addr, Some(fingerprint.clone())).map_err(|e| ErrorInfo::internal(&e))?;

        // 1. Commit to our nonce; learn theirs.
        let ours_nonce = random_bytes::<32>();
        let body =
            PairBody { proof: None, commit: Some(URL_SAFE_NO_PAD.encode(commitment(&ours_nonce))), device: self.shared.device_dto() };
        let resp = client
            .http_client()
            .post(format!("{}{PAIR_PATH}", client_base(&client)))
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| describe_send_error(&e))?;
        match resp.status().as_u16() {
            200 => {}
            429 => return Err(ErrorInfo::new("pair_busy", "The other device is busy with another pairing. Try again shortly.").into()),
            404 | 400 => {
                return Err(ErrorInfo::new(
                    "pair_unsupported",
                    "The other device doesn't support this kind of pairing. Update it to the latest Ferry.",
                )
                .into());
            }
            s => return Err(ErrorInfo::new("pair_failed", format!("Pairing failed (HTTP {s}).")).into()),
        }
        let reply: CommitReply =
            resp.json().await.map_err(|_| ErrorInfo::new("pair_failed", "The other device answered something unexpected."))?;
        let theirs_nonce: [u8; 32] = URL_SAFE_NO_PAD
            .decode(reply.nonce.as_bytes())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| ErrorInfo::new("pair_failed", "The other device answered something unexpected."))?;

        let pairing = OutgoingPairing {
            id: random_token(),
            peer: PeerRef {
                id: device.id.clone(),
                alias: device.custom_alias.clone().unwrap_or(device.alias.clone()),
                device_kind: device.device_kind,
                device_model: device.device_model.clone(),
                verified: true,
            },
            code: comparison_code(&ours, &fingerprint, &ours_nonce, &theirs_nonce),
        };
        let cancel = CancellationToken::new();
        self.outgoing.lock().unwrap().insert(pairing.id.clone(), cancel.clone());

        // 2. Reveal our nonce; the other side shows the same code and asks its person.
        let this = self.clone();
        let id = pairing.id.clone();
        let device_id = device_id.to_string();
        tokio::spawn(async move {
            let request = this.reveal(client, reply.session, ours_nonce);
            let (outcome, error) = tokio::select! {
                r = request => r,
                _ = cancel.cancelled() => (PairingOutcome::Cancelled, None),
            };
            let device = if outcome == PairingOutcome::Paired {
                match this.mark_mine(&device_id) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        this.finish(&id, PairingOutcome::Failed, None, Some(e.info()));
                        return;
                    }
                }
            } else {
                None
            };
            this.finish(&id, outcome, device, error);
        });
        Ok(pairing)
    }

    async fn reveal(&self, client: PeerClient, session: String, nonce: [u8; 32]) -> (PairingOutcome, Option<ErrorInfo>) {
        let body = RevealBody { session, nonce: URL_SAFE_NO_PAD.encode(nonce) };
        let sent = client
            .http_client()
            .post(format!("{}{REVEAL_PATH}", client_base(&client)))
            .json(&body)
            .timeout(CODE_TTL + Duration::from_secs(10))
            .send()
            .await;
        match sent.map(|r| r.status().as_u16()) {
            Ok(200) => (PairingOutcome::Paired, None),
            Ok(403) => (PairingOutcome::Declined, None),
            Ok(408) => (PairingOutcome::Failed, Some(ErrorInfo::new("pair_timeout", "Nobody confirmed on the other device."))),
            Ok(410) => (PairingOutcome::Failed, Some(ErrorInfo::new("pair_expired", "The pairing request expired. Try again."))),
            Ok(429) => (
                PairingOutcome::Failed,
                Some(ErrorInfo::new("pair_busy", "The other device is busy with another pairing. Try again shortly.")),
            ),
            Ok(s) => (PairingOutcome::Failed, Some(ErrorInfo::new("pair_failed", format!("Pairing failed (HTTP {s}).")))),
            Err(e) => (PairingOutcome::Failed, Some(describe_send_error(&e))),
        }
    }

    fn finish(&self, id: &str, outcome: PairingOutcome, device: Option<DeviceSummary>, error: Option<ErrorInfo>) {
        self.outgoing.lock().unwrap().remove(id);
        self.shared.events.emit(EngineEvent::PairingFinished { id: id.to_string(), outcome, device, error });
    }

    pub fn cancel_code_pairing(&self, id: &str) -> bool {
        match self.outgoing.lock().unwrap().get(id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    // ── Code comparison, answering side ───────────────────────────────────

    /// The person here confirmed (or rejected) that the codes match.
    pub fn respond(&self, request_id: &str, accept: bool) -> bool {
        let sender = self.prompts.lock().unwrap().get_mut(request_id).and_then(|p| p.decide.take());
        sender.is_some_and(|tx| tx.send(accept).is_ok())
    }

    // ── Server side ───────────────────────────────────────────────────────

    /// `POST /api/ferry/v1/pair` (mutual TLS already verified by the router).
    pub async fn handle_pair(self: &Arc<Self>, peer: &PeerContext, body: Incoming, routes: &Routes) -> Resp {
        let Some(theirs) = peer.identity.fingerprint().map(str::to_string) else {
            return error(StatusCode::FORBIDDEN, "verified client certificate required");
        };
        if self.failures.check(peer.ip.ip) == Attempt::LockedOut {
            return error(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
        }
        let body: PairBody = match server::read_json(peer, body, server::SMALL_JSON_LIMIT, routes).await {
            Ok(b) => b,
            Err(resp) => return resp,
        };
        if body.device.validate().is_err() {
            return error(StatusCode::BAD_REQUEST, "invalid body");
        }
        let dto = crate::discovery::sanitize_dto(body.device);
        let ours = self.shared.identity.fingerprint.clone();

        match body.proof {
            Some(given) => {
                let given = URL_SAFE_NO_PAD.decode(given.as_bytes()).unwrap_or_default();
                let matched = {
                    let mut offers = self.offers.lock().unwrap();
                    let _ = take_expired(&mut offers);
                    offers.iter().position(|o| verify_proof(&o.secret, &ours, &theirs, &given)).map(|i| offers.remove(i))
                };
                let Some(offer) = matched else {
                    self.failures.record_failure(peer.ip.ip);
                    return error(StatusCode::FORBIDDEN, "invalid or expired pairing code");
                };
                self.failures.record_success(peer.ip.ip);
                match self.store(peer.ip.ip, &theirs, dto) {
                    Ok(device) => {
                        self.shared.events.emit(EngineEvent::PairingOfferClosed { id: offer.id, device: Some(device) });
                        json(StatusCode::OK, &serde_json::json!({ "device": self.shared.device_dto() }))
                    }
                    Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "could not store pairing"),
                }
            }
            None => {
                // Code comparison, step 1: remember the asker's commitment, hand out our nonce.
                let Some(commit) = body
                    .commit
                    .as_deref()
                    .and_then(|c| URL_SAFE_NO_PAD.decode(c.as_bytes()).ok())
                    .and_then(|b| <[u8; 32]>::try_from(b).ok())
                else {
                    return error(StatusCode::BAD_REQUEST, "commitment required (update Ferry)");
                };
                let group = peer_key(peer.ip.ip);
                let nonce = random_bytes::<32>();
                let session = random_token();
                {
                    let prompts = self.prompts.lock().unwrap();
                    let mut commitments = self.commitments.lock().unwrap();
                    let now = Instant::now();
                    commitments.retain(|_, c| c.expires_at > now);
                    let busy = prompts.values().any(|p| p.fingerprint == theirs || p.group == group)
                        || commitments.values().any(|c| c.fingerprint == theirs || c.group == group)
                        || prompts.len() + commitments.len() >= MAX_PROMPTS;
                    if busy {
                        return error(StatusCode::TOO_MANY_REQUESTS, "a pairing request is already open");
                    }
                    commitments.insert(
                        session.clone(),
                        Commitment { fingerprint: theirs, group, commit, nonce, dto, expires_at: now + COMMIT_TTL },
                    );
                }
                json(StatusCode::OK, &CommitReply { session, nonce: URL_SAFE_NO_PAD.encode(nonce) })
            }
        }
    }

    /// `POST /api/ferry/v1/pair/reveal`: code comparison, step 2. The asker
    /// reveals the nonce it committed to; the person here compares codes.
    pub async fn handle_reveal(self: &Arc<Self>, peer: &PeerContext, body: Incoming, routes: &Routes) -> Resp {
        let Some(theirs) = peer.identity.fingerprint().map(str::to_string) else {
            return error(StatusCode::FORBIDDEN, "verified client certificate required");
        };
        if self.failures.check(peer.ip.ip) == Attempt::LockedOut {
            return error(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
        }
        let body: RevealBody = match server::read_json(peer, body, server::SMALL_JSON_LIMIT, routes).await {
            Ok(b) => b,
            Err(resp) => return resp,
        };
        let pending = {
            let mut commitments = self.commitments.lock().unwrap();
            match commitments.get(&body.session) {
                Some(c) if c.fingerprint == theirs => commitments.remove(&body.session),
                _ => None,
            }
        };
        let Some(pending) = pending.filter(|c| c.expires_at > Instant::now()) else {
            return error(StatusCode::GONE, "no such pairing request");
        };
        let revealed: Option<[u8; 32]> = URL_SAFE_NO_PAD.decode(body.nonce.as_bytes()).ok().and_then(|b| b.try_into().ok());
        let Some(revealed) = revealed.filter(|n| bool::from(commitment(n).ct_eq(&pending.commit))) else {
            self.failures.record_failure(peer.ip.ip);
            return error(StatusCode::FORBIDDEN, "nonce does not match the commitment");
        };
        let ours = self.shared.identity.fingerprint.clone();
        let code = comparison_code(&theirs, &ours, &revealed, &pending.nonce);
        self.prompt(peer, theirs, pending.group, code, pending.dto).await
    }

    async fn prompt(self: &Arc<Self>, peer: &PeerContext, theirs: String, group: IpAddr, code: String, dto: DeviceDto) -> Resp {
        let request = PairingRequest {
            id: random_token(),
            peer: self.shared.peer_ref(&peer.identity, &dto, &theirs),
            code,
            expires_at_ms: now_ms() + CODE_TTL.as_millis() as u64,
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut prompts = self.prompts.lock().unwrap();
            if prompts.len() >= MAX_PROMPTS || prompts.values().any(|p| p.fingerprint == theirs || p.group == group) {
                return error(StatusCode::TOO_MANY_REQUESTS, "a pairing request is already open");
            }
            prompts.insert(request.id.clone(), Prompt { fingerprint: theirs.clone(), group, decide: Some(tx) });
        }
        // Closes the prompt however this ends, including the asker hanging up.
        let _guard = PromptGuard { manager: self.clone(), id: request.id.clone() };
        let id = request.id.clone();
        self.shared.events.emit(EngineEvent::PairingRequest { request });

        match tokio::time::timeout(CODE_TTL, rx).await {
            Ok(Ok(true)) => match self.store(peer.ip.ip, &theirs, dto) {
                Ok(device) => {
                    // The person who confirmed hears about it too.
                    self.shared.events.emit(EngineEvent::PairingFinished {
                        id,
                        outcome: PairingOutcome::Paired,
                        device: Some(device),
                        error: None,
                    });
                    json(StatusCode::OK, &serde_json::json!({ "device": self.shared.device_dto() }))
                }
                Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "could not store pairing"),
            },
            // Mismatches and unanswered prompts count toward the lockout, so a
            // nearby device can't keep the person busy with request after request.
            Ok(_) => {
                self.failures.record_failure(peer.ip.ip);
                error(StatusCode::FORBIDDEN, "declined")
            }
            Err(_) => {
                self.failures.record_failure(peer.ip.ip);
                error(StatusCode::REQUEST_TIMEOUT, "no answer")
            }
        }
    }

    /// `POST /api/ferry/v1/unpair`: the other device removed us.
    pub fn handle_unpair(&self, peer: &PeerContext) -> Resp {
        let Some(theirs) = peer.identity.fingerprint() else {
            return error(StatusCode::FORBIDDEN, "verified client certificate required");
        };
        if self.shared.devices.trust(theirs).mine
            && let Ok(Some(device)) = self.shared.devices.update_flags(theirs, Some(false), None, Some(false), None)
        {
            let name = device.custom_alias.unwrap_or(device.alias);
            self.shared.events.emit(EngineEvent::Notice {
                level: NoticeLevel::Info,
                code: "unpaired".into(),
                message: format!("{name} is no longer one of your devices."),
            });
        }
        json(StatusCode::OK, &serde_json::json!({}))
    }

    /// Removes the pairing here and tells the other device (best effort).
    pub fn unpair(&self, device_id: &str) -> Result<Option<DeviceSummary>> {
        let summary = self.shared.devices.update_flags(device_id, Some(false), None, Some(false), None)?;
        let addr = self.shared.devices.channels(device_id).into_iter().find(|a| a.protocol == Protocol::Https);
        if let Some(addr) = addr
            && let Ok(client) = PeerClient::new(&self.shared.identity, addr, Some(device_id.to_string()))
        {
            tokio::spawn(async move {
                let _ = client
                    .http_client()
                    .post(format!("{}{UNPAIR_PATH}", client_base(&client)))
                    .json(&serde_json::json!({}))
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await;
            });
        }
        Ok(summary)
    }

    /// Records a pairing made by the other side: known, trusted, mine.
    fn store(&self, ip: IpAddr, fingerprint: &str, dto: DeviceDto) -> Result<DeviceSummary> {
        let port = dto.port.unwrap_or(crate::settings::DEFAULT_PORT);
        let addr = PeerAddress { host: ip.to_string(), port, protocol: Protocol::Https };
        let id = self.shared.devices.observe(Observation {
            identity: PeerIdentity::Verified { fingerprint: fingerprint.to_string() },
            addr,
            dto,
            rtt_ms: None,
        });
        self.mark_mine(&id)
    }

    fn mark_mine(&self, id: &str) -> Result<DeviceSummary> {
        self.shared
            .devices
            .update_flags(id, Some(true), None, Some(true), None)?
            .ok_or_else(|| ErrorInfo::new("pair_failed", "Couldn't save the pairing.").into())
    }
}

struct PromptGuard {
    manager: Arc<PairingManager>,
    id: String,
}

impl Drop for PromptGuard {
    fn drop(&mut self) {
        self.manager.prompts.lock().unwrap().remove(&self.id);
        self.manager.shared.events.emit(EngineEvent::PairingRequestClosed { id: self.id.clone() });
    }
}

fn take_expired(offers: &mut Vec<Offer>) -> Vec<String> {
    let now = Instant::now();
    let expired = offers.iter().filter(|o| o.expires_at <= now).map(|o| o.id.clone()).collect();
    offers.retain(|o| o.expires_at > now);
    expired
}

fn client_base(client: &PeerClient) -> String {
    client.addr.base_url()
}

fn describe_send_error(err: &reqwest::Error) -> ErrorInfo {
    if err.is_timeout() {
        ErrorInfo::new("pair_timeout", "Nobody confirmed on the other device.")
    } else {
        ErrorInfo::new("pair_failed", "Couldn't reach the other device.").with_hint(err.to_string())
    }
}

// ── Wire formats ──────────────────────────────────────────────────────────

/// `ferry://pair?v=1&fp=<fingerprint>&a=<addr>,<addr>&p=<port>&s=<secret>`
pub fn pair_uri(fingerprint: &str, addresses: &[String], port: u16, secret: &[u8; 16]) -> String {
    let addresses: Vec<&str> = addresses.iter().take(MAX_URI_ADDRESSES).map(String::as_str).collect();
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("v", "1")
        .append_pair("fp", fingerprint)
        .append_pair("a", &addresses.join(","))
        .append_pair("p", &port.to_string())
        .append_pair("s", &URL_SAFE_NO_PAD.encode(secret))
        .finish();
    format!("{URI_PREFIX}{query}")
}

#[derive(Debug, PartialEq)]
struct ParsedUri {
    fingerprint: String,
    addresses: Vec<String>,
    port: u16,
    secret: [u8; 16],
}

impl ParsedUri {
    fn parse(uri: &str) -> Result<Self> {
        let invalid = || ErrorInfo::new("pair_invalid", "That isn't a Ferry pairing code.");
        let uri = uri.trim();
        if uri.len() > 2048 {
            return Err(invalid().into());
        }
        let query = uri.strip_prefix(URI_PREFIX).ok_or_else(invalid)?;
        let params: HashMap<String, String> =
            form_urlencoded::parse(query.as_bytes()).take(16).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        if params.get("v").map(String::as_str) != Some("1") {
            return Err(ErrorInfo::new("pair_version", "This pairing code needs a newer version of Ferry.").into());
        }
        let fingerprint = params
            .get("fp")
            .filter(|f| f.len() == 64 && f.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(invalid)?
            .to_ascii_uppercase();
        let port = params.get("p").and_then(|p| p.parse::<u16>().ok()).filter(|p| *p != 0).ok_or_else(invalid)?;
        let secret: [u8; 16] =
            params.get("s").and_then(|s| URL_SAFE_NO_PAD.decode(s.as_bytes()).ok()).and_then(|b| b.try_into().ok()).ok_or_else(invalid)?;
        // Only literal IP addresses (optionally scoped): a pairing code must
        // not make us resolve or contact arbitrary host names.
        let addresses: Vec<String> =
            params.get("a").map(|a| a.split(',').take(MAX_URI_ADDRESSES).filter_map(literal_address).collect()).unwrap_or_default();
        if addresses.is_empty() {
            return Err(invalid().into());
        }
        Ok(ParsedUri { fingerprint, addresses, port, secret })
    }
}

/// A literal IP address, re-serialised from its parsed form. IPv6 link-local
/// addresses may carry a zone (an interface index or name); nothing else
/// passes, so a pairing code can never smuggle in a host name.
fn literal_address(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (ip, zone) = match raw.split_once('%') {
        Some((ip, zone)) => (ip, Some(zone)),
        None => (raw, None),
    };
    let ip: IpAddr = ip.parse().ok()?;
    match (ip, zone) {
        (ip, None) => Some(ip.to_string()),
        (IpAddr::V6(v6), Some(zone))
            if v6.segments()[0] & 0xffc0 == 0xfe80
                && !zone.is_empty()
                && zone.len() <= 16
                && zone.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            Some(format!("{v6}%{zone}"))
        }
        _ => None,
    }
}

/// SHA-256("ferry-pair-commit/1" ‖ nonce): what the asker commits to first.
fn commitment(nonce: &[u8; 32]) -> [u8; 32] {
    Sha256::new().chain_update(COMMIT_DOMAIN).chain_update(nonce).finalize().into()
}

/// HMAC-SHA256(secret, "ferry-pair/1" ‖ shower's fingerprint ‖ scanner's fingerprint).
fn proof(secret: &[u8; 16], shower: &str, scanner: &str) -> Vec<u8> {
    proof_mac(secret, shower, scanner).finalize().into_bytes().to_vec()
}

fn verify_proof(secret: &[u8; 16], shower: &str, scanner: &str, given: &[u8]) -> bool {
    proof_mac(secret, shower, scanner).verify_slice(given).is_ok()
}

fn proof_mac(secret: &[u8; 16], shower: &str, scanner: &str) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(b"ferry-pair/1");
    mac.update(shower.to_ascii_uppercase().as_bytes());
    mac.update(scanner.to_ascii_uppercase().as_bytes());
    mac
}

/// Six digits both screens show: the first four bytes of
/// SHA-256("ferry-verify/2" ‖ fp(asker) ‖ fp(responder) ‖ nonce(asker) ‖ nonce(responder)),
/// big-endian, modulo 10⁶, as "123 456". Fingerprints are uppercase hex.
pub fn comparison_code(asker: &str, responder: &str, asker_nonce: &[u8; 32], responder_nonce: &[u8; 32]) -> String {
    let digest = Sha256::new()
        .chain_update(b"ferry-verify/2")
        .chain_update(asker.to_ascii_uppercase().as_bytes())
        .chain_update(responder.to_ascii_uppercase().as_bytes())
        .chain_update(asker_nonce)
        .chain_update(responder_nonce)
        .finalize();
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "810E199C15F7E1665839B992533C31A0122185D8CD70529BA813D5F69C6A4CC2";
    const B: &str = "0F3D4A99B1C2D3E4F5061728394A5B6C7D8E9FA0B1C2D3E4F5061728394A5B6C";

    #[test]
    fn codes_bind_both_identities_and_both_nonces() {
        let (na, nb) = ([1u8; 32], [2u8; 32]);
        let code = comparison_code(A, B, &na, &nb);
        assert_eq!(code, comparison_code(&A.to_lowercase(), B, &na, &nb), "case-insensitive");
        assert!(code.len() == 7 && code.as_bytes()[3] == b' ' && code.replace(' ', "").bytes().all(|c| c.is_ascii_digit()), "{code}");
        // Any change on either side changes the code.
        let m = "AAAA".repeat(16);
        assert_ne!(comparison_code(&m, B, &na, &nb), code, "other asker");
        assert_ne!(comparison_code(A, B, &[3u8; 32], &nb), code, "other asker nonce");
        assert_ne!(comparison_code(A, B, &na, &[3u8; 32]), code, "other responder nonce");
        assert_ne!(comparison_code(B, A, &na, &nb), code, "roles swapped");
    }

    #[test]
    fn a_relay_cannot_steer_the_code() {
        // The responder's nonce arrives only after the asker committed, so an
        // attacker's choices are fixed before the code is: across many fresh
        // responder nonces its code matches the real one about 1 in 10⁶ times.
        let target = comparison_code(A, B, &[1u8; 32], &[2u8; 32]);
        let m = "AAAA".repeat(16);
        let hits = (0..20_000u32)
            .filter(|i| {
                let mut nb = [0u8; 32];
                nb[..4].copy_from_slice(&i.to_be_bytes());
                comparison_code(&m, B, &[9u8; 32], &nb) == target
            })
            .count();
        assert!(hits <= 1, "{hits}");
        assert!(bool::from(commitment(&[5u8; 32]).ct_eq(&commitment(&[5u8; 32]))));
        assert!(!bool::from(commitment(&[5u8; 32]).ct_eq(&commitment(&[6u8; 32]))));
    }

    #[test]
    fn proofs_bind_secret_and_both_fingerprints() {
        let secret = [7u8; 16];
        let p = proof(&secret, A, B);
        assert!(verify_proof(&secret, A, B, &p));
        assert!(!verify_proof(&[8u8; 16], A, B, &p), "other secret");
        assert!(!verify_proof(&secret, B, A, &p), "roles swapped");
        assert!(!verify_proof(&secret, A, &"CC".repeat(32), &p), "other scanner");
        assert!(!verify_proof(&secret, A, B, &p[..31]), "truncated");
        assert!(!verify_proof(&secret, A, B, &[]), "empty");
    }

    #[test]
    fn uri_round_trips() {
        let secret = [9u8; 16];
        let uri = pair_uri(A, &["192.168.1.24".into(), "fe80::1%12".into()], 53317, &secret);
        assert!(uri.starts_with("ferry://pair?v=1&fp="), "{uri}");
        let parsed = ParsedUri::parse(&format!("  {uri}\n")).unwrap();
        assert_eq!(
            parsed,
            ParsedUri { fingerprint: A.into(), addresses: vec!["192.168.1.24".into(), "fe80::1%12".into()], port: 53317, secret }
        );
    }

    #[test]
    fn hostile_uris_are_rejected() {
        let good = pair_uri(A, &["10.0.0.2".into()], 53317, &[1; 16]);
        for bad in [
            "https://evil.example/pair?v=1".to_string(),
            good.replace("v=1", "v=9"),
            good.replace(A, "XYZ"),
            good.replace("p=53317", "p=0"),
            good.replace("p=53317", "p=70000"),
            good.replace("a=10.0.0.2", "a=evil.example"),
            good.replace("a=10.0.0.2", "a="),
            format!("{}&s=AAAA", &good[..good.find("&s=").unwrap()]),
            format!("{good}{}", "x".repeat(3000)),
        ] {
            assert!(ParsedUri::parse(&bad).is_err(), "accepted {bad}");
        }
        // Host names among IPs are dropped, not resolved.
        let mixed = good.replace("a=10.0.0.2", "a=evil.example%2C10.0.0.2");
        assert_eq!(ParsedUri::parse(&mixed).unwrap().addresses, vec!["10.0.0.2"]);
        // Zones only on link-local IPv6, and only plain interface ids.
        for (raw, ok) in [
            ("fe80::1%12", Some("fe80::1%12")),
            ("fe80::1%eth0", Some("fe80::1%eth0")),
            ("fe80::1%25]@evil.example:443/x%23", None),
            ("fe80::1%", None),
            ("2001:db8::1%3", None),
            ("10.0.0.2%3", None),
            (" 10.0.0.2 ", Some("10.0.0.2")),
        ] {
            assert_eq!(literal_address(raw).as_deref(), ok, "{raw}");
        }
    }
}
