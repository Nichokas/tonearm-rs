//! Wire messages. JSON, length-prefixed (u32 big endian). Unknown fields are
//! ignored and unknown message types decode to [`Msg::Unknown`], so minor
//! protocol additions stay compatible.

use serde::{Deserialize, Serialize};

use super::MAX_FRAME;
use super::queue::{Snapshot, VersionedOp};

/// Host playback state at one instant of the host's monotonic clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    /// Host incarnation; a newer epoch always wins.
    pub epoch: u64,
    /// Increases with every playback change.
    pub seq: u64,
    pub queue_version: u64,
    /// Queue entry playing (`None`: nothing).
    pub item_id: Option<String>,
    pub track_id: Option<String>,
    pub position_ms: i64,
    /// Host monotonic time at which `position_ms` was true.
    pub host_mono_ns: i64,
    pub playing: bool,
}

impl Anchor {
    /// Host position (ms) at host time `host_now_ns`.
    pub fn position_at(&self, host_now_ns: i64) -> i64 {
        if self.playing {
            // Saturating: anchors come from the network.
            self.position_ms.saturating_add(host_now_ns.saturating_sub(self.host_mono_ns).max(0) / 1_000_000)
        } else {
            self.position_ms
        }
    }

    pub fn is_newer_than(&self, other: &Anchor) -> bool {
        (self.epoch, self.seq) > (other.epoch, other.seq)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    IncompatibleVersion,
    BadSecret,
    Full,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The host closed the session.
    HostEnded,
    /// Nothing heard from the host for too long.
    HostLost,
    /// The guest left.
    Left,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    // guest -> host
    Hello { proto: u32, secret: String, name: String },
    Ping { id: u64, t0: i64 },
    /// Guest has queue `have`; host answers with `Ops` or `Snapshot`.
    Resync { have: u64 },
    Bye,
    // host -> guest
    Welcome { proto: u32, session_id: String, host_name: String, snapshot: Snapshot, anchor: Anchor },
    Reject { reason: RejectReason, proto: u32 },
    Pong { id: u64, t0: i64, t1: i64, t2: i64 },
    Anchor(Anchor),
    Op(VersionedOp),
    Ops { ops: Vec<VersionedOp> },
    Snapshot(Snapshot),
    Heartbeat { host_mono_ns: i64 },
    End { reason: EndReason },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("message too large")]
    TooLarge,
    #[error("bad message: {0}")]
    Bad(String),
}

/// Encodes one framed message.
pub fn encode(m: &Msg) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(m).map_err(|e| FrameError::Bad(e.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge);
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&u32::try_from(body.len()).expect("< MAX_FRAME").to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Length of the frame announced by a 4-byte header.
pub fn frame_len(header: [u8; 4]) -> Result<usize, FrameError> {
    let n = u32::from_be_bytes(header) as usize;
    if n > MAX_FRAME { Err(FrameError::TooLarge) } else { Ok(n) }
}

pub fn decode(body: &[u8]) -> Result<Msg, FrameError> {
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge);
    }
    serde_json::from_slice(body).map_err(|e| FrameError::Bad(e.to_string()))
}

#[cfg(test)]
#[allow(clippy::assertions_on_result_states, reason = "tests")]
mod tests {
    use super::*;
    use crate::live::queue::QueueItem;

    fn anchor() -> Anchor {
        Anchor { epoch: 1, seq: 2, queue_version: 3, item_id: Some("i".into()), track_id: Some("123".into()), position_ms: 1_000, host_mono_ns: 5_000_000_000, playing: true }
    }

    #[test]
    fn frame_round_trip() {
        let m = Msg::Anchor(anchor());
        let f = encode(&m).unwrap();
        let n = frame_len(f[..4].try_into().unwrap()).unwrap();
        assert_eq!(decode(&f[4..4 + n]).unwrap(), m);
    }

    #[test]
    fn unknown_type_and_fields_tolerated() {
        assert_eq!(decode(br#"{"t":"reaction","emoji":"x"}"#).unwrap(), Msg::Unknown);
        assert_eq!(decode(br#"{"t":"ping","id":1,"t0":2,"future":true}"#).unwrap(), Msg::Ping { id: 1, t0: 2 });
        assert!(decode(b"not json").is_err());
    }

    #[test]
    fn full_queue_fits_in_a_frame() {
        let items = (0..crate::live::MAX_QUEUE).map(|_| QueueItem { item_id: crate::live::new_id(), track_id: "123456789".into() }).collect();
        let m = Msg::Snapshot(Snapshot { version: 1, items });
        assert!(encode(&m).is_ok());
    }

    #[test]
    fn oversized_rejected() {
        assert_eq!(frame_len((MAX_FRAME as u32 + 1).to_be_bytes()), Err(FrameError::TooLarge));
    }

    #[test]
    fn position_extrapolation() {
        let a = anchor();
        assert_eq!(a.position_at(6_000_000_000), 2_000);
        assert_eq!(a.position_at(4_000_000_000), 1_000, "never runs backwards");
        let p = Anchor { playing: false, ..a };
        assert_eq!(p.position_at(60_000_000_000), 1_000);
    }

    #[test]
    fn hostile_anchor_does_not_overflow() {
        let a = Anchor { epoch: 0, seq: 0, queue_version: 0, item_id: None, track_id: None, position_ms: i64::MAX, host_mono_ns: i64::MIN, playing: true };
        assert_eq!(a.position_at(i64::MAX), i64::MAX);
    }
}
