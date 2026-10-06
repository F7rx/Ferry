# Vendored: LocalSend protocol core

This crate is a vendored copy of `packages/core` from the LocalSend project.

| | |
|---|---|
| Upstream | https://github.com/localsend/localsend |
| Path | `packages/core` |
| Commit | `2ef4dc6af1b81b3bee69db9b54299e39e6f368d1` (2026-10-04) |
| License | Apache License 2.0 — see `/LICENSE` and `/NOTICE` |
| Copyright | © the LocalSend contributors |

Ferry uses it as a **protocol toolkit**: DTOs, certificate/fingerprint crypto,
the pinned-TLS HTTP client, multicast discovery and the WebRTC transport.
Ferry's own session layer and server live in `crates/ferry-core`.

## Modifications (Apache-2.0 §4(b))

Every modified file carries a `// Modified by the Ferry authors:` header line
describing the change. Summary:

| File | Change |
|---|---|
| `Cargo.toml` | Package renamed `ferry-localsend` (library name unchanged: `localsend`) so it can coexist with the unmodified upstream crate in `tests/interop`; license metadata; Clippy lints allowed, since upstream code is not held to Ferry's lint settings. |
| `src/crypto/cert.rs` | Added `generate_self_signed_named` (Ferry certificates use `CN=Ferry Device`). |
| `src/model/discovery.rs` | `MulticastMessageV2::extra` keeps unknown members (the legacy `announce` flag, Ferry's capability hint). |
| `src/multicast/mod.rs` | `MulticastDevice::extra` is announced; `MulticastHandle::reply` sends the spec's UDP `announce:false` fallback. |
| `src/http/client/{mod,scoped_host,server_cert_verifier}.rs` | Made the pinned verifier, scoped-host helpers and resolver public. |
| `src/http/server/{common/client_cert_verifier,peer_ip}.rs` | Made the client-certificate verifier and `PeerIp::from_remote_addr` public. |
| `tests/{discovery,event_backpressure,multicast}.rs` | Adapted to `MulticastDevice::extra`; the loopback subnet scan test is skipped on macOS, which only routes 127.0.0.1. |
| `src/crypto/token.rs` | Fixed remote-triggerable panic on tokens with < 5 segments and the unchecked `now - salt` underflow. |
| `src/model/transfer.rs` | Test `formats_nanosecond_timestamp` made resolution-aware (Windows `SystemTime` has 100 ns ticks). |

## Syncing with upstream

```sh
git -C .upstream/localsend fetch && git -C .upstream/localsend diff 2ef4dc6 -- packages/core
```

Apply relevant upstream changes, re-run `cargo test -p localsend --features full`,
then update the commit above.
