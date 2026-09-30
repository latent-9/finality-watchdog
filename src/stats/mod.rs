//! Streaming statistics for the finality watchdog.
//!
//! Every component here is designed for an unbounded live stream: constant
//! memory, no buffering of raw samples, and numerically stable updates. The
//! math behind each piece:
//!
//! - [`histogram::Histogram`]: log-linear buckets (base-2, HDR-style) with a
//!   provable relative-width bound, giving p50/p95/p99/p99.9 in O(1) memory.
//! - [`welford::Welford`]: Welford's online algorithm for mean/variance —
//!   numerically stable, unlike the naive sum-of-squares formula.
//! - [`ewma::Ewma`]: EWMA control chart (Lucas & Saccucci) with time-varying
//!   control limits — flags small persistent drifts faster than sigma rules.
//! - [`cusum::Cusum`]: two-sided CUSUM change-point detection — the core of
//!   regime-shift detection (e.g. the Alpenglow finality transition).
//! - [`robust::Robust`]: median / MAD / modified z-scores (Iglewicz-Hoaglin) —
//!   anomaly detection that heavy tails cannot fool.
//! - [`weighted`]: stake-weighted percentiles — the weights are the leader's
//!   activated stake, so the stats answer "what do weighted validators see?",
//!   not just "what did we observe?".

pub mod cusum;
pub mod ewma;
pub mod histogram;
pub mod robust;
pub mod welford;
pub mod weighted;

pub use cusum::Cusum;
pub use ewma::Ewma;
pub use histogram::Histogram;
pub use robust::Robust;
pub use welford::Welford;
