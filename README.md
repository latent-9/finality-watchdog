# finality-watchdog

**A live Solana finality latency watchdog** — it streams slot updates from an Agave validator RPC, measures how long each slot takes to become *confirmed* and *finalized*, weights those latencies by the slot leader's stake, and **statistically detects when the network's finality regime changes** — the tool to have running when the Alpenglow transition ships.

```
cargo build --release
cargo run --release -- --url wss://api.mainnet-beta.solana.com
```

---

## Why this exists

Today, a Solana slot is **finalized** ~12.8 seconds after it is produced: Tower BFT requires 32 slots of exponentially-doubling vote lockouts before a root is irreversible. [Alpenglow](https://www.anza.xyz/blog/alpenglow-a-new-consensus-for-solana) (SIMD-0326, already approved by ~99% of stake) replaces that with Votor + Rotor and targets **~150 ms median finality**.

When that ships, the network's finality latency distribution will shift by an order of magnitude. This tool watches the distribution live and **detects the shift with sequential change-point statistics** — it does not just display a number, it tells you *when the regime changed*.

## What it measures

For every slot that roots, from server-side timestamps in `slotsUpdatesNotification`:

| Latency | Meaning |
|---|---|
| **finality** | freeze → root: how long until the slot is finalized |
| **confirm** | freeze → optimistic confirmation (⅔ stake voted) |
| **root gap** | optimistic confirmation → root: the Tower BFT lockout tail |
| **network overhead** | client arrival − server timestamp: delivery delay (negative = clock skew, counted separately) |

Because the timestamps are **server-side**, the latency distributions are not polluted by the observer's own network — the overhead distribution is measured *separately* against the same stream.

## The math inside

All components are streaming: constant memory, no raw-sample buffering.

- **Log-linear histogram** (HDR-style, base 2) — percentiles p50/p95/p99/p99.9 with a *provable* relative error bound of `2^-(h+1)` (default `h = 8` → ≤ 0.39%), in ~14.6 KiB.
- **Welford's online algorithm** — numerically stable mean/variance (the naive sum-of-squares formula silently loses precision with a large mean).
- **EWMA control chart** (Lucas & Saccucci) — time-varying limits that start narrow and widen to the asymptote `L·σ·sqrt(α/(2-α))`; catches small persistent drifts faster than a sigma rule.
- **Two-sided CUSUM** — the sequential probability ratio test for a mean shift; detects a change of `δ` sigmas with average run length `O(1/δ²)`. This is the regime-shift detector.
- **Robust anomalies** — median / MAD / modified z-scores (Iglewicz-Hoaglin, 50% breakdown point): heavy tails cannot drag the baseline, so a burst of slow slots does not hide the next one.
- **Stake-weighted percentiles** — each slot is weighted by its leader's activated stake (`getLeaderSchedule` + `getVoteAccounts`), so the stats answer *"what does stake-weighted consensus see?"*, not just *"what did this observer sample?"*.

## Sample report

```
=== finality watchdog @ slot 313667214 ===
rooted 1240 | dead 2 | reconnects 1 | unknown kinds 0 | clock skew 0
finality (n=1240): mean 1973ms ± 412 | p50 1877 | p95 2503 | p99 3011 | p99.9 5207 | min 1180 | max 8421
confirm (n=1238): mean 611ms | p50 596 | p99 1090
root gap (n=1238): mean 1362ms | p50 1280 | p99 2145
network overhead (n=1240): mean 41ms | p50 38 | p99 96
stake-weighted (n=1198): p50 1902ms (unweighted 1877) | mean 2001ms | epoch 812 active stake 421.3M SOL | delinquent 0.31%
SPC: armed (baseline 1981ms ± 408) | CUSUM S+ 0.4 S- 0.0 | alarms 0 | EWMA 1965 [1400, 2560] breaches 0
```

And when a shift is detected, it prints immediately:

```
[watchdog] ⚠ REGIME SHIFT at slot 313668001: finality trending FASTER than baseline 1981ms
```

## Design notes

- **Lightweight on the RPC**: the websocket stream is push-based; only three calls poll (`getEpochInfo`, `getLeaderSchedule` once per epoch, `getVoteAccounts` on a 120s timer).
- **Reconnects with capped exponential backoff**, reset after a connection that stayed up for 60s+.
- **Unknown update kinds are ignored** (forward compatibility with new Agave versions) and counted.
- **Slot lifecycle tracking is bounded**: in-flight entries fall behind the highest slot seen are pruned; dead slots are dropped.
- No Solana SDK dependency — raw JSON-RPC over `tokio-tungstenite` (wss) + `ureq` (https polling).

## Tuning

Mainnet's finality latency oscillates in **leader-driven waves** (±~300ms, period
~20-30 slots): each leader's block production quality shifts the distribution
slightly, and a single global baseline flags every wave. For calmer detection on
mainnet, raise the decision interval and warmup — the defaults are tuned to be
sensitive:

```
cargo run --release -- --warmup 200 --cusum-h 8 --ewma-alpha 0.05
```

On a local test validator (or after an Alpenglow-style regime change, where the
shift is an order of magnitude), the defaults fire as designed.

## TL;DR (Bahasa Indonesia)

Tool Rust yang nyambung ke RPC validator Solana, ngukur **berapa lama slot jadi finalized** (sekarang ~12.8 detik karena Tower BFT), dan **deteksi otomatis momen jaringan berubah rezim** — jadi pas Alpenglow ship (target ~150ms), tool ini yang bakal bilang "finality barusan turun drastis" secara statistik (CUSUM), bukan sekadar nampilin angka. Semua komponennya streaming (memori konstan): histogram log-linear dengan error bound terbukti, Welford, EWMA control chart, CUSUM, anomaly detection robust, dan persentil berbobot stake.
