//! The aggregator: turns measurements into streaming statistics, SPC state,
//! and a terminal report.
//!
//! Holds the CUSUM/EWMA detectors (armed after a warmup window), the
//! histograms, and the stake-weighted accumulator. `push_measurement`
//! returns an immediate alert line when something crosses a configured
//! threshold or a detector fires.

use crate::stats::{weighted, Cusum, Ewma, Histogram, Robust, Welford};
use crate::stake::StakeMap;
use crate::tracker::Measurement;

/// How many stake-weighted samples to keep (bounded memory).
const WEIGHTED_SAMPLE_CAP: usize = 8192;

/// One detected regime shift.
#[derive(Debug, Clone)]
pub struct RegimeShift {
    pub slot: u64,
    /// The warmup baseline the shift was detected against (ms).
    pub baseline_mean_ms: f64,
    /// +1 = finality got slower, -1 = faster.
    pub direction: i8,
    /// Detected at this unix ms.
    pub at_ms: u64,
}

/// Counters that are not distributions.
#[derive(Debug, Default, Clone)]
pub struct Counters {
    pub slots_rooted: u64,
    pub dead_slots: u64,
    pub unknown_kinds: u64,
    pub clock_skew: u64,
    pub reconnects: u64,
    pub outliers: u64,
}

/// The watchdog aggregator.
pub struct Watchdog {
    warmup: u64,
    alert_ms: Option<f64>,
    ewma_alpha: f64,
    cusum_h: f64,
    finality_hist: Histogram,
    finality_welford: Welford,
    ewma: Option<Ewma>,
    cusum: Option<Cusum>,
    robust: Robust,
    confirm_hist: Histogram,
    root_gap_hist: Histogram,
    overhead_hist: Histogram,
    weighted_samples: Vec<(f64, f64)>,
    counters: Counters,
    shifts: Vec<RegimeShift>,
    now_ms: fn() -> u64,
}

impl Watchdog {
    /// Creates a watchdog; `now_ms` is injectable for tests.
    pub fn new(
        warmup: u64,
        precision_bits: u32,
        ewma_alpha: f64,
        cusum_h: f64,
        alert_ms: Option<f64>,
        now_ms: fn() -> u64,
    ) -> Self {
        Self {
            warmup,
            alert_ms,
            ewma_alpha,
            cusum_h,
            finality_hist: Histogram::new(precision_bits),
            finality_welford: Welford::default(),
            ewma: None,
            cusum: None,
            robust: Robust::new(1024),
            confirm_hist: Histogram::new(precision_bits),
            root_gap_hist: Histogram::new(precision_bits),
            overhead_hist: Histogram::new(precision_bits),
            weighted_samples: Vec::with_capacity(WEIGHTED_SAMPLE_CAP),
            counters: Counters::default(),
            shifts: Vec::new(),
            now_ms,
        }
    }

    /// Whether the detectors are armed (warmup complete).
    pub fn armed(&self) -> bool {
        self.cusum.is_some()
    }

    /// Counters (slots rooted, dead, skew, …).
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Detected regime shifts, oldest first.
    pub fn shifts(&self) -> &[RegimeShift] {
        &self.shifts
    }

