//! Server configuration (environment variables and tunable limits).

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use ipnet::IpNet;

/// Lifetime of TURN credentials handed out by `GET /v1/turn`.
pub const TURN_CREDENTIAL_TTL: Duration = Duration::from_secs(600);

/// Server configuration.
///
/// `Config::default()` allows any `Origin`, trusts no proxy, has TURN
/// disabled and uses the default [`Limits`].
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Exact browser origins (`scheme://host[:port]`) allowed to connect.
    /// `None` allows every origin. Requests without an `Origin` header
    /// (native clients) are always allowed.
    pub allowed_origins: Option<Vec<String>>,
    /// Proxies whose `X-Forwarded-For` header is honoured.
    pub trusted_proxies: Vec<IpNet>,
    /// TURN REST credentials (`use-auth-secret`), if enabled.
    pub turn: Option<TurnConfig>,
    /// Abuse limits.
    pub limits: Limits,
}

/// coturn `use-auth-secret` configuration.
#[derive(Clone)]
pub struct TurnConfig {
    /// The shared secret (coturn `static-auth-secret`).
    pub secret: Vec<u8>,
    /// `turn:`/`turns:` URLs handed to clients.
    pub urls: Vec<String>,
    /// Credential lifetime.
    pub ttl: Duration,
}

impl fmt::Debug for TurnConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnConfig").field("secret", &"<redacted>").field("urls", &self.urls).field("ttl", &self.ttl).finish()
    }
}

/// Abuse limits. The defaults implement `docs/05-protocol.md` §5.1.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum WebSocket message and frame size.
    pub max_frame_bytes: usize,
    /// Maximum length of the `d` query parameter (encoded).
    pub max_d_param_bytes: usize,
    /// Per-connection token bucket refill rate; every inbound frame costs one token.
    pub frames_per_sec: f64,
    /// Per-connection token bucket capacity.
    pub frame_burst: f64,
    /// Invalid/oversized/forbidden messages tolerated before the socket is closed.
    pub max_violations: u32,
    /// Concurrent connections per IP group.
    pub max_conns_per_group: u32,
    /// Concurrent connections in total.
    pub max_conns: u32,
    /// Refill rate of the per-IP-group HTTP request bucket (`/v1/ws`, `/v1/turn`).
    pub conn_attempts_per_sec: f64,
    /// Capacity of the per-IP-group HTTP request bucket.
    pub conn_attempt_burst: f64,
    /// Outbound queue length per connection; overflowing it disconnects the peer.
    pub outbound_queue: usize,
    /// A single blocked socket write longer than this disconnects the peer.
    pub write_timeout: Duration,
    /// Interval of server-sent WebSocket pings.
    pub ping_interval: Duration,
    /// Connections that send no frame at all (not even a pong) for this long are closed.
    pub idle_timeout: Duration,
    /// Members per room.
    pub max_room_members: usize,
    /// Rooms per connection.
    pub max_rooms_per_conn: usize,
    /// `ROOM_JOIN` attempts per connection per minute.
    pub room_joins_per_minute: u32,
    /// `ROOM_JOIN` attempts on short-code (`c:`) rooms per IP group per
    /// minute, across all of its connections (codes must not be guessable by
    /// reconnecting).
    pub code_joins_per_minute: u32,
    /// `UPDATE` messages per connection per minute (each one fans out).
    pub updates_per_minute: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 64 * 1024,
            max_d_param_bytes: 4 * 1024,
            frames_per_sec: 30.0,
            frame_burst: 60.0,
            max_violations: 10,
            max_conns_per_group: 16,
            max_conns: 10_000,
            conn_attempts_per_sec: 1.0,
            conn_attempt_burst: 30.0,
            outbound_queue: 64,
            write_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(60),
            max_room_members: 32,
            max_rooms_per_conn: 4,
            room_joins_per_minute: 10,
            code_joins_per_minute: 30,
            updates_per_minute: 10,
        }
    }
}

/// An invalid configuration value.
#[derive(Debug)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Reads `FERRY_SIGNAL_ALLOWED_ORIGINS`, `FERRY_SIGNAL_TRUSTED_PROXIES`,
    /// `FERRY_SIGNAL_MAX_CONNS_PER_IP`, `FERRY_SIGNAL_MAX_CONNS`,
    /// `FERRY_TURN_SECRET` and `FERRY_TURN_URLS`. Empty values count as unset.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Like [`Config::from_env`], reading variables through `get`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let var = |key: &str| get(key).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());

        let allowed_origins = var("FERRY_SIGNAL_ALLOWED_ORIGINS").map(|v| parse_origins(&v)).transpose()?;
        let trusted_proxies = var("FERRY_SIGNAL_TRUSTED_PROXIES").map(|v| parse_proxies(&v)).transpose()?.unwrap_or_default();
        let turn = match (var("FERRY_TURN_SECRET"), var("FERRY_TURN_URLS")) {
            (Some(secret), Some(urls)) => {
                Some(TurnConfig { secret: secret.into_bytes(), urls: parse_turn_urls(&urls)?, ttl: TURN_CREDENTIAL_TTL })
            }
            (None, None) => None,
            _ => {
                return Err(ConfigError("FERRY_TURN_SECRET and FERRY_TURN_URLS must be set together".into()));
            }
        };

        let mut limits = Limits::default();
        if let Some(v) = var("FERRY_SIGNAL_MAX_CONNS_PER_IP") {
            limits.max_conns_per_group = parse_count("FERRY_SIGNAL_MAX_CONNS_PER_IP", &v)?;
        }
        if let Some(v) = var("FERRY_SIGNAL_MAX_CONNS") {
            limits.max_conns = parse_count("FERRY_SIGNAL_MAX_CONNS", &v)?;
        }

        Ok(Self { allowed_origins, trusted_proxies, turn, limits })
    }
}

