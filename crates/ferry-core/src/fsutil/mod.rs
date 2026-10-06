//! Filesystem safety: name sanitization, collision-free naming, free space,
//! and the part-file writer every received byte goes through.

pub mod motw;
pub mod part;
pub mod read;
pub mod sanitize;
pub mod space;
pub mod unique;

/// Suffix of files that are still being received.
pub const PART_SUFFIX: &str = ".ferrypart";
