//! Demo "service" entry point.
//!
//! Boots the in-memory matchmaking engine and runs a representative workload so
//! you can see balanced matches being formed. For the heavy concurrent load test,
//! run the `simulate` binary instead (`cargo run --release --bin simulate`).

use std::thread::available_parallelism;

use matchmaker::engine::Config;
use matchmaker::sim::{run_and_report, SimParams};

fn main() {
    println!("=== 5v5 Real-Time Competitive Matchmaker — demo ===\n");

    let workers = available_parallelism().map(|n| n.get()).unwrap_or(4).max(2);

    let params = SimParams {
        players: 50_000,
        producers: 8,
        workers,
        config: Config::default(),
        print_samples: true,
    };

    run_and_report(params);

    println!(
        "\nTip: stress test with  `cargo run --release --bin simulate -- <players> <producers> <workers>`"
    );
}
