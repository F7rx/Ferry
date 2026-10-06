//! WebRTC transfers (docs/05-protocol.md §5): `ferry-dc/1` sessions with Ferry
//! browsers (the PWA) and native devices on other networks, set up through a
//! `ferry-signal` server. Wire-compatible with `apps/app/src/lib/rtc`.

pub mod b64;
pub mod identity;
pub mod manager;
pub mod peer;
pub mod protocol;
pub mod session;
#[cfg(test)]
mod session_tests;
pub mod signaling;
pub mod transcript;
