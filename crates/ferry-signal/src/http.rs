//! HTTP routes: `/v1/ws` (upgrade), `/v1/turn`, `/healthz`.

use std::hash::{BuildHasher, RandomState};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{ConnectInfo, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_PAD_INDIFFERENT;
use serde::Deserialize;
use tokio::sync::watch;
use uuid::Uuid;

use crate::config::Config;
use crate::conn::{self, ConnParams};
use crate::hub::Hub;
use crate::limit::{GroupLimiter, SlotError};
use crate::net::{self, IpGroup};
use crate::protocol::{RawClientInfo, ServerInfo, Violation, error_json};
use crate::turn;

/// An HTTP rejection: status plus message for the JSON error body.
type Rejection = (StatusCode, &'static str);

pub(crate) struct AppState {
    pub config: Config,
    pub hub: Hub,
    pub limiter: Arc<GroupLimiter>,
    pub server_info: ServerInfo,
    /// `true` once the server is shutting down. Every WebSocket connection
    /// holds a receiver until it has finished closing, so
    /// [`watch::Sender::closed`] tells when they are all gone.
    pub shutdown: watch::Sender<bool>,
    /// Random per-process key for pseudonymizing addresses in logs.
    log_key: RandomState,
}

impl AppState {
    pub(crate) fn new(config: Config) -> Self {
        let mut caps = vec!["rooms", "trickle"];
        if config.turn.is_some() {
            caps.push("turn");
        }
        Self {
            limiter: Arc::new(GroupLimiter::new(&config.limits)),
            hub: Hub::default(),
            server_info: ServerInfo { v: 1, caps },
            shutdown: watch::Sender::new(false),
            log_key: RandomState::new(),
            config,
        }
    }

    /// A short, salted, non-reversible tag for an IP group (never log raw IPs).
    pub(crate) fn ip_tag(&self, group: IpGroup) -> String {
        format!("{:08x}", self.log_key.hash_one(group) as u32)
    }

    /// Resolves the client's IP group and applies the per-group request
    /// limit and the Origin allowlist shared by all endpoints.
    fn admit(&self, addr: SocketAddr, headers: &HeaderMap) -> Result<IpGroup, Rejection> {
        let Some(ip) = net::client_ip(addr.ip(), headers, &self.config.trusted_proxies) else {
            tracing::debug!("rejected: unparseable X-Forwarded-For from trusted proxy");
            return Err((StatusCode::BAD_REQUEST, "invalid X-Forwarded-For header"));
        };
        let group = IpGroup::of(ip);
        if !self.limiter.allow_attempt(group) {
            tracing::debug!(ip = %self.ip_tag(group), "rejected: too many requests");
            return Err((StatusCode::TOO_MANY_REQUESTS, "too many requests from this network"));
        }
        if !origin_allowed(&self.config, headers) {
            tracing::debug!(ip = %self.ip_tag(group), "rejected: origin not allowed");
            return Err((StatusCode::FORBIDDEN, "origin not allowed"));
        }
        Ok(group)
    }
}

pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/ws", get(ws_handler))
        .route("/v1/turn", get(turn_handler))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(|| async { json_error(StatusCode::NOT_FOUND, "not found") })
        .with_state(state)
}

/// Every HTTP error body is JSON: `{"type":"ERROR","code":…,"message":…}`.
pub(crate) fn json_error(status: StatusCode, message: &str) -> Response {
    let body = error_json(status.as_u16(), message);
    (status, [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body.as_str().to_owned()).into_response()
}

fn violation_response(v: Violation) -> Response {
    let status = StatusCode::from_u16(v.code).unwrap_or(StatusCode::BAD_REQUEST);
    json_error(status, v.message)
}

/// Browsers send `Origin`; native clients do not and are always allowed.
fn origin_allowed(config: &Config, headers: &HeaderMap) -> bool {
    let Some(allowed) = &config.allowed_origins else {
        return true;
    };
    let mut values = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = values.next() else {
        return true;
    };
    if values.next().is_some() {
        return false;
    }
    origin.to_str().is_ok_and(|origin| allowed.iter().any(|a| a.eq_ignore_ascii_case(origin.trim())))
}

#[derive(Deserialize)]
struct WsQuery {
    d: Option<String>,
}

async fn ws_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    query: Result<Query<WsQuery>, QueryRejection>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let group = match state.admit(addr, &headers) {
        Ok(group) => group,
        Err((status, message)) => return json_error(status, message),
    };
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return json_error(rejection.status(), &rejection.body_text()),
    };
    let Ok(Query(WsQuery { d: Some(d) })) = query else {
        return json_error(StatusCode::BAD_REQUEST, "missing d parameter");
    };
    let limits = &state.config.limits;
    if d.len() > limits.max_d_param_bytes {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "d parameter too large");
    }
    let id = Uuid::new_v4();
    let info = match decode_client_info(&d).and_then(|raw| raw.validate(id)) {
        Ok(info) => info,
        Err(v) => return violation_response(v),
    };
    // Subscribed before the upgrade so that a connection racing with
    // shutdown is still told to close (and waited for).
    let shutdown = state.shutdown.subscribe();
    if *shutdown.borrow() {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "server is shutting down");
    }
    let ip_tag = state.ip_tag(group);
    let slot = match state.limiter.acquire(group) {
        Ok(slot) => slot,
        Err(SlotError::Group) => {
            tracing::debug!(ip = %ip_tag, "rejected: too many connections");
            return json_error(StatusCode::TOO_MANY_REQUESTS, "too many connections from this network");
        }
        Err(SlotError::Server) => {
            tracing::warn!("rejected: connection limit reached");
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "server is full");
        }
    };

    let max_frame = limits.max_frame_bytes;
    let params = ConnParams { id, info, group, ip_tag, slot, shutdown };
    upgrade
        .max_message_size(max_frame)
        .max_frame_size(max_frame)
        .on_failed_upgrade(|err| tracing::debug!("websocket upgrade failed: {err}"))
        .on_upgrade(move |socket| conn::run(socket, state, params))
}