    /// Records one measurement; returns an immediate alert line when the
    /// sample crosses the configured threshold or a detector fires.
    pub fn push_measurement(
        &mut self,
        m: &Measurement,
        stake: Option<u64>,
    ) -> Option<String> {
        self.counters.slots_rooted += 1;
        let finality = m.finality_ms;
        self.finality_hist.push(finality as u64);
        self.finality_welford.push(finality);
        self.robust.push(finality);
        if m.confirm_ms.is_some() {
            self.confirm_hist.push(m.confirm_ms.unwrap() as u64);
        }
        if m.root_gap_ms.is_some() {
            self.root_gap_hist.push(m.root_gap_ms.unwrap() as u64);
        }
        match m.network_overhead_ms {
            Some(overhead) => self.overhead_hist.push(overhead as u64),
            None => self.counters.clock_skew += 1,
        }
        if let Some(stake) = stake {
            if self.weighted_samples.len() == WEIGHTED_SAMPLE_CAP {
                self.weighted_samples.remove(0);
            }
            self.weighted_samples.push((finality, stake as f64));
        }

        let mut alerts: Vec<String> = Vec::new();

        // Outlier classification (robust — heavy tails cannot fool it).
        if let Some(true) = self.robust.is_outlier(finality) {
            self.counters.outliers += 1;
        }

        // Arm the detectors after the warmup window.
        if self.cusum.is_none() && self.counters.slots_rooted >= self.warmup {
            let mean = self.finality_welford.mean().unwrap_or_default();
            let sigma = self
                .finality_welford
                .stddev()
                .unwrap_or(f64::MIN_POSITIVE)
                .max(f64::MIN_POSITIVE);
            self.cusum = Some(Cusum::new(mean, sigma, 0.5, self.cusum_h));
            self.ewma = Some(Ewma::new(mean, sigma, self.ewma_alpha, 3.0));
        }

        if let Some(cusum) = self.cusum.as_mut() {
            if cusum.push(finality) {
                let direction = cusum.last_direction().unwrap_or(1);
                self.shifts.push(RegimeShift {
                    slot: m.slot,
                    baseline_mean_ms: cusum.baseline().0,
                    direction,
                    at_ms: (self.now_ms)(),
                });
                let word = if direction > 0 { "SLOWER" } else { "FASTER" };
                alerts.push(format!(
                    "REGIME SHIFT at slot {}: finality trending {} than baseline {:.0}ms",
                    m.slot, word, cusum.baseline().0
                ));
            }
        }
        if let Some(ewma) = self.ewma.as_mut() {
            if ewma.push(finality) {
                alerts.push(format!(
                    "EWMA drift at slot {}: smoothed {:.0}ms outside [{:.0}, {:.0}]",
                    m.slot,
                    ewma.value(),
                    ewma.lcl(),
                    ewma.ucl()
                ));
            }
        }

        if let Some(alert_ms) = self.alert_ms {
            if finality > alert_ms {
                alerts.push(format!(
                    "THRESHOLD at slot {}: finality {:.0}ms > {:.0}ms",
                    m.slot, finality, alert_ms
                ));
            }
        }

        if alerts.is_empty() {
            None
        } else {
            Some(alerts.join(" | "))
        }
    }

    /// Counts an unknown update kind (forward compatibility).
    pub fn note_unknown_kind(&mut self) {
        self.counters.unknown_kinds += 1;
    }

    /// Counts a reconnect.
    pub fn note_reconnect(&mut self) {
        self.counters.reconnects += 1;
    }

    /// Counts a dead slot.
    pub fn note_dead(&mut self) {
        self.counters.dead_slots += 1;
    }

