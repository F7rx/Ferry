# Protocol: LocalSend compatibility and Ferry extensions

Ferry speaks **LocalSend v2.2** byte-compatibly and layers optional extensions
on top. A LocalSend device never receives anything it doesn't understand: all
extension fields are optional JSON members (serde and dart_mappable both ignore
unknown members), and all extension *behaviour* is used only after the peer has
proven over mutual TLS that it is a Ferry device.

Notation: `→` request, `←` response. All JSON is UTF-8; field names camelCase.

## 1. Compatibility contract

| We guarantee | Detail |
|---|---|
| Discovery | Multicast `224.0.0.167:53317` (+ IPv6 `ff12::fd3a:e420`); answer announcements with `POST /api/localsend/v2/register`; legacy subnet scan; serve `/api/localsend/v1/info` and `/v2/info`. |
| Transport | HTTPS with a self-signed RSA-2048 certificate, **mutual TLS**, HTTP/1.1. Fingerprint = uppercase hex SHA-256 of the certificate DER. Plain HTTP only when the user disables encryption. |
| Upload API | `prepare-upload` (PIN via `?pin=`, 204/401/403/409/429 semantics), `upload` (streamed, exact size, 422 on checksum mismatch), `cancel` (with or without `sessionId`). |
| Download API | Not implemented: LocalSend's `prepare-download` web share is replaced by Ferry's own LAN browser links (§6), which any browser can open without an app. |
| Text messages | A lone `text/*` file with `preview` = message, answered 204. |
| Folder names | Relative paths in `fileName` (`folder/sub/file.ext`), validated component by component. |
| Differences | Ferry accepts **concurrent sessions** (LocalSend answers 409); a LocalSend sender simply never sees 409 from us. |

Version string: Ferry reports `"2.2"` in v2 DTOs.

## 2. Capability advertisement

Every v2 DTO Ferry *sends* (multicast announcement, `register` request/response,
`info`, `prepare-upload.info`) carries:

```json
"ferry": { "v": 1, "caps": ["resume", "verify", "status", "pair"] }
```

This is a hint only (it is unauthenticated on multicast). Before using any
extension, the sender calls, over the pinned mutual-TLS connection:

```
GET /api/ferry/v1/hello
← 200 {"v":1,"caps":[...],"deviceId":"<cert fingerprint>","platform":"windows","app":"0.1.0"}
← 404 (LocalSend) → use plain v2
```

Results are cached per fingerprint for the session.

## 3. HTTP extensions (Ferry ↔ Ferry, mutual TLS only)

All `/api/ferry/v1/*` routes require a **verified** client certificate; requests
without one get 403. Tokens are bound to the session *and* the client fingerprint.

### 3.1 Resumable transfers (`resume`)

`prepare-upload` request gains:

```json
"ferry": { "transferId": "<uuid v4, chosen by sender, stable across reconnects>" }
```

Response gains:

```json
"ferry": { "resumable": true, "offsets": { "<fileId>": 1048576 } }
```

- On first contact the receiver prompts as usual; `offsets` is empty.
- When a sender reconnects (network loss, sleep, either app restarted) it sends
  the **same `transferId`** again. If the receiver has that transfer persisted
  for the *same verified fingerprint*, it accepts **without prompting** and
  returns the session's `sessionId`/tokens (fresh ones when the transfer was
  restored after a restart) plus the confirmed byte offset of every
  unfinished file. Finished files are omitted from `files`; if every file is
  finished it answers **204**.
- Upload with an offset:

  ```
  POST /api/localsend/v2/upload?sessionId=…&fileId=…&token=…&offset=1048576
  body: bytes [offset, size)
  ```

  The receiver truncates its `.ferrypart` to the last fsync'd checkpoint
  ≥ the requested offset. If the requested offset is beyond what it has,
  it answers **416** `{"message":"offset mismatch","offset":<confirmed>}` and
  the sender restarts from `offset`.
