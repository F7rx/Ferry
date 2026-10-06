//! Browser links: a plain-HTTP listener serving Ferry's own page, so any
//! browser on this network can download what you share, or send to you,
//! without installing anything.
//!
//! Browsers reject self-signed certificates, so this cannot be HTTPS on a LAN
//! (same constraint as LocalSend). Mitigations (docs/04-threat-model.md W6,
//! N12): an unguessable 128-bit token in every URL, optional PIN, expiry,
//! Host-header checks against DNS rebinding, strict security headers, and
//! uploads go through the normal receive policy (prompt, PIN, sanitizing).

use crate::error::{ErrorInfo, Result};
use crate::events::EngineEvent;
use crate::model::PeerIdentity;
use crate::net::interfaces;
use crate::net::limits::{Attempt, ConnectionLimiter, FailureTracker};
use crate::proto::{DeviceDto, FileDto, PROTOCOL_VERSION, PrepareUploadRequest};
use crate::receive::ReceiveManager;
use crate::send::{OutFile, SendItem, build_manifest};
use crate::server::{self, PeerContext};
use crate::shared::Shared;
use crate::util::{now_ms, random_token, secret_eq};
use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use indexmap::IndexMap;
use localsend::http::server::PeerIp;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::sync::CancellationToken;

const PAGE: &str = include_str!("../assets/browser.html");
pub const DEFAULT_BROWSER_PORT: u16 = 53319;
pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_SHARES: usize = 16;

type Body = BoxBody<Bytes, std::io::Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareKind {
    /// This device offers files; browsers download them.
    Download,
    /// Browsers send files to this device.
    Upload,
}

/// What the UI shows about a browser link.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserShareInfo {
    pub id: String,
    pub kind: ShareKind,
    /// Full links, best address first; encode the first one in the QR code.
    pub urls: Vec<String>,
    pub expires_at_ms: u64,
    pub pin_required: bool,
    pub file_count: u32,
    pub total_bytes: u64,
    /// Completed downloads (download links).
    pub downloads: u32,
    /// Files received through this link (upload links).
    pub uploads: u32,
    /// Browsers currently downloading.
    pub active: u32,
    /// "Chrome on Android" etc. of the last few visitors.
    pub recent_clients: Vec<String>,
}

struct Share {
    id: String,
    token: String,
    kind: ShareKind,
    files: Vec<OutFile>,
    pin: Option<String>,
    expires_at: Instant,
    expires_at_ms: u64,
    downloads: AtomicU32,
    uploads: AtomicU32,
    active: AtomicU32,
    clients: Mutex<Vec<String>>,
    /// Cancelled when the link is stopped or expires: running downloads end too.
    ended: CancellationToken,
}

/// One HTTP connection: cancelled when its link ends or a download stalls.
struct Conn {
    cancel: CancellationToken,
    /// Downloads in flight on this connection, and when one last made progress.
    busy: AtomicU32,
    last_progress_ms: AtomicU64,
}

/// A download that moves no bytes this long frees its connection slot.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

pub struct BrowserServer {
    shared: Arc<Shared>,
    receive: Arc<ReceiveManager>,
    routes: Arc<server::Routes>,
    shares: Mutex<HashMap<String, Arc<Share>>>,
    listening: tokio::sync::Mutex<Option<(u16, CancellationToken)>>,
    /// Bound port (0 until listening), readable without awaiting `listening`.
    bound_port: AtomicU16,
    pin_failures: FailureTracker,
}

impl BrowserServer {
    pub fn new(shared: Arc<Shared>, receive: Arc<ReceiveManager>, routes: Arc<server::Routes>) -> Arc<Self> {
        Arc::new(Self {
            shared,
            receive,
            routes,
            shares: Mutex::new(HashMap::new()),
            listening: tokio::sync::Mutex::new(None),
            bound_port: AtomicU16::new(0),
            pin_failures: FailureTracker::new(5, 50, Duration::from_secs(5 * 60)),
        })
    }

