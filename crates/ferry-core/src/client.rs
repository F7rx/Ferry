//! Talking to one peer over HTTP(S). The certificate is pinned during the
//! TLS handshake (nothing leaves this device if the peer isn't who we expect),
//! and every call has a deadline suited to what it does.

use crate::identity::Identity;
use crate::model::Protocol;
use crate::proto::{self, *};
use bytes::Bytes;
use futures_util::StreamExt;
use localsend::reqwest;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SHORT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// No body bytes accepted by the socket for this long = connection is dead.
const UPLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(25);
/// After the last byte: the receiver may still fsync / hash a big file.
const UPLOAD_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_ERROR_BODY: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeerAddress {
    /// IP address, possibly scoped (`fe80::1%3`).
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
}

impl PeerAddress {
    pub fn base_url(&self) -> String {
        let host = match localsend::http::client::scoped_host::encode(&self.host) {
            Some(encoded) => encoded,
            None if self.host.contains(':') => format!("[{}]", self.host),
            None => self.host.clone(),
        };
        format!("{}://{}:{}", self.protocol.as_str(), host, self.port)
    }

    pub fn display(&self) -> String {
        if self.host.contains(':') { format!("[{}]:{}", self.host, self.port) } else { format!("{}:{}", self.host, self.port) }
    }

    pub fn ip_version(&self) -> u8 {
        if self.host.contains(':') { 6 } else { 4 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("peer answered {status}: {message}")]
    Status { status: u16, message: String, offset: Option<u64> },
    #[error("the peer presented a different identity")]
    IdentityMismatch,
    #[error("connection failed: {0}")]
    Network(String),
    #[error("connection stalled")]
    Stalled,
    #[error("timed out")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
    #[error("invalid response: {0}")]
    Protocol(String),
    #[error("reading the file failed: {0}")]
    Source(std::io::Error),
}

impl ClientError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ClientError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Worth retrying after a reconnect (as opposed to a final answer).
    pub fn is_connectivity(&self) -> bool {
        matches!(self, ClientError::Network(_) | ClientError::Stalled | ClientError::Timeout)
    }

    fn from_reqwest(err: reqwest::Error) -> ClientError {
        // The pinning verifier's rejection travels inside the connect error.
        let chain = error_chain(&err);
        if chain.contains("fingerprint mismatch") {
            return ClientError::IdentityMismatch;
        }
        if err.is_timeout() {
            return ClientError::Timeout;
        }
        if err.is_decode() {
            return ClientError::Protocol(chain);
        }
        ClientError::Network(chain)
    }
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

pub struct RegisterResult {
    pub device: DeviceDto,
    /// The certificate fingerprint the peer actually used (HTTPS only).
    pub cert_fingerprint: Option<String>,
}

pub enum PrepareResult {
    Accepted(PrepareUploadResponse),
    /// 204: nothing to transfer (all declined, or a text message was received).
    NothingAccepted,
}

pub struct UploadResult {
    /// The receiver's SHA-256 of the complete file (Ferry receivers).
    pub receiver_sha256: Option<String>,
}

pub struct PeerClient {
    http: reqwest::Client,
    pub addr: PeerAddress,
}

/// Builds an HTTP client that presents our certificate and only talks to a
/// peer whose certificate has `expected_fingerprint` (None = trust on first
/// use, for discovery probes).
pub fn build_http_client(identity: &Identity, expected_fingerprint: Option<String>) -> anyhow::Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certs = vec![CertificateDer::from_pem_slice(identity.cert_pem.as_bytes())?];
    let key = PrivateKeyDer::from_pem_slice(identity.key_pem.as_bytes())?;
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(localsend::http::client::server_cert_verifier::PinnedServerCertVerifier::try_new(
            &identity.cert_pem,
            expected_fingerprint,
        )?))
        .with_client_auth_cert(certs, key)?;
    // HTTP/1.1: one stream per connection keeps per-peer limits meaningful and
    // avoids HTTP/2 flow-control windows capping bulk throughput.
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .tls_info(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(localsend::http::client::ScopedHostResolver))
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(16)
        .build()?)
}

impl PeerClient {
    pub fn new(identity: &Identity, addr: PeerAddress, expected_fingerprint: Option<String>) -> anyhow::Result<Self> {
        let pin = match addr.protocol {
            Protocol::Https => expected_fingerprint,
            Protocol::Http => None,
        };
        Ok(PeerClient { http: build_http_client(identity, pin)?, addr })
    }

    /// The pooled HTTP client (pinned to this peer), for sharing between tasks.
    pub fn http_client(&self) -> reqwest::Client {
        self.http.clone()
    }

    /// Reuses an existing client (connection pool) for another address of the
    /// same peer.
    pub fn with_http(http: reqwest::Client, addr: PeerAddress) -> Self {
        PeerClient { http, addr }
    }

    fn url(&self, path: &str, params: &[(&str, &str)]) -> String {
        let mut url = format!("{}{}", self.addr.base_url(), path);
        if !params.is_empty() {
            let query = form_urlencoded::Serializer::new(String::new()).extend_pairs(params.iter().copied()).finish();
            url.push('?');
            url.push_str(&query);
        }
        url
    }

