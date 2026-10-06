//! Pairing two engines as "my devices": QR/link and code comparison.

mod common;

use common::*;
use ferry_core::events::EngineEvent;
use ferry_core::model::{Protocol, TransferState};
use ferry_core::pairing::PairingOutcome;
use ferry_core::{SendItem, Target};

/// The offer's link, pointed at loopback (tests don't rely on a LAN).
fn loopback_uri(uri: &str, port: u16) -> String {
    let (head, query) = uri.split_once('?').unwrap();
    let query: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some(("a", _)) => "a=127.0.0.1".to_string(),
            Some(("p", _)) => format!("p={port}"),
            _ => kv.to_string(),
        })
        .collect();
    format!("{head}?{}", query.join("&"))
}

fn with_param(uri: &str, key: &str, value: &str) -> String {
    let (head, query) = uri.split_once('?').unwrap();
    let query: Vec<String> =
        query.split('&').map(|kv| if kv.starts_with(&format!("{key}=")) { format!("{key}={value}") } else { kv.to_string() }).collect();
    format!("{head}?{}", query.join("&"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn qr_pairing_makes_both_devices_mine() {
    let mut desk = peer("Desk").await;
    let phone = peer("Phone").await;

    let offer = desk.engine.create_pairing_offer().unwrap();
    assert!(offer.uri.starts_with("ferry://pair?v=1&fp="), "{}", offer.uri);
    let uri = loopback_uri(&offer.uri, desk.engine.port());

    let paired = phone.engine.pair_with_uri(&uri).await.unwrap();
    assert_eq!(paired.id, desk.engine.fingerprint());
    assert!(paired.mine && paired.trusted && paired.verified);

    // The showing side learns who scanned it, and keeps them.
    let event = desk.wait_event(T, |e| matches!(e, EngineEvent::PairingOfferClosed { device: Some(_), .. })).await;
    let EngineEvent::PairingOfferClosed { id, device: Some(device) } = event else { unreachable!() };
    assert_eq!(id, offer.id);
    assert_eq!(device.id, phone.engine.fingerprint());
    assert!(device.mine && device.trusted);
    assert_eq!(device.alias, "Phone");

    // Single use.
    let again = phone.engine.pair_with_uri(&uri).await.unwrap_err();
    assert_eq!(again.info().code, "pair_expired");

    // The point of pairing: no prompt between my devices (default auto-accept).
    let src = tempfile::tempdir().unwrap();
    let file = write_file(src.path(), "notes.txt", b"from my phone");
    let ids = phone
        .engine
        .send(
            vec![Target::Address {
                host: "127.0.0.1".into(),
                port: desk.engine.port(),
                protocol: Protocol::Https,
                fingerprint: Some(desk.engine.fingerprint()),
            }],
            vec![SendItem::Path { path: file }],
        )
        .await
        .unwrap();
    let mut phone = phone;
    let sent = phone.wait_final(&ids[0], T).await;
    assert_eq!(sent.state, TransferState::Completed, "{:?}", sent.error);
    assert_eq!(std::fs::read(desk.saved("notes.txt")).unwrap(), b"from my phone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tampered_codes_are_rejected_and_guessing_locks_out() {
    let desk = peer("Desk").await;
    let phone = peer("Phone").await;
    let offer = desk.engine.create_pairing_offer().unwrap();
    let uri = loopback_uri(&offer.uri, desk.engine.port());

    // Wrong secret.
    let forged = with_param(&uri, "s", "AAAAAAAAAAAAAAAAAAAAAA");
    let err = phone.engine.pair_with_uri(&forged).await.unwrap_err();
    assert_eq!(err.info().code, "pair_expired");

    // A code naming another device's fingerprint: the pinned TLS check fails.
    let other = peer("Other").await;
    let wrong_fp = with_param(&uri, "fp", &other.engine.fingerprint());
    let err = phone.engine.pair_with_uri(&wrong_fp).await.unwrap_err();
    assert_eq!(err.info().code, "pair_unreachable");

    // Scanning your own code.
    let err = desk.engine.pair_with_uri(&uri).await.unwrap_err();
    assert_eq!(err.info().code, "pair_self");

    // Guessing: after a few failures even the right code is refused for a while.
    for _ in 0..5 {
        let _ = phone.engine.pair_with_uri(&forged).await;
    }
    let err = phone.engine.pair_with_uri(&uri).await.unwrap_err();
    assert_eq!(err.info().code, "pair_locked");
    assert!(!desk.engine.devices().iter().any(|d| d.mine), "nothing was paired");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_comparison_pairs_after_confirmation() {
    let mut desk = peer("Desk").await;
    let mut laptop = peer("Laptop").await;
    let laptop_id = desk.engine.add_device("127.0.0.1", laptop.engine.port()).await.unwrap().id;

    let pairing = desk.engine.start_code_pairing(&laptop_id).await.unwrap();
    assert_eq!(pairing.peer.alias, "Laptop");

    let event = laptop.wait_event(T, |e| matches!(e, EngineEvent::PairingRequest { .. })).await;
    let EngineEvent::PairingRequest { request } = event else { unreachable!() };
    assert_eq!(request.code, pairing.code, "both screens show the same code");
    assert_eq!(request.peer.alias, "Desk");
    assert!(laptop.engine.respond_pairing(&request.id, true));

    let event = desk.wait_event(T, |e| matches!(e, EngineEvent::PairingFinished { .. })).await;
    let EngineEvent::PairingFinished { id, outcome, device, error } = event else { unreachable!() };
    assert_eq!((id.as_str(), outcome), (pairing.id.as_str(), PairingOutcome::Paired), "{error:?}");
    assert!(device.unwrap().mine);
    let event = laptop.wait_event(T, |e| matches!(e, EngineEvent::PairingFinished { .. })).await;
    assert!(
        matches!(event, EngineEvent::PairingFinished { ref id, outcome: PairingOutcome::Paired, device: Some(ref d), .. } if *id == request.id && d.alias == "Desk")
    );
    laptop.wait_event(T, |e| matches!(e, EngineEvent::PairingRequestClosed { .. })).await;
    let desk_there = laptop.engine.devices().into_iter().find(|d| d.id == desk.engine.fingerprint()).unwrap();
    assert!(desk_there.mine && desk_there.trusted);

    // Unpairing is mutual.
    let after = desk.engine.unpair_device(&laptop_id).unwrap();
    assert!(after.is_none_or(|d| !d.mine));
    laptop.wait_event(T, |e| matches!(e, EngineEvent::Notice { code, .. } if code == "unpaired")).await;
    assert!(!laptop.engine.devices().iter().any(|d| d.mine));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_comparison_can_be_declined_or_withdrawn() {
    let mut desk = peer("Desk").await;
    let mut laptop = peer("Laptop").await;
    let laptop_id = desk.engine.add_device("127.0.0.1", laptop.engine.port()).await.unwrap().id;

    // Declined: codes didn't match.
    let first = desk.engine.start_code_pairing(&laptop_id).await.unwrap();
    let event = laptop.wait_event(T, |e| matches!(e, EngineEvent::PairingRequest { .. })).await;
    let EngineEvent::PairingRequest { request } = event else { unreachable!() };
    laptop.engine.respond_pairing(&request.id, false);
    let event = desk.wait_event(T, |e| matches!(e, EngineEvent::PairingFinished { .. })).await;
    assert!(matches!(event, EngineEvent::PairingFinished { ref id, outcome: PairingOutcome::Declined, .. } if *id == first.id));

    // Withdrawn by the asker: the prompt on the other side closes.
    let second = desk.engine.start_code_pairing(&laptop_id).await.unwrap();
    let event = laptop.wait_event(T, |e| matches!(e, EngineEvent::PairingRequest { .. })).await;
    let EngineEvent::PairingRequest { request } = event else { unreachable!() };
    assert!(desk.engine.cancel_code_pairing(&second.id));
    let event = desk.wait_event(T, |e| matches!(e, EngineEvent::PairingFinished { .. })).await;
    assert!(matches!(event, EngineEvent::PairingFinished { outcome: PairingOutcome::Cancelled, .. }));
    laptop.wait_event(T, |e| matches!(e, EngineEvent::PairingRequestClosed { id } if *id == request.id)).await;
    assert!(!laptop.engine.respond_pairing(&request.id, true), "a withdrawn prompt can't be accepted");
    assert!(!desk.engine.devices().iter().any(|d| d.mine));
    assert!(!laptop.engine.devices().iter().any(|d| d.mine));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_comparison_needs_a_commitment_and_a_matching_reveal() {
    use ferry_core::client::{PeerAddress, PeerClient};
    use ferry_core::identity::Identity;

    let mut desk = peer("Desk").await;
    let relay = Identity::generate().unwrap();
    let addr = PeerAddress { host: "127.0.0.1".into(), port: desk.engine.port(), protocol: Protocol::Https };
    let http = PeerClient::new(&relay, addr.clone(), Some(desk.engine.fingerprint())).unwrap().http_client();
    let base = addr.base_url();
    let device = serde_json::json!({ "alias": "Desk", "version": "2.1", "deviceType": "desktop", "fingerprint": "x", "port": 53317, "protocol": "https", "download": false });
    let post = |path: &str, body: serde_json::Value| http.post(format!("{base}{path}")).json(&body).send();

    // The old, uncommitted request is refused.
    let old = post("/api/ferry/v1/pair", serde_json::json!({ "proof": null, "device": device })).await.unwrap();
    assert_eq!(old.status(), 400);

    // Step 1: a commitment gets a session and the responder's nonce.
    let commit = "A".repeat(43);
    let first = post("/api/ferry/v1/pair", serde_json::json!({ "proof": null, "commit": commit, "device": device })).await.unwrap();
    assert_eq!(first.status(), 200);
    let reply: serde_json::Value = first.json().await.unwrap();
    let session = reply["session"].as_str().unwrap().to_string();
    assert_eq!(reply["nonce"].as_str().unwrap().len(), 43, "32 bytes, base64url");

    // One open request per network: a second one waits its turn.
    let second = post("/api/ferry/v1/pair", serde_json::json!({ "proof": null, "commit": commit, "device": device })).await.unwrap();
    assert_eq!(second.status(), 429);

    // Step 2 with a nonce that doesn't match the commitment: refused, and the session is spent.
    let wrong = post("/api/ferry/v1/pair/reveal", serde_json::json!({ "session": session, "nonce": "B".repeat(43) })).await.unwrap();
    assert_eq!(wrong.status(), 403);
    let again = post("/api/ferry/v1/pair/reveal", serde_json::json!({ "session": session, "nonce": "B".repeat(43) })).await.unwrap();
    assert_eq!(again.status(), 410);

    // Nobody was ever asked to compare codes.
    let asked = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        desk.wait_event(T, |e| matches!(e, EngineEvent::PairingRequest { .. })),
    )
    .await;
    assert!(asked.is_err(), "a prompt was shown");
}
