//! Shared queue: a full snapshot once, then versioned operations.
//!
//! Entries carry their own `item_id`, so the same track can appear twice.
//! Positions are expressed as "after item X" (never indices) so concurrent
//! edits resolve against the order the host already applied. Each op has an
//! `op_id` for deduplication; the host assigns consecutive `version`s.

use std::collections::{BTreeMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use super::MAX_QUEUE;

/// Longest `item_id` / `track_id` accepted (bytes). Together with the ASCII
/// charset this bounds a full snapshot well under `MAX_FRAME`.
pub const MAX_ID_LEN: usize = 40;

/// Ids are short ASCII tokens (no JSON escaping, bounded size).
fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ID_LEN && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
}

impl QueueItem {
    pub fn is_valid(&self) -> bool {
        valid_id(&self.item_id) && valid_id(&self.track_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub item_id: String,
    pub track_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum OpKind {
    /// Insert after `after` (`None`: at the front).
    Insert { after: Option<String>, items: Vec<QueueItem> },
    Remove { item_id: String },
    Move { item_id: String, after: Option<String> },
    Clear,
    /// Replace the whole queue (new context, shuffle order resolved by host).
    Replace { items: Vec<QueueItem> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub op_id: String,
    pub kind: OpKind,
}

/// An op accepted by the host: applying it moves the queue to `version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedOp {
    pub version: u64,
    pub op: Op,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u64,
    pub items: Vec<QueueItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueError {
    #[error("queue limit ({MAX_QUEUE}) reached")]
    Full,
    #[error("unknown queue entry {0}")]
    UnknownItem(String),
    #[error("duplicate queue entry {0}")]
    DuplicateItem(String),
    #[error("invalid queue entry id")]
    InvalidItem,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queue {
    pub version: u64,
    pub items: Vec<QueueItem>,
}

impl Queue {
    fn pos(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|i| i.item_id == id)
    }

    fn insert_index(&self, after: Option<&String>) -> Result<usize, QueueError> {
        match after {
            None => Ok(0),
            Some(a) => self.pos(a).map(|p| p + 1).ok_or_else(|| QueueError::UnknownItem(a.clone())),
        }
    }

    /// A snapshot from the network: within limits, unique item ids, and a
    /// version that can still advance.
    pub fn valid_snapshot(s: &Snapshot) -> bool {
        let mut seen: HashSet<&str> = HashSet::new();
        s.items.len() <= MAX_QUEUE && s.version < u64::MAX && s.items.iter().all(|i| i.is_valid() && seen.insert(&i.item_id))
    }

    fn check_new(&self, items: &[QueueItem], existing: usize) -> Result<(), QueueError> {
        if existing + items.len() > MAX_QUEUE {
            return Err(QueueError::Full);
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for it in items {
            if !it.is_valid() {
                return Err(QueueError::InvalidItem);
            }
            if !seen.insert(&it.item_id) || (existing > 0 && self.pos(&it.item_id).is_some()) {
                return Err(QueueError::DuplicateItem(it.item_id.clone()));
            }
        }
        Ok(())
    }

    /// Validates and applies `kind` without touching `version`. On error the
    /// queue is unchanged.
    pub fn apply_kind(&mut self, kind: &OpKind) -> Result<(), QueueError> {
        match kind {
            OpKind::Insert { after, items } => {
                self.check_new(items, self.items.len())?;
                let at = self.insert_index(after.as_ref())?;
                self.items.splice(at..at, items.iter().cloned());
            }
            OpKind::Remove { item_id } => {
                let p = self.pos(item_id).ok_or_else(|| QueueError::UnknownItem(item_id.clone()))?;
                self.items.remove(p);
            }
            OpKind::Move { item_id, after } => {
                if after.as_ref() == Some(item_id) {
                    return Err(QueueError::UnknownItem(item_id.clone()));
                }
                let p = self.pos(item_id).ok_or_else(|| QueueError::UnknownItem(item_id.clone()))?;
                if let Some(a) = after
                    && self.pos(a).is_none()
                {
                    return Err(QueueError::UnknownItem(a.clone()));
                }
                let it = self.items.remove(p);
                let at = self.insert_index(after.as_ref()).expect("checked");
                self.items.insert(at, it);
            }
            OpKind::Clear => self.items.clear(),
            OpKind::Replace { items } => {
                let empty = Queue::default();
                empty.check_new(items, 0)?;
                self.items.clone_from(items);
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot { version: self.version, items: self.items.clone() }
    }
}

/// Ops kept for guests that fell behind; older gaps get a snapshot.
pub const OP_LOG: usize = 200;

/// Host side: validates, orders and logs ops.
#[derive(Debug, Default, Clone)]
pub struct HostQueue {
    queue: Queue,
    log: VecDeque<VersionedOp>,
    seen_ops: HashSet<String>,
    seen_order: VecDeque<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    New(VersionedOp),
    /// Same `op_id` already applied: nothing to broadcast.
    Duplicate,
}

impl HostQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    pub fn snapshot(&self) -> Snapshot {
        self.queue.snapshot()
    }

    pub fn apply(&mut self, op: Op) -> Result<Accepted, QueueError> {
        if self.seen_ops.contains(&op.op_id) {
            return Ok(Accepted::Duplicate);
        }
        self.queue.apply_kind(&op.kind)?;
        self.queue.version += 1;
        self.seen_ops.insert(op.op_id.clone());
        self.seen_order.push_back(op.op_id.clone());
        if self.seen_order.len() > OP_LOG * 4
            && let Some(old) = self.seen_order.pop_front()
        {
            self.seen_ops.remove(&old);
        }
        let v = VersionedOp { version: self.queue.version, op };
        self.log.push_back(v.clone());
        if self.log.len() > OP_LOG {
            self.log.pop_front();
        }
        Ok(Accepted::New(v))
    }

    /// Ops a guest at `have` needs, or `None` if the log no longer covers them
    /// (send a snapshot instead).
    pub fn ops_since(&self, have: u64) -> Option<Vec<VersionedOp>> {
        if have > self.queue.version {
            return None;
        }
        if have == self.queue.version {
            return Some(Vec::new());
        }
        let first = self.log.front()?.version;
        if have + 1 < first {
            return None;
        }
        Some(self.log.iter().filter(|o| o.version > have).cloned().collect())
    }
}

/// Guest side: applies ops strictly in version order.
#[derive(Debug, Default, Clone)]
pub struct GuestQueue {
    queue: Queue,
    has_snapshot: bool,
    pending: BTreeMap<u64, VersionedOp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestApply {
    /// Queue changed.
    Changed,
    /// Old or duplicate: ignored.
    Ignored,
    /// Buffered behind a gap; ask the host for what is missing if it persists.
    Gap { have: u64 },
    /// The op could not apply on our copy: we diverged, need a snapshot.
    NeedSnapshot,
}

/// Ops buffered behind a gap before we give up and ask for a snapshot.
pub const MAX_PENDING: usize = 256;

impl GuestQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    pub fn has_snapshot(&self) -> bool {
        self.has_snapshot
    }

    /// Replaces our copy. Buffered ops newer than the snapshot are kept and
    /// applied if they follow it.
    pub fn apply_snapshot(&mut self, s: Snapshot) -> GuestApply {
        if self.has_snapshot && s.version < self.queue.version {
            return GuestApply::Ignored;
        }
        if !Queue::valid_snapshot(&s) {
            return GuestApply::NeedSnapshot;
        }
        self.queue = Queue { version: s.version, items: s.items };
        self.has_snapshot = true;
        let v = self.queue.version;
        self.pending.retain(|k, _| *k > v);
        if !self.drain() {
            return GuestApply::NeedSnapshot;
        }
        if self.pending.is_empty() { GuestApply::Changed } else { GuestApply::Gap { have: v } }
    }

    pub fn apply(&mut self, op: VersionedOp) -> GuestApply {
        if !self.has_snapshot {
            self.pending.insert(op.version, op);
            if self.pending.len() > MAX_PENDING {
                self.pending.clear();
                return GuestApply::NeedSnapshot;
            }
            return GuestApply::Gap { have: 0 };
        }
        if op.version <= self.queue.version {
            return GuestApply::Ignored;
        }
        if op.version - self.queue.version > 1 {
            self.pending.insert(op.version, op);
            if self.pending.len() > MAX_PENDING {
                self.pending.clear();
                return GuestApply::NeedSnapshot;
            }
            return GuestApply::Gap { have: self.queue.version };
        }
        if self.queue.apply_kind(&op.op.kind).is_err() {
            return GuestApply::NeedSnapshot;
        }
        self.queue.version = op.version;
        if !self.drain() {
            return GuestApply::NeedSnapshot;
        }
        if self.pending.is_empty() { GuestApply::Changed } else { GuestApply::Gap { have: self.queue.version } }
    }

    /// Applies buffered ops that now follow. `false` if one failed.
    fn drain(&mut self) -> bool {
        while let Some(next) = self.queue.version.checked_add(1)
            && let Some(op) = self.pending.remove(&next)
        {
            if self.queue.apply_kind(&op.op.kind).is_err() {
                self.pending.clear();
                return false;
            }
            self.queue.version = op.version;
        }
        let v = self.queue.version;
        self.pending.retain(|k, _| *k > v);
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::assertions_on_result_states, clippy::manual_ok_err, reason = "tests")]
mod tests {
    use super::*;

    fn it(id: &str, t: &str) -> QueueItem {
        QueueItem { item_id: id.into(), track_id: t.into() }
    }
    fn op(id: &str, kind: OpKind) -> Op {
        Op { op_id: id.into(), kind }
    }
    fn ids(q: &Queue) -> Vec<&str> {
        q.items.iter().map(|i| i.item_id.as_str()).collect()
    }
    fn host_with(n: usize) -> HostQueue {
        let mut h = HostQueue::new();
        let items = (0..n).map(|i| it(&format!("i{i}"), &format!("t{i}"))).collect();
        h.apply(op("init", OpKind::Replace { items })).unwrap();
        h
    }

    #[test]
    fn same_track_twice_allowed() {
        let mut h = HostQueue::new();
        h.apply(op("a", OpKind::Replace { items: vec![it("x", "t1"), it("y", "t1")] })).unwrap();
        assert_eq!(h.queue().items.len(), 2);
    }

    #[test]
    fn duplicate_item_id_rejected() {
        let mut h = host_with(2);
        let e = h.apply(op("a", OpKind::Insert { after: None, items: vec![it("i0", "t")] }));
        assert_eq!(e, Err(QueueError::DuplicateItem("i0".into())));
        assert_eq!(h.queue().version, 1);
    }

    #[test]
    fn insert_move_remove_by_after() {
        let mut h = host_with(3);
        h.apply(op("a", OpKind::Insert { after: Some("i0".into()), items: vec![it("n", "t")] })).unwrap();
        assert_eq!(ids(h.queue()), ["i0", "n", "i1", "i2"]);
        h.apply(op("b", OpKind::Move { item_id: "i0".into(), after: Some("i2".into()) })).unwrap();
        assert_eq!(ids(h.queue()), ["n", "i1", "i2", "i0"]);
        h.apply(op("c", OpKind::Move { item_id: "i2".into(), after: None })).unwrap();
        assert_eq!(ids(h.queue()), ["i2", "n", "i1", "i0"]);
        h.apply(op("d", OpKind::Remove { item_id: "n".into() })).unwrap();
        assert_eq!(ids(h.queue()), ["i2", "i1", "i0"]);
        assert_eq!(h.queue().version, 5);
    }

    #[test]
    fn invalid_ops_leave_queue_untouched() {
        let mut h = host_with(3);
        let before = h.queue().clone();
        assert!(h.apply(op("a", OpKind::Remove { item_id: "zz".into() })).is_err());
        assert!(h.apply(op("b", OpKind::Move { item_id: "i0".into(), after: Some("zz".into()) })).is_err());
        assert!(h.apply(op("c", OpKind::Move { item_id: "i0".into(), after: Some("i0".into()) })).is_err());
        assert!(h.apply(op("d", OpKind::Insert { after: Some("zz".into()), items: vec![it("q", "t")] })).is_err());
        assert_eq!(h.queue(), &before);
    }

    #[test]
    fn cap_500() {
        let mut h = host_with(MAX_QUEUE);
        assert_eq!(h.apply(op("a", OpKind::Insert { after: None, items: vec![it("x", "t")] })), Err(QueueError::Full));
        let too_many = (0..=MAX_QUEUE).map(|i| it(&format!("z{i}"), "t")).collect();
        assert_eq!(h.apply(op("b", OpKind::Replace { items: too_many })), Err(QueueError::Full));
        assert_eq!(h.queue().items.len(), MAX_QUEUE);
    }

    #[test]
    fn host_dedupes_op_ids() {
        let mut h = host_with(1);
        let o = op("same", OpKind::Insert { after: None, items: vec![it("x", "t")] });
        assert!(matches!(h.apply(o.clone()), Ok(Accepted::New(_))));
        assert_eq!(h.apply(o), Ok(Accepted::Duplicate));
        assert_eq!(h.queue().items.len(), 2);
    }

    #[test]
    fn concurrent_moves_serialize_on_host() {
        // Two guests both move relative to i1 based on the same old view.
        let mut h = host_with(4);
        h.apply(op("g1", OpKind::Move { item_id: "i3".into(), after: Some("i1".into()) })).unwrap();
        h.apply(op("g2", OpKind::Move { item_id: "i0".into(), after: Some("i1".into()) })).unwrap();
        assert_eq!(ids(h.queue()), ["i1", "i0", "i3", "i2"]);
        // A move of an entry someone removed is rejected, not misapplied.
        h.apply(op("g3", OpKind::Remove { item_id: "i2".into() })).unwrap();
        assert!(h.apply(op("g4", OpKind::Move { item_id: "i2".into(), after: None })).is_err());
    }

    fn replay(h: &HostQueue, from: u64) -> Vec<VersionedOp> {
        h.ops_since(from).unwrap()
    }

    #[test]
    fn guest_snapshot_then_ops_matches_host() {
        let mut h = host_with(5);
        let mut g = GuestQueue::new();
        g.apply_snapshot(h.snapshot());
        for (n, k) in [
            ("a", OpKind::Remove { item_id: "i2".into() }),
            ("b", OpKind::Insert { after: Some("i4".into()), items: vec![it("x", "t9")] }),
            ("c", OpKind::Move { item_id: "x".into(), after: None }),
        ] {
            let Accepted::New(v) = h.apply(op(n, k)).unwrap() else { panic!() };
            assert_eq!(g.apply(v), GuestApply::Changed);
        }
        assert_eq!(g.queue(), h.queue());
    }

    #[test]
    fn guest_reorders_out_of_order_ops() {
        let mut h = host_with(3);
        let mut g = GuestQueue::new();
        g.apply_snapshot(h.snapshot());
        let mut ops = Vec::new();
        for i in 0..4 {
            let Accepted::New(v) = h.apply(op(&format!("o{i}"), OpKind::Insert { after: None, items: vec![it(&format!("n{i}"), "t")] })).unwrap() else { panic!() };
            ops.push(v);
        }
        assert_eq!(g.apply(ops[2].clone()), GuestApply::Gap { have: 1 });
        assert_eq!(g.apply(ops[3].clone()), GuestApply::Gap { have: 1 });
        assert_eq!(g.apply(ops[0].clone()), GuestApply::Gap { have: 2 });
        assert_eq!(g.apply(ops[1].clone()), GuestApply::Changed);
        assert_eq!(g.queue(), h.queue());
        // Re-delivery is ignored (idempotent).
        assert_eq!(g.apply(ops[1].clone()), GuestApply::Ignored);
        assert_eq!(g.queue(), h.queue());
    }

    #[test]
    fn ops_before_snapshot_are_buffered() {
        let mut h = host_with(2);
        let snap = h.snapshot();
        let Accepted::New(v) = h.apply(op("a", OpKind::Clear)).unwrap() else { panic!() };
        let mut g = GuestQueue::new();
        g.apply(v);
        g.apply_snapshot(snap);
        assert_eq!(g.queue(), h.queue());
    }

    #[test]
    fn stale_snapshot_ignored() {
        let mut h = host_with(2);
        let old = h.snapshot();
        h.apply(op("a", OpKind::Clear)).unwrap();
        let mut g = GuestQueue::new();
        g.apply_snapshot(h.snapshot());
        assert_eq!(g.apply_snapshot(old), GuestApply::Ignored);
        assert_eq!(g.queue(), h.queue());
    }

    #[test]
    fn log_window_and_snapshot_fallback() {
        let mut h = host_with(0);
        for i in 0..(OP_LOG + 50) {
            h.apply(op(&format!("o{i}"), OpKind::Insert { after: None, items: vec![it(&format!("n{i}"), "t")] })).unwrap_or_else(|_| panic!());
        }
        let v = h.queue().version;
        assert_eq!(h.ops_since(v).unwrap().len(), 0);
        assert_eq!(replay(&h, v - 10).len(), 10);
        assert!(h.ops_since(1).is_none(), "beyond log: snapshot");
        assert!(h.ops_since(v + 5).is_none(), "guest ahead (host restarted): snapshot");
    }

    #[test]
    fn reconnect_catches_up_from_log() {
        let mut h = host_with(3);
        let mut g = GuestQueue::new();
        g.apply_snapshot(h.snapshot());
        // Guest disconnects; host keeps editing.
        for i in 0..5 {
            let _ = h.apply(op(&format!("o{i}"), OpKind::Remove { item_id: format!("i{}", i % 3) }));
            h.apply(op(&format!("p{i}"), OpKind::Insert { after: None, items: vec![it(&format!("r{i}"), "t")] })).unwrap();
        }
        for v in h.ops_since(g.queue().version).unwrap() {
            g.apply(v);
        }
        assert_eq!(g.queue(), h.queue());
    }

    #[test]
    fn divergence_requests_snapshot() {
        let mut h = host_with(2);
        let mut g = GuestQueue::new();
        g.apply_snapshot(Snapshot { version: 1, items: vec![] }); // wrong copy
        let Accepted::New(v) = h.apply(op("a", OpKind::Remove { item_id: "i0".into() })).unwrap() else { panic!() };
        assert_eq!(g.apply(v), GuestApply::NeedSnapshot);
        assert_eq!(g.apply_snapshot(h.snapshot()), GuestApply::Changed);
        assert_eq!(g.queue(), h.queue());
    }

    #[test]
    fn guest_rejects_invalid_snapshots() {
        let mut g = GuestQueue::new();
        let dup = Snapshot { version: 1, items: vec![it("a", "t"), it("a", "t")] };
        assert_eq!(g.apply_snapshot(dup), GuestApply::NeedSnapshot);
        let big = Snapshot { version: 1, items: (0..=MAX_QUEUE).map(|i| it(&format!("i{i}"), "t")).collect() };
        assert_eq!(g.apply_snapshot(big), GuestApply::NeedSnapshot);
        let max = Snapshot { version: u64::MAX, items: vec![] };
        assert_eq!(g.apply_snapshot(max), GuestApply::NeedSnapshot);
        assert!(!g.has_snapshot());
    }

    #[test]
    fn snapshot_with_conflicting_buffered_op_needs_snapshot() {
        let mut g = GuestQueue::new();
        let bad = VersionedOp { version: 2, op: op("x", OpKind::Remove { item_id: "nope".into() }) };
        assert_eq!(g.apply(bad), GuestApply::Gap { have: 0 });
        let s = Snapshot { version: 1, items: vec![it("a", "t")] };
        assert_eq!(g.apply_snapshot(s), GuestApply::NeedSnapshot);
    }

    #[test]
    fn pending_before_snapshot_is_capped() {
        let mut g = GuestQueue::new();
        let mut last = GuestApply::Ignored;
        for v in 1..=(MAX_PENDING as u64 + 1) {
            last = g.apply(VersionedOp { version: v, op: op(&format!("o{v}"), OpKind::Clear) });
        }
        assert_eq!(last, GuestApply::NeedSnapshot);
    }
}
