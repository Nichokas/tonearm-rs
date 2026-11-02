//! Listen Together: shared listening sessions.
//!
//! Every listener plays through their own TIDAL account; only metadata
//! (track ids, queue, position, play state) travels between devices.
//! The host is authoritative: it orders every change with a sequence number
//! and guests follow. This module is transport-agnostic and fully
//! deterministic (time is always passed in), so the desktop app and the iOS
//! app share exactly the same protocol and sync rules.
//!
//! All times are monotonic nanoseconds (never wall clock). Each side uses its
//! own monotonic base; the guest maps host time with [`clock::ClockSync`].

#![allow(clippy::missing_panics_doc, reason = "expects guard invariants checked just before")]
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "nanosecond values stay far below 2^52")]

pub mod clock;
pub mod drift;
#[cfg(feature = "live-net")]
pub mod net;
pub mod proto;
pub mod queue;
pub mod session;
pub mod ticket;

/// Bumped on incompatible protocol changes. Peers with another major version
/// are rejected with [`proto::RejectReason::IncompatibleVersion`].
pub const PROTO_VERSION: u32 = 1;
/// QUIC ALPN of the protocol.
pub const ALPN: &[u8] = b"sdat-live/1";
/// Maximum queue length shared in a session.
pub const MAX_QUEUE: usize = 500;
/// Maximum size of one encoded message.
pub const MAX_FRAME: usize = 64 * 1024;
/// Host heartbeat period and the silence after which guests give up.
pub const HEARTBEAT_NS: i64 = 2_000_000_000;
pub const HOST_TIMEOUT_NS: i64 = 10_000_000_000;

/// Random 128-bit id as 32 hex chars (queue entries, ops, sessions).
pub fn new_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// Process-wide monotonic clock in nanoseconds (shared base for every caller in
/// this process, including Swift through the FFI).
pub fn mono_now_ns() -> i64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static BASE: OnceLock<Instant> = OnceLock::new();
    i64::try_from(BASE.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(i64::MAX)
}
