//! Robust anomaly detection: median, MAD, and modified z-scores
//! (Iglewicz & Hoaglin).
//!
//! Unlike mean/stddev z-scores, the modified z-score `0.6745·(x - median)/MAD`
//! cannot be fooled by the heavy tails latency distributions have: the median
//! and MAD have a 50% breakdown point, so a burst of outliers does not drag
//! the baseline with it.

use std::collections::VecDeque;

/// Bounded sample window with robust (median/MAD) statistics.
#[derive(Debug)]
pub struct Robust {
    window: VecDeque<f64>,
    max_samples: usize,
}

/// Iglewicz-Hoaglin modified z-score threshold for outliers.
pub const OUTLIER_Z_THRESHOLD: f64 = 3.5;

impl Robust {
    /// Keeps the last `max_samples` samples.
    pub fn new(max_samples: usize) -> Self {
        assert!(max_samples >= 8, "window too small for robust statistics");
        Self {
            window: VecDeque::with_capacity(max_samples),
            max_samples,
        }
    }

    /// Records a sample, evicting the oldest when full.
    pub fn push(&mut self, x: f64) {
        if self.window.len() == self.max_samples {
            self.window.pop_front();
        }
        self.window.push_back(x);
    }

    /// Sample count. Exercised by unit tests; kept for API completeness.
    #[allow(dead_code)]
    pub fn count(&self) -> usize {
        self.window.len()
    }

    /// Median of the window.
    pub fn median(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.window.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        if n % 2 == 1 {
            Some(sorted[n / 2])
        } else {
            Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0)
        }
    }

    /// Median absolute deviation of the window.
    pub fn mad(&self) -> Option<f64> {
        let med = self.median()?;
        let mut devs: Vec<f64> = self.window.iter().map(|&x| (x - med).abs()).collect();
        if devs.is_empty() {
            return None;
        }
        devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = devs.len();
        let mad = if n % 2 == 1 {
            devs[n / 2]
        } else {
            (devs[n / 2 - 1] + devs[n / 2]) / 2.0
        };
        // MAD of a constant window is 0: fall back to a floor so z-scores
        // stay finite (a constant stream has no outliers by definition).
        Some(if mad <= 0.0 { f64::MIN_POSITIVE } else { mad })
    }

    /// Modified z-score of `x` against the window (Iglewicz-Hoaglin).
    pub fn modified_z(&self, x: f64) -> Option<f64> {
        let med = self.median()?;
        let mad = self.mad()?;
        Some(0.6745 * (x - med) / mad)
    }

    /// Whether `x` is an outlier against the window (|modified z| > 3.5).
    pub fn is_outlier(&self, x: f64) -> Option<bool> {
        self.modified_z(x).map(|z| z.abs() > OUTLIER_Z_THRESHOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Normal-ish data: no false outliers.
    #[test]
    fn normal_data_no_outliers() {
        let mut r = Robust::new(64);
        for i in 0..64u64 {
            r.push(2000.0 + (i % 11) as f64);
        }
        for i in 0..11u64 {
            assert_eq!(r.is_outlier(2000.0 + i as f64), Some(false), "i={i}");
        }
    }

    /// A heavy-tail burst does not drag the baseline: outliers stay detectable.
    #[test]
    fn heavy_tail_not_fooled() {
        let mut r = Robust::new(64);
        for i in 0..48u64 {
            r.push(2000.0 + (i % 11) as f64);
        }
        // Burst of 16 large values fills a quarter of the window.
        for _ in 0..16 {
            r.push(50_000.0);
        }
        assert_eq!(r.is_outlier(50_000.0), Some(true));
        // 48 of 64 samples are baseline (75% majority): the median and MAD
        // still sit at the baseline, so the baseline itself is NOT flagged;
        // the burst cannot drag the baseline with it.
        assert_eq!(r.is_outlier(2000.0), Some(false));
    }

    /// A single extreme outlier is detected while the baseline holds.
    #[test]
    fn single_outlier_detected() {
        let mut r = Robust::new(64);
        for i in 0..63u64 {
            r.push(2000.0 + (i % 11) as f64);
        }
        assert_eq!(r.is_outlier(20_000.0), Some(true));
        // Baseline unaffected: median still ~2005.
        let med = r.median().unwrap();
        assert!((med - 2005.0).abs() < 10.0);
    }

    /// Window eviction keeps the newest samples.
    #[test]
    fn evicts_oldest() {
        let mut r = Robust::new(8);
        for i in 0..16u64 {
            r.push(i as f64);
        }
        assert_eq!(r.count(), 8);
        // Window is 8..=15 → median (11 + 12) / 2 = 11.5.
        assert_eq!(r.median(), Some(11.5));
    }

    /// Constant window: MAD floors, z-scores stay finite.
    #[test]
    fn constant_window_finite() {
        let mut r = Robust::new(8);
        for _ in 0..8 {
            r.push(100.0);
        }
        assert_eq!(r.median(), Some(100.0));
        let z = r.modified_z(100.0).unwrap();
        assert!(z.is_finite());
        assert_eq!(z, 0.0);
    }
}
