//! Slot state machine: turns `slotsUpdatesNotification` events into finality
//! measurements.
//!
//! Tracks per-slot server timestamps through the lifecycle freeze →
//! optimistic confirmation → root (= finalized) and emits a measurement when
//! a slot roots. Dead slots are dropped. Client-side arrival timestamps are
//! compared against server timestamps to expose network overhead: a negative
//! difference means clock skew and is reported as `None` rather than a
//! nonsense negative latency.

use std::collections::HashMap;

use serde::Deserialize;

/// A parsed `SlotUpdate` from the wire (camelCase, `type`-tagged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotUpdate {
    FirstShredReceived {
        slot: u64,
        timestamp: u64,
    },
    Completed {
        slot: u64,
        timestamp: u64,
    },
    CreatedBank {
        slot: u64,
        parent: u64,
        timestamp: u64,
    },
    Frozen {
        slot: u64,
        timestamp: u64,
    },
    Dead {
        slot: u64,
        timestamp: u64,
    },
    OptimisticConfirmation {
        slot: u64,
        timestamp: u64,
    },
    Root {
        slot: u64,
        timestamp: u64,
    },
}

impl SlotUpdate {
    /// The slot this update refers to. Exercised by unit tests; kept for
    /// API completeness.
    #[allow(dead_code)]
    pub fn slot(&self) -> u64 {
        match self {
            Self::FirstShredReceived { slot, .. }
            | Self::Completed { slot, .. }
            | Self::CreatedBank { slot, .. }
            | Self::Frozen { slot, .. }
            | Self::Dead { slot, .. }
            | Self::OptimisticConfirmation { slot, .. }
            | Self::Root { slot, .. } => *slot,
        }
    }
}

/// The wire shape of a slot update notification result. `stats` (on frozen)
/// and future fields are ignored; the watcher only needs slot + timestamp.
#[derive(Debug, Deserialize)]
struct WireUpdate {
    #[serde(rename = "type")]
    kind: String,
    slot: u64,
    timestamp: u64,
    parent: Option<u64>,
}

/// Parses a notification result object into a [`SlotUpdate`]. Unknown update
/// kinds (forward compatibility) are reported as `Err` with the kind name.
pub fn parse_update(value: &serde_json::Value) -> Result<Option<SlotUpdate>, String> {
    let wire: WireUpdate =
        serde_json::from_value(value.clone()).map_err(|e| format!("malformed update: {e}"))?;
    let update = match wire.kind.as_str() {
        "firstShredReceived" => SlotUpdate::FirstShredReceived {
            slot: wire.slot,
            timestamp: wire.timestamp,
        },
        "completed" => SlotUpdate::Completed {
            slot: wire.slot,
            timestamp: wire.timestamp,
        },
        "createdBank" => SlotUpdate::CreatedBank {
            slot: wire.slot,
            parent: wire.parent.ok_or("createdBank without parent")?,
            timestamp: wire.timestamp,
        },
        "frozen" => SlotUpdate::Frozen {
            slot: wire.slot,
            timestamp: wire.timestamp,
        },
        "dead" => SlotUpdate::Dead {
            slot: wire.slot,
            timestamp: wire.timestamp,
        },
        "optimisticConfirmation" => SlotUpdate::OptimisticConfirmation {
            slot: wire.slot,
            timestamp: wire.timestamp,
        },
        "root" => SlotUpdate::Root {
            slot: wire.slot,
            timestamp: wire.timestamp,
        },
        other => return Err(other.to_string()),
    };
    Ok(Some(update))
}

/// A finality measurement for one rooted slot. All latencies are server-side,
/// in milliseconds.
#[derive(Debug, Clone)]
pub struct Measurement {
    pub slot: u64,
    /// freeze → root: the finality latency.
    pub finality_ms: f64,
    /// freeze → optimistic confirmation.
    pub confirm_ms: Option<f64>,
    /// optimistic confirmation → root.
    pub root_gap_ms: Option<f64>,
    /// Client arrival minus server timestamp (network overhead); `None` when
    /// the difference was negative (clock skew).
    pub network_overhead_ms: Option<f64>,
}