- A resumed transfer keeps what the user approved, also across a receiver
  restart: the receiver stores the whole approved offer (ids, names, sizes,
  types and announced `sha256` of the accepted files and of the declined ones)
  and the save folder the user chose. The re-offer is checked against it:
  - it may list fewer files (a sender re-offers only what it hasn't finished,
    declined files included; those still get no token);
  - every listed file must be an approved one with the same name, size and
    type; its `sha256` may be omitted (the approved one still applies) but not
    changed, nor added where none was approved.

  Anything else is refused with **400** `Files differ from the accepted
  transfer` (the stored transfer is kept; there is no new prompt, since a
  sender reusing a `transferId` must send the same files). With checksum
  verification on, a completed file that doesn't match the approved
  `sha256` is answered **422**, its data deleted, and it starts again from
  offset 0; a file gets three attempts in total, counted across restarts.
- A restored transfer writes only into the folder that was approved for it,
  even if the default save folder changed since. If that folder no longer
  exists (it is never recreated) or a stored partial file lies outside it,
  the stored transfer is forgotten (its partial files are not touched) and
  the request is treated as new (the user is asked again). Transfers stored by older versions, which recorded
  neither the folder nor checksums nor declined files, resume only if their
  partial files are inside the current default folder; ids they don't know
  are ignored rather than refused, and a `sha256` in the re-offer counts as
  added.
- A restored transfer needs a free session slot like a new one (409 when the
  peer or the receiver as a whole is at its cap); a sender reconnecting to a session still in memory
  reuses that session's slot.
- Receivers persist resumable transfers for **24 h** after the last activity,
  then delete the partial files and forget the transfer.
- `GET /api/ferry/v1/transfers/<transferId>` ← `{"files":{"<fileId>":{"offset":n,"done":bool}}}`
  lets a reconnecting sender check progress cheaply.

### 3.2 End-to-end verification without pre-hashing (`verify`)

LocalSend senders hash every file *before* sending (a second full read). Ferry
senders do not: both sides hash while streaming.

- On a successful upload a Ferry receiver answers `200 {"sha256":"<lowercase hex>"}` (LocalSend senders ignore bodies).
- The sender compares with its own streaming hash (for resumed files both sides
  re-hash the already-transferred prefix from local disk) and confirms:

  ```
  POST /api/ferry/v1/verify?sessionId=…&fileId=…&token=…
  → {"sha256":"<sender hash>"}
  ← 200 {"ok":true}
  ← 422 {"ok":false}   receiver deletes the file and resets it to offset 0
  ```
- Files ≤ 1 MiB are read into memory once, hashed, and sent with the standard
  `sha256` field instead (no extra round trip).
- Incoming LocalSend transfers that include `sha256` are verified with the
  standard 422 behaviour.

### 3.3 Batched small files (`batch`, Planned)

Not implemented yet: no Ferry device advertises `batch` or serves this route.
The design:

```
POST /api/ferry/v1/upload-batch?sessionId=…
body: repeated frames
  u32 BE  header length
  header  JSON {"fileId","token","size","sha256"}
  bytes   file content (exactly size)
← 200 {"results":{"<fileId>":{"ok":true}|{"ok":false,"status":422}}}
```

Used for files ≤ 1 MiB, up to 64 MiB or 2,000 files per request. Each file is
committed independently, so a broken batch only loses its incomplete file.

### 3.4 Pairing (`pair`)

Two ways to make a device "mine" (trusted both ways, auto-accepted). Both run
over mutual TLS, so each side already knows the other's certificate
fingerprint; the out-of-band step proves there is no relay in the middle.
Implementation: `crates/ferry-core/src/pairing.rs`, tests in
`crates/ferry-core/tests/pairing.rs`.

