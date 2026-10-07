# Ferry architecture

> Fast, private, beautifully designed device-to-device sharing that works almost everywhere.

## 1. Principles

1. **Bytes go device → device.** No component stores file content in the cloud. The only server is a small, self-hostable signaling relay for browsers and remote mode.
2. **LocalSend devices just work.** Ferry speaks LocalSend's HTTP v2 exactly. Everything new is an *opt-in extension* that LocalSend peers never see.
3. **Identity is cryptographic, never claimed.** Trust comes from mutual TLS or transcript signatures, not from JSON fields (see `04-threat-model.md`).
4. **Heavy work never touches the UI thread.** Networking, hashing and disk I/O run on the Rust core's Tokio runtime; the UI receives throttled events.
5. **Bounded everything.** Memory per transfer is constant regardless of file size; every queue, body, table and timeout has a limit.
6. **Honest capabilities.** When an OS or browser can't do something, the UI says so and offers the closest working alternative.

## 2. Decision: one UI codebase (Tauri 2 + Vue 3), shared Rust core

The brief recommends Flutter for native apps **and** a separate TypeScript PWA,
with shared design tokens. We keep the Rust core and the TypeScript PWA, and
render the native apps with the **same Vue UI inside Tauri 2** instead of Flutter.

| | Flutter + separate PWA | **Tauri 2 + one Vue UI** (chosen) |
|---|---|---|
| UI codebases | Two (Dart + TS) implementing the same design system and motion twice | One. Native and web are the same components, tokens and motion. |
| Glass / blur / motion | `BackdropFilter` is costly; motion re-implemented per stack | Native CSS `backdrop-filter`, View Transitions, `linear()` springs; GPU-composited in WebView2/WKWebView/Chromium |
| Rust core binding | flutter_rust_bridge codegen + Dart isolates | Direct in-process Rust calls (Tauri commands + channels); no FFI codegen |
| Web as first-class | Separate app | The *same* app; only the transport adapter differs |
| Mobile maturity | Excellent plugin ecosystem; LocalSend's Kotlin/Swift reusable | Tauri 2 mobile is younger; share extensions, foreground service and pickers need our own Kotlin/Swift plugins |
| Binary size / RAM | ~25 to 40 MB | ~8 to 15 MB (system WebView) |
| Risk | Two UIs drift apart | Linux WebKitGTK blur performance varies → automatic reduced-transparency fallback |

The deciding factors are the design brief (one motion system and one visual
hierarchy everywhere, "first-class" web) and verifiability. **The decision is
reversible:** `ferry-core` has no UI dependency and exposes a plain async Rust
API plus a serde event stream, so a Flutter shell could bind it with
flutter_rust_bridge without touching the engine.

## 3. System overview