    /// Starts a download link for `items` (files are not copied; they are
    /// read from where they are when a browser asks).
    pub async fn share_files(
        self: &Arc<Self>,
        items: Vec<SendItem>,
        pin: Option<String>,
        ttl: Option<Duration>,
    ) -> Result<BrowserShareInfo> {
        let paths: Vec<_> = items.into_iter().filter_map(|i| if let SendItem::Path { path } = i { Some(path) } else { None }).collect();
        let files = tokio::task::spawn_blocking(move || build_manifest(&paths)).await.map_err(ErrorInfo::internal)??;
        if files.is_empty() {
            return Err(ErrorInfo::new("nothing_to_share", "Choose files to share.").into());
        }
        self.start(ShareKind::Download, files, pin, ttl).await
    }

    /// Starts an upload link: browsers can send files to this device.
    pub async fn receive_from_browsers(self: &Arc<Self>, pin: Option<String>, ttl: Option<Duration>) -> Result<BrowserShareInfo> {
        self.start(ShareKind::Upload, Vec::new(), pin, ttl).await
    }

    async fn start(
        self: &Arc<Self>,
        kind: ShareKind,
        files: Vec<OutFile>,
        pin: Option<String>,
        ttl: Option<Duration>,
    ) -> Result<BrowserShareInfo> {
        if let Some(p) = &pin
            && (p.is_empty() || p.len() > 32 || !p.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            return Err(ErrorInfo::new("invalid_pin", "The PIN must be 1 to 32 letters or digits.").into());
        }
        let port =
            self.ensure_listening().await.map_err(|e| ErrorInfo::new("browser_link", format!("Couldn't open a browser link: {e}")))?;
        let ttl = ttl.unwrap_or(DEFAULT_TTL).min(Duration::from_secs(24 * 3600));
        let share = Arc::new(Share {
            id: uuid::Uuid::new_v4().to_string(),
            token: random_token(),
            kind,
            files,
            pin,
            expires_at: Instant::now() + ttl,
            expires_at_ms: now_ms() + ttl.as_millis() as u64,
            downloads: AtomicU32::new(0),
            uploads: AtomicU32::new(0),
            active: AtomicU32::new(0),
            clients: Mutex::new(Vec::new()),
            ended: CancellationToken::new(),
        });
        {
            let mut shares = self.shares.lock().unwrap();
            shares.retain(|_, s| s.expires_at > Instant::now());
            if shares.len() >= MAX_SHARES {
                return Err(ErrorInfo::new("too_many_links", "Too many browser links are open. Stop one first.").into());
            }
            shares.insert(share.token.clone(), share.clone());
        }
        let info = self.info(&share, port);
        self.shared.events.emit(EngineEvent::BrowserShareUpdated { share: info.clone() });
        Ok(info)
    }

    pub fn stop(&self, id: &str) -> bool {
        let removed = {
            let mut shares = self.shares.lock().unwrap();
            let token = shares.values().find(|s| s.id == id).map(|s| s.token.clone());
            token.and_then(|t| shares.remove(&t))
        };
        let removed = removed.map(|share| share.ended.cancel()).is_some();
        if removed {
            self.shared.events.emit(EngineEvent::BrowserShareRemoved { id: id.to_string() });
        }
        removed
    }

    pub fn list(&self) -> Vec<BrowserShareInfo> {
        let port = self.port();
        let shares: Vec<Arc<Share>> = self.shares.lock().unwrap().values().filter(|s| s.expires_at > Instant::now()).cloned().collect();
        shares.iter().map(|s| self.info(s, port)).collect()
    }

    fn port(&self) -> u16 {
        self.bound_port.load(Ordering::Acquire)
    }

    fn info(&self, share: &Share, port: u16) -> BrowserShareInfo {
        let addresses = self.shared.net.read().unwrap().addresses.clone();
        let urls = addresses
            .iter()
            .map(|a| {
                if a.contains(':') {
                    format!("http://[{a}]:{port}/s/{}", share.token)
                } else {
                    format!("http://{a}:{port}/s/{}", share.token)
                }
            })
            .collect();
        BrowserShareInfo {
            id: share.id.clone(),
            kind: share.kind,
            urls,
            expires_at_ms: share.expires_at_ms,
            pin_required: share.pin.is_some(),
            file_count: share.files.len() as u32,
            total_bytes: share.files.iter().map(|f| f.size).sum(),
            downloads: share.downloads.load(Ordering::Relaxed),
            uploads: share.uploads.load(Ordering::Relaxed),
            active: share.active.load(Ordering::Relaxed),
            recent_clients: share.clients.lock().unwrap().clone(),
        }
    }

