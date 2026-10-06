//! Browser links: a plain browser downloads from and uploads to an engine.

mod common;

use common::*;
use ferry_core::SendItem;
use ferry_core::events::EngineEvent;
use ferry_core::model::Decision;
use localsend::reqwest;
use std::time::Duration;

/// `http://127.0.0.1:<port>/s/<token>` for a share URL on any address.
fn local(url: &str) -> String {
    let after = url.split_once("://").unwrap().1;
    let path_start = after.find("/s/").unwrap();
    let host_port = &after[..path_start];
    let port = host_port.rsplit(':').next().unwrap();
    format!("http://127.0.0.1:{port}{}", &after[path_start..])
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_downloads_shared_files() {
    let p = peer("Desk").await;
    let src = tempfile::tempdir().unwrap();
    let data = pattern(3_000_000, 31);
    let path = write_file(src.path(), "Holiday reel.mp4", &data);
    let share = p.engine.share_with_browsers(vec![SendItem::Path { path }], None).await.unwrap();
    assert_eq!(share.file_count, 1);
    let base = local(&share.urls[0]);
    let http = client();

    let page = http.get(&base).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.headers()["content-security-policy"].to_str().unwrap().contains("default-src 'none'"));
    assert_eq!(page.headers()["x-frame-options"], "DENY");

    let info: serde_json::Value = http.get(format!("{base}/api/info")).send().await.unwrap().json().await.unwrap();
    assert_eq!(info["kind"], "download");
    assert_eq!(info["device"]["alias"], "Desk");
    let id = info["files"][0]["id"].as_str().unwrap().to_string();

    let full = http.get(format!("{base}/api/files/{id}")).send().await.unwrap();
    assert_eq!(full.status(), 200);
    assert!(full.headers()["content-disposition"].to_str().unwrap().contains("Holiday reel.mp4"));
    assert_eq!(full.bytes().await.unwrap().as_ref(), data.as_slice());

    // Resumable browser downloads: byte ranges.
    let part = http.get(format!("{base}/api/files/{id}")).header("range", "bytes=1000-1999").send().await.unwrap();
    assert_eq!(part.status(), 206);
    assert_eq!(part.headers()["content-range"], format!("bytes 1000-1999/{}", data.len()));
    assert_eq!(part.bytes().await.unwrap().as_ref(), &data[1000..2000]);

    tokio::time::sleep(Duration::from_millis(100)).await;
    let listed = p.engine.browser_links();
    assert_eq!(listed[0].downloads, 1, "one complete download counted (ranges don't count)");
    assert!(listed[0].recent_clients.iter().any(|c| !c.is_empty()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_links_reject_wrong_tokens_rebinding_and_stopped_links() {
    let p = peer("Desk").await;
    let src = tempfile::tempdir().unwrap();
    let path = write_file(src.path(), "a.txt", b"secret");
    let share = p.engine.share_with_browsers(vec![SendItem::Path { path }], None).await.unwrap();
    let base = local(&share.urls[0]);
    let http = client();

    let wrong = base.rsplit_once('/').unwrap().0.to_string() + "/0123456789abcdef0123456789abcdef";
    assert_eq!(http.get(format!("{wrong}/api/info")).send().await.unwrap().status(), 404);

    // DNS rebinding: a page on evil.example resolving to our IP is refused.
    let rebind = http.get(format!("{base}/api/info")).header("host", "evil.example:1234").send().await.unwrap();
    assert_eq!(rebind.status(), 403);

    assert!(p.engine.stop_browser_link(&share.id));
    assert_eq!(http.get(format!("{base}/api/info")).send().await.unwrap().status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_link_pin() {
    let p = peer("Desk").await;
    let src = tempfile::tempdir().unwrap();
    let path = write_file(src.path(), "a.txt", b"pin protected");
    let share = p.engine.share_with_browsers(vec![SendItem::Path { path }], Some("2580".into())).await.unwrap();
    assert!(share.pin_required);
    let base = local(&share.urls[0]);
    let http = client();
    assert_eq!(http.get(format!("{base}/api/info")).send().await.unwrap().status(), 401);
    assert_eq!(http.get(format!("{base}/api/info")).header("x-ferry-pin", "0000").send().await.unwrap().status(), 401);
    let info: serde_json::Value =
        http.get(format!("{base}/api/info")).header("x-ferry-pin", "2580").send().await.unwrap().json().await.unwrap();
    let id = info["files"][0]["id"].as_str().unwrap();
    // <a download> links carry the PIN in the query.
    let body = http.get(format!("{base}/api/files/{id}?pin=2580")).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"pin protected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_uploads_go_through_the_accept_prompt() {
    let mut p = peer("Desk").await;
    p.auto_respond(Decision::accept_all());
    let link = p.engine.receive_from_browsers(None).await.unwrap();
    let base = local(&link.urls[0]);
    let http = client();
    let ua = "Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Mobile Safari/537.36";

    let info: serde_json::Value = http.get(format!("{base}/api/info")).header("user-agent", ua).send().await.unwrap().json().await.unwrap();
    assert_eq!(info["kind"], "upload");
    // Opening the link is reported to the UI right away.
    p.wait_event(
        T,
        |e| matches!(e, EngineEvent::BrowserShareUpdated { share } if share.recent_clients.iter().any(|c| c == "Chrome on Android")),
    )
    .await;

    let data = pattern(2_500_000, 32);
    let plan: serde_json::Value = http
        .post(format!("{base}/api/prepare"))
        .header("user-agent", ua)
        .json(&serde_json::json!({
            "files": [{ "id": "f0", "fileName": "Camera/IMG_0042.jpg", "size": data.len(), "fileType": "image/jpeg" }],
            "sender": "Maya"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = plan["sessionId"].as_str().unwrap();
    let token = plan["files"]["f0"].as_str().unwrap();
    let up = http
        .post(format!("{base}/api/upload?sessionId={session}&fileId=f0&token={token}"))
        .header("user-agent", ua)
        .body(data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(up.status(), 200);
    let received = p.wait_received(T).await;
    assert_eq!(received.peer.alias, "Maya (Chrome on Android)");
    assert!(!received.peer.verified, "browsers are never verified peers");
    assert_eq!(std::fs::read(p.saved("Camera/IMG_0042.jpg")).unwrap(), data);
    let listed = p.engine.browser_links();
    assert_eq!((listed[0].uploads, listed[0].active), (1, 0), "one finished upload, none in flight");

    // A message from the browser.
    let msg = http
        .post(format!("{base}/api/prepare"))
        .header("user-agent", ua)
        .json(&serde_json::json!({ "files": [{ "id": "m", "fileName": "message.txt", "size": 5, "fileType": "text/plain", "preview": "hello" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(msg.status(), 204);
    let event = p.wait_event(T, |e| matches!(e, EngineEvent::IncomingRequest { request } if request.text.is_some())).await;
    let EngineEvent::IncomingRequest { request } = event else { unreachable!() };
    assert_eq!(request.text.as_deref(), Some("hello"));

    // Download routes don't exist on an upload link.
    assert_eq!(http.get(format!("{base}/api/files/f0")).send().await.unwrap().status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_a_link_ends_downloads_in_progress() {
    let p = peer("Desk").await;
    let src = tempfile::tempdir().unwrap();
    let big = write_file(src.path(), "big.bin", &pattern(64 * 1024 * 1024, 3));
    let link = p.engine.share_with_browsers(vec![SendItem::Path { path: big }], None).await.unwrap();
    let base = local(&link.urls[0]);
    let http = client();
    let info: serde_json::Value = http.get(format!("{base}/api/info")).send().await.unwrap().json().await.unwrap();
    let id = info["files"][0]["id"].as_str().unwrap().to_string();

    let mut resp = http.get(format!("{base}/api/files/{id}")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let mut got = resp.chunk().await.unwrap().map(|c| c.len()).unwrap_or(0);
    assert!(p.engine.stop_browser_link(&link.id));

    // The rest of the body never arrives: the stream ends early or errors.
    let rest = tokio::time::timeout(T, async {
        loop {
            match resp.chunk().await {
                Ok(Some(c)) => got += c.len(),
                Ok(None) | Err(_) => return,
            }
        }
    })
    .await;
    assert!(rest.is_ok(), "download kept the connection open");
    assert!(got < 64 * 1024 * 1024, "the whole file was still delivered ({got} bytes)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_links_have_their_own_pin_not_the_devices() {
    // A device PIN guards LAN senders; a browser link is its own capability.
    let mut p = peer_with("Desk", |s| s.pin = Some("4321".into())).await;
    p.auto_respond(Decision::accept_all());
    let link = p.engine.receive_from_browsers(None).await.unwrap();
    let base = local(&link.urls[0]);
    let data = pattern(300_000, 9);
    let plan = client()
        .post(format!("{base}/api/prepare"))
        .json(&serde_json::json!({ "files": [{ "id": "f", "fileName": "scan.pdf", "size": data.len(), "fileType": "application/pdf" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(plan.status(), 200, "the device PIN must not block a link without its own PIN");
    let plan: serde_json::Value = plan.json().await.unwrap();
    let (session, token) = (plan["sessionId"].as_str().unwrap().to_string(), plan["files"]["f"].as_str().unwrap().to_string());
    let up =
        client().post(format!("{base}/api/upload?sessionId={session}&fileId=f&token={token}")).body(data.clone()).send().await.unwrap();
    assert_eq!(up.status(), 200);
    p.wait_received(T).await;
    assert_eq!(std::fs::read(p.saved("scan.pdf")).unwrap(), data);

    // A link with its own PIN asks for that one.
    let pinned = p.engine.receive_from_browsers(Some("7788".into())).await.unwrap();
    let base = local(&pinned.urls[0]);
    let without = client().get(format!("{base}/api/info")).send().await.unwrap();
    assert_eq!(without.status(), 401);
    let with = client().get(format!("{base}/api/info")).header("x-ferry-pin", "7788").send().await.unwrap();
    assert_eq!(with.status(), 200);
}
