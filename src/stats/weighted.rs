//! Stake-weighted percentiles.
//!
//! The weights are the leader's activated stake at the time the slot was
//! produced, so the statistics answer "what latency does stake-weighted
//! consensus see?" rather than "what did this observer sample?" — a
//! stake-weighted p50 weights a top-stake leader's slot proportionally more
//! than a bottom-decile leader's slot.

/// Weighted nearest-rank percentile (0..=100) of `(value, weight)` pairs.
///
/// Sorts by value, accumulates weights, and returns the value where the
/// cumulative weight crosses `p/100 · total`. Weights ≤ 0 are ignored.
pub fn weighted_percentile(samples: &[(f64, f64)], p: f64) -> Option<f64> {
    debug_assert!((0.0..=100.0).contains(&p), "percentile in 0..=100");
    let mut entries: Vec<(f64, f64)> = samples.iter().copied().filter(|&(_, w)| w > 0.0).collect();
    if entries.is_empty() {
        return None;
    }
    entries.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let total_weight: f64 = entries.iter().map(|&(_, w)| w).sum();
    if total_weight <= 0.0 {
        return None;
    }
    let target = (p / 100.0) * total_weight;
    let mut cum = 0.0;
    for &(value, weight) in &entries {
        cum += weight;
        if cum >= target {
            return Some(value);
        }
    }
    entries.last().map(|&(v, _)| v)
}

/// Total stake of the sample set.
pub fn total_weight(samples: &[(f64, f64)]) -> f64 {
    samples.iter().map(|&(_, w)| w.max(0.0)).sum()
}

/// Mean latency weighted by stake.
pub fn weighted_mean(samples: &[(f64, f64)]) -> Option<f64> {
    let total = total_weight(samples);
    if total <= 0.0 {
        return None;
    }
    Some(samples.iter().map(|&(v, w)| v * w.max(0.0)).sum::<f64>() / total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Equal weights reduce to the ordinary percentile.
    #[test]
    fn equal_weights_match_ordinary() {
        let samples: Vec<(f64, f64)> = (1..=101u64).map(|v| (v as f64, 1.0)).collect();
        assert_eq!(weighted_percentile(&samples, 50.0), Some(51.0));
        assert_eq!(weighted_percentile(&samples, 0.0), Some(1.0));
        assert_eq!(weighted_percentile(&samples, 100.0), Some(101.0));
        assert_eq!(weighted_mean(&samples), Some(51.0));
    }

    /// A heavy weight drags the weighted percentile toward its cluster: 90%
    /// of stake at 100ms, 10% at 2000ms → weighted p50 ≈ 100ms even though
    /// the unweighted median sits between clusters.
    #[test]
    fn heavy_weight_dominates() {
        let mut samples: Vec<(f64, f64)> = Vec::new();
        samples.push((100.0, 90.0));
        samples.push((2000.0, 10.0));
        let p50 = weighted_percentile(&samples, 50.0).unwrap();
        assert!((p50 - 100.0).abs() < 1e-9, "weighted p50 {p50} must sit at 100");
        let p99 = weighted_percentile(&samples, 99.0).unwrap();
        assert!(p99 > 100.0, "p99 {p99} must cross into the heavy tail");
        let mean = weighted_mean(&samples).unwrap();
        assert!((mean - (100.0 * 0.9 + 2000.0 * 0.1)).abs() < 1e-9);
    }

    /// Non-positive weights are ignored.
    #[test]
    fn ignores_non_positive_weights() {
        let samples = [(100.0, 0.0), (200.0, -1.0), (300.0, 2.0)];
        assert_eq!(weighted_percentile(&samples, 50.0), Some(300.0));
        assert_eq!(weighted_mean(&samples), Some(300.0));
    }

    /// Empty input is None.
    #[test]
    fn empty_is_none() {
        assert_eq!(weighted_percentile(&[], 50.0), None);
        assert_eq!(weighted_mean(&[]), None);
    }
}
