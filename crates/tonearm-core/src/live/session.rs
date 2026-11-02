//! Host and guest state machines. Pure: the transport feeds them messages and
//! the current monotonic time, and sends whatever they return.

use std::collections::BTreeMap;

use super::clock::{ClockSync, Sample};
use super::drift::{Correction, DriftController};
use super::proto::{Anchor, EndReason, Msg, RejectReason};
use super::queue::{Accepted, GuestApply, GuestQueue, HostQueue, Op, OpKind, QueueError, QueueItem};
use super::{HEARTBEAT_NS, HOST_TIMEOUT_NS, PROTO_VERSION, new_id};

/// Guests per session in the MVP star topology.
pub const MAX_GUESTS: usize = 8;
/// Host playback changes closer than this are coalesced (seek spam).
pub const DEBOUNCE_NS: i64 = 300_000_000;

#[derive(Debug, Clone, PartialEq)]
pub enum To {
    Peer(String),
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Out {
    pub to: To,
    pub msg: Msg,
}

fn to(peer: &str, msg: Msg) -> Out {
    Out { to: To::Peer(peer.to_owned()), msg }
}
/// Whether a catch-up batch fits one frame; otherwise a snapshot is sent.
fn fits_frame(ops: &[super::queue::VersionedOp]) -> bool {
    super::proto::encode(&Msg::Ops { ops: ops.to_vec() }).is_ok()
}
fn all(msg: Msg) -> Out {
    Out { to: To::All, msg }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestInfo {
    pub peer: String,
    pub name: String,
}

#[derive(Debug)]
pub struct HostSession {
    pub session_id: String,
    pub secret: String,
    pub name: String,
    epoch: u64,
    queue: HostQueue,
    anchor: Anchor,
    pending_anchor: bool,
    last_anchor_sent_ns: i64,
    last_heartbeat_ns: i64,
    guests: BTreeMap<String, GuestInfo>,
    /// Last answered `Resync` per guest (see [`RESYNC_MIN_NS`]).
    last_resync_ns: BTreeMap<String, i64>,
}

impl HostSession {
    pub fn new(name: String, now_ns: i64) -> Self {
        Self {
            session_id: new_id(),
            secret: new_id(),
            name: match name.char_indices().nth(40) {
                Some((cut, _)) => name[..cut].to_owned(),
                None => name,
            },
            // Distinct per run so a restarted host always wins over stale state.
            epoch: u64::try_from(now_ns.max(0)).unwrap_or(0) ^ u64::from(rand::random::<u32>()) | 1,
            queue: HostQueue::new(),
            anchor: Anchor { epoch: 0, seq: 0, queue_version: 0, item_id: None, track_id: None, position_ms: 0, host_mono_ns: now_ns, playing: false },
            pending_anchor: false,
            last_anchor_sent_ns: i64::MIN / 2,
            last_heartbeat_ns: now_ns,
            guests: BTreeMap::new(),
            last_resync_ns: BTreeMap::new(),
        }
        .with_epoch()
    }

    fn with_epoch(mut self) -> Self {
        self.anchor.epoch = self.epoch;
        self
    }

    pub fn guests(&self) -> Vec<GuestInfo> {
        self.guests.values().cloned().collect()
    }

    pub fn anchor(&self) -> &Anchor {
        &self.anchor
    }

    pub fn queue_items(&self) -> &[QueueItem] {
        &self.queue.queue().items
    }

    pub fn on_message(&mut self, peer: &str, msg: Msg, now_ns: i64) -> Vec<Out> {
        match msg {
            Msg::Hello { proto, secret, name } => {
                if proto != PROTO_VERSION {
                    return vec![to(peer, Msg::Reject { reason: RejectReason::IncompatibleVersion, proto: PROTO_VERSION })];
                }
                if secret != self.secret {
                    return vec![to(peer, Msg::Reject { reason: RejectReason::BadSecret, proto: PROTO_VERSION })];
                }
                if !self.guests.contains_key(peer) && self.guests.len() >= MAX_GUESTS {
                    return vec![to(peer, Msg::Reject { reason: RejectReason::Full, proto: PROTO_VERSION })];
                }
                let name: String = name.chars().take(40).collect();
                self.guests.insert(peer.to_owned(), GuestInfo { peer: peer.to_owned(), name });
                let mut anchor = self.anchor.clone();
                anchor.position_ms = anchor.position_at(now_ns);
                anchor.host_mono_ns = now_ns;
                vec![to(peer, Msg::Welcome { proto: PROTO_VERSION, session_id: self.session_id.clone(), host_name: self.name.clone(), snapshot: self.queue.snapshot(), anchor })]
            }
            _ if !self.guests.contains_key(peer) => Vec::new(), // must say Hello first
            Msg::Ping { id, t0 } => vec![to(peer, Msg::Pong { id, t0, t1: now_ns, t2: now_ns })],
            // At most one answer per guest per RESYNC_MIN_NS: a guest asking
            // for full snapshots in a loop cannot make the host buffer them.
            Msg::Resync { .. } if self.last_resync_ns.get(peer).is_some_and(|t| now_ns - t < RESYNC_MIN_NS) => Vec::new(),
            Msg::Resync { have } => {
                self.last_resync_ns.insert(peer.to_owned(), now_ns);
                match self.queue.ops_since(have).filter(|ops| fits_frame(ops)) {
                Some(ops) => {
                    let mut v = vec![to(peer, Msg::Ops { ops })];
                    v.push(to(peer, Msg::Anchor(self.anchor.clone())));
                    v
                }
                None => vec![to(peer, Msg::Snapshot(self.queue.snapshot())), to(peer, Msg::Anchor(self.anchor.clone()))],
                }
            }
            Msg::Bye => {
                self.guests.remove(peer);
                self.last_resync_ns.remove(peer);
                Vec::new()
            }
            // Guests cannot send host messages.
            _ => Vec::new(),
        }
    }

    pub fn peer_disconnected(&mut self, peer: &str) {
        self.guests.remove(peer);
        self.last_resync_ns.remove(peer);
    }

    /// Host player state changed (play, pause, seek, new track). Coalesced
    /// within [`DEBOUNCE_NS`]; the rest goes out on [`Self::tick`].
    pub fn set_playback(&mut self, item_id: Option<String>, track_id: Option<String>, position_ms: i64, playing: bool, now_ns: i64) -> Vec<Out> {
        let track_changed = item_id != self.anchor.item_id || track_id != self.anchor.track_id;
        self.anchor = Anchor {
            epoch: self.epoch,
            seq: self.anchor.seq + 1,
            queue_version: self.queue.queue().version,
            item_id,
            track_id,
            position_ms: position_ms.max(0),
            host_mono_ns: now_ns,
            playing,
        };
        if track_changed || now_ns - self.last_anchor_sent_ns >= DEBOUNCE_NS {
            self.flush_anchor(now_ns)
        } else {
            self.pending_anchor = true;
            Vec::new()
        }
    }

    fn flush_anchor(&mut self, now_ns: i64) -> Vec<Out> {
        self.pending_anchor = false;
        self.last_anchor_sent_ns = now_ns;
        vec![all(Msg::Anchor(self.anchor.clone()))]
    }

    /// Changes the shared queue (host only in the MVP).
    pub fn queue_op(&mut self, kind: OpKind) -> Result<Vec<Out>, QueueError> {
        match self.queue.apply(Op { op_id: new_id(), kind })? {
            Accepted::New(v) => Ok(vec![all(Msg::Op(v))]),
            Accepted::Duplicate => Ok(Vec::new()),
        }
    }

    /// Call about every 250 ms: heartbeats and debounced anchors.
    pub fn tick(&mut self, now_ns: i64) -> Vec<Out> {
        let mut out = Vec::new();
        if self.pending_anchor && now_ns - self.last_anchor_sent_ns >= DEBOUNCE_NS {
            out.extend(self.flush_anchor(now_ns));
        }
        if now_ns - self.last_heartbeat_ns >= HEARTBEAT_NS {
            self.last_heartbeat_ns = now_ns;
            out.push(all(Msg::Heartbeat { host_mono_ns: now_ns }));
        }
        out
    }

    pub fn end(&mut self) -> Vec<Out> {
        self.guests.clear();
        vec![all(Msg::End { reason: EndReason::HostEnded })]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GuestPhase {
    Joining,
    Joined,
    Rejected(RejectReason),
    Ended(EndReason),
}

#[derive(Debug, Clone, PartialEq)]
pub enum GuestEvent {
    Joined { host_name: String },
    Rejected(RejectReason),
    QueueChanged,
    PlaybackChanged,
    Ended(EndReason),
}

/// Where the local player should be.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub item_id: Option<String>,
    pub track_id: Option<String>,
    pub position_ms: i64,
    pub playing: bool,
}

/// Ping burst on join, then one every few seconds.
pub const BURST_PINGS: u64 = 8;
pub const BURST_INTERVAL_NS: i64 = 150_000_000;
pub const PING_INTERVAL_NS: i64 = 3_000_000_000;
/// A queue gap older than this triggers a resync request.
pub const GAP_TIMEOUT_NS: i64 = 2_000_000_000;
/// Minimum time between two answered `Resync`s from one guest. Below
/// [`GAP_TIMEOUT_NS`], so an honest guest's retry is always answered.
pub const RESYNC_MIN_NS: i64 = 500_000_000;

#[derive(Debug)]
pub struct GuestSession {
    secret: String,
    name: String,
    phase: GuestPhase,
    pub host_name: String,
    clock: ClockSync,
    queue: GuestQueue,
    anchor: Option<Anchor>,
    drift: DriftController,
    last_heard_ns: i64,
    next_ping_id: u64,
    pings_sent: u64,
    last_ping_ns: i64,
    outstanding: BTreeMap<u64, i64>,
    gap_since: Option<i64>,
    resync_sent_ns: Option<i64>,
    /// A snapshot was requested and has not arrived; `tick` retries it
    /// (the host may drop a request under [`RESYNC_MIN_NS`]).
    need_snapshot: bool,
}

impl GuestSession {
    pub fn new(secret: String, name: String, now_ns: i64) -> Self {
        Self {
            secret,
            name,
            phase: GuestPhase::Joining,
            host_name: String::new(),
            clock: ClockSync::new(),
            queue: GuestQueue::new(),
            anchor: None,
            drift: DriftController::new(),
            last_heard_ns: now_ns,
            next_ping_id: 0,
            pings_sent: 0,
            last_ping_ns: i64::MIN / 2,
            outstanding: BTreeMap::new(),
            gap_since: None,
            resync_sent_ns: None,
            need_snapshot: false,
        }
    }

    pub fn phase(&self) -> &GuestPhase {
        &self.phase
    }
    pub fn queue_items(&self) -> &[QueueItem] {
        &self.queue.queue().items
    }
    pub fn anchor(&self) -> Option<&Anchor> {
        self.anchor.as_ref()
    }
    pub fn clock(&self) -> &ClockSync {
        &self.clock
    }
    pub fn set_seek_only(&mut self, on: bool) {
        self.drift.seek_only = on;
    }

    pub fn hello(&self) -> Msg {
        Msg::Hello { proto: PROTO_VERSION, secret: self.secret.clone(), name: self.name.clone() }
    }

    /// New connection to the same host (network change, app resumed): the
    /// clock is re-measured and the queue caught up through `Resync`.
    pub fn on_reconnect(&mut self, now_ns: i64) -> Vec<Msg> {
        if matches!(self.phase, GuestPhase::Ended(_) | GuestPhase::Rejected(_)) {
            return Vec::new();
        }
        self.clock.reset();
        self.drift.reset();
        self.pings_sent = 0;
        self.outstanding.clear();
        self.last_heard_ns = now_ns;
        let mut v = vec![self.hello()];
        if self.queue.has_snapshot() {
            v.push(Msg::Resync { have: self.queue.queue().version });
        }
        v
    }

    fn ended(&mut self, r: EndReason) -> Vec<GuestEvent> {
        if matches!(self.phase, GuestPhase::Ended(_)) {
            return Vec::new();
        }
        self.phase = GuestPhase::Ended(r);
        vec![GuestEvent::Ended(r)]
    }

    fn take_anchor(&mut self, a: Anchor) -> bool {
        let newer = self.anchor.as_ref().is_none_or(|cur| a.is_newer_than(cur));
        if newer {
            if self.anchor.as_ref().is_none_or(|cur| cur.item_id != a.item_id || cur.track_id != a.track_id || !a.playing || !cur.playing) {
                self.drift.reset();
            }
            self.anchor = Some(a);
        }
        newer
    }

    /// `before`: queue version before the update, so a partial advance (one
    /// that still leaves a gap) is reported too.
    fn queue_result(&mut self, r: GuestApply, before: u64, now_ns: i64, out: &mut Vec<Msg>, ev: &mut Vec<GuestEvent>) {
        if !matches!(r, GuestApply::Changed) && self.queue.queue().version != before {
            ev.push(GuestEvent::QueueChanged);
        }
        match r {
            GuestApply::Changed => {
                self.gap_since = None;
                ev.push(GuestEvent::QueueChanged);
            }
            GuestApply::Ignored => {}
            GuestApply::Gap { .. } => {
                self.gap_since.get_or_insert(now_ns);
            }
            GuestApply::NeedSnapshot => {
                self.gap_since = None;
                self.need_snapshot = true;
                // Version 0 is never in the log: forces a snapshot.
                out.push(Msg::Resync { have: u64::MAX });
                self.resync_sent_ns = Some(now_ns);
            }
        }
    }

    /// Handles a host message. Returns messages to send and events for the UI.
    pub fn on_message(&mut self, msg: Msg, now_ns: i64) -> (Vec<Msg>, Vec<GuestEvent>) {
        let mut out = Vec::new();
        let mut ev = Vec::new();
        if matches!(self.phase, GuestPhase::Ended(_) | GuestPhase::Rejected(_)) {
            return (out, ev);
        }
        self.last_heard_ns = now_ns;
        if self.phase == GuestPhase::Joining && !matches!(msg, Msg::Welcome { .. } | Msg::Reject { .. }) {
            return (out, ev); // state arrives with the Welcome
        }
        match msg {
            Msg::Welcome { proto, host_name, snapshot, anchor, .. } => {
                if proto != PROTO_VERSION {
                    self.phase = GuestPhase::Rejected(RejectReason::IncompatibleVersion);
                    ev.push(GuestEvent::Rejected(RejectReason::IncompatibleVersion));
                    return (out, ev);
                }
                let first = self.phase == GuestPhase::Joining;
                self.phase = GuestPhase::Joined;
                self.host_name.clone_from(&host_name);
                // A different host epoch means the host restarted: start over.
                if self.anchor.as_ref().is_some_and(|a| a.epoch != anchor.epoch) {
                    self.queue = GuestQueue::new();
                    self.anchor = None;
                }
                let before = self.queue.queue().version;
                let r = self.queue.apply_snapshot(snapshot);
                self.queue_result(r, before, now_ns, &mut out, &mut ev);
                if self.take_anchor(anchor) {
                    ev.push(GuestEvent::PlaybackChanged);
                }
                if first {
                    ev.insert(0, GuestEvent::Joined { host_name });
                }
            }
            Msg::Reject { reason, .. } => {
                self.phase = GuestPhase::Rejected(reason);
                ev.push(GuestEvent::Rejected(reason));
            }
            Msg::Pong { id, t0, t1, t2 } => {
                if self.outstanding.remove(&id) == Some(t0) {
                    self.clock.add(Sample { t0, t1, t2, t3: now_ns });
                }
            }
            Msg::Anchor(a) => {
                if self.take_anchor(a) {
                    ev.push(GuestEvent::PlaybackChanged);
                }
            }
            Msg::Op(op) => {
                let before = self.queue.queue().version;
                let r = self.queue.apply(op);
                self.queue_result(r, before, now_ns, &mut out, &mut ev);
            }
            Msg::Ops { ops } => {
                let before = self.queue.queue().version;
                let mut last = GuestApply::Ignored;
                for op in ops {
                    last = self.queue.apply(op);
                    if last == GuestApply::NeedSnapshot {
                        break;
                    }
                }
                self.resync_sent_ns = None;
                let changed = self.queue.queue().version != before;
                let r = if changed && matches!(last, GuestApply::Ignored) { GuestApply::Changed } else { last };
                self.queue_result(r, before, now_ns, &mut out, &mut ev);
            }
            Msg::Snapshot(s) => {
                self.resync_sent_ns = None;
                self.need_snapshot = false;
                let before = self.queue.queue().version;
                let r = self.queue.apply_snapshot(s);
                self.queue_result(r, before, now_ns, &mut out, &mut ev);
            }
            Msg::End { reason } => ev.extend(self.ended(reason)),
            Msg::Heartbeat { .. } | Msg::Hello { .. } | Msg::Ping { .. } | Msg::Resync { .. } | Msg::Bye | Msg::Unknown => {}
        }
        (out, ev)
    }

    /// Call about every 100-250 ms. Sends pings and resyncs, detects a lost host.
    pub fn tick(&mut self, now_ns: i64) -> (Vec<Msg>, Vec<GuestEvent>) {
        let mut out = Vec::new();
        let mut ev = Vec::new();
        match self.phase {
            GuestPhase::Ended(_) | GuestPhase::Rejected(_) => return (out, ev),
            GuestPhase::Joining | GuestPhase::Joined => {}
        }
        if now_ns - self.last_heard_ns > HOST_TIMEOUT_NS {
            ev.extend(self.ended(EndReason::HostLost));
            return (out, ev);
        }
        if self.phase == GuestPhase::Joined {
            let interval = if self.pings_sent < BURST_PINGS { BURST_INTERVAL_NS } else { PING_INTERVAL_NS };
            if now_ns - self.last_ping_ns >= interval {
                let id = self.next_ping_id;
                self.next_ping_id += 1;
                self.pings_sent += 1;
                self.last_ping_ns = now_ns;
                self.outstanding.insert(id, now_ns);
                // Lost pongs must not pile up.
                while self.outstanding.len() > 16 {
                    self.outstanding.pop_first();
                }
                out.push(Msg::Ping { id, t0: now_ns });
            }
            if let Some(since) = self.gap_since
                && now_ns - since > GAP_TIMEOUT_NS
                && self.resync_sent_ns.is_none_or(|t| now_ns - t > GAP_TIMEOUT_NS)
            {
                out.push(Msg::Resync { have: self.queue.queue().version });
                self.resync_sent_ns = Some(now_ns);
            } else if self.need_snapshot && self.resync_sent_ns.is_none_or(|t| now_ns - t > GAP_TIMEOUT_NS) {
                out.push(Msg::Resync { have: u64::MAX });
                self.resync_sent_ns = Some(now_ns);
            }
        }
        (out, ev)
    }

    /// Where the player should be now (`latency_ms`: audio output latency
    /// plus the user's manual offset). `None` until the clock is calibrated.
    pub fn target(&self, now_ns: i64, latency_ms: i64) -> Option<Target> {
        let a = self.anchor.as_ref()?;
        let host_now = self.clock.to_host(now_ns)?;
        let pos = a.position_at(host_now).saturating_add(if a.playing { latency_ms } else { 0 });
        Some(Target { item_id: a.item_id.clone(), track_id: a.track_id.clone(), position_ms: pos.max(0), playing: a.playing })
    }

    /// Drift correction for a player at `actual_ms` playing the target item.
    pub fn correction(&mut self, now_ns: i64, latency_ms: i64, actual_ms: i64) -> Correction {
        match self.target(now_ns, latency_ms) {
            Some(t) if t.playing => self.drift.decide(t.position_ms, actual_ms, now_ns),
            _ => Correction::None,
        }
    }

    pub fn leave(&mut self) -> Vec<Msg> {
        self.ended(EndReason::Left);
        vec![Msg::Bye]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;
    const S: i64 = 1_000 * MS;

    #[test]
    fn resync_spam_is_rate_limited() {
        let mut h = HostSession::new("Host".into(), 0);
        let secret = h.secret.clone();
        h.on_message("g", Msg::Hello { proto: PROTO_VERSION, secret, name: "G".into() }, 0);
        h.queue_op(OpKind::Replace { items: items(3) }).unwrap();
        let snaps = |o: &[Out]| o.iter().filter(|o| matches!(o.msg, Msg::Snapshot(_))).count();
        let mut answered = 0;
        // 100 requests within 100 ms: only the first is answered.
        for i in 0..100 {
            answered += snaps(&h.on_message("g", Msg::Resync { have: u64::MAX }, S + i * MS));
        }
        assert_eq!(answered, 1);
        // A retry after the guest's own gap timeout is answered.
        assert_eq!(snaps(&h.on_message("g", Msg::Resync { have: u64::MAX }, S + GAP_TIMEOUT_NS)), 1);
    }

    fn items(n: usize) -> Vec<QueueItem> {
        (0..n).map(|i| QueueItem { item_id: format!("i{i}"), track_id: format!("t{i}") }).collect()
    }

    /// In-memory link: guest clock = host clock - `skew`, one-way delay `lat`.
    struct World {
        host: HostSession,
        guest: GuestSession,
        skew: i64,
        lat: i64,
        events: Vec<GuestEvent>,
    }

    impl World {
        fn new(skew: i64, lat: i64) -> Self {
            let host = HostSession::new("Host".into(), 100 * S);
            let guest = GuestSession::new(host.secret.clone(), "Guest".into(), 100 * S - skew);
            Self { host, guest, skew, lat, events: Vec::new() }
        }
        /// Delivers guest messages sent at host time `t` and the replies.
        fn send_guest(&mut self, msgs: Vec<Msg>, t: i64) {
            for m in msgs {
                let replies = self.host.on_message("g", m, t + self.lat);
                self.deliver(replies, t + self.lat);
            }
        }
        fn deliver(&mut self, outs: Vec<Out>, t: i64) {
            for o in outs {
                let (back, ev) = self.guest.on_message(o.msg, t + self.lat - self.skew);
                self.events.extend(ev);
                self.send_guest(back, t + self.lat);
            }
        }
        fn guest_tick(&mut self, t: i64) {
            let (m, ev) = self.guest.tick(t - self.skew);
            self.events.extend(ev);
            self.send_guest(m, t);
        }
        fn join(&mut self, t: i64) {
            let h = self.guest.hello();
            self.send_guest(vec![h], t);
        }
    }

    #[test]
    fn dropped_snapshot_request_is_retried() {
        use crate::live::queue::Snapshot;
        let mut w = World::new(0, MS);
        w.join(100 * S);
        let resyncs = |m: &[Msg]| m.iter().filter(|m| matches!(m, Msg::Resync { have: u64::MAX })).count();
        // An unusable snapshot makes the guest ask for a new one; the host
        // drops that request (rate limit), so nothing comes back.
        let t = 101 * S;
        let (out, _) = w.guest.on_message(Msg::Snapshot(Snapshot { version: u64::MAX, items: vec![] }), t);
        assert_eq!(resyncs(&out), 1);
        let (out, _) = w.guest.tick(t + GAP_TIMEOUT_NS / 2);
        assert_eq!(resyncs(&out), 0, "no retry before the timeout");
        let (out, _) = w.guest.tick(t + GAP_TIMEOUT_NS + MS);
        assert_eq!(resyncs(&out), 1, "retried after the timeout");
        // A good snapshot stops the retries.
        let good = w.host.queue.snapshot();
        w.guest.on_message(Msg::Snapshot(good), t + GAP_TIMEOUT_NS + 2 * MS);
        let (out, _) = w.guest.tick(t + 3 * GAP_TIMEOUT_NS);
        assert_eq!(resyncs(&out), 0);
    }

    #[test]
    fn join_snapshot_and_clock_then_target_matches_host() {
        let mut w = World::new(3_600 * S, 25 * MS);
        w.host.queue_op(OpKind::Replace { items: items(5) }).unwrap();
        w.host.set_playback(Some("i1".into()), Some("t1".into()), 10_000, true, 100 * S);
        w.join(101 * S);
        assert!(matches!(w.events[0], GuestEvent::Joined { .. }));
        assert_eq!(w.guest.queue_items(), w.host.queue_items());
        let mut t = 101 * S;
        for _ in 0..10 {
            t += 200 * MS;
            w.guest_tick(t);
        }
        let off = w.guest.clock().offset().expect("calibrated");
        assert!((off - 3_600 * S).abs() < MS, "offset {off}");
        let target = w.guest.target(t - w.skew, 0).unwrap();
        let host_pos = w.host.anchor().position_at(t);
        assert!((target.position_ms - host_pos).abs() <= 1, "{} vs {host_pos}", target.position_ms);
        assert_eq!(target.item_id.as_deref(), Some("i1"));
        // Output latency moves the target ahead.
        assert_eq!(w.guest.target(t - w.skew, 200).unwrap().position_ms, target.position_ms + 200);
    }

    #[test]
    fn rejects_wrong_secret_and_version_and_full() {
        let mut h = HostSession::new("H".into(), 0);
        let r = h.on_message("x", Msg::Hello { proto: 1, secret: "nope".into(), name: "x".into() }, 0);
        assert_eq!(r[0].msg, Msg::Reject { reason: RejectReason::BadSecret, proto: PROTO_VERSION });
        let r = h.on_message("x", Msg::Hello { proto: 2, secret: h.secret.clone(), name: "x".into() }, 0);
        assert_eq!(r[0].msg, Msg::Reject { reason: RejectReason::IncompatibleVersion, proto: PROTO_VERSION });
        for i in 0..MAX_GUESTS {
            h.on_message(&format!("p{i}"), Msg::Hello { proto: 1, secret: h.secret.clone(), name: "x".into() }, 0);
        }
        let r = h.on_message("late", Msg::Hello { proto: 1, secret: h.secret.clone(), name: "x".into() }, 0);
        assert_eq!(r[0].msg, Msg::Reject { reason: RejectReason::Full, proto: PROTO_VERSION });
        // Unknown peers cannot ping or resync.
        assert!(h.on_message("stranger", Msg::Ping { id: 0, t0: 0 }, 0).is_empty());
    }

    #[test]
    fn stale_or_duplicate_anchor_ignored() {
        let mut w = World::new(0, MS);
        w.join(0);
        let a1 = w.host.set_playback(Some("i".into()), None, 0, true, S);
        let a2 = w.host.set_playback(Some("j".into()), None, 0, true, 2 * S);
        w.deliver(a2.clone(), 2 * S);
        w.events.clear();
        w.deliver(a1, 2 * S);
        w.deliver(a2, 2 * S);
        assert!(w.events.is_empty());
        assert_eq!(w.guest.anchor().unwrap().item_id.as_deref(), Some("j"));
    }

    #[test]
    fn seek_spam_is_debounced() {
        let mut h = HostSession::new("H".into(), 0);
        assert_eq!(h.set_playback(Some("i".into()), None, 0, true, S).len(), 1);
        assert!(h.set_playback(Some("i".into()), None, 5_000, true, S + 50 * MS).is_empty());
        assert!(h.set_playback(Some("i".into()), None, 9_000, true, S + 100 * MS).is_empty());
        let out = h.tick(S + 300 * MS);
        let Msg::Anchor(a) = &out[0].msg else { panic!("{out:?}") };
        assert_eq!(a.position_ms, 9_000, "only the last seek goes out");
        // A track change is never delayed.
        assert_eq!(h.set_playback(Some("k".into()), None, 0, true, S + 310 * MS).len(), 1);
    }

    #[test]
    fn track_change_without_item_id_is_not_debounced() {
        let mut h = HostSession::new("H".into(), 0);
        assert_eq!(h.set_playback(None, Some("t1".into()), 0, true, S).len(), 1);
        assert_eq!(h.set_playback(None, Some("t2".into()), 0, true, S + MS).len(), 1);
    }

    #[test]
    fn ops_batch_leaving_a_gap_keeps_resync_timer() {
        use super::super::queue::{Op, VersionedOp};
        let mut w = World::new(0, MS);
        w.host.queue_op(OpKind::Replace { items: items(3) }).unwrap();
        w.join(0);
        let v = w.guest.queue_items().len();
        assert_eq!(v, 3);
        let have = 1; // snapshot version after one Replace
        let mk = |ver: u64, id: &str| VersionedOp { version: ver, op: Op { op_id: id.into(), kind: OpKind::Remove { item_id: format!("i{}", ver - 2) } } };
        let (_, ev) = w.guest.on_message(Msg::Ops { ops: vec![mk(have + 1, "a"), mk(have + 3, "c")] }, S);
        assert!(ev.contains(&GuestEvent::QueueChanged), "{ev:?}");
        let (out, _) = w.guest.tick(S + GAP_TIMEOUT_NS + 10 * MS);
        assert!(out.iter().any(|m| matches!(m, Msg::Resync { .. })), "{out:?}");
    }

    #[test]
    fn single_op_partial_advance_reports_change() {
        use super::super::queue::{Op, VersionedOp};
        let mut w = World::new(0, MS);
        w.host.queue_op(OpKind::Replace { items: items(4) }).unwrap();
        w.join(0);
        let mk = |ver: u64| VersionedOp { version: ver, op: Op { op_id: format!("o{ver}"), kind: OpKind::Remove { item_id: format!("i{}", ver - 2) } } };
        let (_, ev) = w.guest.on_message(Msg::Op(mk(4)), S);
        assert!(!ev.contains(&GuestEvent::QueueChanged));
        let (_, ev) = w.guest.on_message(Msg::Op(mk(2)), S);
        assert!(ev.contains(&GuestEvent::QueueChanged), "{ev:?}");
    }

    #[test]
    fn oversized_catch_up_falls_back_to_snapshot() {
        let mut w = World::new(0, MS);
        w.join(0);
        let big = |p: &str| (0..500).map(|i| QueueItem { item_id: format!("{p}{i:031}"), track_id: format!("{i:09}") }).collect();
        w.host.queue_op(OpKind::Replace { items: big("a") }).unwrap();
        w.host.queue_op(OpKind::Replace { items: big("b") }).unwrap();
        let out = w.host.on_message("g", Msg::Resync { have: 0 }, S);
        assert!(matches!(out[0].msg, Msg::Snapshot(_)), "{:?}", out[0].msg);
        assert!(out.iter().all(|o| super::super::proto::encode(&o.msg).is_ok()));
    }

    #[test]
    fn lost_ops_recovered_by_resync() {
        let mut w = World::new(0, MS);
        w.host.queue_op(OpKind::Replace { items: items(3) }).unwrap();
        w.join(0);
        let lost = w.host.queue_op(OpKind::Remove { item_id: "i0".into() }).unwrap();
        drop(lost);
        let o = w.host.queue_op(OpKind::Clear).unwrap();
        w.deliver(o, S);
        assert_ne!(w.guest.queue_items(), w.host.queue_items());
        w.guest_tick(S + GAP_TIMEOUT_NS + 10 * MS);
        assert_eq!(w.guest.queue_items(), w.host.queue_items());
    }

    #[test]
    fn reconnect_catches_up_and_recalibrates() {
        let mut w = World::new(0, MS);
        w.host.queue_op(OpKind::Replace { items: items(3) }).unwrap();
        w.join(0);
        for i in 1..12 {
            w.guest_tick(i * 200 * MS);
        }
        assert!(w.guest.clock().offset().is_some());
        // Connection drops; host keeps going.
        w.host.peer_disconnected("g");
        w.host.queue_op(OpKind::Remove { item_id: "i1".into() }).unwrap();
        w.host.set_playback(Some("i2".into()), Some("t2".into()), 0, true, 5 * S);
        let msgs = w.guest.on_reconnect(6 * S);
        assert!(w.guest.clock().offset().is_none(), "clock re-measured");
        w.send_guest(msgs, 6 * S);
        assert_eq!(w.guest.queue_items(), w.host.queue_items());
        assert_eq!(w.guest.anchor().unwrap().item_id.as_deref(), Some("i2"));
        assert_eq!(*w.guest.phase(), GuestPhase::Joined);
    }

    #[test]
    fn host_restart_new_epoch_replaces_state() {
        let mut w = World::new(0, MS);
        w.host.set_playback(Some("old".into()), None, 0, true, S);
        w.join(S);
        let secret = w.host.secret.clone();
        w.host = HostSession::new("Host".into(), 50 * S);
        w.host.secret = secret;
        w.host.set_playback(Some("new".into()), None, 0, true, 51 * S);
        let msgs = w.guest.on_reconnect(52 * S);
        w.send_guest(msgs, 52 * S);
        assert_eq!(w.guest.anchor().unwrap().item_id.as_deref(), Some("new"));
    }

    #[test]
    fn host_end_and_host_lost() {
        let mut w = World::new(0, MS);
        w.join(0);
        let o = w.host.end();
        w.deliver(o, S);
        assert_eq!(*w.guest.phase(), GuestPhase::Ended(EndReason::HostEnded));
        assert!(w.events.contains(&GuestEvent::Ended(EndReason::HostEnded)));

        let mut w = World::new(0, MS);
        w.join(0);
        w.guest_tick(HOST_TIMEOUT_NS - S);
        assert_eq!(*w.guest.phase(), GuestPhase::Joined);
        // Heartbeats keep it alive.
        let hb = w.host.tick(HOST_TIMEOUT_NS);
        w.deliver(hb, HOST_TIMEOUT_NS);
        w.guest_tick(HOST_TIMEOUT_NS + 5 * S);
        assert_eq!(*w.guest.phase(), GuestPhase::Joined);
        // Silence: ended once, with a clear reason.
        let (_, ev) = w.guest.tick(3 * HOST_TIMEOUT_NS);
        assert_eq!(ev, vec![GuestEvent::Ended(EndReason::HostLost)]);
        let (_, ev) = w.guest.tick(4 * HOST_TIMEOUT_NS);
        assert!(ev.is_empty());
        assert!(w.guest.on_reconnect(4 * HOST_TIMEOUT_NS).is_empty());
    }

    #[test]
    fn paused_target_is_fixed_and_no_correction() {
        let mut w = World::new(0, MS);
        w.join(0);
        for i in 1..12 {
            w.guest_tick(i * 200 * MS);
        }
        let o = w.host.set_playback(Some("i".into()), None, 42_000, false, 3 * S);
        w.deliver(o, 3 * S);
        assert_eq!(w.guest.target(60 * S, 300).unwrap().position_ms, 42_000);
        assert_eq!(w.guest.correction(60 * S, 0, 0), Correction::None);
    }

    #[test]
    fn drift_correction_uses_host_time() {
        let mut w = World::new(7 * S, 10 * MS);
        w.join(0);
        for i in 1..12 {
            w.guest_tick(i * 200 * MS);
        }
        let o = w.host.set_playback(Some("i".into()), None, 0, true, 3 * S);
        w.deliver(o, 3 * S);
        let local = 13 * S - 7 * S; // host time 13 s => host at 10_000 ms
        assert_eq!(w.guest.correction(local, 0, 10_000), Correction::None);
        assert_eq!(w.guest.correction(local, 0, 9_900), Correction::Rate(1.02));
        assert_eq!(w.guest.correction(local + 10 * MS, 0, 5_000), Correction::Seek(10_010));
    }

    #[test]
    fn worst_case_welcome_and_snapshot_fit_frame() {
        use crate::live::queue::{QueueItem, MAX_ID_LEN};
        use crate::live::MAX_QUEUE;
        let mut h = HostSession::new("\u{0}".repeat(500), 0);
        let items: Vec<QueueItem> = (0..MAX_QUEUE)
            .map(|i| QueueItem { item_id: format!("{i:0>MAX_ID_LEN$}"), track_id: "9".repeat(MAX_ID_LEN) })
            .collect();
        h.queue_op(OpKind::Replace { items }).unwrap();
        h.set_playback(Some("x".repeat(MAX_ID_LEN)), Some("y".repeat(MAX_ID_LEN)), i64::MAX, true, 0);
        let w = h.on_message("p", Msg::Hello { proto: PROTO_VERSION, secret: h.secret.clone(), name: "g".into() }, 0);
        assert!(matches!(&w[0].msg, Msg::Welcome { .. }));
        crate::live::proto::encode(&w[0].msg).unwrap();
        for o in &h.on_message("p", Msg::Resync { have: 0 }, 0) {
            crate::live::proto::encode(&o.msg).unwrap();
        }
        // Oversized or escaping ids are rejected on ingest.
        let bad = vec![QueueItem { item_id: "a".repeat(MAX_ID_LEN + 1), track_id: "t".into() }];
        assert_eq!(h.queue_op(OpKind::Replace { items: bad }), Err(QueueError::InvalidItem));
        let bad = vec![QueueItem { item_id: "a\"b".into(), track_id: "t".into() }];
        assert_eq!(h.queue_op(OpKind::Replace { items: bad }), Err(QueueError::InvalidItem));
    }
}