/// A positive connection count.
fn parse_count(key: &str, value: &str) -> Result<u32, ConfigError> {
    value.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| ConfigError(format!("{key}: `{value}` is not a positive integer")))
}

fn split_list(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(str::trim).filter(|s| !s.is_empty())
}

fn parse_origins(value: &str) -> Result<Vec<String>, ConfigError> {
    let mut origins = Vec::new();
    for origin in split_list(value) {
        let origin = origin.trim_end_matches('/');
        let valid = origin == "null"
            || origin.split_once("://").is_some_and(|(scheme, rest)| !scheme.is_empty() && !rest.is_empty() && !rest.contains('/'));
        if !valid || origin.contains(char::is_whitespace) {
            return Err(ConfigError(format!("FERRY_SIGNAL_ALLOWED_ORIGINS: `{origin}` is not an origin (expected scheme://host[:port])")));
        }
        origins.push(origin.to_ascii_lowercase());
    }
    if origins.is_empty() {
        return Err(ConfigError("FERRY_SIGNAL_ALLOWED_ORIGINS contains no origins".into()));
    }
    Ok(origins)
}

fn parse_proxies(value: &str) -> Result<Vec<IpNet>, ConfigError> {
    split_list(value)
        .map(|entry| {
            entry
                .parse::<IpNet>()
                .or_else(|_| entry.parse::<IpAddr>().map(IpNet::from))
                .map_err(|_| ConfigError(format!("FERRY_SIGNAL_TRUSTED_PROXIES: `{entry}` is not an IP address or CIDR")))
        })
        .collect()
}

fn parse_turn_urls(value: &str) -> Result<Vec<String>, ConfigError> {
    let urls: Vec<String> = split_list(value).map(str::to_owned).collect();
    for url in &urls {
        let lower = url.to_ascii_lowercase();
        let rest = lower.strip_prefix("turn:").or_else(|| lower.strip_prefix("turns:"));
        if rest.is_none_or(str::is_empty) || url.contains(char::is_whitespace) {
            return Err(ConfigError(format!("FERRY_TURN_URLS: `{url}` is not a turn: or turns: URL")));
        }
    }
    if urls.is_empty() {
        return Err(ConfigError("FERRY_TURN_URLS contains no URLs".into()));
    }
    Ok(urls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        Config::from_lookup(|k| vars.get(k).cloned())
    }

    #[test]
    fn empty_environment_is_permissive() {
        let c = config(&[]).unwrap();
        assert!(c.allowed_origins.is_none());
        assert!(c.trusted_proxies.is_empty());
        assert!(c.turn.is_none());
        assert_eq!(c.limits.max_conns_per_group, Limits::default().max_conns_per_group);
        let c = config(&[("FERRY_SIGNAL_ALLOWED_ORIGINS", "  ")]).unwrap();
        assert!(c.allowed_origins.is_none());
    }

    #[test]
    fn parses_lists() {
        let c = config(&[
            ("FERRY_SIGNAL_ALLOWED_ORIGINS", "https://Ferry.example/, http://localhost:5173"),
            ("FERRY_SIGNAL_TRUSTED_PROXIES", "10.0.0.0/8, 192.0.2.1,::1"),
            ("FERRY_TURN_SECRET", "s3cret"),
            ("FERRY_TURN_URLS", "turn:turn.example:3478?transport=udp,turns:turn.example:5349"),
        ])
        .unwrap();
        assert_eq!(c.allowed_origins.unwrap(), ["https://ferry.example", "http://localhost:5173"]);
        assert_eq!(c.trusted_proxies.len(), 3);
        assert!(c.trusted_proxies[1].contains(&"192.0.2.1".parse::<IpAddr>().unwrap()));
        let turn = c.turn.unwrap();
        assert_eq!(turn.secret, b"s3cret");
        assert_eq!(turn.urls.len(), 2);
        assert_eq!(turn.ttl, TURN_CREDENTIAL_TTL);
        assert!(!format!("{turn:?}").contains("s3cret"));
    }

    #[test]
    fn parses_connection_caps() {
        let c = config(&[("FERRY_SIGNAL_MAX_CONNS_PER_IP", "200"), ("FERRY_SIGNAL_MAX_CONNS", " 50000 ")]).unwrap();
        assert_eq!(c.limits.max_conns_per_group, 200);
        assert_eq!(c.limits.max_conns, 50_000);
        assert!(config(&[("FERRY_SIGNAL_MAX_CONNS_PER_IP", "0")]).is_err());
        assert!(config(&[("FERRY_SIGNAL_MAX_CONNS", "-1")]).is_err());
        assert!(config(&[("FERRY_SIGNAL_MAX_CONNS", "lots")]).is_err());
    }

    #[test]
    fn rejects_bad_values() {
        assert!(config(&[("FERRY_SIGNAL_ALLOWED_ORIGINS", "ferry.example")]).is_err());
        assert!(config(&[("FERRY_SIGNAL_ALLOWED_ORIGINS", "https://a.example/path")]).is_err());
        assert!(config(&[("FERRY_SIGNAL_TRUSTED_PROXIES", "10.0.0.0/33")]).is_err());
        assert!(config(&[("FERRY_SIGNAL_TRUSTED_PROXIES", "proxy.local")]).is_err());
        assert!(config(&[("FERRY_TURN_SECRET", "x")]).is_err());
        assert!(config(&[("FERRY_TURN_SECRET", "x"), ("FERRY_TURN_URLS", "stun:a")]).is_err());
        assert!(config(&[("FERRY_TURN_SECRET", "x"), ("FERRY_TURN_URLS", "turn:")]).is_err());
    }
}
