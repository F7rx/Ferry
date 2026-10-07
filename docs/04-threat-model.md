# Threat model

Ferry moves arbitrary files between devices that often do not know each other,
over networks the user does not control. This document lists what we defend,
against whom, and the concrete control for each threat. Every control here is
either implemented (✅), planned with a milestone (⏳), or an accepted residual
risk (⚠️); nothing is left implicit.

## Assets

| Asset | Why it matters |
|---|---|
| File contents in transit | The user's private data. |
| Files on the receiver's disk | A transfer must never overwrite, escape the save folder, or plant executables that launch. |
| Device identity keys (TLS cert key, Ed25519 key) | Whoever holds them *is* the device to every peer that trusts it. |
| Trust relationships ("My devices", trusted, favorites) | Trusted peers can skip the accept prompt. |
| Availability of the receiver | A peer must not be able to lock everyone else out or exhaust memory/disk. |
| Metadata (device names, history) | Reveals who sends what to whom. |

## Adversaries

1. **Malicious LAN peer**: on the same Wi-Fi (café, office, hotel, campus). Can send any packet, spoof IPs, run a modified client.
2. **Passive LAN eavesdropper**: can read traffic on open/shared networks.
3. **Active MITM on the LAN**: ARP/DHCP spoofing; can intercept and alter connections.
4. **Malicious or compromised signaling server**: sees signaling for browser/remote transfers, can drop/alter/inject messages.
5. **Malicious website**: runs JavaScript in the user's browser (drive-by), can target the local API or the signaling server from the victim's browser.
6. **Malicious file**: content crafted to exploit the receiver (names, sizes, types).
7. **TURN relay operator**: sees relayed ciphertext and connection metadata.

Out of scope: a compromised OS or a malicious app with the same user's privileges
(they can read our key store and files directly); physical access to an unlocked device.

## Identity model

- **LAN identity** = SHA-256 fingerprint of the device's self-signed TLS
  certificate (LocalSend-compatible). Both sides present certificates (mutual
  TLS); the server learns the client's fingerprint from the handshake, never
  from a JSON field.
- **Peer identity states** are explicit everywhere in the engine and the UI:
  - `Verified(fingerprint)`: proven by mutual TLS (or by a signature bound to the WebRTC DTLS transcript).
  - `Unverified`: the peer did not present a certificate.
  - `PlainHttp`: the peer uses LocalSend's unencrypted mode.

  Only `Verified` peers can ever be trusted, auto-accepted, or matched to a
  favorite. Upstream LocalSend falls back to the self-reported `fingerprint`
  field when no certificate is present, which lets a LAN peer impersonate a
  favorite while web share is on. Ferry never does this.
- **First contact is trust-on-first-use.** It becomes *verified* only by an
  out-of-band step: scanning the other device's pairing QR, or both users
  confirming the same 6-digit verification code derived from both fingerprints
  (protocol §3.4). Pairing codes are single-use, expire after 5 minutes, and
  repeated wrong proofs lock the guessing IP out; confirming a code always
  takes a deliberate click, never a keyboard default or an auto-accept rule.
  The comparison code mixes a nonce from each side and the asker commits to
  its nonce first, so a relay can't grind certificates until its code matches.
- **Names are untrusted display text.** Device aliases from the network lose
  control, bidi-override and zero-width characters (as file names do), so one
  device can't render as another's name.
- **WebRTC identity** = a persistent Ed25519 key per device. After DTLS
  connects, each side signs a transcript containing both DTLS fingerprints as it
  observed them, the session id and fresh nonces. A signaling server that
  swaps SDPs (MITM) produces mismatched fingerprints and fails verification.

## Threats and controls

### Network peers

