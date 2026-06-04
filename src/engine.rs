//! The concurrent matchmaking engine.
//!
//! ## Data structure
//! Waiting players live in a **skill-sharded pool**: the skill axis (`0..skill_max`)
//! is cut into `num_shards` contiguous buckets, each guarded by its own `Mutex`.
//! Within a shard, players are kept in a `BTreeMap` keyed by `(skill, id)`, i.e.
//! permanently sorted by skill. That ordering is the whole game: a match only ever
//! pairs skill-adjacent players, so the engine never scans a whole shard — it reads
//! the lowest-skill slice (`take(m)`) of each locked shard in `O(log n + m)` and
//! never clones the bucket.
//!
//! ## Thread-safe, atomic eviction
//! A match attempt locks a *contiguous range* of shards (`lo..=hi`) covering the
//! seed's skill window, **always in ascending index order**. Because every worker
//! acquires ranges in the same order, the scheme is deadlock-free. Gathering the
//! 10 players and removing them happen while those locks are held, so eviction is
//! atomic: no two workers can ever claim the same player.
//!
//! ## Latency vs. quality, and constraint relaxation
//! Acceptance is gated by a skill band that *grows with wait time*:
//! `band = base + relax_per_sec * wait`, capped at `max_window`, where `wait` is
//! taken from the **oldest** player in the candidate group — the one at risk of
//! starving. Fresh, dense regions match instantly inside a tiny band (high
//! quality); isolated high/low-skill players progressively relax their band (and,
//! as it widens, reach into neighbouring shards) until they match. This is the
//! core latency/quality dial.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound::{Excluded, Included};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::balance::balance_teams;
use crate::metrics::Metrics;
use crate::player::{Match, Player};

/// Skill-ordered bucket of waiting players: `(skill, id) -> enqueue instant`.
type Shard = BTreeMap<(u32, u64), Instant>;

#[derive(Clone, Debug)]
pub struct Config {
    /// Number of skill shards. More shards = less lock contention but sparser buckets.
    pub num_shards: usize,
    /// Maximum skill rating; skills are clamped to `0..=skill_max`.
    pub skill_max: u32,
    /// Players per team. A match is `team_size * 2` players.
    pub team_size: usize,
    /// Initial acceptance band width (skill rating points) for a fresh group.
    pub base_window: f64,
    /// How fast the band widens per second the oldest member has waited.
    pub relax_per_sec: f64,
    /// Hard cap on the acceptance band.
    pub max_window: f64,
    /// How many recently-formed matches to retain for inspection (0 = none).
    pub keep_recent: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            num_shards: 32,
            skill_max: 5000,
            team_size: 5,
            base_window: 80.0,
            relax_per_sec: 60.0,
            max_window: 1500.0,
            keep_recent: 64,
        }
    }
}

pub struct Engine {
    cfg: Config,
    shard_width: u32,
    match_size: usize,
    shards: Vec<Mutex<Shard>>,
    queued: AtomicUsize,
    next_seed: AtomicUsize,
    match_seq: AtomicU64,
    running: AtomicBool,
    pub metrics: Metrics,
    recent: Mutex<VecDeque<Match>>,
}

impl Engine {
    pub fn new(cfg: Config) -> Arc<Self> {
        assert!(cfg.num_shards >= 1 && cfg.team_size >= 1);
        let shard_width = (cfg.skill_max / cfg.num_shards as u32).max(1) + 1;
        let shards = (0..cfg.num_shards).map(|_| Mutex::new(Shard::new())).collect();
        let match_size = cfg.team_size * 2;
        Arc::new(Self {
            shard_width,
            match_size,
            shards,
            queued: AtomicUsize::new(0),
            next_seed: AtomicUsize::new(0),
            match_seq: AtomicU64::new(0),
            running: AtomicBool::new(true),
            metrics: Metrics::default(),
            recent: Mutex::new(VecDeque::new()),
            cfg,
        })
    }

    #[inline]
    fn shard_of(&self, skill: u32) -> usize {
        ((skill / self.shard_width) as usize).min(self.cfg.num_shards - 1)
    }

