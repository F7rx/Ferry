//! Two native connectors meet through a real (in-process) ferry-signal and
//! open an authenticated `ferry-dc/1` session over real WebRTC (webrtc-rs).

use ferry_core::rtc::identity::RtcIdentity;
use ferry_core::rtc::peer::{ConnectOptions, Connector, ConnectorConfig};
use ferry_core::rtc::protocol::DeviceInfo;
use ferry_core::rtc::session::{SessionEvent, TransferRequest};
use ferry_core::rtc::signaling::{ClientInfoOut, SignalingClient, SignalingConfig, SignalingEvent};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

async fn signal_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(ferry_signal::serve(listener, ferry_signal::Config::default()));
    format!("ws://{addr}/v1/ws")
}

struct Side {
    connector: Arc<Connector>,
    events: tokio::sync::mpsc::UnboundedReceiver<SignalingEvent>,
    identity: Arc<RtcIdentity>,
}

fn side(url: &str, alias: &str) -> Side {
    let identity = Arc::new(RtcIdentity::generate());
    let info = ClientInfoOut {
        alias: alias.into(),
        device_model: None,
        device_type: Some("desktop".into()),
        token: alias.into(),
        public_key: identity.public_key().into(),
        nearby: None,
    };
    let (client, events) = SignalingClient::start(SignalingConfig::new(url, info));
    let mut config =
        ConnectorConfig::new(identity.clone(), DeviceInfo { alias: alias.into(), device_type: "desktop".into(), platform: "test".into() });
    config.include_loopback = true;
    Side { connector: Connector::new(client, config), events, identity }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_peers_connect_through_signaling() {
    let url = signal_server().await;
    let mut a = side(&url, "A");
    let mut b = side(&url, "B");
    // A learns B's client id from presence.
    let b_id = loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), a.events.recv()).await.unwrap().unwrap();
        match ev {
            SignalingEvent::Hello { peers, .. } if !peers.is_empty() => break peers[0].id.clone(),
            SignalingEvent::Join { peer } => break peer.id.clone(),
            _ => {}
        }
    };
    // B answers whatever comes in.
    let b_connector = b.connector.clone();
    let a_key = a.identity.public_key().to_string();
    let answering = tokio::spawn(async move {
        loop {
            let ev = b.events.recv().await.unwrap();
            if let Some(incoming) = b_connector.handle_signal(&ev) {
                return b_connector
                    .accept(incoming, ConnectOptions { expected_peer_key: Some(a_key), room_secret: None, ice_servers: vec![] })
                    .await;
            }
        }
    });
    let a_connector = a.connector.clone();
    tokio::spawn(async move {
        while let Some(ev) = a.events.recv().await {
            a_connector.handle_signal(&ev);
        }
    });
    let started = std::time::Instant::now();
    let connected = a
        .connector
        .connect(&b_id, ConnectOptions { expected_peer_key: Some(b.identity.public_key().into()), room_secret: None, ice_servers: vec![] })
        .await
        .unwrap();
    let mut answered = answering.await.unwrap().unwrap();
    eprintln!("connected in {:?}, relayed={:?} remote={:?}", started.elapsed(), connected.relayed, connected.remote_address);
    assert_eq!(connected.session.peer().unwrap().key, b.identity.public_key());
    assert_eq!(answered.session.peer().unwrap().short_code, connected.session.peer().unwrap().short_code);

    // A text-only offer goes through and is accepted with nothing to stream.
    let sender = connected.session.clone();
    let send = tokio::spawn(async move {
        sender.send_transfer(TransferRequest { transfer_id: "t1".into(), files: vec![], text: Some("hello".into()) }).await
    });
    let offer = loop {
        match answered.events.recv().await.unwrap() {
            SessionEvent::Offer(o) => break o,
            _ => continue,
        }
    };
    assert_eq!(offer.text.as_deref(), Some("hello"));
    offer.accept(Some(vec![]), &[]).await.unwrap();
    let outcome = send.await.unwrap().unwrap();
    assert!(!outcome.declined);
    connected.session.close(None);
    tokio::time::timeout(Duration::from_secs(10), answered.session.closed()).await.expect("remote side sees the close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetches_turn_credentials_when_the_server_offers_them() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ferry_signal::Config {
        turn: Some(ferry_signal::TurnConfig {
            secret: b"s3cret".to_vec(),
            urls: vec!["turn:turn.example:3478".into()],
            ttl: Duration::from_secs(600),
        }),
        ..Default::default()
    };
    tokio::spawn(ferry_signal::serve(listener, config));
    let url = format!("ws://{addr}/v1/ws");
    let identity = RtcIdentity::generate();
    let info = ClientInfoOut {
        alias: "T".into(),
        device_model: None,
        device_type: None,
        token: "t".into(),
        public_key: identity.public_key().into(),
        nearby: None,
    };
    let (client, mut events) = SignalingClient::start(SignalingConfig::new(&url, info));
    loop {
        if let SignalingEvent::Hello { server, .. } = events.recv().await.unwrap() {
            assert!(server.unwrap().caps.iter().any(|c| c == "turn"));
            break;
        }
    }
    let turn = client.fetch_turn().await.unwrap().expect("credentials");
    assert_eq!(turn.ttl, 600);
    let server = &turn.ice_servers[0];
    assert_eq!(server.urls, vec!["turn:turn.example:3478".to_string()]);
    let me = client.client().unwrap().id;
    assert!(server.username.ends_with(&format!(":{me}")), "{}", server.username);
    assert!(!server.credential.is_empty());
}
