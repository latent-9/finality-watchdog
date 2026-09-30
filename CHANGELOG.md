# Changelog

## 0.1.0 — 2026-10-01

Initial release.

- Live `slotsUpdatesSubscribe` stream over websocket (wss) with capped
  exponential backoff reconnects.
- Finality measurement per rooted slot: freeze → root, freeze → optimistic
  confirmation, confirmation → root gap, and client-vs-server network
  overhead (clock skew counted separately).
- Streaming statistics: log-linear histogram (provable `2^-(h+1)` relative
  error bound), Welford mean/variance, EWMA control chart, two-sided CUSUM
  regime-shift detection, robust (median/MAD) anomaly detection, and
  stake-weighted percentiles from the leader schedule + vote accounts.
- Threshold, drift, and regime-shift alerting.
