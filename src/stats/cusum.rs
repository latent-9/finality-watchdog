//! Two-sided CUSUM change-point detection — the core of regime-shift
//! detection.
//!
//! CUSUM accumulates standardized deviations from the baseline: it is the
//! sequential probability ratio test for a mean shift, and detects a change
//! of `δ` sigmas with average run length `O(1/δ²)` — the statistically
//! correct tool for "the network's finality regime just changed" (e.g. the
//! Alpenglow transition from ~2s to ~150ms).

/// Two-sided CUSUM over a stream, calibrated to a baseline.
#[derive(Debug, Clone)]
pub struct Cusum {
    /// Allowance: half the shift to detect, in sigma units.
    k: f64,
    /// Decision interval: alarm when the accumulated statistic exceeds `h`.
    h: f64,
    mu0: f64,
    sigma0: f64,
    s_plus: f64,
    s_minus: f64,
    samples: u64,
    alarms: u64,
    /// Direction of the last alarm: +1 mean increased, -1 mean decreased.
    last_direction: Option<i8>,
}

impl Cusum {
    /// Creates a CUSUM calibrated to `(mu0, sigma0)` — the warmup mean and
    /// stddev. `k` is the allowance in sigma units (0.5 detects a 1σ shift
    /// efficiently); `h` is the decision interval (4–5 gives an in-control
    /// ARL in the hundreds).
    pub fn new(mu0: f64, sigma0: f64, k: f64, h: f64) -> Self {
        assert!(sigma0 > 0.0, "sigma0 > 0");
        assert!(k > 0.0 && h > 0.0, "k, h > 0");
        Self {
            k,
            h,
            mu0,
            sigma0,
            s_plus: 0.0,
            s_minus: 0.0,
            samples: 0,
            alarms: 0,
            last_direction: None,
        }
    }

    /// Records a sample; returns whether a change-point was detected (the
    /// accumulator resets after an alarm, re-arming for the next shift).
    pub fn push(&mut self, x: f64) -> bool {
        self.samples += 1;
        let z = (x - self.mu0) / self.sigma0;
        self.s_plus = (self.s_plus + z - self.k).max(0.0);
        self.s_minus = (self.s_minus - z - self.k).max(0.0);
        if self.s_plus > self.h {
            self.alarms += 1;
            self.last_direction = Some(1);
            self.s_plus = 0.0;
            self.s_minus = 0.0;
            return true;
        }
        if self.s_minus > self.h {
            self.alarms += 1;
            self.last_direction = Some(-1);
            self.s_plus = 0.0;
            self.s_minus = 0.0;
            return true;
        }
        false
    }

    /// Current upper (mean-increase) accumulator.
    pub fn s_plus(&self) -> f64 {
        self.s_plus
    }

    /// Current lower (mean-decrease) accumulator.
    pub fn s_minus(&self) -> f64 {
        self.s_minus
    }

    /// Number of change-points detected.
    pub fn alarms(&self) -> u64 {
        self.alarms
    }

    /// Direction of the last alarm.
    pub fn last_direction(&self) -> Option<i8> {
        self.last_direction
    }

    /// Samples since calibration. Exercised by unit tests; kept for API
    /// completeness.
    #[allow(dead_code)]
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// The baseline this CUSUM was calibrated against.
    pub fn baseline(&self) -> (f64, f64) {
        (self.mu0, self.sigma0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An exactly-at-baseline stream never alarms (deterministic: z = 0 keeps
    /// both accumulators at zero forever).
    #[test]
    fn stable_stream_never_alarms() {
        let mut c = Cusum::new(100.0, 5.0, 0.5, 5.0);
        for _ in 0..10_000 {
            assert!(!c.push(100.0));
        }
        assert_eq!(c.alarms(), 0);
        assert_eq!(c.s_plus(), 0.0);
        assert_eq!(c.s_minus(), 0.0);
    }

    /// A persistent +1.5σ shift alarms deterministically: each sample adds
    /// 1.0 to the upper accumulator, so with h = 5 the alarm fires on the
    /// 6th sample.
    #[test]
    fn upward_shift_alarms_deterministically() {
        let mut c = Cusum::new(100.0, 5.0, 0.5, 5.0);
        let mut alarm_at = None;
        for i in 1..=20u64 {
            if c.push(107.5) {
                alarm_at = Some(i);
                break;
            }
        }
        assert_eq!(alarm_at, Some(6));
        assert_eq!(c.last_direction(), Some(1));
        assert_eq!(c.alarms(), 1);
        // Re-armed: accumulators reset after the alarm.
        assert_eq!(c.s_plus(), 0.0);
    }

    /// A persistent -1.5σ shift alarms downward.
    #[test]
    fn downward_shift_alarms() {
        let mut c = Cusum::new(100.0, 5.0, 0.5, 5.0);
        let mut alarm_at = None;
        for i in 1..=20u64 {
            if c.push(92.5) {
                alarm_at = Some(i);
                break;
            }
        }
        assert_eq!(alarm_at, Some(6));
        assert_eq!(c.last_direction(), Some(-1));
    }

    /// After an alarm the detector re-arms: a second shift is detected again.
    #[test]
    fn re_arms_after_alarm() {
        let mut c = Cusum::new(100.0, 5.0, 0.5, 5.0);
        let mut alarms = 0;
        // First shift, back to baseline, second shift.
        for _ in 0..6 {
            if c.push(107.5) {
                alarms += 1;
            }
        }
        for _ in 0..20 {
            c.push(100.0);
        }
        for _ in 0..6 {
            if c.push(92.5) {
                alarms += 1;
            }
        }
        assert_eq!(alarms, 2);
        assert_eq!(c.last_direction(), Some(-1));
    }

    /// CUSUM is shift-sensitive: a moderate outlier (z below h) does not
    /// alarm, but a huge one exceeds h in a single step — which is also an
    /// alarm (the watcher reports it; Robust handles outlier classification).
    #[test]
    fn outlier_semantics() {
        let mut c = Cusum::new(100.0, 5.0, 0.5, 5.0);
        // z = 0.8: s_plus = 0.3 — no alarm.
        assert!(!c.push(104.0));
        assert_eq!(c.alarms(), 0);
        // z = 20: s_plus = 19.5 > h — single-step alarm.
        assert!(c.push(200.0));
        assert_eq!(c.alarms(), 1);
    }
}
