//! Log-linear histogram (HDR-style, base 2) with a provable relative-width
//! bound: O(1) memory percentiles for an unbounded stream.
//!
//! Scheme: with `half_bits = h`, values below `2^(h+1)` are indexed linearly
//! (exact). Above that, the top `h+1` bits of the value form the bucket, so a
//! bucket covering `[m << s, (m+1) << s)` has width `2^s` where `s = e - h`
//! and `e = floor_log2(v)`. Relative width is therefore at most `2^-h`
//! (bucket width over its own lower bound), and the half-width error of the
//! reported percentile at most `2^-(h+1)`.

/// Log-linear bucket counts plus an exact running sum and min/max.
#[derive(Debug)]
pub struct Histogram {
    half_bits: u32,
    linear_end: u64,
    counts: Vec<u32>,
    total: u64,
    sum: u128,
    min: u64,
    max: u64,
    seen: bool,
}

impl Histogram {
    /// Creates a histogram over u64 values with relative error at most
    /// `2^-(h+1)`. Memory is `(65 - h) * 2^h` counters: `h = 8` is 14.6 KiB
    /// with error ≤ 0.39%.
    pub fn new(half_bits: u32) -> Self {
        assert!((1..=16).contains(&half_bits), "half_bits must be in 1..=16");
        let linear_end = 1u64 << (half_bits + 1);
        let counts_len = (1u64 << half_bits) * (65 - half_bits as u64);
        Self {
            half_bits,
            linear_end,
            counts: vec![0; counts_len as usize],
            total: 0,
            sum: 0,
            min: u64::MAX,
            max: 0,
            seen: false,
        }
    }

    /// Floor of log2 for non-zero u64.
    fn floor_log2(v: u64) -> u32 {
        63 - v.leading_zeros()
    }

    /// Bucket index for `v`.
    fn index(&self, v: u64) -> usize {
        if v < self.linear_end {
            return v as usize;
        }
        let h = 1u64 << self.half_bits;
        let e = Self::floor_log2(v) as u64;
        let m = v >> (e - self.half_bits as u64); // top h+1 bits, in [2^h, 2^(h+1))
        (self.linear_end + (e - self.half_bits as u64 - 1) * h + (m - h)) as usize
    }

    /// Bucket range `[start, start+width)` for an index, saturating at
    /// `u64::MAX` (only reachable for values near `u64::MAX`).
    fn bucket_range(&self, idx: usize) -> (u64, u64) {
        let idx = idx as u64;
        if idx < self.linear_end {
            return (idx, 1);
        }
        let h = 1u64 << self.half_bits;
        let rel = idx - self.linear_end;
        let e = self.half_bits as u64 + 1 + rel / h;
        let m = h + rel % h;
        let shift = e - self.half_bits as u64;
        let start = m << shift;
        // Width is always 2^shift; the exclusive end `(m+1) << shift` may
        // exceed u64::MAX for the top bucket (values near u64::MAX); callers
        // use checked_add when they need the end.
        let width = 1u64 << shift;
        (start, width)
    }

    /// Records a sample.
    pub fn push(&mut self, v: u64) {
        let idx = self.index(v);
        self.counts[idx] = self.counts[idx].saturating_add(1);
        self.total += 1;
        self.sum += v as u128;
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
        self.seen = true;
    }

    /// Merges another histogram's counts into this one (must share config).
    /// Exercised by unit tests; kept for API completeness.
    #[allow(dead_code)]
    pub fn merge(&mut self, other: &Histogram) {
        assert_eq!(self.half_bits, other.half_bits, "config mismatch");
        self.counts
            .iter_mut()
            .zip(other.counts.iter())
            .for_each(|(a, b)| *a = a.saturating_add(*b));
        self.total += other.total;
        self.sum += other.sum;
        if other.seen {
            self.min = self.min.min(other.min);
            self.max = self.max.max(other.max);
            self.seen = true;
        }
    }

    /// Exact running mean of all pushed samples.
    pub fn mean(&self) -> Option<f64> {
        if self.total == 0 {
            None
        } else {
            Some(self.sum as f64 / self.total as f64)
        }
    }

    /// Minimum observed value.
    pub fn min(&self) -> Option<u64> {
        if self.seen {
            Some(self.min)
        } else {
            None
        }
    }

    /// Maximum observed value.
    pub fn max(&self) -> Option<u64> {
        if self.seen {
            Some(self.max)
        } else {
            None
        }
    }

    /// Total sample count.
    pub fn count(&self) -> u64 {
        self.total
    }

