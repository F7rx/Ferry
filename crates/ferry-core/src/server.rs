//! The HTTPS server: LocalSend v2 routes plus Ferry's `/api/ferry/v1`.
//!
//! Hardening over upstream (docs/04-threat-model.md): per-peer (/64) and total
//! connection caps, TLS-handshake and header-read timeouts, size-limited JSON
//! bodies drawn from a global byte budget, and mandatory client certificates so
//! every request carries a proven identity.

use crate::model::{PeerIdentity, Protocol};
use crate::net::limits::{ConnectionLimiter, peer_key};
use crate::proto::{API_FERRY, API_V1_INFO, API_V2, DeviceDto, ErrorBody};
use crate::receive::ReceiveManager;
use crate::shared::Shared;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use localsend::http::server::PeerIp;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);
const JSON_READ_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_CONNECTIONS_PER_PEER: usize = 24;
const MAX_CONNECTIONS: usize = 128;
/// Total bytes of JSON bodies being buffered at once, across all requests.
const JSON_BUDGET_KIB: usize = 64 * 1024;
pub const SMALL_JSON_LIMIT: usize = 64 * 1024;
pub const PREPARE_JSON_LIMIT: usize = 8 * 1024 * 1024;
/// Large bodies (above SMALL_JSON_LIMIT) one IP group may have in flight, so a
/// single host can't hold the shared budget and starve everyone else.
const LARGE_JSON_PER_PEER: u32 = 2;

pub type Resp = Response<Full<Bytes>>;

/// Messages from request handlers to other engine components. Sent with
/// `try_send`: request handling never waits on them.
#[derive(Debug)]
pub enum ServerSignal {
    /// A peer registered with us (it answered our announcement, or scanned).
    Registered { identity: PeerIdentity, ip: PeerIp, dto: DeviceDto },
    /// A peer asked to cancel `session_id`; may be an outgoing transfer of ours.
    CancelReceived { identity: PeerIdentity, ip: PeerIp, session_id: Option<String> },
}

#[derive(Clone, Debug)]
pub struct PeerContext {
    pub ip: PeerIp,
    pub identity: PeerIdentity,
}

pub struct Routes {
    pub shared: Arc<Shared>,
    pub receive: Arc<ReceiveManager>,
    pub pairing: Arc<crate::pairing::PairingManager>,
    pub signals: mpsc::Sender<ServerSignal>,
    json_budget: Semaphore,
    large_json_inflight: Mutex<HashMap<IpAddr, u32>>,
}

impl Routes {
    pub fn new(
        shared: Arc<Shared>,
        receive: Arc<ReceiveManager>,
        pairing: Arc<crate::pairing::PairingManager>,
        signals: mpsc::Sender<ServerSignal>,
    ) -> Arc<Self> {
        Arc::new(Self {
            shared,
            receive,
            pairing,
            signals,
            json_budget: Semaphore::new(JSON_BUDGET_KIB),
            large_json_inflight: Mutex::new(HashMap::new()),
        })
    }
}

pub struct ServerHandle {
    pub port: u16,
    pub ipv6: bool,
    pub protocol: Protocol,
    stop: CancellationToken,
    tasks: TaskTracker,
}

impl ServerHandle {
    pub async fn stop(&self) {
        self.stop.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}

/// Binds `port` on IPv4 and (best effort) IPv6. Port 0 picks a free port.
pub async fn start(routes: Arc<Routes>, port: u16, tls: bool) -> std::io::Result<ServerHandle> {
    let v4 = tokio::net::TcpListener::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port)).await?;
    let bound = v4.local_addr()?.port();
    let v6 = bind_v6_only(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), bound)).ok();

    let acceptor = if tls {
        Some(tls_acceptor(&routes.shared.identity.cert_pem, &routes.shared.identity.key_pem).map_err(std::io::Error::other)?)
    } else {
        None
    };
    let stop = CancellationToken::new();
    let tasks = TaskTracker::new();
    let limiter = ConnectionLimiter::new(MAX_CONNECTIONS_PER_PEER, MAX_CONNECTIONS);
    let ipv6 = v6.is_some();

    for listener in std::iter::once(v4).chain(v6) {
        let routes = routes.clone();
        let acceptor = acceptor.clone();
        let stop = stop.clone();
        let tasks2 = tasks.clone();
        let limiter = limiter.clone();
        tasks.spawn(async move {
            accept_loop(listener, routes, acceptor, limiter, stop, tasks2).await;
        });
    }
    tracing::info!("Listening on port {bound} (TLS: {tls}, IPv6: {ipv6})");
    Ok(ServerHandle { port: bound, ipv6, protocol: if tls { Protocol::Https } else { Protocol::Http }, stop, tasks })
}

