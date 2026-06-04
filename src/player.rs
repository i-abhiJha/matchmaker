//! Core domain types: an individual `Player` waiting in queue and a formed `Match`.

use std::time::Instant;

/// A player waiting in the matchmaking queue.
///
/// `Player` is `Copy` so it can be cheaply pulled out of a shard, reasoned about,
/// and re-inserted without heap churn. `skill` is an MMR-style rating (e.g. 0..5000).
#[derive(Clone, Copy, Debug)]
pub struct Player {
    pub id: u64,
    pub skill: u32,
    /// Wall-clock instant the player entered the queue. Used for wait-time based
    /// constraint relaxation and for fairness (oldest players matched first).
    pub enqueued_at: Instant,
}

impl Player {
    pub fn new(id: u64, skill: u32) -> Self {
        Self {
            id,
            skill,
            enqueued_at: Instant::now(),
        }
    }

    /// Seconds the player has been waiting in queue.
    #[inline]
    pub fn wait_secs(&self) -> f64 {
        self.enqueued_at.elapsed().as_secs_f64()
    }
}

/// A formed match: two balanced teams plus quality metadata.
#[derive(Clone, Debug)]
pub struct Match {
    pub id: u64,
    pub team_a: Vec<Player>,
    pub team_b: Vec<Player>,
    /// Absolute difference between the two teams' summed skill. Lower is fairer.
    pub imbalance: u32,
    /// Mean skill across all 10 players.
    pub avg_skill: f64,
    /// Mean queue wait across the 10 players, in seconds.
    pub avg_wait_secs: f64,
    /// Skill spread (max - min) of the 10 players. A proxy for match quality.
    pub skill_spread: u32,
}