1. **QR / link.** Device A shows
   `ferry://pair?v=1&fp=<A fingerprint>&a=<ip>,<ip>&p=<port>&s=<secret>`, where
   `secret` is 16 random bytes (base64url, no padding). Single use, valid for
   5 minutes, at most 4 open at once. `a` holds literal IP addresses only
   (scoped IPv6 allowed); host names are ignored, so a code can't make the
   scanner resolve or contact arbitrary hosts.
   Device B connects to each address in turn with the certificate pinned to
   `fp` (registering itself as usual), then sends
   `POST /api/ferry/v1/pair` `{"proof": "<base64url HMAC>", "device": <DeviceDto>}` with
   `proof = HMAC-SHA256(secret, "ferry-pair/1" ‖ fpA ‖ fpB)` (fingerprints as
   uppercase hex). A checks the proof against its open codes in constant time,
   consumes the matching one, stores B as mine and answers
   `200 {"device": <A DeviceDto>}`; B then stores A. No prompt on A: showing
   the code was the consent. A wrong proof answers `403`; five failures from one
   IP lock it out for five minutes (`429`), even for the right code.
2. **Code comparison** (commit, then reveal, so nobody can steer the code):
   1. A picks a random 32-byte nonce `nA` and sends `POST /api/ferry/v1/pair`
      `{"proof": null, "commit": b64url(SHA-256("ferry-pair-commit/1" ‖ nA)), "device": <DeviceDto>}`.
      B stores the commitment for 30 s and answers at once
      `200 {"session": "<id>", "nonce": b64url(nB)}` with its own random 32-byte `nB`.
      Requests without a commitment get `400`.
   2. A sends `POST /api/ferry/v1/pair/reveal` `{"session": "<id>", "nonce": b64url(nA)}`
      from the same certificate. B checks the commitment in constant time
      (`403` on mismatch, counted toward the lockout; `410` for an unknown,
      used or expired session), then asks its user; the request stays open.
   3. Both screens show the first four bytes of
      `SHA-256("ferry-verify/2" ‖ fpA ‖ fpB ‖ nA ‖ nB)` (A = asker, B = responder,
      uppercase hex fingerprints), read big-endian, modulo 10⁶, as `"123 456"`.
      A relay in the middle is committed to its nonce before it sees `nB`, so
      it can't grind certificates or nonces toward a matching code; each
      attempt is a fresh 1-in-10⁶ guess that costs a prompt the user must confirm.

   B answers the reveal with `200` when its user confirms the codes match,
   `403` when they don't, `408` after 2 minutes; declines and timeouts count
   toward the per-IP lockout. One open request per device and per IP group
   (IPv4 address or IPv6 /64), four in total (`429` otherwise). If A hangs up,
   B's prompt closes. "They match" has no keyboard shortcut and is never
   auto-answered (the CLI only asks with `--accept ask`).
   Addresses in a QR/link (flow 1) are literal IPs; only link-local IPv6 may
   carry a zone, and only an interface number or plain name.

Revoking is local and immediate: `POST /api/ferry/v1/unpair` `{}` tells the
other device (best effort), which then removes the pairing and the trust on
its side too and tells its user. Only the device itself can unpair itself, since the
route requires its verified certificate.

## 4. Discovery behaviour