    /// Approximate `p`-th percentile (0..=100) via linear interpolation within
    /// the bucket holding the target rank. Error is bounded by the bucket's
    /// half-width relative to the bucket's lower bound (≤ `2^-(h+1)` relative
    /// for values above the linear region).
    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.total == 0 {
            return None;
        }
        debug_assert!((0.0..=100.0).contains(&p), "percentile in 0..=100");
        let target = ((p / 100.0) * self.total as f64).ceil().max(1.0) as u64;
        let mut cum = 0u64;
        for (idx, &c) in self.counts.iter().enumerate() {
            if c == 0 {
                continue;
            }
            if cum + c as u64 >= target {
                let (start, width) = self.bucket_range(idx);
                let pos = (target - cum - 1) as f64;
                let frac = pos / c as f64;
                return Some(start as f64 + frac * width as f64);
            }
            cum += c as u64;
        }
        self.max().map(|m| m as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip, monotonicity, and the relative-width bound, exhaustively
    /// for all values up to 100_000 at a small precision (fast enough to run
    /// on every `cargo test`).
    #[test]
    fn bucket_scheme_properties_hold_exhaustively() {
        let h = 4;
        let hist = Histogram::new(h);
        let linear_end = 1u64 << (h + 1);
        let mut prev_idx = 0usize;
        for v in 1u64..=100_000 {
            let idx = hist.index(v);
            assert!(idx >= prev_idx, "monotonic at v={v}");
            prev_idx = idx;
            let (start, width) = hist.bucket_range(idx);
            assert!(
                v >= start && v < start + width,
                "contains v={v} in [{start},{})+{width}",
                start + width
            );
            // Relative width: exact in the linear region, ≤ 2^-h above it.
            if v >= linear_end {
                assert!(
                    (width as f64) <= (v as f64) * (1.0 / (1u64 << h) as f64) * 1.0000001,
                    "width {width} of v={v} exceeds 2^-{h}"
                );
            } else {
                assert_eq!(width, 1);
            }
        }
    }

    /// Extreme values must not panic or overflow, and must round-trip.
    #[test]
    fn extreme_values_round_trip() {
        let hist = Histogram::new(8);
        for v in [u64::MAX, u64::MAX - 1, 1u64 << 63, (1 << 63) - 1, 512, 511] {
            let idx = hist.index(v);
            let (start, width) = hist.bucket_range(idx);
            let end = start.checked_add(width);
            assert!(
                v >= start && end.map_or(true, |e| v < e),
                "v={v} start={start} width={width} end={end:?}"
            );
        }
    }

    /// Exact percentiles in the linear region.
    #[test]
    fn percentiles_exact_in_linear_region() {
        let mut hist = Histogram::new(8);
        for v in 1..=101u64 {
            hist.push(v);
        }
        assert_eq!(hist.count(), 101);
        assert_eq!(hist.percentile(50.0), Some(51.0));
        assert_eq!(hist.percentile(0.0), Some(1.0));
        assert_eq!(hist.percentile(100.0), Some(101.0));
        assert_eq!(hist.mean(), Some(51.0));
        assert_eq!(hist.min(), Some(1));
        assert_eq!(hist.max(), Some(101));
    }

    /// Percentile error is bounded by the bucket half-width: push a uniform
    /// spread and check every percentile against the exact value.
    #[test]
    fn percentile_error_bounded() {
        let h = 8;
        let mut hist = Histogram::new(h);
        // Every value 1..=100_000 once: the p-th percentile is exactly p/1000.
        for v in 1..=100_000u64 {
            hist.push(v);
        }
        for p in [1.0, 10.0, 25.0, 50.0, 75.0, 90.0, 99.0, 99.9] {
            let exact = p / 100.0 * 100_000.0;
            let approx = hist.percentile(p).unwrap();
            let bound = exact * (1.0 / (1u64 << (h + 1)) as f64) + 1.0;
            assert!(
                (approx - exact).abs() <= bound,
                "p{p}: approx {approx} vs exact {exact} (bound {bound})"
            );
        }
    }

    /// Merge restores the combined distribution.
    #[test]
    fn merge_combines_counts() {
        let mut a = Histogram::new(8);
        let mut b = Histogram::new(8);
        a.push(100);
        a.push(200);
        b.push(300);
        a.merge(&b);
        assert_eq!(a.count(), 3);
        assert_eq!(a.mean(), Some(200.0));
        assert_eq!(a.min(), Some(100));
        assert_eq!(a.max(), Some(300));
        assert_eq!(a.percentile(50.0), Some(200.0));
    }
}