fn bind_v6_only(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let socket = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    socket.set_only_v6(true)?;
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    tokio::net::TcpListener::from_std(socket.into())
}

fn tls_acceptor(cert_pem: &str, key_pem: &str) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certs = vec![CertificateDer::from_pem_slice(cert_pem.as_bytes())?];
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())?;
    let verifier = localsend::http::server::common::client_cert_verifier::CustomClientCertVerifier::try_new(cert_pem, true)?;
    let mut config = rustls::ServerConfig::builder().with_client_cert_verifier(Arc::new(verifier)).with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

const KEEPALIVE: socket2::TcpKeepalive =
    socket2::TcpKeepalive::new().with_time(Duration::from_secs(30)).with_interval(Duration::from_secs(5));

async fn accept_loop(
    listener: tokio::net::TcpListener,
    routes: Arc<Routes>,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
    limiter: Arc<ConnectionLimiter>,
    stop: CancellationToken,
    tasks: TaskTracker,
) {
    let mut backoff = Duration::from_millis(50);
    loop {
        let accepted = tokio::select! {
            r = listener.accept() => r,
            _ = stop.cancelled() => return,
        };
        let (tcp, remote) = match accepted {
            Ok(a) => {
                backoff = Duration::from_millis(50);
                a
            }
            Err(err) => {
                // Transient (peer reset, fd exhaustion during a scan): back off and continue.
                tracing::warn!("accept failed: {err}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
                continue;
            }
        };
        let Some(permit) = limiter.try_acquire(remote.ip()) else {
            tracing::debug!("connection limit reached for {remote}");
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let _ = socket2::SockRef::from(&tcp).set_tcp_keepalive(&KEEPALIVE);
        let routes = routes.clone();
        let acceptor = acceptor.clone();
        let stop = stop.clone();
        tasks.spawn(async move {
            let _permit = permit;
            tokio::select! {
                _ = serve(tcp, remote, acceptor, routes) => {}
                _ = stop.cancelled() => {}
            }
        });
    }
}

async fn serve(tcp: tokio::net::TcpStream, remote: SocketAddr, acceptor: Option<tokio_rustls::TlsAcceptor>, routes: Arc<Routes>) {
    let ip = PeerIp::from_remote_addr(&remote);
    match acceptor {
        Some(acceptor) => {
            let tls = match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(tls)) => tls,
                Ok(Err(err)) => {
                    tracing::debug!("TLS handshake with {remote} failed: {err}");
                    return;
                }
                Err(_) => {
                    tracing::debug!("TLS handshake with {remote} timed out");
                    return;
                }
            };
            let identity = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .map(|cert| PeerIdentity::Verified { fingerprint: localsend::crypto::cert::fingerprint_from_cert_der(cert) })
                .unwrap_or(PeerIdentity::Unverified);
            serve_http(tls, PeerContext { ip, identity }, routes).await;
        }
        None => serve_http(tcp, PeerContext { ip, identity: PeerIdentity::PlainHttp }, routes).await,
    }
}

async fn serve_http<I>(io: I, peer: PeerContext, routes: Arc<Routes>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
        let routes = routes.clone();
        let peer = peer.clone();
        async move { Ok::<_, std::convert::Infallible>(route(req, peer, routes).await) }
    });
    let result = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .keep_alive(true)
        .max_buf_size(64 * 1024)
        .serve_connection(TokioIo::new(io), service)
        .await;
    if let Err(err) = result {
        tracing::debug!("connection ended: {err}");
    }
}

async fn route(req: Request<Incoming>, peer: PeerContext, routes: Arc<Routes>) -> Resp {
    let path = req.uri().path().to_string();
    let query = parse_query(req.uri().query());
    let method = req.method().clone();

    if let Some(rest) = path.strip_prefix(API_FERRY) {
        // Ferry extensions need a proven identity.
        if !peer.identity.is_verified() {
            return error(StatusCode::FORBIDDEN, "verified client certificate required");
        }
        return match (method, rest) {
            (Method::GET, "/hello") => json(StatusCode::OK, &routes.shared.ferry_hello()),
            (Method::POST, "/verify") => routes.receive.verify(&peer, &query, req.into_body(), &routes).await,
            (Method::POST, "/pair") => routes.pairing.handle_pair(&peer, req.into_body(), &routes).await,
            (Method::POST, "/pair/reveal") => routes.pairing.handle_reveal(&peer, req.into_body(), &routes).await,
            (Method::POST, "/unpair") => routes.pairing.handle_unpair(&peer),
            (Method::GET, r) if r.starts_with("/transfers/") => routes.receive.transfer_status(&peer, &r["/transfers/".len()..]),
            _ => error(StatusCode::NOT_FOUND, "not found"),
        };
    }

    match (method, path.as_str()) {
        (Method::GET | Method::POST, p) if p == API_V1_INFO || p == format!("{API_V2}/info") => {
            json(StatusCode::OK, &routes.shared.info_dto())
        }
        (Method::POST, p) if p == format!("{API_V2}/register") => register(req, peer, routes).await,
        (Method::POST, p) if p == format!("{API_V2}/prepare-upload") => {
            routes.receive.prepare_upload(&peer, &query, req.into_body(), &routes).await
        }
        (Method::POST, p) if p == format!("{API_V2}/upload") => routes.receive.upload(&peer, &query, req.into_body()).await,
        (Method::POST, p) if p == format!("{API_V2}/cancel") => {
            let session_id = query.get("sessionId").cloned();
            let handled = routes.receive.cancel_from_peer(&peer, session_id.as_deref());
            if !handled {
                let _ = routes.signals.try_send(ServerSignal::CancelReceived { identity: peer.identity.clone(), ip: peer.ip, session_id });
            }
            Response::new(Full::new(Bytes::new()))
        }
        _ => error(StatusCode::NOT_FOUND, "not found"),
    }
}

