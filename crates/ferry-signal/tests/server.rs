//! End-to-end tests: the real server on an ephemeral loopback port, driven
//! by real WebSocket clients (tokio-tungstenite) and raw HTTP/1.1 requests.
//!
//! Every client connects from 127.0.0.1, so they all share one IP group
//! ("nearby") unless they opt out with `ext.nearby: false` or a test trusts
//! the loopback address as a proxy and sends `X-Forwarded-For`.

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ferry_signal::{Config, Limits, TurnConfig, close};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha1::Sha1;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// Upper bound for anything the test expects to happen.
const STEP: Duration = Duration::from_secs(5);
/// How long a client listens to show that nothing arrives.
const QUIET: Duration = Duration::from_millis(300);

const ROOM_1: &str = "r:AAAAAAAAAAAAAAAAAAAAAA";
const ROOM_2: &str = "r:BBBBBBBBBBBBBBBBBBBBBB";

// ── Harness ─────────────────────────────────────────────────────────────

struct Server {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Server {
    async fn start(config: Config) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(ferry_signal::serve_with_shutdown(listener, config, async {
            let _ = stopped.await;
        }));
        Self { addr, stop, task }
    }

    async fn with_limits(limits: Limits) -> Self {
        Self::start(Config { limits, ..Config::default() }).await
    }

    /// Connects and reads the `HELLO`.
    async fn connect(&self, info: &Value) -> Client {
        self.connect_with(info, &[]).await
    }

    async fn connect_with(&self, info: &Value, headers: &[(&'static str, &str)]) -> Client {
        match self.try_connect(info, headers).await {
            Ok(client) => client,
            Err(err) => panic!("connection refused: {err}"),
        }
    }

    async fn try_connect(&self, info: &Value, headers: &[(&'static str, &str)]) -> Result<Client, WsError> {
        let d = URL_SAFE_NO_PAD.encode(info.to_string());
        let mut request = format!("ws://{}/v1/ws?d={d}", self.addr).into_client_request()?;
        for (name, value) in headers {
            request.headers_mut().append(*name, HeaderValue::from_str(value).unwrap());
        }
        let (ws, _) = timeout(STEP, connect_async(request)).await.expect("connect timed out")?;
        let mut client = Client { ws, id: String::new(), hello: Value::Null };
        client.hello = client.recv_type("HELLO").await;
        client.id = client.hello["client"]["id"].as_str().unwrap().to_owned();
        Ok(client)
    }

    /// The HTTP status of a refused WebSocket connection.
    async fn refused(&self, info: &Value, headers: &[(&'static str, &str)]) -> u16 {
        match self.try_connect(info, headers).await {
            Err(WsError::Http(response)) => response.status().as_u16(),
            Err(err) => panic!("unexpected error: {err}"),
            Ok(_) => panic!("connection unexpectedly accepted"),
        }
    }

    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> HttpResponse {
        http_get(self.addr, path, headers).await
    }

    /// Triggers shutdown and waits for `serve_with_shutdown` to return.
    async fn shutdown(self) -> Duration {
        let started = Instant::now();
        let _ = self.stop.send(());
        timeout(Duration::from_secs(10), self.task)
            .await
            .expect("server did not shut down")
            .expect("server task panicked")
            .expect("server returned an error");
        started.elapsed()
    }
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    id: String,
    hello: Value,
}

impl Client {
    async fn send(&mut self, message: Value) {
        self.send_text(&message.to_string()).await;
    }

    async fn send_text(&mut self, text: &str) {
        self.ws.send(Message::text(text)).await.unwrap();
    }

    /// Next message as JSON. Pings are answered by tungstenite and skipped.
    async fn recv(&mut self) -> Value {
        let next = async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    other => panic!("expected a message, got {other:?}"),
                }
            }
        };
        timeout(STEP, next).await.expect("timed out waiting for a message")
    }

    async fn recv_type(&mut self, ty: &str) -> Value {
        let message = self.recv().await;
        assert_eq!(message["type"], ty, "unexpected message: {message}");
        message
    }

    async fn recv_error(&mut self, code: u16) -> Value {
        let error = self.recv_type("ERROR").await;
        assert_eq!(error["code"], code, "unexpected error: {error}");
        error
    }

    /// Fails if a message (or a close) arrives within [`QUIET`].
    async fn assert_quiet(&mut self) {
        self.assert_quiet_for(QUIET).await;
    }

    async fn assert_quiet_for(&mut self, duration: Duration) {
        let next = async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    other => return other,
                }
            }
        };
        if let Ok(other) = timeout(duration, next).await {
            panic!("expected silence, got {other:?}");
        }
    }

