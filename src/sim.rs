//! Load generator / simulation harness.
//!
//! Spawns many producer threads that concurrently slam the engine with synthetic
//! player requests, runs the matching workers in parallel, drains the queue, and
//! prints a performance + quality report.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::engine::{Config, Engine};
use crate::metrics::MetricsSnapshot;
use crate::rng::Rng;

pub struct SimParams {
    /// Total players to inject across all producers.
    pub players: u64,
    /// Number of concurrent producer (injection) threads.
    pub producers: usize,
    /// Number of matching worker threads.
    pub workers: usize,
    /// Engine configuration.
    pub config: Config,
    /// Print a few sample formed matches at the end.
    pub print_samples: bool,
}

/// Sample a skill rating with a roughly bell-shaped distribution centered on
/// `max/2` (average of 4 uniforms — a cheap Irwin–Hall approximation of normal).
pub fn sample_skill(rng: &mut Rng, max: u32) -> u32 {
    let s: f64 = (0..4).map(|_| rng.next_f64()).sum::<f64>() / 4.0;
    (s * max as f64) as u32
}

pub struct SimReport {
    pub snapshot: MetricsSnapshot,
    pub inject_elapsed: Duration,
    pub total_elapsed: Duration,
}

/// Run the simulation and print a report. Returns the final report for tests.
pub fn run_and_report(params: SimParams) -> SimReport {
    let engine = Engine::new(params.config.clone());
    let workers = engine.spawn_workers(params.workers);

    println!(
        "Injecting {} players via {} producer threads into a {}-shard pool, {} matching workers...",
        params.players, params.producers, params.config.num_shards, params.workers
    );

    let id_ctr = Arc::new(AtomicU64::new(0));
    let go = Arc::new(AtomicBool::new(false));
    let per = params.players / params.producers as u64;
    let skill_max = params.config.skill_max;

    // Build producers, then release them all at once for a true concurrent burst.
    let producer_handles: Vec<_> = (0..params.producers)
        .map(|t| {
            let e = Arc::clone(&engine);
            let ids = Arc::clone(&id_ctr);
            let go = Arc::clone(&go);
            thread::spawn(move || {
                let mut rng = Rng::new(0xD1CE_5EED ^ (t as u64).wrapping_mul(0x9E37_79B9));
                while !go.load(Relaxed) {
                    std::hint::spin_loop();
                }
                for _ in 0..per {
                    let id = ids.fetch_add(1, Relaxed);
                    e.enqueue(id, sample_skill(&mut rng, skill_max));
                }
            })
        })
        .collect();

    let start = Instant::now();
    go.store(true, Relaxed);
    for h in producer_handles {
        h.join().unwrap();
    }
    let inject_elapsed = start.elapsed();

    // Drain remaining players (workers keep running). A fixed injection batch always
    // leaves a small, skill-sparse tail that cannot gather a full team of 10 — so we
    // stop when the queue is empty OR matching has *stalled* (no progress for a short
    // window), and we measure throughput over the active window (time of last match),
    // not the trailing idle wait.
    let min_remaining = params.config.team_size * 2;
    let stall_after = Duration::from_millis(250);
    let safety_deadline = Instant::now() + Duration::from_secs(30);
    let mut last_matches = engine.metrics.matches.load(Relaxed);
    let mut last_progress = Instant::now();
    loop {
        let now = Instant::now();
        let matches = engine.metrics.matches.load(Relaxed);
        if matches > last_matches {
            last_matches = matches;
            last_progress = now;
        }
        if engine.queued() < min_remaining {
            last_progress = now;
            break;
        }
        if now.duration_since(last_progress) > stall_after || now >= safety_deadline {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    // Active window = injection start .. last match formed (excludes trailing idle).
    let total_elapsed = last_progress.saturating_duration_since(start);

    engine.stop();
    for w in workers {
        w.join().unwrap();
    }

    let snapshot = engine.metrics.snapshot(engine.queued());
    print_report(&snapshot, inject_elapsed, total_elapsed);

    if params.print_samples {
        println!("\nSample formed matches (team A skills | team B skills | imbalance):");
        for m in engine.recent_matches().into_iter().take(5) {
            let a: Vec<u32> = m.team_a.iter().map(|p| p.skill).collect();
            let b: Vec<u32> = m.team_b.iter().map(|p| p.skill).collect();
            println!(
                "  match #{:<5} {:?} (Σ{}) | {:?} (Σ{}) | Δ{:<4} spread {}",
                m.id,
                a,
                a.iter().sum::<u32>(),
                b,
                b.iter().sum::<u32>(),
                m.imbalance,
                m.skill_spread
            );
        }
    }

    SimReport {
        snapshot,
        inject_elapsed,
        total_elapsed,
    }
}

fn print_report(snap: &MetricsSnapshot, inject: Duration, total: Duration) {
    let inj_s = inject.as_secs_f64();
    let tot_s = total.as_secs_f64();
    let inject_rate = if inj_s > 0.0 {
        snap.enqueued as f64 / inj_s
    } else {
        0.0
    };
    let match_rate = if tot_s > 0.0 {
        snap.matches as f64 / tot_s
    } else {
        0.0
    };
    let player_throughput = if tot_s > 0.0 {
        snap.players_matched as f64 / tot_s
    } else {
        0.0
    };
    let empty_ratio = if snap.attempts > 0 {
        snap.empty_attempts as f64 / snap.attempts as f64 * 100.0
    } else {
        0.0
    };

    println!("\n================ MATCHMAKING REPORT ================");
    println!("Throughput");
    println!("  players injected      : {}", snap.enqueued);
    println!("  injection rate        : {:>12.0} players/s", inject_rate);
    println!("  matches formed        : {}", snap.matches);
    println!("  match rate            : {:>12.0} matches/s", match_rate);
    println!("  player throughput     : {:>12.0} players/s (matched)", player_throughput);
    println!("  injection wall time   : {:>12.3} s", inj_s);
    println!("  total wall time       : {:>12.3} s", tot_s);
    println!("Latency / fairness");
    println!("  avg queue wait        : {:>12.3} ms", snap.avg_wait_ms);
    println!("  max queue depth       : {}", snap.max_queue_depth);
    println!("  players still waiting : {}", snap.queued_now);
    println!("Match quality");
    println!("  avg team imbalance    : {:>12.2} skill (|ΣA-ΣB|)", snap.avg_imbalance);
    println!("  avg skill spread      : {:>12.2} skill (max-min of 10)", snap.avg_spread);
    println!("Engine internals");
    println!("  matching attempts     : {}", snap.attempts);
    println!("  empty-seed attempts   : {:>12.1} %", empty_ratio);
    println!("====================================================");
}