- Announce bursts at +0.1 s / +0.6 s / +2.6 s on start, on network change, and on manual refresh.
- Answer announcements via HTTP `register`; if that fails, send one `announce:false` multicast datagram (spec fallback LocalSend doesn't implement).
- Answers are deduplicated per (fingerprint, source IP) for 2 s and rate-limited per source; only sources on a local subnet are answered.
- Interfaces: skip down, loopback, 169.254/16 and known virtual/VPN adapters unless the user opts in.
- Device table rules: see `02-architecture.md` §4: plain-HTTP answers never merge into verified devices.

## 5. WebRTC

### 5.1 Signaling (`ferry-signal`)

Wire-compatible with LocalSend's `/v1/ws`:

```
wss://<host>/v1/ws?d=<base64url-nopad(JSON)>
JSON: {alias, version, deviceModel?, deviceType?(UPPERCASE), token, ext?}
ext:  {"v":1, "caps":["rooms","trickle","ferry-dc"], "key":"<Ed25519 public key, base64url>", "nearby"?:bool}
```

Legacy messages (`HELLO`, `JOIN`, `UPDATE`, `LEFT`, `OFFER`, `ANSWER`, `ERROR`)
behave as upstream; peers sharing a public IP (IPv6: /64) see each other
("nearby on this network"), unless a client sends `ext.nearby: false` (it then
meets peers only in rooms). Extension messages are sent only to clients that
supplied `ext`:

| Direction | Message |
|---|---|
| S→C | `HELLO` gains `server: {"v":1,"caps":[...]}` (`"turn"` when `/v1/turn` is enabled) |
| C→S | `ROOM_JOIN {room}` · `ROOM_LEAVE {room}` |
| S→C | `ROOM_HELLO {room, peers}` · `ROOM_PEER_JOINED {room, peer}` · `ROOM_PEER_LEFT {room, peerId}` |
| C↔S | `ICE {target, sessionId, candidate}` (trickle; `candidate`: `RTCIceCandidateInit` object, `null` = end) |
| C↔S | `CANCEL {target, sessionId}` |
| C→S | `PING` ← `PONG` |
| S→C | `ERROR {code, message, sessionId?, room?}`, always JSON (upstream sends a bare string that crashes clients) |

As with upstream `OFFER`/`ANSWER`, relayed messages reach the target with the
sender's `peer` info in place of `target`. Relays work between nearby peers and
between peers sharing a room; anything else answers `ERROR 404` (`ICE`/`CANCEL`
to a client without `ext`: `403`).

TURN (optional, off by default): `GET /v1/turn?peer=<own client id>` →
`{iceServers: [{urls, username: "<unix expiry>:<id>", credential}], ttl}`, coturn
`use-auth-secret` credentials valid for 10 minutes, issued only to a connected
client asking from its own network.

Room ids: link/QR rooms use `r:` + base64url(SHA-256("ferry-room/1" ‖ secret))[0..22];
the secret travels only in the URL fragment (`https://…/#room=<secret>`), never to
the server. Short codes are `c:` + 6 digits, with server-side attempt limits.

Server limits: 64 KiB frames, per-connection token bucket (all frames count),
32 peers per room, non-blocking fan-out (slow peers are disconnected), 60 s idle
timeout with pings, Origin allowlist, `X-Forwarded-For` only from trusted proxies.

### 5.2 Data channel `ferry-dc/1`

One channel, label `ferry/1`, ordered and reliable, created by the offerer.
Text frames carry JSON control messages; binary frames carry the bytes of the
file currently being streamed. Chunk size: 64 KiB if the remote SDP's
`max-message-size` allows it, else 16 KiB. Sender backpressure: pause above
1 MiB `bufferedAmount`, resume below 256 KiB. Flow control: a sender keeps at
most 16 MiB of file bytes in flight beyond the receiver's last `progress`
report; receivers report at least every 1 MiB they have processed (hashed and
written) and close the session when a sender overruns the window, so a slow
disk never makes a receiver buffer a file in memory.

**Handshake (identity bound to DTLS):**

1. Both: `{"t":"hello","v":1,"alg":"ed25519"|"p256","key":<raw public key>,"nonce":<32 random bytes>,"device":{alias,deviceType,platform},"caps":[...]}`
   (binary values base64url without padding; `p256` only where WebCrypto lacks Ed25519).
2. Both compute `T = SHA-256(enc("ferry-dc/1") ‖ enc(sessionId) ‖ enc(fpOfferer) ‖ enc(fpAnswerer) ‖ enc(nonceOfferer) ‖ enc(nonceAnswerer) ‖ enc(keyOfferer) ‖ enc(keyAnswerer))`,
   `enc(x) = u32be(len(x)) ‖ x` (strings UTF-8), with `fp*` = the `a=fingerprint` values from the local/remote SDP as each side observed them,
   normalized to `"<algo lowercase> <HEX:UPPER:WITH:COLONS>"` (several distinct lines: sorted, joined with `,`).
3. Both: `{"t":"auth","sig":Sign(key, "ferry-dc/1 auth " ‖ role ‖ T),"mac":HMAC-SHA256(roomKey, T)?}` with
   `roomKey = SHA-256("ferry-room-key/1" ‖ roomSecret)`; signatures are 64 bytes (P-256: IEEE P1363 `r ‖ s`).
4. Verify signature (and MAC in secret rooms). A signaling MITM yields different fingerprints on each leg → verification fails.
   Fatal errors are reported with `{"t":"error","code","message"}` right before the channel closes.

First-contact UIs show a 6-digit verification code derived from `T` the same way as §3.4's
code (first four bytes, big-endian, modulo 10⁶).

**Transfer:**

```
S: {"t":"offer","transferId","files":[{"id","name","size","mime","modified"?}],"text"?,"more"?:true}
R: {"t":"answer","transferId","accept":["id",…],"offsets":{"id":n},"declined"?:true,"more"?:true}
   per accepted file:
S: {"t":"file","id","offset"}  → binary frames …  → {"t":"file-end","id","sha256"}
R: {"t":"progress","transferId","bytes"}   (flow control, while binary frames arrive)
R: {"t":"file-ack","id","ok":bool,"sha256","error"?}
S: {"t":"done","transferId"}
either: {"t":"cancel","transferId","reason"?} · {"t":"ping"}/{"t":"pong"} every 10 s · {"t":"error","code","message"}
```

Offers and answers that do not fit one control message are split into
consecutive frames with the same `transferId`, all but the last carrying
`"more":true`; `text` travels in the first offer frame and each answer frame's
`offsets` name only ids it accepts (a declining answer is a single frame).
`sha256` always covers the whole file: on resume both sides re-hash the prefix
from local storage.

Limits: control messages ≤ 64 KiB; one offer ≤ 8 MiB in total and ≤ 10,000
files; ids 1 to 256 printable ASCII characters, transfer ids unique within a
session; sizes safe integers (total < 2⁵³); `text` ≤ 60 KiB as a JSON string.
`name` is a relative `/`-separated path (≤ 1,024 characters, ≤ 32 levels,
components ≤ 255 bytes); offers with control characters, lone surrogates,
backslashes, absolute paths, drive prefixes or empty / all-dot components
(judged after removing invisible characters) are rejected outright, and
receivers still sanitize each component (threat model F1/F2). The receiver
counts bytes against the declared size and rejects overruns. Undecided offers
are cancelled after the decision timeout (`reason: "timeout"`, default 5 min)
and handshakes that do not complete within 30 s are dropped. 30 s without any
frame from the peer = connection lost → reconnect through signaling and offer
the same `transferId` again; the receiver resumes with `offsets`.

**Clarifications** (from the native implementation, `crates/ferry-core/src/rtc`):

- Byte-level compatibility is pinned by cross-language vectors:
  `crates/ferry-core/tests/vectors/rtc.json`, generated from the TypeScript
  reference by `apps/app/src/lib/rtc/vectors.gen.test.ts`. Both implementations
  must pass them.
- Outbound JSON uses the member order of the reference objects (as
  `JSON.stringify` writes them). In `answer.offsets`, array-index-like ids
  (`"0"`, `"10"`) come first in ascending order, then the others in accept
  order (JavaScript's own-key order). Receivers must not depend on member order.
- Limits stated in characters count UTF-16 code units (JavaScript `length`);
  limits in bytes count UTF-8 bytes. A control frame containing a lone-surrogate
  escape (`"\ud800"`) may be rejected as a whole by strict JSON parsers;
  senders must not produce one (such file names are invalid anyway).
- `ICE.candidate.sdpMid` must name the data m-line's `a=mid` (browsers reject
  candidates with an unknown mid such as `""`). Candidates are sent only after
  the OFFER/ANSWER they belong to.
- Native devices sign with the engine identity's Ed25519 key (stored with the
  TLS identity) and list WebRTC peers as `rtc:<base64url identity key>`; every
  session pins the key the peer announced in `ext.key`, so trust set on such a
  device id is bound to that key.
