//! TURN REST credentials (coturn `use-auth-secret`).
//!
//! `username = "<unix expiry>:<client id>"`,
//! `credential = base64(HMAC-SHA1(secret, username))`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha1::Sha1;
use uuid::Uuid;

use crate::config::TurnConfig;

/// Body of `GET /v1/turn`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnResponse {
    ice_servers: Vec<IceServer>,
    ttl: u64,
}

#[derive(Debug, Serialize)]
struct IceServer {
    urls: Vec<String>,
    username: String,
    credential: String,
}

pub(crate) fn credentials(config: &TurnConfig, client: Uuid, now_unix: u64) -> TurnResponse {
    let ttl = config.ttl.as_secs();
    let username = format!("{}:{client}", now_unix.saturating_add(ttl));
    let credential = sign(&config.secret, &username);
    TurnResponse { ice_servers: vec![IceServer { urls: config.urls.clone(), username, credential }], ttl }
}

fn sign(secret: &[u8], username: &str) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(username.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn hmac_sha1_matches_rfc2202() {
        // RFC 2202 test case 2: HMAC-SHA1("Jefe", "what do ya want for nothing?")
        // = effcdf6ae5eb2fa2d27416d5f184df9c259a7c79.
        assert_eq!(sign(b"Jefe", "what do ya want for nothing?"), "7/zfauXrL6LSdBbV8YTfnCWafHk=");
    }

    #[test]
    fn credential_format() {
        let config = TurnConfig { secret: b"s3cret".to_vec(), urls: vec!["turn:turn.example:3478".into()], ttl: Duration::from_secs(600) };
        let id = Uuid::nil();
        let body = serde_json::to_value(credentials(&config, id, 1_000)).unwrap();
        assert_eq!(body["ttl"], 600);
        let server = &body["iceServers"][0];
        assert_eq!(server["urls"][0], "turn:turn.example:3478");
        assert_eq!(server["username"], format!("1600:{id}"));
        assert_eq!(server["credential"], sign(b"s3cret", &format!("1600:{id}")));
    }
}
