//! Lock-free health metrics.
//!
//! Monitoring must never slow down the matching loop, so every counter is a plain
//! atomic updated with `Relaxed` ordering. The hot path only ever does atomic
//! `fetch_add`s; a reader thread takes a `snapshot()` to derive human-readable
//! aggregates. No locks, no allocations on the matching path.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

#[derive(Default)]
pub struct Metrics {
    /// Total players ever enqueued.
    pub enqueued: AtomicU64,
    /// Matches formed.
    pub matches: AtomicU64,
    /// Players placed into a match.
    pub players_matched: AtomicU64,
    /// Total matching attempts (one scan of a seed window).
    pub attempts: AtomicU64,
    /// Attempts that hit an empty seed shard.
    pub empty_attempts: AtomicU64,
    /// Sum of per-player queue wait at match time, in microseconds.
    pub total_wait_us: AtomicU64,
    /// Sum of per-match team imbalance.
    pub total_imbalance: AtomicU64,
    /// Sum of per-match skill spread (max-min).
    pub total_spread: AtomicU64,
    /// High-water mark of the queue depth.
    pub max_queue_depth: AtomicUsize,
}

impl Metrics {
    #[inline]
    pub fn record_match(&self, players: usize, total_wait_us: u64, imbalance: u32, spread: u32) {
        self.matches.fetch_add(1, Relaxed);
        self.players_matched.fetch_add(players as u64, Relaxed);
        self.total_wait_us.fetch_add(total_wait_us, Relaxed);
        self.total_imbalance.fetch_add(imbalance as u64, Relaxed);
        self.total_spread.fetch_add(spread as u64, Relaxed);
    }

    /// Update the queue-depth high-water mark with a lock-free CAS loop.
    #[inline]
    pub fn observe_depth(&self, depth: usize) {
        let mut cur = self.max_queue_depth.load(Relaxed);
        while depth > cur {
            match self
                .max_queue_depth
                .compare_exchange_weak(cur, depth, Relaxed, Relaxed)
            {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
    }

    pub fn snapshot(&self, queued_now: usize) -> MetricsSnapshot {
        let matches = self.matches.load(Relaxed);
        let players = self.players_matched.load(Relaxed);
        let wait = self.total_wait_us.load(Relaxed);
        let imb = self.total_imbalance.load(Relaxed);
        let spread = self.total_spread.load(Relaxed);
        let attempts = self.attempts.load(Relaxed);
        let empty = self.empty_attempts.load(Relaxed);
        MetricsSnapshot {
            enqueued: self.enqueued.load(Relaxed),
            matches,
            players_matched: players,
            attempts,
            empty_attempts: empty,
            queued_now,
            max_queue_depth: self.max_queue_depth.load(Relaxed),
            avg_wait_ms: if players > 0 {
                (wait as f64 / players as f64) / 1000.0
            } else {
                0.0
            },
            avg_imbalance: if matches > 0 {
                imb as f64 / matches as f64
            } else {
                0.0
            },
            avg_spread: if matches > 0 {
                spread as f64 / matches as f64
            } else {
                0.0
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub enqueued: u64,
    pub matches: u64,
    pub players_matched: u64,
    pub attempts: u64,
    pub empty_attempts: u64,
    pub queued_now: usize,
    pub max_queue_depth: usize,
    /// Mean per-player queue wait at match time, milliseconds.
    pub avg_wait_ms: f64,
    /// Mean team imbalance (skill-sum delta) per match.
    pub avg_imbalance: f64,
    /// Mean skill spread per match.
    pub avg_spread: f64,
}