    fn touched(&self, share: &Share) {
        let info = self.info(share, self.port());
        self.shared.events.emit(EngineEvent::BrowserShareUpdated { share: info });
    }

    /// Removes expired links (called from housekeeping).
    pub fn prune(&self) {
        let expired: Vec<String> = {
            let mut shares = self.shares.lock().unwrap();
            let now = Instant::now();
            let ids = shares
                .values()
                .filter(|s| s.expires_at <= now)
                .map(|s| {
                    s.ended.cancel();
                    s.id.clone()
                })
                .collect();
            shares.retain(|_, s| s.expires_at > now);
            ids
        };
        for id in expired {
            self.shared.events.emit(EngineEvent::BrowserShareRemoved { id });
        }
    }

    async fn ensure_listening(self: &Arc<Self>) -> std::io::Result<u16> {
        let mut listening = self.listening.lock().await;
        if let Some((port, _)) = listening.as_ref() {
            return Ok(*port);
        }
        let v4 = match tokio::net::TcpListener::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), DEFAULT_BROWSER_PORT)).await {
            Ok(l) => l,
            Err(_) => tokio::net::TcpListener::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)).await?,
        };
        let port = v4.local_addr()?.port();
        let v6 = tokio::net::TcpListener::bind(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port)).await.ok();
        let stop = self.shared.shutdown.child_token();
        let limiter = ConnectionLimiter::new(16, 64);
        for listener in std::iter::once(v4).chain(v6) {
            let this = self.clone();
            let stop = stop.clone();
            let limiter = limiter.clone();
            tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! { r = listener.accept() => r, _ = stop.cancelled() => return };
                    let Ok((tcp, remote)) = accepted else { continue };
                    let Some(permit) = limiter.try_acquire(remote.ip()) else { continue };
                    let _ = tcp.set_nodelay(true);
                    let this = this.clone();
                    let stop = stop.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let peer = PeerContext { ip: PeerIp::from_remote_addr(&remote), identity: PeerIdentity::PlainHttp };
                        let ctl = Arc::new(Conn {
                            cancel: stop.child_token(),
                            busy: AtomicU32::new(0),
                            last_progress_ms: AtomicU64::new(now_ms()),
                        });
                        // Hyper never polls a body the client stopped reading, so a
                        // stalled download is noticed from outside and dropped.
                        let watch = ctl.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::select! {
                                    _ = tokio::time::sleep(Duration::from_secs(15)) => {}
                                    _ = watch.cancel.cancelled() => return,
                                }
                                let idle = now_ms().saturating_sub(watch.last_progress_ms.load(Ordering::Relaxed));
                                if watch.busy.load(Ordering::Relaxed) > 0 && idle > STALL_TIMEOUT.as_millis() as u64 {
                                    watch.cancel.cancel();
                                    return;
                                }
                            }
                        });
                        let route_ctl = ctl.clone();
                        let service = hyper::service::service_fn(move |req| {
                            let this = this.clone();
                            let peer = peer.clone();
                            let conn = route_ctl.clone();
                            async move { Ok::<_, std::convert::Infallible>(this.route(req, peer, port, conn).await) }
                        });
                        let conn = hyper::server::conn::http1::Builder::new()
                            .timer(TokioTimer::new())
                            .header_read_timeout(Duration::from_secs(15))
                            .serve_connection(TokioIo::new(tcp), service);
                        tokio::select! { _ = conn => {}, _ = ctl.cancel.cancelled() => {} }
                        ctl.cancel.cancel();
                    });
                }
            });
        }
        tracing::info!("Browser links on port {port}");
        *listening = Some((port, stop));
        self.bound_port.store(port, Ordering::Release);
        Ok(port)
    }

    async fn route(self: &Arc<Self>, req: Request<Incoming>, peer: PeerContext, port: u16, conn: Arc<Conn>) -> Response<Body> {
        let mut resp = self.route_inner(req, peer, port, conn).await;
        let h = resp.headers_mut();
        h.insert("x-content-type-options", "nosniff".parse().unwrap());
        h.insert("referrer-policy", "no-referrer".parse().unwrap());
        h.insert("x-frame-options", "DENY".parse().unwrap());
        h.insert("cache-control", "no-store".parse().unwrap());
        resp
    }

    async fn route_inner(self: &Arc<Self>, req: Request<Incoming>, peer: PeerContext, port: u16, conn: Arc<Conn>) -> Response<Body> {
        // DNS rebinding: only answer to our own IP addresses (and loopback).
        if !host_allowed(req.headers().get(hyper::header::HOST).and_then(|h| h.to_str().ok()), port) {
            return text(StatusCode::FORBIDDEN, "Open the link exactly as shown in Ferry.");
        }
        let path = req.uri().path().to_string();
        let query = server::parse_query(req.uri().query());
        let Some(rest) = path.strip_prefix("/s/") else {
            return text(StatusCode::NOT_FOUND, "Not found");
        };
        let (token, sub) = rest.split_once('/').map(|(t, s)| (t, format!("/{s}"))).unwrap_or((rest, String::new()));
        let share = {
            let shares = self.shares.lock().unwrap();
            shares.iter().find(|(t, _)| secret_eq(t, token)).map(|(_, s)| s.clone())
        };
        let Some(share) = share.filter(|s| s.expires_at > Instant::now()) else {
            return html(StatusCode::NOT_FOUND, &gone_page());
        };

        if req.method() == Method::GET && sub.is_empty() {
            let mut resp = html(StatusCode::OK, PAGE);
            resp.headers_mut().insert(
                "content-security-policy",
                "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'self'; form-action 'none'; base-uri 'none'".parse().unwrap(),
            );
            return resp;
        }

        // Every API call needs the share PIN when one is set.
        if let Some(pin) = &share.pin {
            let given =
                req.headers().get("x-ferry-pin").and_then(|v| v.to_str().ok()).map(str::to_string).or_else(|| query.get("pin").cloned());
            if self.pin_failures.check(peer.ip.ip) == Attempt::LockedOut {
                return json(StatusCode::TOO_MANY_REQUESTS, &serde_json::json!({ "error": "Too many attempts. Wait a few minutes." }));
            }
            match given {
                Some(g) if secret_eq(&g, pin) => self.pin_failures.record_success(peer.ip.ip),
                Some(_) => {
                    self.pin_failures.record_failure(peer.ip.ip);
                    return json(StatusCode::UNAUTHORIZED, &serde_json::json!({ "error": "Wrong PIN", "pinRequired": true }));
                }
                None => return json(StatusCode::UNAUTHORIZED, &serde_json::json!({ "pinRequired": true })),
            }
        }
        if remember_client(&share, &req) {
            self.touched(&share);
        }

        match (req.method().clone(), sub.as_str(), share.kind) {
            (Method::GET, "/api/info", _) => {
                let local = self.shared.local_device();
                let files: Vec<serde_json::Value> =
                    share.files.iter().map(|f| serde_json::json!({ "id": f.id, "name": f.name, "size": f.size, "mime": f.mime })).collect();
                json(
                    StatusCode::OK,
                    &serde_json::json!({
                        "kind": share.kind,
                        "device": { "alias": local.alias, "kind": local.device_kind },
                        "files": files,
                        "expiresAtMs": share.expires_at_ms,
                    }),
                )
            }
            (Method::GET, s, ShareKind::Download) if s.starts_with("/api/files/") => {
                let id = &s["/api/files/".len()..];
                match share.files.iter().find(|f| f.id == id) {
                    Some(file) => {
                        self.download(&share, file, req.headers().get(hyper::header::RANGE).and_then(|v| v.to_str().ok()), &conn).await
                    }
                    None => text(StatusCode::NOT_FOUND, "No such file"),
                }
            }
            (Method::POST, "/api/prepare", ShareKind::Upload) => {
                // Admission first: rate limit and receiving switch cost no body parsing.
                // The link (token + its own PIN, checked above) replaces the device PIN.
                if let Err(resp) = self.receive.admit_link(&peer) {
                    return boxed(resp);
                }
                let body: BrowserPrepare = match server::read_json(&peer, req.into_body(), server::PREPARE_JSON_LIMIT, &self.routes).await {
                    Ok(b) => b,
                    Err(resp) => return boxed(resp),
                };
                let request = PrepareUploadRequest {
                    info: DeviceDto {
                        alias: browser_alias(body.sender.as_deref(), &share),
                        version: PROTOCOL_VERSION.to_string(),
                        device_model: Some("Browser".into()),
                        device_type: Some(crate::model::DeviceKind::Web),
                        fingerprint: String::new(),
                        port: None,
                        protocol: None,
                        download: false,
                        ferry: None,
                    },
                    files: body.files.into_iter().map(|f| (f.id.clone(), f)).collect::<IndexMap<_, _>>(),
                    ferry: None,
                };
                boxed(self.receive.prepare_parsed(&peer, request).await)
            }
            (Method::POST, "/api/upload", ShareKind::Upload) => {
                share.active.fetch_add(1, Ordering::Relaxed);
                self.touched(&share);
                let mut guard = UploadGuard { server: self.clone(), share: share.clone(), ok: false };
                let resp = self.receive.upload(&peer, &query, req.into_body()).await;
                guard.ok = resp.status().is_success();
                boxed(resp)
            }
            (Method::POST, "/api/cancel", ShareKind::Upload) => {
                self.receive.cancel_from_peer(&peer, query.get("sessionId").map(String::as_str));
                text(StatusCode::OK, "")
            }
            _ => text(StatusCode::NOT_FOUND, "Not found"),
        }
    }

    async fn download(self: &Arc<Self>, share: &Arc<Share>, file: &OutFile, range: Option<&str>, conn: &Arc<Conn>) -> Response<Body> {
        let Some(path) = file.path.clone() else { return text(StatusCode::NOT_FOUND, "No such file") };
        let mut f = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(_) => return text(StatusCode::GONE, "This file is no longer available on the sharing device."),
        };
        let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
        if len == 0 {
            share.downloads.fetch_add(1, Ordering::Relaxed);
            self.touched(share);
            let mut r = text(StatusCode::OK, "");
            r.headers_mut().insert("content-type", "application/octet-stream".parse().unwrap());
            r.headers_mut().insert("content-disposition", content_disposition(&file.name).parse().unwrap());
            return r;
        }
        let (start, end) = match parse_range(range, len) {
            Ok(r) => r,
            Err(()) => {
                let mut r = text(StatusCode::RANGE_NOT_SATISFIABLE, "");
                r.headers_mut().insert("content-range", format!("bytes */{len}").parse().unwrap());
                return r;
            }
        };
        if start > 0 && f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return text(StatusCode::INTERNAL_SERVER_ERROR, "Read error");
        }
        let count = end + 1 - start;
        let stream = tokio_util::io::ReaderStream::with_capacity(f.take(count), 256 * 1024);
        // Count a download when its last byte has been handed to the socket.
        share.active.fetch_add(1, Ordering::Relaxed);
        self.touched(share);
        conn.busy.fetch_add(1, Ordering::Relaxed);
        conn.last_progress_ms.store(now_ms(), Ordering::Relaxed);
        // Stopping or expiring the link ends this download's connection.
        {
            let (ended, cancel) = (share.ended.clone(), conn.cancel.clone());
            tokio::spawn(async move {
                tokio::select! {
                    _ = ended.cancelled() => cancel.cancel(),
                    _ = cancel.cancelled() => {}
                }
            });
        }
        let guard = DownloadGuard {
            server: self.clone(),
            share: share.clone(),
            conn: conn.clone(),
            complete: false,
            sent: 0,
            target: count,
            full: start == 0 && count == len,
        };
        let stream = futures_util::stream::unfold((stream, guard), |(mut s, mut g)| async move {
            match s.try_next().await {
                Ok(Some(chunk)) => {
                    g.conn.last_progress_ms.store(now_ms(), Ordering::Relaxed);
                    g.sent += chunk.len() as u64;
                    if g.sent >= g.target {
                        g.complete = true;
                    }
                    Some((Ok(Frame::data(chunk)), (s, g)))
                }
                Ok(None) => None,
                Err(e) => Some((Err(e), (s, g))),
            }
        });
        let mut resp = Response::new(StreamBody::new(stream).boxed());
        *resp.status_mut() = if range.is_some() && count != len { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
        let h = resp.headers_mut();
        h.insert("content-type", "application/octet-stream".parse().unwrap());
        h.insert("content-length", count.to_string().parse().unwrap());
        h.insert("accept-ranges", "bytes".parse().unwrap());
        h.insert("content-disposition", content_disposition(&file.name).parse().unwrap());
        if resp.status() == StatusCode::PARTIAL_CONTENT {
            resp.headers_mut().insert("content-range", format!("bytes {start}-{end}/{len}").parse().unwrap());
        }
        resp
    }
}