    /// Reads up to the server's close frame and returns its code.
    async fn close_code(&mut self) -> u16 {
        let next = async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Close(frame))) => {
                        return frame.map_or(1005, |f| u16::from(f.code));
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    other => panic!("expected a close frame, got {other:?}"),
                }
            }
        };
        timeout(STEP, next).await.expect("timed out waiting for the close frame")
    }

    /// Closes cleanly and waits for the server to end the connection.
    async fn close(mut self) {
        let _ = self.ws.close(None).await;
        let _ = timeout(STEP, async { while self.ws.next().await.is_some() {} }).await;
    }

    async fn ping(&mut self) {
        self.send(json!({"type": "PING"})).await;
        self.recv_type("PONG").await;
    }
}

/// A LocalSend client (no `ext`).
fn legacy(alias: &str) -> Value {
    json!({"alias": alias, "version": "2.1", "deviceType": "desktop", "token": "t"})
}

/// A Ferry client that is visible to its IP group.
fn ferry(alias: &str) -> Value {
    json!({
        "alias": alias, "version": "2.2", "deviceType": "WEB", "token": "t",
        "ext": {"v": 1, "caps": ["rooms", "trickle", "ferry-dc"], "key": "AAAA"}
    })
}

/// A Ferry client that only takes part in rooms (`ext.nearby: false`).
fn hidden(alias: &str) -> Value {
    let mut info = ferry(alias);
    info["ext"]["nearby"] = json!(false);
    info
}

fn trusting_loopback_proxy() -> Config {
    Config { trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()], ..Config::default() }
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
    }
}

/// A raw HTTP/1.1 GET; reads exactly one response (by `Content-Length`).
async fn http_get(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> HttpResponse {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();

    let read = async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(response) = parse_response(&buf) {
                return response;
            }
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed mid-response: {:?}", String::from_utf8_lossy(&buf));
            buf.extend_from_slice(&chunk[..n]);
        }
    };
    timeout(STEP, read).await.expect("timed out waiting for the HTTP response")
}

fn parse_response(buf: &[u8]) -> Option<HttpResponse> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..end]).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split(' ').nth(1)?.parse().unwrap();
    let headers: Vec<(String, String)> =
        lines.filter_map(|line| line.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
    let len: usize =
        headers.iter().find(|(k, _)| k == "content-length").map(|(_, v)| v.parse().unwrap()).expect("response without Content-Length");
    let body = buf.get(end + 4..end + 4 + len)?;
    Some(HttpResponse { status, headers, body: String::from_utf8(body.to_vec()).unwrap() })
}

const UPGRADE_HEADERS: [(&str, &str); 4] = [
    ("Connection", "Upgrade"),
    ("Upgrade", "websocket"),
    ("Sec-WebSocket-Version", "13"),
    ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
];

