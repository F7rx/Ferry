//! Ferry's transfer engine.
//!
//! LocalSend v2 compatible on the wire (via the vendored `localsend` crate),
//! with Ferry's extensions layered on top. See `docs/02-architecture.md`.

pub mod browser;
pub mod client;
pub mod db;
pub mod devices;
pub mod diagnostics;
pub mod discovery;
pub mod engine;
pub mod error;
pub mod events;
pub mod fsutil;
pub mod identity;
pub mod model;
pub mod net;
pub mod pairing;
pub mod proto;
pub mod receive;
pub mod rtc;
pub mod send;
pub mod server;
pub mod settings;
pub mod shared;
pub mod transfer;
pub mod util;

pub use error::{ErrorInfo, FerryError, Result};

pub use engine::{Engine, EngineConfig};
pub use send::{SendItem, Target};
pub use settings::Settings;
