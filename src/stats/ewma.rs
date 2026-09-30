//! EWMA control chart (Lucas & Saccucci) with time-varying control limits.
//!
//! The EWMA statistic `z_t = α·x_t + (1-α)·z_{t-1}` smooths noise while
//! remaining sensitive to persistent shifts: its limits START NARROW and
//! widen toward the asymptote `L·σ·sqrt(α/(2-α))`; the chart is most
//! sensitive right after calibration, and a drift of a fraction of a sigma
//! is flagged within a handful of samples, faster than a 3-sigma rule on the
//! raw values.

/// EWMA control chart over a stream, calibrated to a baseline.
#[derive(Debug, Clone)]
pub struct Ewma {
    alpha: f64,
    limits_l: f64,
    center: f64,
    sigma: f64,
    z: f64,
    t: u64,
    breaches: u64,
}

impl Ewma {
    /// Creates a chart calibrated to `(center, sigma)`: the warmup mean and
    /// stddev. `alpha` in (0, 1] controls smoothing; `limits_l` is the control
    /// limit width in sigma units (3.0 is the classical choice).
    pub fn new(center: f64, sigma: f64, alpha: f64, limits_l: f64) -> Self {
        assert!(alpha > 0.0 && alpha <= 1.0, "alpha in (0,1]");
        assert!(limits_l > 0.0, "limits_l > 0");
        Self {
            alpha,
            limits_l,
            center,
            sigma: sigma.max(f64::MIN_POSITIVE),
            z: center,
            t: 0,
            breaches: 0,
        }
    }

    /// Records a sample; returns whether the smoothed statistic breached a
    /// control limit (a persistent drift indicator, not a one-off outlier).
    pub fn push(&mut self, x: f64) -> bool {
        self.t += 1;
        self.z = self.alpha * x + (1.0 - self.alpha) * self.z;
        let factor = (self.alpha / (2.0 - self.alpha)
            * (1.0 - (1.0 - self.alpha).powi(2 * self.t as i32)))
        .sqrt();
        let ucl = self.center + self.limits_l * self.sigma * factor;
        let lcl = self.center - self.limits_l * self.sigma * factor;
        let breach = self.z > ucl || self.z < lcl;
        if breach {
            self.breaches += 1;
        }
        breach
    }

    /// Current smoothed statistic.
    pub fn value(&self) -> f64 {
        self.z
    }

    /// Upper control limit for the current sample index.
    pub fn ucl(&self) -> f64 {
        let factor = (self.alpha / (2.0 - self.alpha)
            * (1.0 - (1.0 - self.alpha).powi(2 * (self.t.max(1)) as i32)))
        .sqrt();
        self.center + self.limits_l * self.sigma * factor
    }

    /// Lower control limit for the current sample index.
    pub fn lcl(&self) -> f64 {
        let factor = (self.alpha / (2.0 - self.alpha)
            * (1.0 - (1.0 - self.alpha).powi(2 * (self.t.max(1)) as i32)))
        .sqrt();
        self.center - self.limits_l * self.sigma * factor
    }

    /// Number of limit breaches observed.
    pub fn breaches(&self) -> u64 {
        self.breaches
    }

    /// Samples seen since calibration. Exercised by unit tests; kept for
    /// API completeness.
    #[allow(dead_code)]
    pub fn samples(&self) -> u64 {
        self.t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A constant stream exactly at the baseline never breaches.
    #[test]
    fn stable_stream_never_breaches() {
        let mut e = Ewma::new(100.0, 5.0, 0.2, 3.0);
        for _ in 0..1000 {
            assert!(!e.push(100.0));
        }
        assert_eq!(e.breaches(), 0);
        assert!((e.value() - 100.0).abs() < 1e-9);
    }

    /// A persistent +2σ shift breaches within a bounded number of samples.
    #[test]
    fn persistent_shift_breaches() {
        let mut e = Ewma::new(100.0, 5.0, 0.2, 3.0);
        let mut breach_at = None;
        for i in 1..=200u64 {
            if e.push(110.0) {
                breach_at = Some(i);
                break;
            }
        }
        let at = breach_at.expect("must breach");
        assert!(at <= 30, "breach at sample {at}, expected within 30");
    }

    /// Random-ish noise around the baseline mostly stays in control.
    #[test]
    fn noise_stays_in_control() {
        // Deterministic pseudo-noise: alternating ±1σ pattern.
        let mut e = Ewma::new(100.0, 5.0, 0.1, 3.0);
        let mut breaches = 0;
        for i in 0..500u64 {
            let x = 100.0 + if i % 2 == 0 { 5.0 } else { -5.0 };
            if e.push(x) {
                breaches += 1;
            }
        }
        assert!(breaches <= 5, "symmetric noise breached {breaches} times");
    }

    /// Limits widen from the calibration point toward the asymptote: the
    /// chart is most sensitive right after calibration.
    #[test]
    fn limits_widen_to_asymptote() {
        let e = Ewma::new(100.0, 5.0, 0.2, 3.0);
        let early = e.ucl() - 100.0; // t=1
        let e_late = {
            let mut e2 = Ewma::new(100.0, 5.0, 0.2, 3.0);
            for _ in 0..50 {
                e2.push(100.0);
            }
            e2
        };
        let late = e_late.ucl() - 100.0;
        assert!(
            late > early,
            "late limit {late} must be wider than early {early}"
        );
        // Asymptote: L·σ·sqrt(α/(2-α)).
        let asymptote = 3.0 * 5.0 * (0.2 / 1.8f64).sqrt();
        assert!((late - asymptote).abs() < 1e-6);
    }
}