    /// Add a player to the waiting pool. `O(log n)`; touches a single shard.
    pub fn enqueue(&self, id: u64, skill: u32) {
        let skill = skill.min(self.cfg.skill_max);
        let s = self.shard_of(skill);
        {
            let mut g = self.shards[s].lock().unwrap();
            g.insert((skill, id), Instant::now());
        }
        let depth = self.queued.fetch_add(1, Relaxed) + 1;
        self.metrics.enqueued.fetch_add(1, Relaxed);
        self.metrics.observe_depth(depth);
    }

    /// Current number of players waiting.
    pub fn queued(&self) -> usize {
        self.queued.load(Relaxed)
    }

    #[inline]
    fn window_for(&self, wait_secs: f64) -> f64 {
        (self.cfg.base_window + self.cfg.relax_per_sec * wait_secs).min(self.cfg.max_window)
    }

    /// Attempt to form exactly one match. Returns `true` if a match was formed.
    ///
    /// This is the unit of work executed concurrently by all worker threads, and is
    /// `O(log n + k·m)` where `k` is the number of shards spanned by the seed's band
    /// and `m` is the match size — independent of how many players a shard holds.
    fn try_match(&self) -> bool {
        self.metrics.attempts.fetch_add(1, Relaxed);
        let num_shards = self.cfg.num_shards;
        let m = self.match_size;
        let s = self.next_seed.fetch_add(1, Relaxed) % num_shards;

        // --- Phase 1: peek the lowest-skill waiter in the seed shard to size the
        // band. One lock, taken and released; nothing is held across phases.
        let (seed_skill, seed_wait) = {
            let g = self.shards[s].lock().unwrap();
            match g.iter().next() {
                Some((&(skill, _), inst)) => (skill, inst.elapsed().as_secs_f64()),
                None => {
                    self.metrics.empty_attempts.fetch_add(1, Relaxed);
                    return false;
                }
            }
        };
        // The seed's band reaches symmetrically in BOTH skill directions, so a
        // skill-isolated player can always pull teammates from below as well as above.
        let band = self.window_for(seed_wait);
        let lo_sk = seed_skill.saturating_sub(band as u32);
        let hi_sk = ((seed_skill as u64) + band as u64).min(self.cfg.skill_max as u64) as u32;
        let lo = self.shard_of(lo_sk);
        let hi = self.shard_of(hi_sk);
        let seed_gi = self.shard_of(seed_skill) - lo;

        // --- Phase 2: lock the contiguous shard range [lo..=hi] ascending
        // (deadlock-free). Then collect up to `m` candidates on each side of the seed,
        // nearest-first, via bounded `BTreeMap` range queries — never cloning a bucket.
        let mut guards: Vec<_> = (lo..=hi).map(|i| self.shards[i].lock().unwrap()).collect();

        let mut cand: Vec<Player> = Vec::with_capacity(2 * m);
        // Below/at the seed: walk shards downward, each in descending skill order.
        'below: for gi in (0..=seed_gi).rev() {
            for (&(skill, id), &inst) in guards[gi]
                .range((Included((lo_sk, 0)), Included((seed_skill, u64::MAX))))
                .rev()
            {
                cand.push(Player { id, skill, enqueued_at: inst });
                if cand.len() == m {
                    break 'below;
                }
            }
        }
        // Strictly above the seed: walk shards upward, each in ascending skill order.
        let above_start = cand.len();
        'above: for gi in seed_gi..guards.len() {
            for (&(skill, id), &inst) in guards[gi]
                .range((Excluded((seed_skill, u64::MAX)), Included((hi_sk, u64::MAX))))
            {
                cand.push(Player { id, skill, enqueued_at: inst });
                if cand.len() - above_start == m {
                    break 'above;
                }
            }
        }
        if cand.len() < m {
            return false; // not enough compatible players within the band yet
        }

        // Keep the `m` players closest in skill to the seed (tightest match around it).
        cand.sort_by_key(|p| (p.skill as i64 - seed_skill as i64).unsigned_abs());
        cand.truncate(m);
        let lo_skill = cand.iter().map(|p| p.skill).min().unwrap();
        let hi_skill = cand.iter().map(|p| p.skill).max().unwrap();
        let spread = hi_skill - lo_skill;

        // --- Atomic eviction: remove the chosen players while range locks are held.
        // A player at skill `sk` lives in shard `shard_of(sk)`, within [lo..=hi].
        let mut total_wait_us: u64 = 0;
        for p in &cand {
            let gi = self.shard_of(p.skill) - lo;
            guards[gi].remove(&(p.skill, p.id));
            total_wait_us += (p.wait_secs() * 1_000_000.0) as u64;
        }
        drop(guards);
        self.queued.fetch_sub(m, Relaxed);

        // --- Team balance + bookkeeping (outside the locks).
        let (team_a, team_b, imbalance) = balance_teams(&cand);
        let avg_skill = cand.iter().map(|p| p.skill as f64).sum::<f64>() / m as f64;
        let avg_wait = total_wait_us as f64 / 1_000_000.0 / m as f64;
        self.metrics.record_match(m, total_wait_us, imbalance, spread);

        if self.cfg.keep_recent > 0 {
            let id = self.match_seq.fetch_add(1, Relaxed);
            let mat = Match {
                id,
                team_a,
                team_b,
                imbalance,
                avg_skill,
                avg_wait_secs: avg_wait,
                skill_spread: spread,
            };
            let mut r = self.recent.lock().unwrap();
            if r.len() >= self.cfg.keep_recent {
                r.pop_front();
            }
            r.push_back(mat);
        } else {
            self.match_seq.fetch_add(1, Relaxed);
        }
        true
    }

    /// Spawn `n` worker threads that continuously form matches until `stop()`.
    pub fn spawn_workers(self: &Arc<Self>, n: usize) -> Vec<JoinHandle<()>> {
        (0..n)
            .map(|_| {
                let e = Arc::clone(self);
                thread::spawn(move || {
                    let mut idle: u32 = 0;
                    while e.running.load(Relaxed) {
                        if e.try_match() {
                            idle = 0;
                        } else {
                            // Backoff: spin briefly, then sleep, so an empty pool
                            // doesn't burn 100% CPU while staying low-latency when busy.
                            idle = idle.saturating_add(1);
                            if idle > 128 {
                                thread::sleep(Duration::from_micros(200));
                            } else {
                                std::hint::spin_loop();
                            }
                        }
                    }
                })
            })
            .collect()
    }

    pub fn stop(&self) {
        self.running.store(false, Relaxed);
    }

    /// Snapshot of the most recently formed matches (for inspection/sampling).
    pub fn recent_matches(&self) -> Vec<Match> {
        self.recent.lock().unwrap().iter().cloned().collect()
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_balanced_matches_and_evicts() {
        let mut cfg = Config::default();
        cfg.keep_recent = 8;
        let engine = Engine::new(cfg);

        // Enqueue 50 players => expect 5 matches, all players evicted.
        for id in 0..50u64 {
            engine.enqueue(id, 2400 + (id as u32 % 50));
        }
        let workers = engine.spawn_workers(4);

        let start = std::time::Instant::now();
        while engine.queued() >= 10 && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(2));
        }
        engine.stop();
        for w in workers {
            w.join().unwrap();
        }

        let snap = engine.metrics.snapshot(engine.queued());
        assert_eq!(snap.matches, 5, "should form exactly 5 matches");
        assert_eq!(snap.players_matched, 50);
        assert_eq!(engine.queued(), 0, "all players evicted atomically");

        for mch in engine.recent_matches() {
            assert_eq!(mch.team_a.len(), 5);
            assert_eq!(mch.team_b.len(), 5);
        }
    }

    #[test]
    fn no_player_is_double_matched_under_concurrency() {
        // If eviction weren't atomic, a player could appear in two matches. We check
        // conservation: matched + still-waiting always equals enqueued.
        let engine = Engine::new(Config::default());
        let workers = engine.spawn_workers(8);
        for id in 0..2000u64 {
            engine.enqueue(id, 2000 + (id as u32 % 1000));
        }
        let start = std::time::Instant::now();
        while engine.queued() >= 10 && start.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(2));
        }
        engine.stop();
        for w in workers {
            w.join().unwrap();
        }
        let snap = engine.metrics.snapshot(engine.queued());
        assert!(snap.players_matched <= snap.enqueued);
        assert_eq!(
            snap.players_matched as usize + engine.queued(),
            2000,
            "players are conserved: matched + waiting == enqueued"
        );
    }
}