    pub async fn register(&self, dto: &DeviceDto, timeout: Duration) -> Result<RegisterResult, ClientError> {
        let response = self
            .http
            .post(self.url(&format!("{API_V2}/register"), &[]))
            .json(dto)
            .timeout(timeout)
            .send()
            .await
            .map_err(ClientError::from_reqwest)?;
        let cert_fingerprint = cert_fingerprint(&response);
        let response = check(response).await?;
        let device: DeviceDto = read_json(response).await?;
        Ok(RegisterResult { device, cert_fingerprint })
    }

    pub async fn info(&self) -> Result<RegisterResult, ClientError> {
        let response = self
            .http
            .get(self.url(&format!("{API_V2}/info"), &[]))
            .timeout(SHORT_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(ClientError::from_reqwest)?;
        let cert_fingerprint = cert_fingerprint(&response);
        let response = check(response).await?;
        Ok(RegisterResult { device: read_json(response).await?, cert_fingerprint })
    }

    /// Ferry capability handshake. `Ok(None)` = not a Ferry device.
    pub async fn hello(&self) -> Result<Option<FerryHello>, ClientError> {
        let response = self
            .http
            .get(self.url(&format!("{API_FERRY}/hello"), &[]))
            .timeout(SHORT_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(ClientError::from_reqwest)?;
        if matches!(response.status().as_u16(), 404 | 405) {
            return Ok(None);
        }
        let response = check(response).await?;
        Ok(Some(read_json(response).await?))
    }

    /// Held open by the receiver until its user decides.
    pub async fn prepare_upload(
        &self,
        request: &PrepareUploadRequest,
        pin: Option<&str>,
        decision_timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<PrepareResult, ClientError> {
        let params: Vec<(&str, &str)> = pin.map(|p| vec![("pin", p)]).unwrap_or_default();
        let send = self.http.post(self.url(&format!("{API_V2}/prepare-upload"), &params)).json(request).timeout(decision_timeout).send();
        let response = tokio::select! {
            r = send => r.map_err(ClientError::from_reqwest)?,
            _ = cancel.cancelled() => return Err(ClientError::Cancelled),
        };
        if response.status().as_u16() == 204 {
            return Ok(PrepareResult::NothingAccepted);
        }
        let response = check(response).await?;
        Ok(PrepareResult::Accepted(read_json(response).await?))
    }

    /// Streams one file. `progress` counts bytes accepted by the socket.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload(
        &self,
        session_id: &str,
        file_id: &str,
        token: &str,
        offset: Option<u64>,
        chunks: mpsc::Receiver<std::io::Result<Bytes>>,
        progress: Arc<AtomicU64>,
        cancel: &CancellationToken,
    ) -> Result<UploadResult, ClientError> {
        let offset_str = offset.map(|o| o.to_string());
        let mut params = vec![("sessionId", session_id), ("fileId", file_id), ("token", token)];
        if let Some(o) = offset_str.as_deref() {
            params.push(("offset", o));
        }

        let last_progress = Arc::new(SharedInstant::now());
        let body_done = CancellationToken::new();
        let source_error: Arc<std::sync::Mutex<Option<std::io::Error>>> = Default::default();
        // Bytes are counted when hyper takes them for the socket, so progress
        // and the stall watchdog reflect what the network actually accepts.
        let stream = futures_util::stream::unfold(
            (chunks, progress, last_progress.clone(), body_done.clone(), source_error.clone()),
            |(mut rx, progress, last, done, error)| async move {
                match rx.recv().await {
                    Some(Ok(chunk)) => {
                        progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                        last.touch();
                        Some((Ok(chunk), (rx, progress, last, done, error)))
                    }
                    Some(Err(err)) => {
                        let kind = err.kind();
                        *error.lock().unwrap() = Some(err);
                        rx.close();
                        Some((Err(std::io::Error::new(kind, "source read failed")), (rx, progress, last, done, error)))
                    }
                    None => {
                        done.cancel();
                        None
                    }
                }
            },
        );

        let request = self
            .http
            .post(self.url(&format!("{API_V2}/upload"), &params))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .send();
        tokio::pin!(request);

        let response = loop {
            tokio::select! {
                r = &mut request => break r,
                _ = cancel.cancelled() => return Err(ClientError::Cancelled),
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    let limit = if body_done.is_cancelled() { UPLOAD_RESPONSE_TIMEOUT } else { UPLOAD_STALL_TIMEOUT };
                    if last_progress.elapsed() > limit {
                        return Err(ClientError::Stalled);
                    }
                }
            }
        };

        if let Some(err) = source_error.lock().unwrap().take() {
            return Err(ClientError::Source(err));
        }
        let response = response.map_err(ClientError::from_reqwest)?;
        let response = check(response).await?;
        let body: UploadResponse = read_json_lenient(response).await;
        Ok(UploadResult { receiver_sha256: body.sha256 })
    }

    pub async fn verify(&self, session_id: &str, file_id: &str, token: &str, sha256: &str) -> Result<bool, ClientError> {
        let response = self
            .http
            .post(self.url(&format!("{API_FERRY}/verify"), &[("sessionId", session_id), ("fileId", file_id), ("token", token)]))
            .json(&VerifyRequest { sha256: sha256.to_string() })
            .timeout(SHORT_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(ClientError::from_reqwest)?;
        if response.status().as_u16() == 422 {
            return Ok(false);
        }
        let response = check(response).await?;
        let body: VerifyResponse = read_json(response).await?;
        Ok(body.ok)
    }

    pub async fn transfer_status(&self, transfer_id: &str) -> Result<Option<TransferStatusResponse>, ClientError> {
        let response = self
            .http
            .get(self.url(&format!("{API_FERRY}/transfers/{transfer_id}"), &[]))
            .timeout(SHORT_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(ClientError::from_reqwest)?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let response = check(response).await?;
        Ok(Some(read_json(response).await?))
    }

    /// Best effort: tells the peer to drop the session.
    pub async fn cancel(&self, session_id: Option<&str>) {
        let params: Vec<(&str, &str)> = session_id.map(|s| vec![("sessionId", s)]).unwrap_or_default();
        let _ = self.http.post(self.url(&format!("{API_V2}/cancel"), &params)).timeout(Duration::from_secs(3)).send().await;
    }
}

fn cert_fingerprint(response: &reqwest::Response) -> Option<String> {
    let info = response.extensions().get::<reqwest::tls::TlsInfo>()?;
    info.peer_certificate().map(localsend::crypto::cert::fingerprint_from_cert_der)
}

async fn check(response: reqwest::Response) -> Result<reqwest::Response, ClientError> {
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let body = read_capped(response).await.unwrap_or_default();
    let parsed = serde_json::from_slice::<ErrorBody>(&body).ok();
    Err(ClientError::Status {
        status,
        message: parsed.as_ref().map(|e| e.message.clone()).unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned()),
        offset: parsed.and_then(|e| e.offset),
    })
}

async fn read_capped(response: reqwest::Response) -> Result<Vec<u8>, ClientError> {
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ClientError::from_reqwest)?;
        out.extend_from_slice(&chunk);
        if out.len() > proto_max_response() {
            return Err(ClientError::Protocol("response too large".into()));
        }
    }
    Ok(out)
}