    /// Renders the terminal report.
    pub fn render(&self, current_slot: u64, stake_map: Option<&StakeMap>) -> String {
        let mut out = String::new();
        out.push_str(&format!("=== finality watchdog @ slot {current_slot} ===\n"));
        out.push_str(&format!(
            "rooted {} | dead {} | in-flight — | reconnects {} | unknown kinds {} | clock skew {}\n",
            self.counters.slots_rooted,
            self.counters.dead_slots,
            self.counters.reconnects,
            self.counters.unknown_kinds,
            self.counters.clock_skew,
        ));

        if self.counters.slots_rooted == 0 {
            out.push_str("waiting for the first rooted slot…\n");
            return out;
        }

        let f = &self.finality_hist;
        let w = &self.finality_welford;
        out.push_str(&format!(
            "finality (n={}): mean {:.0}ms ± {:.0} | p50 {:.0} | p95 {:.0} | p99 {:.0} | p99.9 {:.0} | min {:.0} | max {:.0}\n",
            f.count(),
            w.mean().unwrap_or_default(),
            w.stddev().unwrap_or_default(),
            f.percentile(50.0).unwrap_or_default(),
            f.percentile(95.0).unwrap_or_default(),
            f.percentile(99.0).unwrap_or_default(),
            f.percentile(99.9).unwrap_or_default(),
            f.min().unwrap_or_default(),
            f.max().unwrap_or_default(),
        ));

        if self.confirm_hist.count() > 0 {
            out.push_str(&format!(
                "confirm (n={}): mean {:.0}ms | p50 {:.0} | p99 {:.0}\n",
                self.confirm_hist.count(),
                self.confirm_hist.mean().unwrap_or_default(),
                self.confirm_hist.percentile(50.0).unwrap_or_default(),
                self.confirm_hist.percentile(99.0).unwrap_or_default(),
            ));
        }
        if self.root_gap_hist.count() > 0 {
            out.push_str(&format!(
                "root gap (n={}): mean {:.0}ms | p50 {:.0} | p99 {:.0}\n",
                self.root_gap_hist.count(),
                self.root_gap_hist.mean().unwrap_or_default(),
                self.root_gap_hist.percentile(50.0).unwrap_or_default(),
                self.root_gap_hist.percentile(99.0).unwrap_or_default(),
            ));
        }
        if self.overhead_hist.count() > 0 {
            out.push_str(&format!(
                "network overhead (n={}): mean {:.0}ms | p50 {:.0} | p99 {:.0}\n",
                self.overhead_hist.count(),
                self.overhead_hist.mean().unwrap_or_default(),
                self.overhead_hist.percentile(50.0).unwrap_or_default(),
                self.overhead_hist.percentile(99.0).unwrap_or_default(),
            ));
        }

        // Stake-weighted statistics.
        if !self.weighted_samples.is_empty() {
            let unweighted_p50 = f.percentile(50.0).unwrap_or_default();
            let weighted_p50 =
                weighted::weighted_percentile(&self.weighted_samples, 50.0).unwrap_or_default();
            let weighted_mean = weighted::weighted_mean(&self.weighted_samples).unwrap_or_default();
            out.push_str(&format!(
                "stake-weighted (n={}): p50 {:.0}ms (unweighted {:.0}) | mean {:.0}ms",
                self.weighted_samples.len(),
                weighted_p50,
                unweighted_p50,
                weighted_mean,
            ));
            if let Some(map) = stake_map {
                out.push_str(&format!(
                    " | epoch {} active stake {:.1}M SOL | delinquent {:.2}%",
                    map.epoch,
                    map.total_active_stake() as f64 / 1_000_000_000_000.0,
                    map.delinquent_fraction * 100.0,
                ));
            }
            out.push('\n');
        }

        // SPC state.
        match self.cusum.as_ref() {
            None => out.push_str(&format!(
                "SPC: warming up {}/{} samples\n",
                self.counters.slots_rooted, self.warmup
            )),
            Some(c) => {
                out.push_str(&format!(
                    "SPC: armed (baseline {:.0}ms ± {:.0}) | CUSUM S+ {:.1} S- {:.1} | alarms {}",
                    c.baseline().0,
                    c.baseline().1,
                    c.s_plus(),
                    c.s_minus(),
                    c.alarms(),
                ));
                if let Some(e) = self.ewma.as_ref() {
                    out.push_str(&format!(
                        " | EWMA {:.0} [{:.0}, {:.0}] breaches {}",
                        e.value(),
                        e.lcl(),
                        e.ucl(),
                        e.breaches(),
                    ));
                }
                out.push('\n');
            }
        }

        if self.counters.outliers > 0 {
            out.push_str(&format!(
                "outliers (modified z > 3.5) in window: {}\n",
                self.counters.outliers
            ));
        }
        if !self.shifts.is_empty() {
            out.push_str("regime shifts:\n");
            for shift in self.shifts.iter().rev().take(5).rev() {
                let word = if shift.direction > 0 { "slower" } else { "faster" };
                out.push_str(&format!(
                    "  slot {} (at {} UTC): {} than baseline {:.0}ms\n",
                    shift.slot,
                    crate::format_unix_ms(shift.at_ms),
                    word,
                    shift.baseline_mean_ms
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::Measurement;

    fn measurement(slot: u64, finality_ms: f64) -> Measurement {
        Measurement {
            slot,
            finality_ms,
            confirm_ms: Some(finality_ms / 4.0),
            root_gap_ms: Some(finality_ms * 3.0 / 4.0),
            network_overhead_ms: Some(5.0),
        }
    }

    fn watchdog() -> Watchdog {
        Watchdog::new(10, 8, 0.1, 5.0, None, || 1_000_000)
    }

    /// Warmup then stable stream: no alerts, detectors armed.
    #[test]
    fn warmup_then_stable() {
        let mut wd = watchdog();
        for slot in 1..=10u64 {
            assert!(wd.push_measurement(&measurement(slot, 2000.0), None).is_none());
        }
        assert!(wd.armed());
        for slot in 11..=100u64 {
            assert!(wd.push_measurement(&measurement(slot, 2000.0), None).is_none());
        }
        assert_eq!(wd.counters().slots_rooted, 100);
        assert_eq!(wd.counters().outliers, 0);
    }

    /// A regime shift is detected and recorded.
    #[test]
    fn regime_shift_detected() {
        let mut wd = watchdog();
        // Varied warmup so sigma > 0: a constant stream degenerates the
        // CUSUM baseline (sigma clamps to MIN_POSITIVE and both directions
        // collapse).
        for slot in 1..=10u64 {
            let f = 2000.0 + (slot % 3) as f64; // 2000, 2001, 2002
            wd.push_measurement(&measurement(slot, f), None);
        }
        assert!(wd.armed());
        let mut shift_seen = false;
        for slot in 11..=200u64 {
            if wd.push_measurement(&measurement(slot, 1500.0), None).is_some() {
                shift_seen = true;
                break;
            }
        }
        assert!(shift_seen, "regime shift must be detected");
        assert!(!wd.shifts().is_empty());
        assert_eq!(wd.shifts()[0].direction, -1);
        assert!((wd.shifts()[0].baseline_mean_ms - 2001.0).abs() < 1.0);
    }

    /// Threshold alerts fire immediately.
    #[test]
    fn threshold_alerts() {
        let mut wd = Watchdog::new(10, 8, 0.1, 5.0, Some(3000.0), || 1_000_000);
        for slot in 1..=5u64 {
            assert!(wd.push_measurement(&measurement(slot, 2000.0), None).is_none());
        }
        let alert = wd
            .push_measurement(&measurement(6, 4000.0), None)
            .expect("threshold alert");
        assert!(alert.contains("THRESHOLD"), "alert: {alert}");
    }

    /// Stake-weighted samples accumulate and render.
    #[test]
    fn stake_weighted_accumulates() {
        let mut wd = watchdog();
        for slot in 1..=20u64 {
            wd.push_measurement(&measurement(slot, 2000.0), Some(1000));
        }
        assert_eq!(wd.counters().slots_rooted, 20);
        let report = wd.render(20, None);
        assert!(report.contains("stake-weighted"), "report:\n{report}");
    }

    /// Clock skew counts separately and the overhead histogram skips it.
    #[test]
    fn clock_skew_counted() {
        let mut wd = watchdog();
        let mut m = measurement(1, 2000.0);
        m.network_overhead_ms = None;
        wd.push_measurement(&m, None);
        assert_eq!(wd.counters().clock_skew, 1);
    }

    /// Report renders the warming-up state before any samples.
    #[test]
    fn renders_warmup_state() {
        let wd = watchdog();
        let report = wd.render(0, None);
        assert!(report.contains("waiting for the first rooted slot"));
        let mut wd2 = watchdog();
        wd2.push_measurement(&measurement(1, 2000.0), None);
        let report = wd2.render(1, None);
        assert!(report.contains("SPC: warming up 1/10"), "report:\n{report}");
    }
}
