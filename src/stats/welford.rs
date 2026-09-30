//! Welford's online algorithm for streaming mean and variance.
//!
//! Numerically stable: unlike the naive sum-of-squares formula, the update
//! never computes a difference of two large nearly-equal quantities, so the
//! variance stays accurate even for streams with a large mean and small
//! spread (e.g. finality latencies around 2s with ~10ms sigma).

/// Online mean / variance via Welford's algorithm.
#[derive(Debug, Clone, Default)]
pub struct Welford {
    n: u64,
    mean: f64,
    m2: f64,
}

impl Welford {
    /// Records a sample.
    pub fn push(&mut self, x: f64) {
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;
    }

    /// Number of samples. Exercised by unit tests; kept for API completeness.
    #[allow(dead_code)]
    pub fn count(&self) -> u64 {
        self.n
    }

    /// Running mean.
    pub fn mean(&self) -> Option<f64> {
        (self.n > 0).then_some(self.mean)
    }

    /// Sample variance (Bessel-corrected).
    pub fn variance(&self) -> Option<f64> {
        if self.n > 1 {
            Some(self.m2 / (self.n - 1) as f64)
        } else {
            None
        }
    }

    /// Sample standard deviation.
    pub fn stddev(&self) -> Option<f64> {
        self.variance().map(|v| v.sqrt())
    }

    /// Merges another accumulator (for sharded collection). Exercised by
    /// unit tests; kept for API completeness.
    #[allow(dead_code)]
    pub fn merge(&mut self, other: &Welford) {
        if other.n == 0 {
            return;
        }
        if self.n == 0 {
            *self = other.clone();
            return;
        }
        let total = self.n + other.n;
        let delta = other.mean - self.mean;
        // Combined mean and parallel-axis theorem for the second moment.
        self.mean += delta * other.n as f64 / total as f64;
        self.m2 += other.m2 + delta * delta * self.n as f64 * other.n as f64 / total as f64;
        self.n = total;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Welford matches the naive two-pass computation.
    #[test]
    fn matches_two_pass() {
        let mut w = Welford::default();
        for x in [1000.0, 1002.0, 999.0, 1001.5, 998.5, 1003.0, 997.0] {
            w.push(x);
        }
        let xs = [1000.0, 1002.0, 999.0, 1001.5, 998.5, 1003.0, 997.0];
        let n = xs.len() as f64;
        let naive_mean = xs.iter().sum::<f64>() / n;
        let naive_var = xs.iter().map(|x| (x - naive_mean).powi(2)).sum::<f64>() / (n - 1.0);
        assert!((w.mean().unwrap() - naive_mean).abs() < 1e-12);
        assert!((w.variance().unwrap() - naive_var).abs() < 1e-9);
        assert_eq!(w.count(), 7);
    }

    /// Numerical stability: a large mean with a small spread must not produce
    /// a negative or wildly-off variance (the naive formula can).
    #[test]
    fn numerically_stable() {
        let mut w = Welford::default();
        for i in 0..10_000u64 {
            w.push(2_000_000_000.0 + (i % 7) as f64);
        }
        assert!(w.variance().unwrap() > 0.0);
        assert!(w.variance().unwrap() < 100.0);
        // 10_000 samples of (i % 7): counts are 4·1429 + 3·1428, so the true
        // mean of the added part is 2.9994, not 3.0.
        let expected_mean = 2_000_000_000.0 + 2.9994;
        assert!((w.mean().unwrap() - expected_mean).abs() < 1e-3);
    }

    /// Merge of two halves equals the whole.
    #[test]
    fn merge_equals_whole() {
        let xs: Vec<f64> = (0..1000).map(|i| i as f64).collect();
        let mut whole = Welford::default();
        let mut a = Welford::default();
        let mut b = Welford::default();
        for (i, &x) in xs.iter().enumerate() {
            whole.push(x);
            if i % 2 == 0 {
                a.push(x);
            } else {
                b.push(x);
            }
        }
        a.merge(&b);
        assert!((a.mean().unwrap() - whole.mean().unwrap()).abs() < 1e-9);
        assert!((a.variance().unwrap() - whole.variance().unwrap()).abs() < 1e-6);
        assert_eq!(a.count(), 1000);
    }

    /// Empty and single-sample edge cases.
    #[test]
    fn edge_cases() {
        let mut w = Welford::default();
        assert_eq!(w.mean(), None);
        assert_eq!(w.variance(), None);
        w.push(5.0);
        assert_eq!(w.mean(), Some(5.0));
        assert_eq!(w.variance(), None);
        assert_eq!(w.stddev(), None);
    }
}