fn proto_max_response() -> usize {
    // prepare-upload responses list one token per file: 50k files ≈ 4 MiB.
    8 * 1024 * 1024
}

async fn read_json<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T, ClientError> {
    let body = read_capped(response).await?;
    serde_json::from_slice(&body).map_err(|e| ClientError::Protocol(e.to_string()))
}

async fn read_json_lenient<T: serde::de::DeserializeOwned + Default>(response: reqwest::Response) -> T {
    match read_capped(response).await {
        Ok(body) if !body.is_empty() && body.len() <= MAX_ERROR_BODY => serde_json::from_slice(&body).unwrap_or_default(),
        _ => T::default(),
    }
}

/// An `Instant` that can be updated from another task.
struct SharedInstant {
    base: Instant,
    offset_ms: AtomicU64,
}

impl SharedInstant {
    fn now() -> Self {
        SharedInstant { base: Instant::now(), offset_ms: AtomicU64::new(0) }
    }

    fn touch(&self) {
        self.offset_ms.store(self.base.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn elapsed(&self) -> Duration {
        let last = Duration::from_millis(self.offset_ms.load(Ordering::Relaxed));
        self.base.elapsed().saturating_sub(last)
    }
}

/// Maps a peer's HTTP answer to a user-facing error.
pub fn describe_status(err: &ClientError, alias: &str) -> crate::ErrorInfo {
    use crate::ErrorInfo;
    match err {
        ClientError::Status { status, message, .. } => match status {
            401 if message.to_lowercase().contains("invalid") => ErrorInfo::pin_invalid(),
            401 => ErrorInfo::pin_required(),
            403 => ErrorInfo::declined(),
            409 => ErrorInfo::busy(),
            429 => ErrorInfo::too_many_attempts(),
            422 => ErrorInfo::new("checksum_mismatch", "The receiver reported damaged data."),
            507 => ErrorInfo::new("receiver_disk_full", format!("{alias} doesn't have enough free space.")),
            _ => ErrorInfo::new("peer_error", format!("{alias} reported an error ({status}).")),
        },
        ClientError::IdentityMismatch => ErrorInfo::identity_changed(alias),
        ClientError::Network(_) | ClientError::Timeout => ErrorInfo::unreachable(alias),
        ClientError::Stalled => ErrorInfo::new("connection_lost", format!("Lost the connection to {alias}.")),
        ClientError::Cancelled => ErrorInfo::cancelled(),
        ClientError::Protocol(e) => ErrorInfo::new("protocol", format!("{alias} sent an unexpected answer ({e}).")),
        ClientError::Source(e) => ErrorInfo::new("file_unreadable", format!("A file couldn't be read: {e}")),
    }
}

#[allow(unused)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<PeerClient>();
    let _ = proto::PROTOCOL_VERSION;
}