/// Decodes `d`: base64url (padding optional) of a JSON `ClientInfoWithoutId`.
fn decode_client_info(d: &str) -> Result<RawClientInfo, Violation> {
    let json = URL_SAFE_PAD_INDIFFERENT.decode(d.trim()).map_err(|_| Violation::invalid("d is not valid base64url"))?;
    serde_json::from_slice(&json).map_err(|_| Violation::invalid("d is not a valid client info"))
}

#[derive(Deserialize)]
struct TurnQuery {
    peer: Option<String>,
}

async fn turn_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    query: Result<Query<TurnQuery>, QueryRejection>,
) -> Response {
    let mut response = turn_response(&state, addr, &headers, query);
    // Browsers fetch this cross-origin from the web app.
    if let Some(allow) = cors_allow_origin(&state.config, &headers) {
        let h = response.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow);
        h.append(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
}

fn turn_response(state: &AppState, addr: SocketAddr, headers: &HeaderMap, query: Result<Query<TurnQuery>, QueryRejection>) -> Response {
    let Some(turn) = &state.config.turn else {
        return json_error(StatusCode::NOT_FOUND, "TURN is not configured");
    };
    let group = match state.admit(addr, headers) {
        Ok(group) => group,
        Err((status, message)) => return json_error(status, message),
    };
    let Ok(Query(TurnQuery { peer: Some(peer) })) = query else {
        return json_error(StatusCode::BAD_REQUEST, "missing peer parameter");
    };
    let Ok(peer) = Uuid::parse_str(&peer) else {
        return json_error(StatusCode::BAD_REQUEST, "peer is not a client id");
    };
    // Only a connected client, asking from its own network, gets credentials.
    if state.hub.group_of(peer) != Some(group) {
        return json_error(StatusCode::FORBIDDEN, "unknown peer");
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    ([(header::CACHE_CONTROL, "no-store")], Json(turn::credentials(turn, peer, now))).into_response()
}

fn cors_allow_origin(config: &Config, headers: &HeaderMap) -> Option<HeaderValue> {
    let origin = headers.get(header::ORIGIN)?;
    match &config.allowed_origins {
        None => Some(HeaderValue::from_static("*")),
        Some(_) if origin_allowed(config, headers) => Some(origin.clone()),
        Some(_) => None,
    }
}