async fn register(req: Request<Incoming>, peer: PeerContext, routes: Arc<Routes>) -> Resp {
    let dto: DeviceDto = match read_json(&peer, req.into_body(), SMALL_JSON_LIMIT, &routes).await {
        Ok(dto) => dto,
        Err(resp) => return resp,
    };
    if dto.validate().is_err() {
        return error(StatusCode::BAD_REQUEST, "invalid body");
    }
    // Under TLS the certificate is the identity, whatever the body claims.
    let _ = routes.signals.try_send(ServerSignal::Registered { identity: peer.identity.clone(), ip: peer.ip, dto });
    json(StatusCode::OK, &routes.shared.register_response_dto())
}

/// Reads a JSON body under a size cap, a deadline, the global budget and,
/// for large bodies, a per-IP-group cap.
pub async fn read_json<T: serde::de::DeserializeOwned>(
    peer: &PeerContext,
    body: Incoming,
    limit: usize,
    routes: &Routes,
) -> Result<T, Resp> {
    let hint = hyper::body::Body::size_hint(&body).upper().map(|n| n as usize).unwrap_or(limit);
    if hint > limit {
        return Err(error(StatusCode::PAYLOAD_TOO_LARGE, "body too large"));
    }
    let _inflight = if hint > SMALL_JSON_LIMIT {
        let key = peer_key(peer.ip.ip);
        let mut inflight = routes.large_json_inflight.lock().unwrap();
        let n = inflight.entry(key).or_insert(0);
        if *n >= LARGE_JSON_PER_PEER {
            return Err(error(StatusCode::TOO_MANY_REQUESTS, "too many requests in flight"));
        }
        *n += 1;
        Some(InflightGuard { routes, key })
    } else {
        None
    };
    let kib = hint.div_ceil(1024).max(1) as u32;
    let _permit = match tokio::time::timeout(Duration::from_secs(5), routes.json_budget.acquire_many(kib)).await {
        Ok(Ok(permit)) => permit,
        _ => return Err(error(StatusCode::SERVICE_UNAVAILABLE, "server busy")),
    };
    let collected = tokio::time::timeout(JSON_READ_TIMEOUT, Limited::new(body, limit).collect()).await;
    let bytes = match collected {
        Ok(Ok(c)) => c.to_bytes(),
        Ok(Err(_)) => return Err(error(StatusCode::BAD_REQUEST, "invalid body")),
        Err(_) => return Err(error(StatusCode::REQUEST_TIMEOUT, "timed out reading body")),
    };
    serde_json::from_slice(&bytes).map_err(|_| error(StatusCode::BAD_REQUEST, "invalid body"))
}

struct InflightGuard<'a> {
    routes: &'a Routes,
    key: IpAddr,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        let mut inflight = self.routes.large_json_inflight.lock().unwrap();
        if let Some(n) = inflight.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                inflight.remove(&self.key);
            }
        }
    }
}

pub fn parse_query(query: Option<&str>) -> HashMap<String, String> {
    query
        .map(|q| form_urlencoded::parse(q.as_bytes()).take(16).map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
        .unwrap_or_default()
}

pub fn json<T: serde::Serialize>(status: StatusCode, body: &T) -> Resp {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let mut resp = Response::new(Full::new(Bytes::from(bytes)));
    *resp.status_mut() = status;
    resp.headers_mut().insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/json"));
    resp
}

pub fn error(status: StatusCode, message: &str) -> Resp {
    json(status, &ErrorBody { message: message.to_string(), offset: None })
}

pub fn empty(status: StatusCode) -> Resp {
    let mut resp = Response::new(Full::new(Bytes::new()));
    *resp.status_mut() = status;
    resp
}