struct DownloadGuard {
    server: Arc<BrowserServer>,
    share: Arc<Share>,
    conn: Arc<Conn>,
    complete: bool,
    sent: u64,
    target: u64,
    full: bool,
}

/// Counts an upload as active until its request finishes or is dropped.
struct UploadGuard {
    server: Arc<BrowserServer>,
    share: Arc<Share>,
    ok: bool,
}
impl Drop for UploadGuard {
    fn drop(&mut self) {
        self.share.active.fetch_sub(1, Ordering::Relaxed);
        if self.ok {
            self.share.uploads.fetch_add(1, Ordering::Relaxed);
        }
        self.server.touched(&self.share);
    }
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        self.conn.busy.fetch_sub(1, Ordering::Relaxed);
        self.share.active.fetch_sub(1, Ordering::Relaxed);
        if self.complete && self.full {
            self.share.downloads.fetch_add(1, Ordering::Relaxed);
        }
        self.server.touched(&self.share);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserPrepare {
    files: Vec<FileDto>,
    #[serde(default)]
    sender: Option<String>,
}

fn browser_alias(given: Option<&str>, share: &Share) -> String {
    let clients = share.clients.lock().unwrap();
    let ua = clients.last().cloned().unwrap_or_else(|| "a browser".into());
    match given.map(str::trim).filter(|g| !g.is_empty()) {
        Some(name) => format!("{} ({ua})", crate::proto::clean_alias(name).chars().take(40).collect::<String>()),
        None => format!("Browser · {ua}"),
    }
}

/// Records who opened the link; true when the list changed.
fn remember_client(share: &Share, req: &Request<Incoming>) -> bool {
    let ua = req.headers().get(hyper::header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let label = describe_user_agent(ua);
    let mut clients = share.clients.lock().unwrap();
    if clients.last() != Some(&label) {
        clients.retain(|c| c != &label);
        clients.push(label);
        if clients.len() > 5 {
            clients.remove(0);
        }
        return true;
    }
    false
}

/// "Chrome on Android" from a User-Agent string (display only).
pub fn describe_user_agent(ua: &str) -> String {
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("OPR/") {
        "Opera"
    } else if ua.contains("SamsungBrowser") {
        "Samsung Internet"
    } else if ua.contains("Firefox/") || ua.contains("FxiOS") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "a browser"
    };
    let os = if ua.contains("Android") {
        "Android"
    } else if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Mac OS X") {
        "Mac"
    } else if ua.contains("CrOS") {
        "ChromeOS"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        ""
    };
    if os.is_empty() { browser.to_string() } else { format!("{browser} on {os}") }
}

