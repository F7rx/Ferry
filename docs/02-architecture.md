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
│ tray · notifications · dialogs · deep links ·  │  │ /v1/ws (LocalSend-compatible)    │
│ single instance · autostart · share targets    │  │ + rooms, trickle ICE, TURN creds │
└──────────┬─────────────────────────────────────┘  └──────────────────────────────────┘
┌──────────▼──────────────── ferry-core (Rust, Tokio) ───────────────────────────────┐
│ Engine ─ identity · settings · trust store · history/inbox (SQLite)                │
│        ─ discovery (multicast, register, scan, favourites, manual, QR)              │
│        ─ server: LocalSend v2 API + /api/ferry/v1 extensions (multi-session)        │
│        ─ receive: decisions, auto-accept policy, PIN, safe writer (.ferrypart)      │
│        ─ send: per-target sessions, group drops, resume, adaptive concurrency       │
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
| `crates/ferry-cli` | `ferry` binary: headless send/receive, diagnostics, benchmarks. |
| `crates/ferry-signal` | Signaling server (Docker image). |
| `apps/app` | Vue 3 UI: the PWA build and the frontend embedded by Tauri. |
| `apps/app/src-tauri` | Tauri 2 shell (desktop now; Android/iOS targets). |
| `tests/interop` | Separate Cargo workspace pinned to *unmodified* upstream LocalSend, used as the reference peer. |
| `crates/ferry-core/examples/bench.rs` | Throughput and memory benchmark (`cargo run --release -p ferry-core --example bench`). |

## 4. ferry-core

### Concurrency model

- One multi-threaded Tokio runtime owned by the `Engine`. The Tauri shell calls
  async engine methods from its own async runtime; nothing blocks the WebView thread.
- Disk I/O uses `tokio::fs` / `spawn_blocking` with 1 MiB buffers; hashing is
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
- Bounded (256 devices, 8 channels each), TTL-based `Lost` events (online → stale after 30 s without confirmation, removed after 10 min unless favourite/trusted).
- Channel choice: lowest measured RTT among confirmed channels; on failure the sender falls back to the next channel instead of failing.

### Receive pipeline

1. `prepare-upload` arrives → body read with a hard cap → validated (file count, name lengths, sizes, total vs free disk space).
2. Policy: `Verified` + trusted + auto-accept on → accept immediately; otherwise an `IncomingRequest` event goes to the UI (and an OS notification). Pending decisions time out (default 5 min).
3. Accepted files get tokens bound to session + peer. Multiple sessions may run concurrently (caps: 4 active per peer, 16 total).
4. Each upload streams into `<dest>/<name>.ferrypart` opened with `create_new`, hashing as it writes, size-enforced. Ferry peers may send `offset` to resume.
5. On completion: compare SHA-256 (sender-provided or Ferry hash exchange) → fsync → no-replace rename to a unique final name → apply timestamps → history entry → `TransferCompleted` event.

### Send pipeline

1. Items are expanded lazily (folders walked on a blocking task, never the UI thread) into a manifest with relative paths.
2. One `OutgoingSession` per target device (group drops = N sessions sharing the manifest).
3. `prepare-upload` without pre-hashing (LocalSend allows `sha256: null`). If the peer is Ferry, the session is resumable and hashes are exchanged at end of file.
4. Uploads: small files concurrently (up to 8 in flight), large files 2 at a time; a global semaphore caps total in-flight streams across all sessions to protect the disk.
5. Network loss → `Reconnecting` state; the sender keeps probing the peer's known channels and re-discovers it by fingerprint (IP may have changed), then continues from the receiver's confirmed offsets.

## 5. Transport negotiation

| Pair | Preferred → fallback |
|---|---|
| Native ↔ native (same LAN) | Pinned HTTPS v2 (+ Ferry extensions when both are Ferry) over the lowest-RTT confirmed channel (IPv6 or IPv4) → other channels → WebRTC via signaling (if both online) |
| Native ↔ LocalSend | Pinned HTTPS v2 (or plain HTTP when that peer has encryption off, shown as "Not encrypted") |
| Browser ↔ browser | WebRTC data channel: host candidates (LAN) → STUN (server-reflexive) → TURN relay (shown as "Relayed", still end-to-end encrypted) |
| Browser ↔ native | (a) WebRTC via signaling when the native app is online; (b) LAN browser link served by the native app over HTTP |
| Remote (outside LAN) | WebRTC with link/QR room; TURN only if direct fails |

The connection actually in use is always visible on the transfer card
(`Nearby · Encrypted · IPv6`, `Remote · Direct`, `Remote · Relayed`).

## 6. Storage layout

| Data | Where | Format |
|---|---|---|
| Identity (TLS cert + key, Ed25519 key) | `<data>/identity/` | PEM; private keys DPAPI-protected on Windows, 0600 elsewhere |
| Settings | `<data>/settings.json` | JSON, atomic write, versioned |
| Devices, trust, history, inbox, resume manifests | `<data>/ferry.db` | SQLite (WAL) |
| Partial downloads | next to their destination | `*.ferrypart` + row in `ferry.db` |

`<data>` = `%APPDATA%\Ferry` · `~/Library/Application Support/Ferry` ·
`$XDG_DATA_HOME/ferry` · app sandbox on mobile. The PWA keeps the equivalent in
IndexedDB/OPFS.

## 7. Performance design

- Constant memory per transfer: 1 MiB read buffers, at most 4 chunks in flight per stream.
- No pre-hashing; SHA-256 computed on the I/O path at both ends.
- HTTP/1.1 per stream with `TCP_NODELAY`; parallel streams instead of HTTP/2 (upstream measured HTTP/2 flow-control windows capping throughput).
- Many small files: concurrent uploads, then (Ferry↔Ferry) a batched upload endpoint that streams many small files in one request.
- WebRTC: 64 KiB messages with `bufferedAmountLowThreshold` backpressure; receiver writes straight to disk.
- Benchmarks (1 MB, 100 MB, 1 GB, 10 GB, 10,000 small files) run with `crates/ferry-core/examples/bench.rs`.

## 8. Testing strategy

| Layer | How |
|---|---|
| Unit | Sanitizer, unique naming, policy, rate limiters, progress math, DTO round trips. |
| Engine integration | Two engines in one process on loopback: send, receive, cancel, decline, PIN, checksum mismatch, resume after dropped connection, restart resume, group drop, 10k files, malicious names. |
| Interop | `tests/interop` runs *unmodified* upstream LocalSend (pinned commit): the upstream CLI sends to Ferry; an upstream-server harness receives from Ferry. |
| Fault injection | A TCP proxy that drops/stalls connections mid-transfer to test reconnect and resume. |
| Web | Vitest for protocol and state; Playwright end-to-end runs for browser↔browser and browser↔native. |
| Visual | Playwright screenshots in light/dark/reduced-motion at phone, tablet and desktop widths. |
| Manual / devices | Checked by hand on real Windows, macOS, Linux, Android and iOS hardware before a release. |
