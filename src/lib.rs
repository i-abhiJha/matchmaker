//! A high-performance, thread-safe 5v5 matchmaking engine.
//!
//! The engine keeps waiting players in an in-memory, skill-sharded pool and runs
//! a set of worker threads that concurrently scan the pool, form balanced
//! 10-player matches (two teams of five), and atomically evict the matched
//! players. See the module docs and the README for the design rationale.

pub mod balance;
pub mod engine;
pub mod metrics;
pub mod player;
pub mod rng;
pub mod sim;

pub use engine::{Config, Engine};
pub use metrics::{Metrics, MetricsSnapshot};
pub use player::{Match, Player};