// ── Presence and rooms ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nearby_clients_see_each_other_like_localsend() {
    let server = Server::start(Config::default()).await;
    let mut a = server.connect(&legacy("A")).await;
    assert_eq!(a.hello["client"]["alias"], "A");
    assert_eq!(a.hello["client"]["deviceType"], "DESKTOP");
    assert_eq!(a.hello["peers"], json!([]));
    assert!(a.hello.get("server").is_none(), "legacy HELLO must not change");

    let mut b = server.connect(&legacy("B")).await;
    assert_eq!(b.hello["peers"][0]["id"], a.id.as_str());
    let join = a.recv_type("JOIN").await;
    assert_eq!(join["peer"]["id"], b.id.as_str());
    assert_eq!(join["peer"]["alias"], "B");

    b.send(json!({"type": "UPDATE", "info": legacy("B2")})).await;
    let update = a.recv_type("UPDATE").await;
    assert_eq!(update["peer"]["id"], b.id.as_str());
    assert_eq!(update["peer"]["alias"], "B2");
    b.assert_quiet().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_join_the_same_room_and_see_each_other() {
    let server = Server::start(Config::default()).await;
    let mut a = server.connect(&hidden("A")).await;
    assert_eq!(a.hello["server"], json!({"v": 1, "caps": ["rooms", "trickle"]}));
    let mut b = server.connect(&hidden("B")).await;
    // Room-only clients are invisible to their IP group.
    assert_eq!(b.hello["peers"], json!([]));
    a.assert_quiet().await;

    a.send(json!({"type": "ROOM_JOIN", "room": ROOM_1})).await;
    let hello = a.recv_type("ROOM_HELLO").await;
    assert_eq!(hello, json!({"type": "ROOM_HELLO", "room": ROOM_1, "peers": []}));

    b.send(json!({"type": "ROOM_JOIN", "room": ROOM_1})).await;
    let hello = b.recv_type("ROOM_HELLO").await;
    assert_eq!(hello["peers"][0]["id"], a.id.as_str());
    assert_eq!(hello["peers"][0]["ext"]["key"], "AAAA");
    let joined = a.recv_type("ROOM_PEER_JOINED").await;
    assert_eq!(joined["room"], ROOM_1);
    assert_eq!(joined["peer"]["id"], b.id.as_str());

    // A short-code room works the same way.
    a.send(json!({"type": "ROOM_JOIN", "room": "c:123456"})).await;
    a.recv_type("ROOM_HELLO").await;
    b.send(json!({"type": "ROOM_JOIN", "room": "c:123456"})).await;
    assert_eq!(b.recv_type("ROOM_HELLO").await["peers"][0]["id"], a.id.as_str());
    assert_eq!(a.recv_type("ROOM_PEER_JOINED").await["room"], "c:123456");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signaling_is_relayed_only_to_the_addressed_peer() {
    let server = Server::start(Config::default()).await;
    let mut a = server.connect(&ferry("A")).await;
    let mut b = server.connect(&ferry("B")).await;
    let mut c = server.connect(&ferry("C")).await;
    a.recv_type("JOIN").await;
    a.recv_type("JOIN").await;
    b.recv_type("JOIN").await;

    a.send(json!({"type": "OFFER", "target": b.id, "sessionId": "s1", "sdp": "eJzLSM3JyQcABiwCFQ"})).await;
    let offer = b.recv_type("OFFER").await;
    assert_eq!(offer["peer"]["id"], a.id.as_str());
    assert_eq!(offer["sessionId"], "s1");
    assert_eq!(offer["sdp"], "eJzLSM3JyQcABiwCFQ");

    b.send(json!({"type": "ANSWER", "target": a.id, "sessionId": "s1", "sdp": "answer"})).await;
    let answer = a.recv_type("ANSWER").await;
    assert_eq!(answer["peer"]["id"], b.id.as_str());
    assert_eq!(answer["sdp"], "answer");

    // Browsers trickle `RTCIceCandidateInit` objects; `null` ends the list.
    let candidate = json!({
        "candidate": "candidate:1 1 udp 2122260223 192.0.2.1 54321 typ host",
        "sdpMid": "0", "sdpMLineIndex": 0, "usernameFragment": "abcd"
    });
    a.send(json!({"type": "ICE", "target": b.id, "sessionId": "s1", "candidate": candidate})).await;
    let ice = b.recv_type("ICE").await;
    assert_eq!(ice["peer"]["id"], a.id.as_str());
    assert_eq!(ice["candidate"], candidate);
    a.send(json!({"type": "ICE", "target": b.id, "sessionId": "s1", "candidate": null})).await;
    assert_eq!(b.recv_type("ICE").await["candidate"], Value::Null);
    a.send(json!({"type": "CANCEL", "target": b.id, "sessionId": "s1"})).await;
    assert_eq!(b.recv_type("CANCEL").await["sessionId"], "s1");

    // Unknown targets (and yourself) are answered with a correlated 404.
    for target in [uuid::Uuid::new_v4().to_string(), a.id.clone()] {
        a.send(json!({"type": "OFFER", "target": target, "sessionId": "s2", "sdp": "x"})).await;
        let error = a.recv_error(404).await;
        assert_eq!(error["sessionId"], "s2");
    }

    c.assert_quiet().await;
    b.assert_quiet().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rooms_are_isolated() {
    let server = Server::start(Config::default()).await;
    let mut a = server.connect(&hidden("A")).await;
    let mut b = server.connect(&hidden("B")).await;
    let mut c = server.connect(&hidden("C")).await;
    let mut d = server.connect(&hidden("D")).await;
    for (client, room) in [(&mut a, ROOM_1), (&mut b, ROOM_1), (&mut c, ROOM_2), (&mut d, ROOM_2)] {
        client.send(json!({"type": "ROOM_JOIN", "room": room})).await;
        client.recv_type("ROOM_HELLO").await;
    }
    assert_eq!(a.recv_type("ROOM_PEER_JOINED").await["peer"]["id"], b.id.as_str());
    assert_eq!(c.recv_type("ROOM_PEER_JOINED").await["peer"]["id"], d.id.as_str());

    a.send(json!({"type": "OFFER", "target": c.id, "sessionId": "s", "sdp": "x"})).await;
    a.recv_error(404).await;
    a.send(json!({"type": "OFFER", "target": b.id, "sessionId": "s", "sdp": "x"})).await;
    b.recv_type("OFFER").await;
    c.assert_quiet().await;
    d.assert_quiet().await;

    // Leaving the room ends the route.
    b.send(json!({"type": "ROOM_LEAVE", "room": ROOM_1})).await;
    let left = a.recv_type("ROOM_PEER_LEFT").await;
    assert_eq!(left, json!({"type": "ROOM_PEER_LEFT", "room": ROOM_1, "peerId": b.id}));
    a.send(json!({"type": "OFFER", "target": b.id, "sessionId": "s", "sdp": "x"})).await;
    a.recv_error(404).await;
    b.assert_quiet().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ip_groups_are_isolated_and_forwarded_for_needs_a_trusted_proxy() {
    let server = Server::start(trusting_loopback_proxy()).await;
    let home = [("X-Forwarded-For", "203.0.113.1")];
    let mut x1 = server.connect_with(&legacy("X1"), &home).await;
    let x2 = server.connect_with(&legacy("X2"), &home).await;
    assert_eq!(x2.hello["peers"][0]["id"], x1.id.as_str());
    x1.recv_type("JOIN").await;

    let mut y = server.connect_with(&legacy("Y"), &[("X-Forwarded-For", "198.51.100.7")]).await;
    assert_eq!(y.hello["peers"], json!([]));
    y.send(json!({"type": "OFFER", "target": x1.id, "sessionId": "s", "sdp": "x"})).await;
    y.recv_error(404).await;
    x1.assert_quiet().await;

    // Without a trusted proxy the header is ignored: both are 127.0.0.1.
    let server = Server::start(Config::default()).await;
    let mut p = server.connect_with(&legacy("P"), &[("X-Forwarded-For", "203.0.113.1")]).await;
    let q = server.connect_with(&legacy("Q"), &[("X-Forwarded-For", "198.51.100.7")]).await;
    assert_eq!(q.hello["peers"][0]["id"], p.id.as_str());
    p.recv_type("JOIN").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn departures_are_announced() {
    let server = Server::start(Config::default()).await;
    let mut a = server.connect(&legacy("A")).await;
    let b = server.connect(&legacy("B")).await;
    a.recv_type("JOIN").await;
    let b_id = b.id.clone();
    b.close().await;
    assert_eq!(a.recv_type("LEFT").await, json!({"type": "LEFT", "peerId": b_id}));

    let mut c = server.connect(&hidden("C")).await;
    let mut d = server.connect(&hidden("D")).await;
    c.send(json!({"type": "ROOM_JOIN", "room": ROOM_1})).await;
    c.recv_type("ROOM_HELLO").await;
    d.send(json!({"type": "ROOM_JOIN", "room": ROOM_1})).await;
    d.recv_type("ROOM_HELLO").await;
    c.recv_type("ROOM_PEER_JOINED").await;
    // An abrupt disconnect (no close handshake) is announced too.
    let d_id = d.id.clone();
    drop(d);
    let left = c.recv_type("ROOM_PEER_LEFT").await;
    assert_eq!(left["peerId"], d_id.as_str());
    a.assert_quiet().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_clients_get_no_extensions() {
    let server = Server::start(Config::default()).await;
    let mut old = server.connect(&legacy("Old")).await;
    let mut new = server.connect(&ferry("New")).await;
    old.recv_type("JOIN").await;

    for message in [
        json!({"type": "ROOM_JOIN", "room": ROOM_1}),
        json!({"type": "ICE", "target": new.id, "sessionId": "s", "candidate": null}),
        json!({"type": "PING"}),
    ] {
        old.send(message).await;
        old.recv_error(403).await;
    }
    new.send(json!({"type": "ICE", "target": old.id, "sessionId": "s", "candidate": null})).await;
    let error = new.recv_error(403).await;
    assert_eq!(error["message"], "target does not support this message");
    // The upstream message set still works between them.
    new.send(json!({"type": "OFFER", "target": old.id, "sessionId": "s", "sdp": "x"})).await;
    assert_eq!(old.recv_type("OFFER").await["peer"]["id"], new.id.as_str());
}

// ── Abuse handling ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_messages_are_rejected_without_crashing() {
    let server = Server::with_limits(Limits { max_violations: 5, ..Limits::default() }).await;
    let mut a = server.connect(&ferry("A")).await;

    for (text, message) in [
        ("{not json", "invalid JSON"),
        (r#"{"no":"type"}"#, "missing message type"),
        (r#"{"type":"NOPE"}"#, "unknown message type"),
        (r#"{"type":"OFFER","target":"not-a-uuid","sessionId":"s","sdp":"x"}"#, "invalid message"),
    ] {
        a.send_text(text).await;
        assert_eq!(a.recv_error(400).await["message"], message);
        a.ping().await; // still connected
    }
    // Empty frames are LocalSend keep-alives, not violations.
    a.send_text("").await;
    a.ping().await;

    // The fifth violation closes the connection.
    a.ws.send(Message::binary(vec![1, 2, 3])).await.unwrap();
    a.recv_error(400).await;
    assert_eq!(a.close_code().await, close::POLICY_VIOLATION);

    // Everyone else is unaffected.
    let mut b = server.connect(&ferry("B")).await;
    b.ping().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_messages_are_rejected() {
    let server = Server::start(Config::default()).await;
    let mut a = server.connect(&ferry("A")).await;
    let mut b = server.connect(&ferry("B")).await;
    a.recv_type("JOIN").await;

    // Oversized fields inside a valid frame: an error, the connection stays.
    let sdp = "x".repeat(50 * 1024);
    a.send(json!({"type": "OFFER", "target": b.id, "sessionId": "s", "sdp": sdp})).await;
    let error = a.recv_error(413).await;
    assert_eq!(error["sessionId"], "s");
    let candidate = json!({"candidate": "x".repeat(5000)});
    a.send(json!({"type": "ICE", "target": b.id, "sessionId": "s", "candidate": candidate})).await;
    a.recv_error(413).await;
    a.ping().await;
    b.assert_quiet().await;

    // A frame over 64 KiB: error, then close 1009. Send and read
    // concurrently, since the server stops reading the frame early.
    let (mut sink, mut stream) = a.ws.split();
    let sender = tokio::spawn(async move {
        let _ = sink.send(Message::text("x".repeat(70 * 1024))).await;
        sink
    });
    let read = async {
        let mut seen = Vec::new();
        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Text(text) => seen.push(serde_json::from_str::<Value>(&text).unwrap()),
                Message::Close(frame) => return (seen, frame.map(|f| u16::from(f.code))),
                _ => {}
            }
        }
        (seen, None)
    };
    let (seen, code) = timeout(STEP, read).await.expect("no close frame");
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0]["code"], 413);
    assert_eq!(code, Some(close::MESSAGE_TOO_BIG));
    let _ = sender.await;

    // B saw A leave; the server keeps serving.
    b.recv_type("LEFT").await;
    let mut c = server.connect(&ferry("C")).await;
    c.ping().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_ip_connection_limit() {
    let server = Server::start(Config {
        limits: Limits { max_conns_per_group: 2, conn_attempt_burst: 1000.0, ..Limits::default() },
        ..trusting_loopback_proxy()
    })
    .await;
    let a = server.connect(&legacy("A")).await;
    let _b = server.connect(&legacy("B")).await;
    assert_eq!(server.refused(&legacy("C"), &[]).await, 429);
    // Another network is not affected.
    let elsewhere = [("X-Forwarded-For", "198.51.100.7")];
    let _c = server.connect_with(&legacy("C"), &elsewhere).await;

    // Closing a connection frees its slot.
    a.close().await;
    let deadline = Instant::now() + STEP;
    loop {
        match server.try_connect(&legacy("D"), &[]).await {
            Ok(_) => break,
            Err(WsError::Http(r)) if r.status() == 429 && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(err) => panic!("slot was not released: {err}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn total_connection_limit() {
    let server = Server::with_limits(Limits { max_conns: 2, ..Limits::default() }).await;
    let _a = server.connect(&legacy("A")).await;
    let _b = server.connect(&legacy("B")).await;
    assert_eq!(server.refused(&legacy("C"), &[]).await, 503);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_rate_limit_per_ip() {
    let server = Server::with_limits(Limits { conn_attempts_per_sec: 0.0, conn_attempt_burst: 2.0, ..Limits::default() }).await;
    let _a = server.connect(&legacy("A")).await;
    let _b = server.connect(&legacy("B")).await;
    assert_eq!(server.refused(&legacy("C"), &[]).await, 429);
    let response = server.get("/v1/turn?peer=x", &[]).await;
    // TURN is off: that answer comes before the limiter.
    assert_eq!(response.status, 404);
    assert_eq!(server.get("/healthz", &[]).await.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_rate_limit() {
    let server = Server::with_limits(Limits { frames_per_sec: 0.0, frame_burst: 5.0, ..Limits::default() }).await;
    let mut a = server.connect(&ferry("A")).await;
    for _ in 0..5 {
        a.ping().await;
    }
    a.send(json!({"type": "PING"})).await;
    assert_eq!(a.recv_error(429).await["message"], "rate limit exceeded");
    assert_eq!(a.close_code().await, close::RATE_LIMITED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_caps() {
    let server = Server::with_limits(Limits { max_room_members: 2, max_rooms_per_conn: 1, ..Limits::default() }).await;
    let mut a = server.connect(&hidden("A")).await;
    let mut b = server.connect(&hidden("B")).await;
    let mut c = server.connect(&hidden("C")).await;
    for client in [&mut a, &mut b] {
        client.send(json!({"type": "ROOM_JOIN", "room": ROOM_1})).await;
        client.recv_type("ROOM_HELLO").await;
    }
    a.recv_type("ROOM_PEER_JOINED").await;

    c.send(json!({"type": "ROOM_JOIN", "room": ROOM_1})).await;
    let full = c.recv_error(409).await;
    assert_eq!(full["room"], ROOM_1);
    a.send(json!({"type": "ROOM_JOIN", "room": ROOM_2})).await;
    assert_eq!(a.recv_error(403).await["message"], "too many rooms");
    // Rejoining a room you are in just repeats the ROOM_HELLO.
    a.send(json!({"type": "ROOM_JOIN", "room": ROOM_1})).await;
    assert_eq!(a.recv_type("ROOM_HELLO").await["peers"][0]["id"], b.id.as_str());

    for room in ["", "x:123456", "c:12345", "r:short", "r:AAAAAAAAAAAAAAAAAAAAA="] {
        c.send(json!({"type": "ROOM_JOIN", "room": room})).await;
        assert_eq!(c.recv_error(400).await["message"], "invalid room id");
    }
    b.assert_quiet().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_code_joins_are_limited_per_network() {
    let server = Server::with_limits(Limits { code_joins_per_minute: 2, max_rooms_per_conn: 8, ..Limits::default() }).await;
    let mut a = server.connect(&hidden("A")).await;
    for code in ["c:000001", "c:000002"] {
        a.send(json!({"type": "ROOM_JOIN", "room": code})).await;
        a.recv_type("ROOM_HELLO").await;
    }
    a.send(json!({"type": "ROOM_JOIN", "room": "c:000003"})).await;
    assert_eq!(a.recv_error(429).await["room"], "c:000003");

    // Reconnecting does not buy more guesses; link rooms are not affected.
    let mut b = server.connect(&hidden("B")).await;
    b.send(json!({"type": "ROOM_JOIN", "room": "c:000004"})).await;
    b.recv_error(429).await;
    b.send(json!({"type": "ROOM_JOIN", "room": ROOM_1})).await;
    b.recv_type("ROOM_HELLO").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_connections_are_closed() {
    let server = Server::with_limits(Limits {
        idle_timeout: Duration::from_millis(300),
        ping_interval: Duration::from_secs(60),
        ..Limits::default()
    })
    .await;
    let mut a = server.connect(&ferry("A")).await;
    assert_eq!(a.close_code().await, close::IDLE_TIMEOUT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_pings_keep_live_connections_open() {
    let server = Server::with_limits(Limits {
        idle_timeout: Duration::from_millis(400),
        ping_interval: Duration::from_millis(100),
        ..Limits::default()
    })
    .await;
    let mut a = server.connect(&ferry("A")).await;
    // tungstenite answers the server's pings while we read.
    a.assert_quiet_for(Duration::from_millis(1200)).await;
    a.ping().await;
}

// ── HTTP ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_connect_requests_are_rejected() {
    let server = Server::start(Config::default()).await;
    let ws = |d: &str| format!("/v1/ws?d={d}");
    let encode = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    let long_alias = legacy(&"a".repeat(65));
    let cases = [
        ("/v1/ws".to_owned(), 400, "missing d parameter"),
        (ws("!!!!"), 400, "d is not valid base64url"),
        (ws(&URL_SAFE_NO_PAD.encode("[1,2]")), 400, "d is not a valid client info"),
        (ws(&encode(&json!({"alias": "a"}))), 400, "d is not a valid client info"),
        (ws(&encode(&long_alias)), 413, "alias too long"),
        (ws(&"A".repeat(5000)), 413, "d parameter too large"),
    ];
    for (path, status, message) in cases {
        let response = server.get(&path, &UPGRADE_HEADERS).await;
        assert_eq!(response.status, status, "{path}");
        assert_eq!(response.header("content-type"), Some("application/json"));
        assert_eq!(response.json(), json!({"type": "ERROR", "code": status, "message": message}));
    }
    // A plain GET (no upgrade) is refused with a JSON error too.
    let response = server.get(&ws(&encode(&legacy("A"))), &[]).await;
    assert!((400..500).contains(&response.status));
    assert_eq!(response.json()["type"], "ERROR");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origin_allowlist() {
    let server = Server::start(Config { allowed_origins: Some(vec!["https://app.example".to_owned()]), ..Config::default() }).await;
    let evil = [("Origin", "https://evil.example")];
    assert_eq!(server.refused(&ferry("A"), &evil).await, 403);
    server.connect_with(&ferry("A"), &[("Origin", "https://app.example")]).await;
    // Native clients send no Origin.
    server.connect(&ferry("B")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_and_unknown_routes() {
    let server = Server::start(Config::default()).await;
    let health = server.get("/healthz", &[]).await;
    assert_eq!(health.status, 200);
    assert_eq!(health.body, "ok");
    let missing = server.get("/v1/files", &[]).await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json(), json!({"type": "ERROR", "code": 404, "message": "not found"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_is_off_by_default() {
    let server = Server::start(Config::default()).await;
    let a = server.connect(&ferry("A")).await;
    assert_eq!(a.hello["server"]["caps"], json!(["rooms", "trickle"]));
    let response = server.get(&format!("/v1/turn?peer={}", a.id), &[]).await;
    assert_eq!(response.status, 404);
    assert_eq!(response.json()["message"], "TURN is not configured");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_credentials_are_short_lived_hmacs() {
    let secret = b"coturn-static-auth-secret";
    let urls = vec!["turn:turn.example:3478?transport=udp".to_owned(), "turns:turn.example:5349".to_owned()];
    let server = Server::start(Config {
        turn: Some(TurnConfig { secret: secret.to_vec(), urls: urls.clone(), ttl: Duration::from_secs(600) }),
        ..trusting_loopback_proxy()
    })
    .await;
    let home = [("X-Forwarded-For", "203.0.113.1")];
    let a = server.connect_with(&ferry("A"), &home).await;
    assert_eq!(a.hello["server"]["caps"], json!(["rooms", "trickle", "turn"]));

    let path = format!("/v1/turn?peer={}", a.id);
    let response = server.get(&path, &home).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.header("cache-control"), Some("no-store"));
    let body = response.json();
    assert_eq!(body["ttl"], 600);
    let server_entry = &body["iceServers"][0];
    assert_eq!(server_entry["urls"], json!(urls));
    let username = server_entry["username"].as_str().unwrap();
    let (expiry, peer) = username.split_once(':').unwrap();
    assert_eq!(peer, a.id);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let expiry: u64 = expiry.parse().unwrap();
    assert!((now + 590..=now + 610).contains(&expiry), "expiry {expiry}, now {now}");
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).unwrap();
    mac.update(username.as_bytes());
    assert_eq!(server_entry["credential"], STANDARD.encode(mac.finalize().into_bytes()));

    // Browsers fetch it cross-origin.
    let mut headers = home.to_vec();
    headers.push(("Origin", "https://app.example"));
    let response = server.get(&path, &headers).await;
    assert_eq!(response.header("access-control-allow-origin"), Some("*"));

    // Only for a connected client, asked from that client's network.
    let elsewhere = [("X-Forwarded-For", "198.51.100.7")];
    assert_eq!(server.get(&path, &elsewhere).await.status, 403);
    let unknown = format!("/v1/turn?peer={}", uuid::Uuid::new_v4());
    assert_eq!(server.get(&unknown, &home).await.status, 403);
    assert_eq!(server.get("/v1/turn?peer=nope", &home).await.status, 400);
    assert_eq!(server.get("/v1/turn", &home).await.status, 400);
}

// ── Shutdown ────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_shutdown_closes_websockets() {
    let server = Server::start(Config::default()).await;
    let addr = server.addr;
    let mut a = server.connect(&legacy("A")).await;
    let mut b = server.connect(&ferry("B")).await;
    let mut c = server.connect(&hidden("C")).await;
    a.recv_type("JOIN").await;
    for client in [&mut b, &mut c] {
        client.send(json!({"type": "ROOM_JOIN", "room": ROOM_1})).await;
        client.recv_type("ROOM_HELLO").await;
    }
    b.recv_type("ROOM_PEER_JOINED").await;

    let shutdown = tokio::spawn(server.shutdown());
    // Everyone is told the server is going away (no LEFT storm first).
    for mut client in [a, b, c] {
        assert_eq!(client.close_code().await, close::GOING_AWAY);
        client.close().await;
    }
    let took = shutdown.await.unwrap();
    assert!(took < Duration::from_millis(1500), "shutdown took {took:?}");

    // The listener is gone.
    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_does_not_wait_for_unresponsive_clients() {
    let server = Server::start(Config::default()).await;
    // Never reads again, so it never completes the close handshake.
    let _stuck = server.connect(&ferry("A")).await;
    let took = server.shutdown().await;
    assert!(took < Duration::from_secs(6), "shutdown took {took:?}");
}