| # | Threat | Control | Status |
|---|---|---|---|
| N1 | Eavesdropping on LAN transfers | TLS 1.2/1.3 (rustls) by default; plain HTTP only when the user turns encryption off for legacy peers, labelled "Not encrypted". | ✅ |
| N2 | MITM on LAN | Sender pins the receiver's certificate fingerprint *during the handshake* (no request bytes leave on mismatch). Fingerprint changes for a known device raise an "identity changed" warning and drop trust. | ✅ |
| N3 | Spoofed device identity / impersonating a trusted device | Identity comes only from mutual TLS (`Verified`), never from JSON. Auto-accept requires `Verified` + explicit trust. | ✅ |
| N4 | Replay of upload tokens | Tokens are 128-bit random, single file, bound to session + peer fingerprint (or IP for unverified peers), invalidated on completion/cancel/expiry. | ✅ |
| N5 | Fake discovery announcements (spam, spoofed aliases) | Discovered devices are untrusted until they answer a mutual-TLS `/register`; the device table is bounded (LRU, 256 entries) and entries expire. Duplicate aliases are disambiguated in the UI by fingerprint suffix. | ✅ |
| N6 | Request floods / prompt spam | Per-IP (IPv6 /64) connection caps, token-bucket rate limits on prepare-upload, at most 3 pending prompts per peer and 16 overall (excess gets 409); the rate limit answers 429. The browser app limits incoming sessions (2 per peer) and messages (10 per peer and 30 overall per minute). The desktop app's WebRTC path caps pending prompts the same way, limits messages like the browser (10 per peer and 30 overall per minute; more are declined), and caps transfers with files at 8 per peer (identity key) and 32 overall, counted from admission, while the user decides, until they end (more are declined). | ✅ |
| N7 | Memory exhaustion via JSON bodies | Every JSON body is size-capped (64 KiB for register/info, 8 MiB for prepare-upload) and file counts are capped; a global in-flight JSON byte budget bounds the worst case. | ✅ |
| N8 | Slowloris / idle connections holding slots | Header-read timeout, TLS-handshake timeout, idle (no-progress) timeout on bodies, keep-alive idle timeout. | ✅ |
| N9 | Disk exhaustion | Free-space check against the declared size before accepting; writes are length-checked against the declared size; per-session byte budget. | ✅ |
| N10 | One sender blocking all others (upstream's single session slot) | Multiple concurrent sessions with per-peer and global caps (8 sessions per peer, meaning an IPv4 address or IPv6 /64, and 32 overall; excess gets 409); a request holds its session slot from admission on (also while its user decides), so approvals and restored transfers can't add up past the caps; pending decisions and idle sessions time out. | ✅ |
| N11 | PIN brute force | Constant-time compare; failures counted per IP (/64 for IPv6) *and* globally with time decay; lockout is temporary, not until restart. | ✅ |
| N12 | DNS rebinding / CSRF against the local API | The HTTPS API requires client certificates (browsers cannot present them). The plain-HTTP browser-share listener checks `Host` against our own addresses, requires an unguessable per-share token in the path, and never accepts cross-origin requests. | ✅ |
| N13 | Single-instance "show" endpoint reachable from LAN (upstream `/show`) | Not exposed at all. The desktop shell uses OS single-instance locking. | ✅ |

### Malicious files and metadata

| # | Threat | Control | Status |
|---|---|---|---|
| F1 | Path traversal (`../`, absolute paths, drive letters, UNC, alternate data streams `a:b`) | Each path component is validated and sanitized independently; `..`/`.`/empty components and absolute/rooted paths are rejected; the final path is re-checked to be inside the destination after normalization. | ✅ |
| F2 | Malformed/hostile names (control chars, bidi overrides U+202E, zero-width chars, Windows reserved names incl. `COM¹`/`CONIN$`, trailing dots/spaces, overlong names) | Sanitizer strips control and format (Cf) characters, maps reserved names, NFC-normalizes, truncates to 255 bytes *preserving the extension*. | ✅ |
| F3 | Overwriting existing files / duplicate names | Received data goes to a fresh `*.ferrypart` file opened with `create_new` (files of 1 MiB or less that start at offset 0 are written straight to a unique final name with `create_new`); on completion it is moved to a unique final name (`name (2).ext`, …) with a no-replace rename, falling back to a hard link or an exclusive-create copy where the file system has no such rename. Nothing is ever truncated or replaced. | ✅ |
| F4 | Symlink/junction tricks in the destination | `create_new` refuses existing entries (including symlinks); folder components are created by us and checked not to be symlinks. | ✅ |
| F5 | Executables or archives that auto-run | Nothing is opened, executed or extracted automatically. Opening is always an explicit user action; on Windows every received file is tagged with the Mark-of-the-Web (`Zone.Identifier`, ZoneId=3, written to the part file before it becomes visible) so SmartScreen and Office Protected View apply; volumes without alternate data streams (FAT32/exFAT) can't carry the tag. | ✅ Windows · macOS quarantine attribute ⬜ |
| F6 | Content/type mismatch (`.pdf` that is an `.exe`) | The UI shows the real extension, not the declared MIME type; MIME is advisory only. | ✅ |
| F7 | Corrupted or truncated data | Exact size enforcement; SHA-256 computed while streaming and compared with the sender's hash (LocalSend's `sha256` field, or Ferry's end-of-file hash exchange). Mismatch → 422 / retry, never a silently wrong file. | ✅ |
| F8 | Huge previews / text payloads | Text/preview fields capped (1 MiB); thumbnails are generated locally, never decoded from peer-supplied previews without size limits. | ✅ |

### Browser and web

| # | Threat | Control | Status |
|---|---|---|---|
| W1 | Malicious website connecting to the signaling server as the victim (drive-by offers) | Origin allowlist on the signaling server; browsers never auto-accept offers from unknown peers; every offer requires a user decision unless the peer is a verified trusted device. | ✅ (server) / ✅ (client) |
| W2 | Room squatting via spoofed `X-Forwarded-For` | Forwarded headers are honoured only from configured trusted proxies. | ✅ |
| W3 | Signaling server MITM (swapping SDPs) | Identity-bound transcript signatures over both DTLS fingerprints (see Identity model). QR/link rooms additionally HMAC the transcript with the room secret carried in the URL *fragment*, which never reaches the server. | ✅ (QR/link) / ⚠️ short numeric codes rely on verification words (PAKE ⏳ M7) |
| W4 | Signaling abuse (huge frames, floods, slow readers blocking fan-out) | 64 KiB frame cap, field length limits, per-connection token buckets counting every frame, non-blocking fan-out that drops slow peers, idle timeouts with pings, room size caps. | ✅ |
| W5 | Weak randomness in browser session ids/tokens | `crypto.getRandomValues` only. | ✅ |
| W6 | Unencrypted browser-share links on the LAN | Browsers reject self-signed certificates, so LAN links are plain HTTP (same as LocalSend). Mitigations: unguessable 128-bit token in the path, optional PIN, expiry, per-request approval, explicit "Not encrypted on this network" label. Payload encryption with a key carried in the URL fragment is planned. | ⚠️ (fragment-key encryption ⏳ M9) |
| W7 | Receiving a huge file into browser memory | Received files stream to the Origin Private File System chunk by chunk. Browsers without OPFS writers keep the whole file as a Blob in IndexedDB, and refuse files over 256 MB there. | ✅ |

### Relay and remote

| # | Threat | Control | Status |
|---|---|---|---|
| R1 | Relay reads data | DTLS end-to-end; the relay only sees ciphertext. While a transfer runs, its card shows `Encrypted · Peer-to-peer · Relayed` when either end of the selected ICE candidate pair is a TURN relay, and `Direct` when neither is. When the route can't be told (no selected pair, or unreadable candidate types), the card leaves it out rather than claim Direct. | ✅ |
| R2 | Permanent copies in the cloud | There is no upload storage anywhere; the signaling server holds no file bytes. TURN only forwards packets. | ✅ |
| R3 | TURN credential abuse | Optional and off by default. `ferry-signal` issues short-lived (10 min) coturn `use-auth-secret` credentials per connected client (username = expiry + client id), only to a request from that client's own network; never long-lived static credentials in clients. Operators should deny relaying to private ranges (sample config in `crates/ferry-signal/README.md`). | ✅ server · ✅ clients |

### Local data

| # | Threat | Control | Status |
|---|---|---|---|
| L1 | Private key theft from settings | Keys live in a separate file, never in settings; on Windows encrypted with DPAPI (user scope); 0600 permissions elsewhere. OS keychains (macOS Keychain, Android Keystore, iOS Keychain) ⏳ M8. | ✅ Windows / ⏳ others |
| L2 | Corrupt settings wiping identity (upstream behaviour) | Identity is stored separately from settings; settings writes are atomic (temp + fsync + rename) and debounced. | ✅ |
| L3 | History leaking content | History stores metadata (name, size, device, time, path); message text is kept only with *Remember message text*. With that setting off, a message's entry is named "Message" and nothing of its text is stored; with it on, the entry is named after its first line (up to 80 characters) and keeps the text. With history off, messages and sent items are not recorded. Browser: with history off, received files stay listed in the Inbox (name, size, sender, time), since the Inbox is the only way back to them. Clearing history keeps received files in the Inbox; removing an Inbox item deletes its stored file; *Delete received files* deletes them all. Partial files of transfers that can no longer resume (24 h) are deleted the next time the app opens. | ✅ |
| L4 | Telemetry | None. No analytics, no crash upload, no update pings unless the user opts in. | ✅ |
| L5 | Desktop webview reading arbitrary files | The asset protocol scope starts empty and the webview has no file-system permissions. Each received, completed file is allowed individually (never a folder) for previews, and only if its path is absolute and local (no UNC, device or stream paths on Windows), it is a regular file and not a symlink, and it lies inside its transfer's save folder. Grants are held in memory until the app exits; removing a history entry does not revoke its grant. | ✅ |

## Accepted residual risks

- **TOFU on first contact.** Two devices that have never met cannot know each other without an out-of-band check. We make that check easy (QR, verification code) and visible ("Not verified"), not mandatory.
- **LocalSend legacy plain-HTTP mode** is unauthenticated by design. Ferry speaks it only when the user turns encryption off or a legacy peer requires it, and such peers are never trusted.
- **Short numeric room codes** (6 digits) depend on the signaling server's honesty until the PAKE lands; QR and link joins do not.
- **LAN browser links** are unencrypted until fragment-key payload encryption lands (W6).
- **Metadata visible to the signaling server**: device aliases and the fact that two peers connected. Never file names or contents.
