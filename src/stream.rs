//! Websocket stream: subscribes to `slotsUpdatesSubscribe` on an Agave
//! validator and forwards parsed updates with client-side arrival timestamps.
//!
//! Reconnects with capped exponential backoff; the backoff resets after a
//! connection that stayed up for a while, so a blip does not leave the
//! watcher sleeping 30 seconds forever. Pings are answered; unknown update
//! kinds are ignored (forward compatibility with new Agave versions).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::tracker::parse_update;

const MAX_BACKOFF_SECS: u64 = 30;
const BACKOFF_RESET_SECS: u64 = 60;

const SUBSCRIBE_REQUEST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"slotsUpdatesSubscribe"}"#;

/// Events from the stream task.
#[derive(Debug)]
pub enum StreamEvent {
    /// A parsed slot update with the client-side arrival time (unix ms).
    Update(crate::tracker::SlotUpdate, u64),
    /// The stream (re)connected.
    Connected,
    /// An update kind this version does not know (forward compatibility).
    UnknownKind,
}

/// Unix milliseconds now.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Runs the stream until shutdown is signaled. Errors are logged and retried
/// with backoff — this function only returns on shutdown or an unrecoverable
/// task failure.
pub async fn run(url: String, tx: mpsc::Sender<StreamEvent>, mut shutdown: watch::Receiver<bool>) {
    let mut backoff = 1u64;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let started = Instant::now();
        match connect_and_stream(&url, &tx, &mut shutdown).await {
            Ok(()) => return,
            Err(error) => {
                eprintln!("[watchdog] stream error: {error:#}; retrying in {backoff}s");
            }
        }
        if started.elapsed() >= Duration::from_secs(BACKOFF_RESET_SECS) {
            backoff = 1;
        }
        let jitter = (u64::from(Instant::now().elapsed().subsec_nanos() % 1000)) / 1000;
        tokio::time::sleep(Duration::from_secs(backoff) + Duration::from_millis(jitter)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF_SECS);
    }
}

/// Connects, subscribes, and forwards updates until shutdown or an error.
async fn connect_and_stream(
    url: &str,
    tx: &mpsc::Sender<StreamEvent>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let (ws, _response) = connect_async(url)
        .await
        .with_context(|| format!("connect to {url}"))?;
    let (mut write, mut read) = ws.split();
    write
        .send(Message::text(SUBSCRIBE_REQUEST))
        .await
        .context("subscribe")?;
    tx.send(StreamEvent::Connected).await.ok();
    'outer: loop {
        let next = tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    write.close().await.ok();
                    return Ok(());
                }
                continue 'outer;
            }
            next = read.next() => next,
        };
        let message = next
            .context("stream ended")?
            .context("websocket read")?;
        match message {
            Message::Text(text) => handle_text(&text, tx).await,
            Message::Ping(data) => write.send(Message::Pong(data)).await.context("pong")?,
            Message::Close(_) => return Err(anyhow!("server closed the stream")),
            _ => {}
        }
    }
}

/// Handles one text frame: notification → parse → forward with arrival time.
async fn handle_text(text: &str, tx: &mpsc::Sender<StreamEvent>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return; // non-JSON frame — ignore
    };
    if value.get("method").and_then(Value::as_str) != Some("slotsUpdatesNotification") {
        return; // subscribe ack or other notification type
    }
    let Some(result) = value.pointer("/params/result") else {
        return;
    };
    match parse_update(result) {
        Ok(Some(update)) => {
            let arrival = now_unix_ms();
            tx.send(StreamEvent::Update(update, arrival)).await.ok();
        }
        Ok(None) => {}
        Err(_unknown_kind) => {
            // Forward compatibility: a newer Agave may add update kinds.
            tx.send(StreamEvent::UnknownKind).await.ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A notification frame parses and forwards; a subscribe ack does not.
    #[tokio::test]
    async fn handle_text_forwards_notifications_only() {
        let (tx, mut rx) = mpsc::channel(16);
        let notification = r#"{"jsonrpc":"2.0","method":"slotsUpdatesNotification",
            "params":{"result":{"type":"frozen","slot":42,"timestamp":1000,"stats":{}},"subscription":1}}"#;
        handle_text(notification, &tx).await;
        match rx.recv().await.expect("event") {
            StreamEvent::Update(update, _arrival) => assert_eq!(update.slot(), 42),
            other => panic!("unexpected event: {other:?}"),
        }
        // Subscribe ack: no event.
        handle_text(r#"{"jsonrpc":"2.0","result":7,"id":1}"#, &tx).await;
        assert!(rx.try_recv().is_err());
        // Unknown kind: forwarded as UnknownKind (counted upstream), no panic.
        handle_text(
            r#"{"jsonrpc":"2.0","method":"slotsUpdatesNotification","params":{"result":{"type":"fromTheFuture","slot":1,"timestamp":2}}}"#,
            &tx,
        )
        .await;
        match rx.try_recv() {
            Ok(StreamEvent::UnknownKind) => {}
            other => panic!("expected UnknownKind, got {other:?}"),
        }
        // Non-JSON: no panic.
        handle_text("not json", &tx).await;
        assert!(rx.try_recv().is_err());
    }

    /// now_unix_ms is within a second of the system clock.
    #[test]
    fn now_unix_ms_sane() {
        let now = now_unix_ms();
        let system = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        assert!(now <= system && system - now < 1_000);
    }
}
