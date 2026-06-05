//! Injects thousands of concurrent player
//! requests into the engine to demonstrate performance under load.
//!
//! Usage:
//!   cargo run --release --bin simulate -- [players] [producers] [workers]

use std::thread::available_parallelism;

use matchmaker::engine::Config;
use matchmaker::sim::{run_and_report, SimParams};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let players: u64 = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let producers: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8)
        .max(1);
    let default_workers = available_parallelism().map(|n| n.get()).unwrap_or(4).max(2);
    let workers: usize = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_workers)
        .max(1);

    let params = SimParams {
        players,
        producers,
        workers,
        config: Config::default(),
        print_samples: false,
    };

    run_and_report(params);
}