/// Only our own addresses (or loopback) may be used to reach the page.
fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else { return false };
    let (h, p) = match host.rsplit_once(':') {
        Some((h, _)) if !h.ends_with(']') && h.contains(':') => (host, None), // bare IPv6 without port
        Some((h, p)) => (h, p.parse::<u16>().ok()),
        None => (host, None),
    };
    if p.is_some_and(|p| p != port) {
        return false;
    }
    let h = h.trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let Ok(ip) = h.split('%').next().unwrap_or(h).parse::<IpAddr>() else { return false };
    ip.is_loopback() || interfaces::list(true).iter().any(|i| i.addr == ip)
}

/// `Range: bytes=a-b` → inclusive (start, end). Multi-ranges are not supported.
fn parse_range(range: Option<&str>, len: u64) -> std::result::Result<(u64, u64), ()> {
    if len == 0 {
        // Callers special-case empty files; a range into nothing is unsatisfiable.
        return if range.is_some() { Err(()) } else { Ok((0, 0)) };
    }
    let Some(spec) = range.and_then(|r| r.strip_prefix("bytes=")) else { return Ok((0, len - 1)) };
    if spec.contains(',') {
        return Ok((0, len - 1));
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", suffix) => {
            let n: u64 = suffix.parse().map_err(|_| ())?;
            (len.saturating_sub(n), len - 1)
        }
        (a, "") => (a.parse().map_err(|_| ())?, len - 1),
        (a, b) => (a.parse().map_err(|_| ())?, b.parse::<u64>().map_err(|_| ())?.min(len - 1)),
    };
    if start > end || start >= len {
        return Err(());
    }
    Ok((start, end))
}