- `text` with files: receivers show (and record in history) the text only
  after the user accepts at least one file, once per peer and transfer (not
  again for a resumed re-offer); a declined offer's text is never shown. Native
  senders never combine text and files; each text is its own transfer.
- Native WebRTC resume works from memory only: it survives a dropped
  connection but not an app restart, and a re-offer is matched by file id,
  name and size. LAN resume (§3.1) is the persisted one.

### 5.3 LocalSend web compatibility (`ls-v1`)

LocalSend's browser app uses a different data-channel protocol (label `data`,
nonce/token handshake, `"0"` delimiters, 16 KiB chunks, no checksums) and only
connects to `public.localsend.org`. Ferry implements the signaling side
compatibly; the `ls-v1` data-channel adapter is planned as an opt-in mode (M4b)
and would be used only for peers that don't advertise `ext`.

## 6. LAN browser links

A native device can serve a one-off page to any browser on the same network:
**download links** (PC → browser) and **upload links** (browser → PC). This is
Ferry's own scheme; it is not part of the LocalSend protocol.

| | |
|---|---|
| Listener | Plain HTTP/1.1 on port `53319` (falls back to a random port), IPv4 and IPv6, started lazily on the first link. Up to 16 connections per IP, 64 total. |
| Link | `http://<lan-ip>:<port>/s/<token>`, token = 128 random bits (32 hex). One URL per local address; the UI shows the first and renders it as a QR code. |
| Lifetime | 1 hour by default, at most 16 open links; stopped links and expired links answer 404 with a short "link ended" page, and downloads still running on them are cut off. A download that moves no bytes for 60 s loses its connection, so stalled clients can't hold the connection slots. |
| `GET /s/{t}` | The self-contained page (`crates/ferry-core/assets/browser.html`). CSP `default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'self'; form-action 'none'; base-uri 'none'`, plus `X-Frame-Options: DENY`, `nosniff`, `no-referrer`, `no-store`. |
| `GET /s/{t}/api/info` | `{kind, device: {alias, kind}, files: [{id, name, size, mime}], expiresAtMs}`. |
| `GET /s/{t}/api/files/{id}` | Download links only. Streams the file; single `Range` requests answer 206/416; `Content-Disposition` with an RFC 5987 UTF-8 filename. A *complete* full download increments the link's download count. |
| `POST /s/{t}/api/prepare` | Upload links only. Body `{files: [FileDto…], sender?}` (LocalSend `FileDto` shape). Runs the same admission as a LAN `prepare-upload`: rate limit, "receiving" switch, device PIN, file-name validation, disk-space check, and the **accept prompt** (browsers are never trusted, so they never skip it). The sender appears as `"<sender> (<browser> on <OS>)"`, device type `web`. Answers like `prepare-upload` (session id + per-file tokens, 204 for a text message). |
| `POST /s/{t}/api/upload?sessionId&fileId&token` | Streams one file into the normal receive pipeline (part file, hash, no-replace rename). |
| `POST /s/{t}/api/cancel?sessionId` | Cancels the upload session. |
| PIN | Optional per link. Every API call then needs `x-ferry-pin: <pin>` (or `?pin=`). Wrong PINs are counted per IP: 5 failures lock that IP out for 5 minutes (429). The device's own receive PIN (for LAN senders) doesn't apply to links: the link token and the link's PIN are the capability, and every upload still goes through the accept prompt. |
| DNS rebinding | Requests are answered only when `Host` is one of this device's own addresses (or loopback) with the link port; otherwise 403. |

Security properties and limits: the token is the capability, so anyone who
sees the URL or QR code can use the link until it expires or is stopped. Traffic
is **not encrypted** (browsers can't verify a self-signed certificate without a
warning), which the UI and the page both say. Payload encryption with a key in
the URL fragment (never sent to the server) is planned (threat model W6).