```
┌───────────────────────── Vue 3 UI (apps/app) ──────────────────────────────┐
│ design system (tokens, glass, motion) · views · stores                     │
│ Platform adapter ── native.ts (Tauri IPC)  |  web.ts (WebRTC + IndexedDB)  │
└──────────┬──────────────────────────────────────────┬──────────────────────┘
           │ commands + event channel                 │ WSS + DTLS data channels
┌──────────▼────────── Tauri shell (src-tauri) ──┐  ┌──▼─────── ferry-signal ──────────┐
│ tray · notifications · dialogs · clipboard ·   │  │ /v1/ws (LocalSend-compatible)    │
│ single instance · autostart · Explorer verb    │  │ + rooms, trickle ICE, TURN creds │
└──────────┬─────────────────────────────────────┘  └──────────────────────────────────┘
┌──────────▼──────────────── ferry-core (Rust, Tokio) ───────────────────────────────┐
│ Engine ─ identity · settings · trust store · history/inbox (SQLite)                │
│        ─ discovery (multicast, register, scan, favourites, manual) · pairing (QR)   │
│        ─ server: LocalSend v2 API + /api/ferry/v1 extensions (multi-session)        │
│        ─ receive: decisions, auto-accept policy, PIN, safe writer (.ferrypart)      │
│        ─ send: per-target sessions, group drops, resume, bounded concurrency        │
│        ─ browser share (plain-HTTP listener with tokens) · diagnostics · WebRTC     │
└──────────┬──────────────────────────────────────────────────────────────────────────┘
┌──────────▼─────── crates/localsend (vendored LocalSend core, Apache-2.0) ──────────┐
│ DTOs · cert/fingerprint crypto · pinned-TLS client · multicast sockets · discovery │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

### Repository layout

| Path | What |
|---|---|
| `crates/localsend` | Vendored LocalSend protocol core (modifications listed in `UPSTREAM.md`). |
| `crates/ferry-core` | The engine. No UI dependencies. |
| `crates/ferry-cli` | `ferry` binary: headless send/receive, browser links, private links, pairing. |
| `crates/ferry-signal` | Signaling server (a Dockerfile is included; no image is published). |
| `apps/app` | Vue 3 UI: the PWA build and the frontend embedded by Tauri. |
| `apps/app/src-tauri` | Tauri 2 shell for the desktop app. Android and iOS projects are Planned and not generated yet. |
| `tests/interop` | Separate Cargo workspace pinned to *unmodified* upstream LocalSend, used as the reference peer. |
| `crates/ferry-core/examples/bench.rs` | Throughput and memory benchmark (`cargo run --release -p ferry-core --example bench`). |

## 4. ferry-core

### Concurrency model

- One multi-threaded Tokio runtime owned by the `Engine`. The Tauri shell calls
  async engine methods from its own async runtime; nothing blocks the WebView thread.
- Disk I/O uses `tokio::fs` / `spawn_blocking` (256 KiB reads with up to 4 in
  flight, a 1 MiB write buffer); hashing is
  done inline on the I/O task (SHA-256 with SHA-NI runs at > 1.5 GB/s, faster than
  Wi-Fi and most disks).
- Every long operation takes a `CancellationToken`; dropping a transfer handle cancels it.
- Events go out on a `tokio::sync::broadcast` channel of `EngineEvent` (serde,
  tagged). Progress events are coalesced to ≤ 10 Hz per transfer so a 10 Gbit
  link cannot flood the UI.

### Peer identity

```rust
enum PeerIdentity { Verified { fingerprint: String }, Unverified, PlainHttp }
```

Carried by every incoming request and every discovered channel. Trust,
favourites, auto-accept and "My devices" key on `Verified` fingerprints only.

### Device table (replaces upstream's store)

- Key: verified fingerprint. Channels (address, port, protocol, last RTT, last seen) are attached only after a pinned HTTPS `/register` confirms them, or, for legacy plain-HTTP peers, into a *separate* unverified entry that can never shadow a verified one (fixes upstream's downgrade poisoning).
- Bounded (256 devices, 8 channels each), TTL-based `Lost` events (offline after 75 s without confirmation, removed after 10 min unless remembered: favourite, trusted or paired).
- Channel choice: fewest recent failures, then lowest measured RTT, among confirmed channels (a favourite's last known address is tried last); on failure the sender falls back to the next channel instead of failing.

### Receive pipeline

1. `prepare-upload` arrives → body read with a hard cap → validated (file count, name lengths, sizes, total vs free disk space).
2. Policy: `Verified` + trusted + auto-accept on → accept immediately; otherwise an `IncomingRequest` event goes to the UI (and an OS notification). Pending decisions time out (default 5 min).
3. Accepted files get tokens bound to session + peer. Multiple sessions may run concurrently. Admission is atomic and capped per peer (an IPv4 address or IPv6 /64) and in total: 3 waiting prompts per peer and 16 overall, 8 sessions per peer and 32 overall; excess requests get 409.
4. Each upload streams into `<dest>/<name>.ferrypart` opened with `create_new`, hashing as it writes, size-enforced. Files of 1 MiB or less that start at offset 0 are hashed in memory and written straight to a unique final name with `create_new`. Ferry peers may send `offset` to resume; a resumed transfer keeps the folder and checksums approved at the first accept (see `05-protocol.md`).
5. On completion: compare SHA-256 (sender-provided or Ferry hash exchange, when checksum verification is on) → fsync (files of 8 MiB or more) → Mark-of-the-Web on Windows → no-replace rename to a unique final name (hard link, or an exclusive-create copy where links are unsupported) → apply timestamps → history entry → `TransferCompleted` event.

### Send pipeline

1. Items are expanded lazily (folders walked on a blocking task, never the UI thread) into a manifest with relative paths.
2. One `OutgoingSession` per target device (group drops = N sessions sharing the manifest).
3. `prepare-upload` without pre-hashing (LocalSend allows `sha256: null`). If the peer is Ferry, the session is resumable and hashes are exchanged at end of file.
4. Uploads: small files concurrently (up to 8 in flight, the `parallel_files` setting), files of 1 MiB or more 2 at a time; a global semaphore caps in-flight streams across all sessions at 24 to protect the disk.
5. Network loss → `Reconnecting` state; the sender keeps probing the peer's known channels and re-discovers it by fingerprint (IP may have changed), then continues from the receiver's confirmed offsets.

## 5. Transport negotiation

| Pair | Preferred → fallback |
|---|---|
| Native ↔ native (same LAN) | Pinned HTTPS v2 (+ Ferry extensions when both are Ferry) over the best confirmed channel (IPv6 or IPv4) → other channels. A device reached through signaling is a separate `rtc:` entry; automatic fallback from LAN to WebRTC is Planned. |
| Native ↔ LocalSend | Pinned HTTPS v2 (or plain HTTP when that peer has encryption off, shown as "Not encrypted") |
| Browser ↔ browser | WebRTC data channel: host candidates (LAN) → STUN (server-reflexive) → TURN relay (shown as "Relayed", still end-to-end encrypted) |
| Browser ↔ native | (a) WebRTC via signaling when the native app is online; (b) LAN browser link served by the native app over HTTP |
| Remote (outside LAN) | WebRTC with link/QR room; TURN only if direct fails |

The connection in use is shown on the transfer card while it runs
(`Encrypted · Nearby · IPv6`, `Encrypted · Peer-to-peer · Direct`,
`Encrypted · Peer-to-peer · Relayed`). The browser leaves out Direct or Relayed
when it cannot tell which ICE candidate pair is in use; the native app reports
Direct in that case.

## 6. Storage layout

| Data | Where | Format |
|---|---|---|
| Identity (TLS cert + key, Ed25519 key) | `<data>/identity/` | PEM; private keys DPAPI-protected on Windows, 0600 elsewhere |
| Settings | `<data>/settings.json` | JSON, atomic write, versioned |
| Devices (with trust, favourite and paired flags), history (the Inbox is built from it), resume manifests | `<data>/ferry.db` | SQLite (WAL) |
| Partial downloads | next to their destination | `*.ferrypart` (+ a row in `ferry.db` for resumable transfers) |

For the desktop app `<data>` is Tauri's app data folder for `app.ferry.desktop`:
`%APPDATA%\app.ferry.desktop` · `~/Library/Application Support/app.ferry.desktop` ·
`$XDG_DATA_HOME/app.ferry.desktop`. The CLI keeps its own data in the `cli`
folder of the platform's data directory for Ferry. Mobile storage is Planned.
The PWA keeps the equivalent in IndexedDB (identity, settings, history, resume
records) and OPFS (received files).

## 7. Performance design

- Constant memory per transfer: 256 KiB reads with at most 4 in flight per stream (1 MiB read-ahead) and a 1 MiB write buffer.
- No pre-hashing; SHA-256 computed on the I/O path at both ends.
- HTTP/1.1 per stream with `TCP_NODELAY`; parallel streams instead of HTTP/2 (upstream measured HTTP/2 flow-control windows capping throughput).
- Many small files: concurrent uploads. A batched Ferry↔Ferry endpoint that streams many small files in one request is Planned.
- WebRTC: 64 KiB messages (16 KiB when the peer allows less) with `bufferedAmountLowThreshold` backpressure; receiver writes straight to disk (OPFS in the browser).
- Benchmarks (1 MB, 100 MB, 1 GB, 10,000 small files; 10 GB on request) run with `crates/ferry-core/examples/bench.rs`.

## 8. Testing strategy

| Layer | How |
|---|---|
| Unit | Sanitizer, unique naming, policy, rate limiters, progress math, DTO round trips. |
| Engine integration | Two engines in one process on loopback (`crates/ferry-core/tests`): send, receive, cancel, decline, partial accept, PIN, duplicate names, concurrent senders, group drop, checksum mismatch, resume after a dropped connection, restart resume, resume integrity, receive admission limits, browser links, pairing, WebRTC. |
| Desktop shell | `apps/app/src-tauri/tests` against Tauri's mock runtime: which received files the webview may preview. |
| Interop | `tests/interop` runs *unmodified* upstream LocalSend (pinned commit): upstream's own HTTP client sends to Ferry; upstream's server receives from Ferry. |
| Fault injection | A TCP proxy that drops/stalls connections mid-transfer to test reconnect and resume. |
| Web | Vitest for protocol and state. Playwright scripts drive real Chrome: browser↔browser and browser↔native CLI over WebRTC and the PWA checks run in CI; the desktop app scripts run by hand. |
| Visual | Playwright screenshots in light/dark/reduced-motion at phone, tablet and desktop widths (run by hand). |
| Manual / devices | The desktop app is checked by hand on Windows. The macOS and Linux installers are built by the release workflow but not tested by hand. Android and iOS builds are Planned. |
