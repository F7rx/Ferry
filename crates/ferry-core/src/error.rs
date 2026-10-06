//! Errors. Internally everything is `anyhow`/`FerryError`; whatever reaches the
//! UI is an [`ErrorInfo`]: a stable code, a human sentence and, where we can
//! give one, an actionable hint. Raw socket errors never reach users.

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum FerryError {
    #[error("{0}")]
    User(ErrorInfo),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Db(#[from] rusqlite::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T, E = FerryError> = std::result::Result<T, E>;

/// An error as presented to the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorInfo {
    /// Stable machine-readable code, e.g. `peer_unreachable`.
    pub code: String,
    /// One sentence describing what happened.
    pub message: String,
    /// What the user can do about it, when we know.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl std::fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl ErrorInfo {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into(), hint: None }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn declined() -> Self {
        Self::new("declined", "The receiver declined the transfer.")
    }

    pub fn cancelled_by_peer() -> Self {
        Self::new("cancelled_by_peer", "The other device cancelled the transfer.")
    }

    pub fn cancelled() -> Self {
        Self::new("cancelled", "Transfer cancelled.")
    }

    pub fn busy() -> Self {
        Self::new("receiver_busy", "The receiver is busy with another transfer.")
            .with_hint("Try again when its current transfer has finished.")
    }

    pub fn pin_required() -> Self {
        Self::new("pin_required", "The receiver requires a PIN.")
    }

    pub fn pin_invalid() -> Self {
        Self::new("pin_invalid", "The PIN was not accepted.")
    }

    pub fn too_many_attempts() -> Self {
        Self::new("too_many_attempts", "Too many attempts.").with_hint("Wait a minute before trying again.")
    }

    pub fn checksum_mismatch(name: &str) -> Self {
        Self::new("checksum_mismatch", format!("“{name}” arrived damaged and was discarded."))
            .with_hint("Send it again; Ferry retried automatically but the data kept differing.")
    }

    pub fn unreachable(alias: &str) -> Self {
        Self::new("peer_unreachable", format!("Can't reach {alias}.")).with_hint(
            "Make sure both devices are on the same network and Ferry is open on the other device. A firewall may be blocking port 53317.",
        )
    }

    pub fn identity_changed(alias: &str) -> Self {
        Self::new(
            "identity_changed",
            format!("{alias} answered with a different identity than before."),
        )
        .with_hint("This can mean the app was reinstalled, or that another device is impersonating it. Verify the device before trusting it again.")
    }

    pub fn disk_full(needed: u64, available: u64) -> Self {
        Self::new(
            "disk_full",
            format!("Not enough space: needs {}, {} available.", crate::util::format_bytes(needed), crate::util::format_bytes(available)),
        )
        .with_hint("Free up space or choose another save location.")
    }

    pub fn file_unreadable(name: &str, err: &std::io::Error) -> Self {
        let message = match err.kind() {
            std::io::ErrorKind::NotFound => format!("“{name}” no longer exists."),
            std::io::ErrorKind::PermissionDenied => format!("Ferry isn't allowed to read “{name}”."),
            _ => format!("“{name}” couldn't be read."),
        };
        Self::new("file_unreadable", message)
    }

    pub fn internal(err: impl std::fmt::Display) -> Self {
        Self::new("internal", format!("Something went wrong: {err}"))
    }
}

impl From<ErrorInfo> for FerryError {
    fn from(value: ErrorInfo) -> Self {
        FerryError::User(value)
    }
}

impl FerryError {
    /// The user-facing form of this error.
    pub fn info(&self) -> ErrorInfo {
        match self {
            FerryError::User(info) => info.clone(),
            FerryError::Io(err) if err.kind() == std::io::ErrorKind::StorageFull => {
                ErrorInfo::new("disk_full", "The disk is full.").with_hint("Free up space or choose another save location.")
            }
            other => ErrorInfo::internal(other),
        }
    }
}
