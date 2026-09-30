//! Stake context: maps slots to their leader's activated stake.
//!
//! `getLeaderSchedule` returns, per validator identity, the slot indices
//! **relative to the first slot of the epoch they lead** (verified against
//! the Agave source: `runtime/src/leader_schedule_utils.rs`). Mapping an
//! absolute slot to its leader therefore needs the epoch's start slot from
//! `getEpochInfo` (`absoluteSlot - slotIndex`), and the stake comes from
//! `getVoteAccounts` (`nodePubkey` → `activatedStake`, active validators only).

use std::collections::HashMap;

use serde::Deserialize;

/// Slot → leader → activated stake map for one epoch.
#[derive(Debug, Clone, Default)]
pub struct StakeMap {
    pub epoch: u64,
    /// Absolute slot of the epoch's first slot.
    pub epoch_start: u64,
    pub slots_in_epoch: u64,
    /// Leader identity → activated stake in lamports (non-delinquent only).
    pub stakes: HashMap<String, u64>,
    /// Epoch-relative slot index → leader identity.
    pub leaders: HashMap<u64, String>,
    /// Delinquent stake fraction (0..1), for context in reports.
    pub delinquent_fraction: f64,
}

impl StakeMap {
    /// Builds a map from raw RPC pieces: the leader schedule (identity →
    /// epoch-relative slot indices) and the vote accounts (identity →
    /// activated stake + delinquent flag).
    pub fn from_parts(
        epoch: u64,
        epoch_start: u64,
        slots_in_epoch: u64,
        schedule: &HashMap<String, Vec<u64>>,
        vote_accounts: &[VoteAccount],
    ) -> Self {
        let mut stakes = HashMap::new();
        let mut delinquent: u64 = 0;
        let mut active: u64 = 0;
        for account in vote_accounts {
            if account.delinquent {
                delinquent += account.activated_stake;
            } else {
                active += account.activated_stake;
                stakes.insert(account.node_pubkey.clone(), account.activated_stake);
            }
        }
        let total = active + delinquent;
        let delinquent_fraction = if total == 0 {
            0.0
        } else {
            delinquent as f64 / total as f64
        };
        let mut leaders = HashMap::with_capacity(schedule.values().map(Vec::len).sum::<usize>());
        for (identity, indices) in schedule {
            for &index in indices {
                leaders.insert(index, identity.clone());
            }
        }
        Self {
            epoch,
            epoch_start,
            slots_in_epoch,
            stakes,
            leaders,
            delinquent_fraction,
        }
    }

    /// The leader's activated stake (lamports) for an absolute slot; `None`
    /// when the slot is outside this epoch or the leader has no active stake.
    pub fn stake_for_slot(&self, slot: u64) -> Option<u64> {
        let index = slot.checked_sub(self.epoch_start)?;
        if index >= self.slots_in_epoch {
            return None;
        }
        let identity = self.leaders.get(&index)?;
        self.stakes.get(identity).copied()
    }

    /// Whether this map still covers `slot` (it goes stale at epoch bounds).
    /// Exercised by unit tests; kept for API completeness.
    #[allow(dead_code)]
    pub fn covers(&self, slot: u64) -> bool {
        let index = slot.checked_sub(self.epoch_start);
        match index {
            Some(index) => index < self.slots_in_epoch,
            None => false,
        }
    }

    /// Total active stake in lamports.
    pub fn total_active_stake(&self) -> u64 {
        self.stakes.values().sum()
    }
}

/// One vote account from `getVoteAccounts` (fields the watcher needs).
#[derive(Debug, Clone, Deserialize)]
pub struct VoteAccount {
    #[serde(rename = "nodePubkey")]
    pub node_pubkey: String,
    #[serde(rename = "activatedStake")]
    pub activated_stake: u64,
    #[serde(default)]
    pub delinquent: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> StakeMap {
        let mut schedule = HashMap::new();
        schedule.insert("LeaderA".to_string(), vec![0, 2, 4]);
        schedule.insert("LeaderB".to_string(), vec![1, 3]);
        let accounts = vec![
            VoteAccount {
                node_pubkey: "LeaderA".to_string(),
                activated_stake: 3_000,
                delinquent: false,
            },
            VoteAccount {
                node_pubkey: "LeaderB".to_string(),
                activated_stake: 1_000,
                delinquent: false,
            },
            VoteAccount {
                node_pubkey: "LeaderGone".to_string(),
                activated_stake: 1_000,
                delinquent: true,
            },
        ];
        StakeMap::from_parts(700, 10_000, 5, &schedule, &accounts)
    }

    /// Absolute slot → leader → stake mapping.
    #[test]
    fn maps_slots_to_stake() {
        let m = fixture();
        assert_eq!(m.stake_for_slot(10_000), Some(3_000)); // index 0 → LeaderA
        assert_eq!(m.stake_for_slot(10_001), Some(1_000)); // index 1 → LeaderB
        assert_eq!(m.stake_for_slot(10_004), Some(3_000)); // index 4 → LeaderA
    }

    /// Slots outside the epoch are not covered.
    #[test]
    fn outside_epoch_not_covered() {
        let m = fixture();
        assert_eq!(m.stake_for_slot(9_999), None);
        assert_eq!(m.stake_for_slot(10_005), None);
        assert!(!m.covers(10_005));
        assert!(m.covers(10_004));
    }

    /// Delinquent validators contribute no stake.
    #[test]
    fn delinquent_excluded() {
        let m = fixture();
        assert_eq!(m.total_active_stake(), 4_000);
        assert!((m.delinquent_fraction - 0.2).abs() < 1e-12); // 1000 of 5000
    }

    /// Schedule entries overlapping (a leader listed twice for one slot)
    /// keep the map consistent: the real RPC schedule never does this, but
    /// the map must not corrupt. The winner depends on HashMap iteration
    /// order (randomized), so only "some active stake wins" is asserted.
    #[test]
    fn overlapping_schedule_does_not_panic() {
        let mut schedule = HashMap::new();
        schedule.insert("A".to_string(), vec![0, 0]);
        schedule.insert("B".to_string(), vec![0]);
        let accounts = vec![
            VoteAccount {
                node_pubkey: "A".to_string(),
                activated_stake: 1,
                delinquent: false,
            },
            VoteAccount {
                node_pubkey: "B".to_string(),
                activated_stake: 2,
                delinquent: false,
            },
        ];
        let m = StakeMap::from_parts(0, 0, 1, &schedule, &accounts);
        let won = m.stake_for_slot(0).expect("some stake");
        assert!(won == 1 || won == 2, "won {won}");
    }
}