fn content_disposition(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let ascii: String = base.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let encoded = percent_encoding::utf8_percent_encode(base, percent_encoding::NON_ALPHANUMERIC).to_string();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

fn boxed(resp: Response<Full<Bytes>>) -> Response<Body> {
    resp.map(|b| b.map_err(|never| match never {}).boxed())
}

fn text(status: StatusCode, body: &str) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from(body.to_string())).map_err(|never| match never {}).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert("content-type", "text/plain; charset=utf-8".parse().unwrap());
    r
}

fn html(status: StatusCode, body: &str) -> Response<Body> {
    let mut r = text(status, body);
    r.headers_mut().insert("content-type", "text/html; charset=utf-8".parse().unwrap());
    r
}

fn json<T: Serialize>(status: StatusCode, body: &T) -> Response<Body> {
    let mut r = text(status, &serde_json::to_string(body).unwrap_or_default());
    r.headers_mut().insert("content-type", "application/json".parse().unwrap());
    r
}

fn gone_page() -> String {
    r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Link expired</title>
<style>body{margin:0;min-height:100vh;display:grid;place-items:center;font:16px/1.5 system-ui,sans-serif;background:#ebeff6;color:#0d1424}@media(prefers-color-scheme:dark){body{background:#0a0d15;color:#e9edf5}}main{max-width:28rem;padding:2rem;text-align:center}h1{font-size:1.5rem;letter-spacing:-.02em}</style>
</head><body><main><h1>This link has expired</h1><p>Ask for a new link on the sharing device in Ferry.</p></main></body></html>"#
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), Ok((0, 99)));
        assert_eq!(parse_range(Some("bytes=10-19"), 100), Ok((10, 19)));
        assert_eq!(parse_range(Some("bytes=90-"), 100), Ok((90, 99)));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Ok((90, 99)));
        assert_eq!(parse_range(Some("bytes=10-500"), 100), Ok((10, 99)));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Err(()));
        assert_eq!(parse_range(Some("bytes=20-10"), 100), Err(()));
    }

    #[test]
    fn user_agents() {
        assert_eq!(
            describe_user_agent(
                "Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Mobile Safari/537.36"
            ),
            "Chrome on Android"
        );
        assert_eq!(
            describe_user_agent(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 19_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/19.0 Mobile/15E148 Safari/604.1"
            ),
            "Safari on iPhone"
        );
    }

    #[test]
    fn host_checks() {
        assert!(host_allowed(Some("127.0.0.1:53319"), 53319));
        assert!(host_allowed(Some("localhost:53319"), 53319));
        assert!(!host_allowed(Some("127.0.0.1:1234"), 53319));
        assert!(!host_allowed(Some("evil.example:53319"), 53319));
        assert!(!host_allowed(None, 53319));
        assert!(host_allowed(Some("[::1]:53319"), 53319));
    }

    #[test]
    fn disposition_is_safe_and_utf8() {
        let d = content_disposition("Album/Résumé \"final\".pdf");
        assert!(d.starts_with("attachment; filename=\"R_sum_ _final_.pdf\""), "{d}");
        assert!(d.contains("filename*=UTF-8''R%C3%A9sum%C3%A9%20%22final%22%2Epdf"), "{d}");
        assert!(!d.contains("Album"));
    }
}
