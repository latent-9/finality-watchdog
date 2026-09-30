//! finality-watchdog: live Solana finality latency watchdog.
//!
//! Streams slot updates from an Agave validator RPC, measures the
//! freeze → confirm → finalize latency distributions, weights them by the
//! slot leader's activated stake, and statistically detects consensus regime
//! shifts with CUSUM: the tool to watch when the Alpenglow transition ships.

mod report;
mod rpc;
mod stake;
mod stats;
mod stream;
mod tracker;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::{mpsc, watch};

use crate::report::Watchdog;
use crate::stake::StakeMap;
use crate::stream::{now_unix_ms, StreamEvent};
use crate::tracker::{SlotUpdate, Tracker};

/// Live Solana finality latency watchdog: streams slot updates, measures
/// finality latency distributions, and detects regime shifts (e.g. Alpenglow).
#[derive(Parser, Debug)]
#[command(name = "finality-watchdog", version)]
struct Args {
    /// RPC websocket URL (wss://…); HTTP calls derive from it.
    #[arg(long, default_value = "wss://api.mainnet-beta.solana.com")]
    url: String,
    /// Samples before the CUSUM/EWMA detectors arm.
    #[arg(long, default_value_t = 100)]
    warmup: u64,
    /// Report interval in seconds.
    #[arg(long, default_value_t = 10)]
    report_interval_secs: u64,
    /// EWMA smoothing factor, in (0, 1].
    #[arg(long, default_value_t = 0.1)]
    ewma_alpha: f64,
    /// CUSUM decision interval, in sigma units.
    #[arg(long, default_value_t = 5.0)]
    cusum_h: f64,
    /// Alert when a slot's finality exceeds this many milliseconds.
    #[arg(long)]
    alert_ms: Option<f64>,
    /// Histogram precision bits, 1..=16 (error ≤ 2^-(h+1); 8 is 0.39%).
    #[arg(long, default_value_t = 8)]
    precision_bits: u32,
    /// Stake context poll interval in seconds.
    #[arg(long, default_value_t = 120)]
    stake_poll_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let http_url = args
        .url
        .replacen("wss", "https", 1)
        .replacen("ws", "http", 1);
    println!("[watchdog] streaming slot updates from {}", args.url);

    let (event_tx, mut event_rx) = mpsc::channel(4096);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Stream task: websocket → raw updates with arrival timestamps.
    let stream_handle = tokio::spawn(stream::run(args.url.clone(), event_tx, shutdown_rx.clone()));

    // Stake task: poll the leader schedule + vote accounts.
    let (stake_tx, stake_rx) = watch::channel(None::<Arc<StakeMap>>);
    let stake_url = http_url.clone();
    let poll_secs = args.stake_poll_secs;
    tokio::spawn(async move {
        loop {
            match rpc::fetch_stake_map(&stake_url).await {
                Ok(map) => stake_tx.send(Some(Arc::new(map))).ok(),
                Err(error) => {
                    eprintln!("[watchdog] stake fetch failed: {error:#}");
                    None
                }
            };
            tokio::time::sleep(Duration::from_secs(poll_secs)).await;
        }
    });

    // Aggregator: owns the tracker and the watchdog.
    let mut tracker = Tracker::new();
    let mut watchdog = Watchdog::new(
        args.warmup,
        args.precision_bits,
        args.ewma_alpha,
        args.cusum_h,
        args.alert_ms,
        now_unix_ms,
    );
    let mut report_timer =
        tokio::time::interval(Duration::from_secs(args.report_interval_secs.max(1)));
    loop {
        tokio::select! {
            _ = report_timer.tick() => {
                let map = stake_rx.borrow().clone();
                print!("{}", watchdog.render(tracker.max_seen(), tracker.in_flight(), map.as_deref()));
            }
            event = event_rx.recv() => match event {
                Some(StreamEvent::Update(update, arrival)) => {
                    if matches!(update, SlotUpdate::Dead { .. }) {
                        watchdog.note_dead();
                    }
                    if let Some(measurement) = tracker.on_update(update, arrival) {
                        let stake = stake_rx
                            .borrow()
                            .as_ref()
                            .and_then(|map| map.stake_for_slot(measurement.slot));
                        if let Some(alert) = watchdog.push_measurement(&measurement, stake) {
                            println!("[watchdog] ⚠ {alert}");
                        }
                    }
                    if tracker.in_flight() > 2048 {
                        tracker.prune();
                    }
                }
                Some(StreamEvent::Connected) => watchdog.note_reconnect(),
                Some(StreamEvent::UnknownKind) => watchdog.note_unknown_kind(),
                None => break,
            },
            _ = tokio::signal::ctrl_c() => {
                shutdown_tx.send(true).ok();
                break;
            }
        }
    }
    stream_handle.await?;
    // Final summary on exit.
    println!("[watchdog] shut down. {}", {
        let mut summary = format!(
            "detectors armed: {} | total rooted: {} | dead: {} | regime shifts: {}",
            watchdog.armed(),
            watchdog.counters().slots_rooted,
            watchdog.counters().dead_slots,
            watchdog.shifts().len(),
        );
        for shift in watchdog.shifts().iter().rev().take(5) {
            summary.push_str(&format!(
                "\n  slot {} at {} UTC: {} than baseline {:.0}ms",
                shift.slot,
                format_unix_ms(shift.at_ms),
                if shift.direction > 0 {
                    "slower"
                } else {
                    "faster"
                },
                shift.baseline_mean_ms,
            ));
        }
        summary
    });
    Ok(())
}

/// Formats unix milliseconds as `HH:MM:SS` UTC (no chrono dependency).
fn format_unix_ms(ms: u64) -> String {
    let secs = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}