#[derive(Debug, Default, Clone)]
struct SlotState {
    frozen_ts: Option<u64>,
    confirmed_ts: Option<u64>,
}

/// Tracks slot lifecycle timestamps and emits measurements on root.
#[derive(Debug)]
pub struct Tracker {
    slots: HashMap<u64, SlotState>,
    max_seen: u64,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
            max_seen: 0,
        }
    }

    /// Applies an update; returns a measurement when the slot rooted.
    pub fn on_update(&mut self, update: SlotUpdate, arrival_ms: u64) -> Option<Measurement> {
        match update {
            SlotUpdate::Frozen { slot, timestamp } => {
                self.note(slot);
                self.slots.entry(slot).or_default().frozen_ts = Some(timestamp);
                None
            }
            SlotUpdate::OptimisticConfirmation { slot, timestamp } => {
                self.note(slot);
                self.slots.entry(slot).or_default().confirmed_ts = Some(timestamp);
                None
            }
            SlotUpdate::Root { slot, timestamp } => {
                self.note(slot);
                let state = self.slots.remove(&slot)?;
                let frozen = state.frozen_ts?;
                let finality_ms = timestamp.saturating_sub(frozen) as f64;
                let confirm_ms = state.confirmed_ts.map(|c| c.saturating_sub(frozen) as f64);
                let root_gap_ms = state
                    .confirmed_ts
                    .map(|c| timestamp.saturating_sub(c) as f64);
                let network_overhead_ms = arrival_ms.checked_sub(timestamp).map(|o| o as f64);
                Some(Measurement {
                    slot,
                    finality_ms,
                    confirm_ms,
                    root_gap_ms,
                    network_overhead_ms,
                })
            }
            SlotUpdate::Dead { slot, .. } => {
                self.note(slot);
                self.slots.remove(&slot);
                None
            }
            SlotUpdate::FirstShredReceived { slot, .. }
            | SlotUpdate::Completed { slot, .. }
            | SlotUpdate::CreatedBank { slot, .. } => {
                self.note(slot);
                None
            }
        }
    }

    fn note(&mut self, slot: u64) {
        self.max_seen = self.max_seen.max(slot);
    }

    /// Highest slot seen.
    pub fn max_seen(&self) -> u64 {
        self.max_seen
    }

    /// Number of in-flight (not yet rooted) slots.
    pub fn in_flight(&self) -> usize {
        self.slots.len()
    }

    /// Drops in-flight entries that fell far behind the highest slot seen;
    /// their Root (if any) was missed, e.g. across a reconnect.
    pub fn prune(&mut self) {
        let horizon = self.max_seen.saturating_sub(1024);
        self.slots.retain(|&slot, _| slot >= horizon);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frozen(slot: u64, ts: u64) -> SlotUpdate {
        SlotUpdate::Frozen {
            slot,
            timestamp: ts,
        }
    }

    fn confirmed(slot: u64, ts: u64) -> SlotUpdate {
        SlotUpdate::OptimisticConfirmation {
            slot,
            timestamp: ts,
        }
    }

    fn rooted(slot: u64, ts: u64) -> SlotUpdate {
        SlotUpdate::Root {
            slot,
            timestamp: ts,
        }
    }

    /// Full lifecycle emits a measurement with the correct latencies.
    #[test]
    fn full_lifecycle_measures() {
        let mut t = Tracker::new();
        assert!(t.on_update(frozen(100, 1_000), 1_001).is_none());
        assert!(t.on_update(confirmed(100, 1_400), 1_402).is_none());
        assert_eq!(t.in_flight(), 1);
        let m = t.on_update(rooted(100, 2_500), 2_503).expect("measurement");
        assert_eq!(m.slot, 100);
        assert_eq!(m.finality_ms, 1_500.0); // 2500 - 1000
        assert_eq!(m.confirm_ms, Some(400.0)); // 1400 - 1000
        assert_eq!(m.root_gap_ms, Some(1_100.0)); // 2500 - 1400
        assert_eq!(m.network_overhead_ms, Some(3.0)); // 2503 - 2500
        assert_eq!(t.in_flight(), 0);
    }

    /// Root without a frozen timestamp (missed events) yields no measurement.
    #[test]
    fn root_without_frozen_skips() {
        let mut t = Tracker::new();
        assert!(t.on_update(rooted(100, 2_500), 2_501).is_none());
        assert_eq!(t.in_flight(), 0);
    }

    /// Root without confirmation still measures finality only.
    #[test]
    fn root_without_confirmation_measures_finality() {
        let mut t = Tracker::new();
        t.on_update(frozen(100, 1_000), 1_001);
        let m = t.on_update(rooted(100, 2_500), 2_501).expect("measurement");
        assert_eq!(m.finality_ms, 1_500.0);
        assert_eq!(m.confirm_ms, None);
        assert_eq!(m.root_gap_ms, None);
    }

    /// Dead slots are dropped without measurement.
    #[test]
    fn dead_slot_dropped() {
        let mut t = Tracker::new();
        t.on_update(frozen(100, 1_000), 1_001);
        t.on_update(
            SlotUpdate::Dead {
                slot: 100,
                timestamp: 1_100,
            },
            1_101,
        );
        assert_eq!(t.in_flight(), 0);
        // A late root for the dead slot must not measure.
        assert!(t.on_update(rooted(100, 2_500), 2_501).is_none());
    }

    /// Negative arrival − server difference (clock skew) reports None, not a
    /// negative latency.
    #[test]
    fn clock_skew_reported_as_none() {
        let mut t = Tracker::new();
        t.on_update(frozen(100, 1_000), 1_001);
        let m = t.on_update(rooted(100, 5_000), 4_999).expect("measurement");
        assert_eq!(m.network_overhead_ms, None);
        assert_eq!(m.finality_ms, 4_000.0); // server-side math unaffected
    }

    /// Prune drops stale in-flight entries and keeps recent ones.
    #[test]
    fn prune_keeps_recent() {
        let mut t = Tracker::new();
        t.on_update(frozen(100, 1_000), 1_001);
        t.on_update(frozen(2_500, 1_100), 1_101);
        // Simulate the stream advancing: max_seen 3100 → horizon 2076.
        t.note(3_100);
        t.prune();
        assert_eq!(t.in_flight(), 1); // 2500 ≥ 2076 survives, 100 pruned
        assert!(t.slots.contains_key(&2_500));
        assert!(!t.slots.contains_key(&100));
    }

    /// Parser: happy path for every kind + unknown kinds are Err.
    #[test]
    fn parser_round_trip() {
        let mk = |kind: &str, extra: &str| {
            format!(r#"{{"type":"{kind}","slot":42,"timestamp":1000{extra}}}"#)
        };
        for kind in [
            "firstShredReceived",
            "completed",
            "frozen",
            "dead",
            "optimisticConfirmation",
            "root",
        ] {
            let v: serde_json::Value = serde_json::from_str(&mk(kind, "")).unwrap();
            let parsed = parse_update(&v).unwrap().expect("some update");
            assert_eq!(parsed.slot(), 42);
        }
        // createdBank carries a parent.
        let v: serde_json::Value =
            serde_json::from_str(&mk("createdBank", r#","parent":41"#)).unwrap();
        match parse_update(&v).unwrap().unwrap() {
            SlotUpdate::CreatedBank { slot, parent, .. } => {
                assert_eq!((slot, parent), (42, 41));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // Unknown kind → Err with the kind name.
        let v: serde_json::Value = serde_json::from_str(&mk("createdFoo", "")).unwrap();
        assert_eq!(parse_update(&v).unwrap_err(), "createdFoo");
        // Malformed JSON → Err.
        let v: serde_json::Value = serde_json::from_str(r#"{"slot":"x"}"#).unwrap();
        assert!(parse_update(&v).is_err());
    }
}
