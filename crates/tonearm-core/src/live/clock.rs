//! NTP-like estimation of the host clock offset.
//!
//! The guest sends `t0` (its clock), the host stamps receive `t1` and send
//! `t2` (host clock), the guest stamps `t3` on reply. Then
//! `offset = ((t1 - t0) + (t2 - t3)) / 2` and `rtt = (t3 - t0) - (t2 - t1)`.
//! Samples with the smallest RTT are the least skewed by queueing, so we keep
//! the minimum-RTT sample in a sliding window and smooth it with an EMA.

/// One completed ping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub t0: i64,
    pub t1: i64,
    pub t2: i64,
    pub t3: i64,
}

impl Sample {
    /// Saturating: peer-supplied stamps must not overflow.
    pub fn rtt(&self) -> i64 {
        self.t3.saturating_sub(self.t0).saturating_sub(self.t2.saturating_sub(self.t1))
    }
    /// host - local.
    pub fn offset(&self) -> i64 {
        i64::midpoint(self.t1.saturating_sub(self.t0), self.t2.saturating_sub(self.t3))
    }
}

/// Window of samples considered for the minimum RTT.
pub const WINDOW_NS: i64 = 30_000_000_000;
/// An offset jump above this resets the filter (route change, clock step).
pub const JUMP_NS: i64 = 50_000_000;
/// Samples needed before the estimate is trusted.
pub const MIN_SAMPLES: usize = 4;
const EMA_ALPHA: f64 = 0.3;

#[derive(Debug, Default, Clone)]
pub struct ClockSync {
    samples: Vec<Sample>,
    smoothed: Option<f64>,
    /// Samples since the last reset (start, jump or [`Self::reset`]).
    total: usize,
}

impl ClockSync {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a sample. Returns `false` for an impossible one (negative RTT),
    /// which is dropped.
    pub fn add(&mut self, s: Sample) -> bool {
        if s.rtt() < 0 || s.t3 < s.t0 || s.t2 < s.t1 {
            return false;
        }
        // A sample's offset error is at most rtt/2 (any delay asymmetry). One
        // that disagrees with the estimate by more than that plus JUMP_NS
        // means the clock or route jumped: restart from it.
        if let Some(prev) = self.smoothed
            && (s.offset() as f64 - prev).abs() > (JUMP_NS + s.rtt()) as f64
        {
            self.samples.clear();
            self.smoothed = None;
            self.total = 0;
        }
        self.samples.retain(|o| s.t3 - o.t3 <= WINDOW_NS);
        self.samples.push(s);
        self.total += 1;
        let best = self.best().expect("just pushed").offset() as f64;
        self.smoothed = Some(match self.smoothed {
            Some(prev) => prev + EMA_ALPHA * (best - prev),
            None => best,
        });
        true
    }

    fn best(&self) -> Option<&Sample> {
        self.samples.iter().min_by_key(|s| s.rtt())
    }

    /// Estimated host - local offset, once enough samples arrived.
    pub fn offset(&self) -> Option<i64> {
        (self.total >= MIN_SAMPLES).then(|| self.smoothed.map(|o| o.round() as i64)).flatten()
    }

    /// Smallest RTT in the window.
    pub fn min_rtt(&self) -> Option<i64> {
        self.best().map(Sample::rtt)
    }

    pub fn to_host(&self, local: i64) -> Option<i64> {
        self.offset().and_then(|o| local.checked_add(o))
    }

    /// Forget everything (after a reconnect or network change).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn sample_count(&self) -> usize {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Host clock = local + offset; symmetric one-way delays `up`/`down`.
    fn ping(at: i64, offset: i64, up: i64, down: i64, proc_: i64) -> Sample {
        let t0 = at;
        let t1 = at + up + offset;
        let t2 = t1 + proc_;
        let t3 = t2 - offset + down;
        Sample { t0, t1, t2, t3 }
    }

    #[test]
    fn exact_with_symmetric_delay() {
        let s = ping(1_000, 5_000_000_000, 20_000_000, 20_000_000, 1_000_000);
        assert_eq!(s.offset(), 5_000_000_000);
        assert_eq!(s.rtt(), 40_000_000);
    }

    #[test]
    fn needs_min_samples() {
        let mut c = ClockSync::new();
        for i in 0..3 {
            c.add(ping(i * 100_000_000, 7, 1_000_000, 1_000_000, 0));
            assert_eq!(c.offset(), None);
        }
        c.add(ping(400_000_000, 7, 1_000_000, 1_000_000, 0));
        assert_eq!(c.offset(), Some(7));
    }

    #[test]
    fn min_rtt_filters_asymmetric_outliers() {
        let off = 123_456_789;
        let mut c = ClockSync::new();
        // A clean sample, then congested ones with very asymmetric delay
        // (each alone would be off by tens of ms).
        c.add(ping(0, off, 5_000_000, 5_000_000, 0));
        for i in 1..10 {
            c.add(ping(i * 200_000_000, off, 80_000_000, 5_000_000, 0));
        }
        let est = c.offset().unwrap();
        assert!((est - off).abs() < 1_000_000, "est {est}");
        assert_eq!(c.min_rtt(), Some(10_000_000));
    }

    #[test]
    fn window_expires_old_best() {
        let mut c = ClockSync::new();
        c.add(ping(0, 0, 1_000_000, 1_000_000, 0));
        c.add(ping(WINDOW_NS + 1_000_000_000, 0, 10_000_000, 10_000_000, 0));
        assert_eq!(c.min_rtt(), Some(20_000_000));
    }

    #[test]
    fn rejects_negative_rtt() {
        let mut c = ClockSync::new();
        assert!(!c.add(Sample { t0: 100, t1: 0, t2: 1_000, t3: 150 }));
        assert_eq!(c.sample_count(), 0);
    }

    #[test]
    fn jump_resets_filter() {
        let mut c = ClockSync::new();
        for i in 0..5 {
            c.add(ping(i * 100_000_000, 0, 1_000_000, 1_000_000, 0));
        }
        assert_eq!(c.offset(), Some(0));
        // Host clock steps by 1 s (e.g. host app restarted its base).
        c.add(ping(600_000_000, 1_000_000_000, 2_000_000, 2_000_000, 0));
        // One post-jump sample is not enough to trust again.
        assert_eq!(c.offset(), None);
        for i in 0..3 {
            c.add(ping(700_000_000 + i * 100_000_000, 1_000_000_000, 2_000_000, 2_000_000, 0));
        }
        assert_eq!(c.offset(), Some(1_000_000_000));
    }

    #[test]
    fn hostile_stamps_do_not_overflow() {
        let mut c = ClockSync::new();
        for i in 0..4 {
            c.add(Sample { t0: i, t1: i64::MAX, t2: i64::MAX, t3: i + 1 });
        }
        assert_eq!(c.to_host(i64::MAX / 2 + 10), None);
    }

    #[test]
    fn ema_smooths_small_changes() {
        let mut c = ClockSync::new();
        for i in 0..4 {
            c.add(ping(i, 0, 1_000_000, 1_000_000, 0));
        }
        // A slightly better (lower RTT) sample with a 10 ms different offset
        // moves the estimate only part of the way.
        c.add(ping(10, 10_000_000, 500_000, 500_000, 0));
        let o = c.offset().unwrap();
        assert!(o > 0 && o < 10_000_000, "{o}");
    }
}
