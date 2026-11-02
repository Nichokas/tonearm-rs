//! Drift decisions: how a guest corrects its player towards the host.

/// Below this error nothing is done.
pub const DEADBAND_MS: i64 = 30;
/// Above this error the player seeks instead of nudging the rate.
pub const SEEK_MS: i64 = 300;
/// Rate change while catching up or slowing down.
pub const NUDGE: f64 = 0.02;
/// Minimum time between two corrections (hysteresis).
pub const MIN_INTERVAL_NS: i64 = 2_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Correction {
    /// Change nothing: keep the current rate (a nudge may still be active;
    /// its end is always reported explicitly as `Rate(1.0)`).
    None,
    /// Play at this rate until the next decision.
    Rate(f64),
    /// Seek to this position (ms) **and** play at rate 1.0: a seek always
    /// cancels any active nudge.
    Seek(i64),
}

#[derive(Debug, Clone, Default)]
pub struct DriftController {
    last_correction_ns: Option<i64>,
    /// Direction of the active nudge: `1` faster, `-1` slower, `0` none.
    nudging: i8,
    /// A nudge was forgotten by [`Self::reset`] while the player may still run
    /// at the nudged rate: the next decision restores 1.0 first.
    rate_dirty: bool,
    /// Seek only (low power mode, or a player without rate control).
    pub seek_only: bool,
}

impl DriftController {
    pub fn new() -> Self {
        Self::default()
    }

    /// `expected_ms`: where the host is (already including output latency);
    /// `actual_ms`: where the local player is; `now_ns`: local monotonic time.
    pub fn decide(&mut self, expected_ms: i64, actual_ms: i64, now_ns: i64) -> Correction {
        let err = expected_ms - actual_ms; // > 0: we are behind
        let abs = err.abs();
        if self.rate_dirty && !(abs > SEEK_MS || self.seek_only) {
            self.rate_dirty = false;
            return Correction::Rate(1.0);
        }
        if abs < DEADBAND_MS {
            if self.nudging != 0 {
                self.nudging = 0;
                return Correction::Rate(1.0);
            }
            return Correction::None;
        }
        // A large error always seeks right away; small ones respect hysteresis.
        if abs > SEEK_MS || (self.seek_only && abs >= DEADBAND_MS) {
            if abs <= SEEK_MS && self.too_soon(now_ns) {
                return Correction::None;
            }
            self.last_correction_ns = Some(now_ns);
            self.nudging = 0;
            self.rate_dirty = false;
            return Correction::Seek(expected_ms.max(0));
        }
        let dir: i8 = if err > 0 { 1 } else { -1 };
        if self.nudging == dir {
            return Correction::None; // already correcting this way
        }
        // A flipped error (e.g. we overshot, or the host seeked) is corrected
        // right away: holding the wrong-way nudge would only grow it.
        if self.nudging == 0 && self.too_soon(now_ns) {
            return Correction::None;
        }
        self.last_correction_ns = Some(now_ns);
        self.nudging = dir;
        Correction::Rate(1.0 + f64::from(dir) * NUDGE)
    }

    fn too_soon(&self, now_ns: i64) -> bool {
        self.last_correction_ns.is_some_and(|t| now_ns - t < MIN_INTERVAL_NS)
    }

    /// Forget hysteresis (new track, seek by host, resume after interruption).
    pub fn reset(&mut self) {
        self.last_correction_ns = None;
        self.rate_dirty |= self.nudging != 0;
        self.nudging = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: i64 = 1_000_000_000;

    #[test]
    fn deadband_ignored() {
        let mut d = DriftController::new();
        assert_eq!(d.decide(10_000, 10_029, 0), Correction::None);
        assert_eq!(d.decide(10_029, 10_000, 0), Correction::None);
    }

    #[test]
    fn small_error_nudges_rate_in_right_direction() {
        let mut d = DriftController::new();
        assert_eq!(d.decide(10_100, 10_000, 0), Correction::Rate(1.02)); // behind
        let mut d = DriftController::new();
        assert_eq!(d.decide(10_000, 10_100, 0), Correction::Rate(0.98)); // ahead
    }

    #[test]
    fn nudge_ends_when_back_in_deadband() {
        let mut d = DriftController::new();
        d.decide(10_100, 10_000, 0);
        assert_eq!(d.decide(10_200, 10_150, S), Correction::None); // still nudging
        assert_eq!(d.decide(10_300, 10_290, 2 * S), Correction::Rate(1.0));
        assert_eq!(d.decide(10_400, 10_390, 3 * S), Correction::None);
    }

    #[test]
    fn large_error_seeks_immediately_even_during_hysteresis() {
        let mut d = DriftController::new();
        d.decide(10_100, 10_000, 0);
        assert_eq!(d.decide(20_000, 10_000, 1), Correction::Seek(20_000));
    }

    #[test]
    fn boundaries() {
        let mut d = DriftController::new();
        assert_eq!(d.decide(10_300, 10_000, 0), Correction::Rate(1.02));
        let mut d = DriftController::new();
        assert_eq!(d.decide(10_301, 10_000, 0), Correction::Seek(10_301));
        let mut d = DriftController::new();
        assert_eq!(d.decide(10_030, 10_000, 0), Correction::Rate(1.02));
    }

    #[test]
    fn hysteresis_between_corrections() {
        let mut d = DriftController::new();
        d.decide(10_100, 10_000, 0);
        d.decide(10_010, 10_000, S / 2); // back to 1.0
        assert_eq!(d.decide(10_100, 10_000, S), Correction::None); // < 2 s
        assert_eq!(d.decide(10_100, 10_000, 2 * S), Correction::Rate(1.02));
    }

    #[test]
    fn seek_only_mode() {
        let mut d = DriftController { seek_only: true, ..Default::default() };
        assert_eq!(d.decide(10_100, 10_000, 0), Correction::Seek(10_100));
        assert_eq!(d.decide(10_100, 10_000, S), Correction::None);
        assert_eq!(d.decide(10_100, 10_000, 3 * S), Correction::Seek(10_100));
    }

    #[test]
    fn seek_cancels_nudge_so_no_stale_reset() {
        let mut d = DriftController::new();
        assert_eq!(d.decide(10_100, 10_000, 0), Correction::Rate(1.02));
        assert_eq!(d.decide(20_000, 10_000, 1), Correction::Seek(20_000));
        // Seek implies rate 1.0: back in the deadband nothing else is due.
        assert_eq!(d.decide(20_010, 20_000, S), Correction::None);
    }

    #[test]
    fn nudge_flips_when_error_changes_sign() {
        let mut d = DriftController::new();
        assert_eq!(d.decide(10_100, 10_000, 0), Correction::Rate(1.02));
        assert_eq!(d.decide(10_000, 10_100, 1), Correction::Rate(0.98));
        assert_eq!(d.decide(10_000, 10_100, 2), Correction::None);
    }

    #[test]
    fn reset_during_nudge_restores_rate() {
        let mut d = DriftController::new();
        assert_eq!(d.decide(10_100, 10_000, 0), Correction::Rate(1.02));
        d.reset(); // pause / reconnect
        assert_eq!(d.decide(10_010, 10_000, S), Correction::Rate(1.0));
        assert_eq!(d.decide(10_010, 10_000, 2 * S), Correction::None);
    }

    #[test]
    fn never_seeks_negative() {
        let mut d = DriftController::new();
        assert_eq!(d.decide(-500, 1_000, 0), Correction::Seek(0));
    }
}
