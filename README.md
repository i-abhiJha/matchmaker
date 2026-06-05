# 5v5 Real-Time Competitive Matchmaker

A matchmaking engine written in Rust. It keeps waiting players in memory and keeps
pulling them into balanced 5v5 matches (two teams of five). Under the hood it's a
skill-sharded, lock-striped player pool that several worker threads scan at the same
time. Players are evicted atomically when they get matched, the skill constraints
relax the longer someone waits, teams are balanced optimally, and the health counters
are all lock-free.

No external crates, just `std`. It builds and runs offline.

```
.
├── Cargo.toml
├── README.md
└── src
    ├── lib.rs          # crate root / re-exports
    ├── player.rs       # Player + Match domain types
    ├── engine.rs       # the concurrent matchmaking engine (core logic + tests)
    ├── balance.rs      # optimal 2-team partition (+ tests)
    ├── metrics.rs      # lock-free atomic health metrics
    ├── rng.rs          # tiny SplitMix64 PRNG (no rand crate)
    ├── sim.rs          # load-generator / simulation harness
    ├── main.rs         # demo "service" binary
    └── bin/simulate.rs # stress-test binary
```

## Quick start

```bash
# Run the unit + concurrency tests
cargo test --release

# Run the demo service (injects 50k players, prints a few sample matches)
cargo run --release --bin matchmaker

# Stress test: inject N players with P producer threads and W matching workers
cargo run --release --bin simulate -- 1000000 16 8
#                                      ^players  ^producers ^workers
```

### Numbers I saw locally (Apple Silicon, 8 workers)

| Workload | Player throughput | Avg queue wait | Avg team imbalance | Avg skill spread |
|---|---|---|---|---|
| 500k players | ~1.0 M players/s | 2–90 ms | < 5 skill pts | 9–81 skill pts |
| 1M players | ~0.9 M players/s | ~80 ms | ~5 skill pts | ~39 skill pts |

A couple of matches it formed (notice how close the two skill sums are):

```
match #4927  [3315,3325,3333,3350,3363] (Σ16686) | [3325,3332,3337,3345,3348] (Σ16687) | Δ1  spread 48
match #4929  [1939,1931,1920,1960,1905] (Σ9655)  | [1934,1930,1918,1964,1909] (Σ9655)  | Δ0  spread 59
```

---

## How I approached each problem

### 1. The core algorithm: latency vs. match quality

This is the central tension. Wait longer and you can assemble a tighter, fairer
lobby; ship something now and it's looser. I turned that into an explicit per-player
dial instead of a fixed threshold:

```
band(wait) = min(base_window + relax_per_sec * wait, max_window)
```

Each player's acceptable skill band starts narrow (`base_window = 80` rating points,
so high quality) and grows the longer they've been sitting in the queue. A match only
goes through when all 10 players fit inside the band of the oldest member (the one
most at risk). You get two behaviours falling out of the same rule:

- Players in fresh or crowded skill regions match almost immediately inside a tiny
  band, which means high quality.
- Players who are off on their own at the top or bottom of the ladder keep relaxing
  until they find a game, which keeps their wait bounded.

The simulation shows the trade-off pretty clearly. Run it with a deep backlog (1
worker, ~90 ms wait) and the matcher has a big candidate pool to choose from, so it
forms very tight matches (spread around 9). Drain the queue as fast as it fills (8
workers, ~2 ms wait) and there are fewer candidates near any given seed, so matches
come out looser (spread around 81), though still inside one base band. Lower latency,
slightly lower quality. That's the curve I'd expect.

### 2. Thread-safe state and atomic eviction

The pool is sharded by skill and striped by lock. The skill axis (`0..skill_max`) is
chopped into `num_shards` contiguous buckets, and each bucket is a
`Mutex<BTreeMap<(skill, id), Instant>>`. Because the map is keyed by skill, the one
thing a worker actually needs — "give me the waiters near skill *s*" — is just a
bounded `BTreeMap` range query, `O(log n + m)`. A bucket never gets cloned or scanned
end to end.

A match attempt does three things:

1. Peeks the seed shard under one short lock to figure out how wide the seed's band is.
2. Locks the contiguous shard range `[lo..=hi]` that the band covers, always in
   ascending index order.
3. Gathers up to `m` nearest candidates on each side of the seed and removes the
   chosen 10, all while it still holds those range locks.

Steps 2 and 3 are what make eviction atomic: the gather and the removal happen in one
critical section over exactly the shards involved, so two workers can't both grab the
same player. There's a test, `no_player_is_double_matched_under_concurrency`, that
checks the invariant `matched + waiting == enqueued` with 8 workers hammering it.

On deadlocks: every worker takes multi-shard ranges in the same ascending order and
never holds a lock across the phase-1 peek. With one global acquisition order there's
no way to build a cycle, so no deadlock. The striping also means workers operating on
different skill ranges run fully in parallel; you only get contention where the
population (and therefore the matches) bunches up.

### 3. Time-based constraint relaxation

This is the same `band(wait)` function, and the important part is that the band
reaches out symmetrically in both skill directions. Take a lone high-MMR player: they
become the lowest-skill seed in their own shard, and as their band widens it reaches
*down* into the neighbouring shards to pull in teammates. Acceptance is measured off
the oldest member, so the player closest to starving is the one whose tolerance gets
relaxed, never a player who just joined vetoing the lobby. Once the band hits
`max_window` it stops growing, which acts as a quality floor. The only players left
unmatched after that are a genuinely sparse tail with fewer than 10 compatible peers
anywhere in the pool. In production they'd just wait for the next batch of arrivals,
or get filled out with bots.

### 4. Team balancing

Splitting 10 gathered players into two fair teams of five is a balanced-partition
problem, which is NP-hard in general, but here the roster is fixed and tiny so I don't
care. I brute-force all `C(10,5)/2 = 126` partitions (pinning player 0 to team A drops
the mirror-image duplicates) and keep the one that minimises
`|Σ skill_A − Σ skill_B|`. It's a flat ~126 iterations per match and it's optimal for
those 10 players, which is why the sample matches above land 0–2 rating points apart.
`balance.rs` has a test that re-derives the optimum independently and checks they
agree.

### 5. Health metrics that don't slow anything down

Every counter is a plain atomic (`AtomicU64` / `AtomicUsize`) bumped with `Relaxed`
ordering, so the matching hot path only ever does `fetch_add`s. No locks, no
allocation. The queue-depth high-water mark is a lock-free CAS loop. A separate
monitor thread reads a good-enough `snapshot()` off to the side to work out rates,
average wait, average imbalance and spread. The upshot is that observability never
serialises against the matchmaking loop or slows it down.

---

## Complexity

Let `N` = players waiting, `m` = match size (10), `k` = shards a band spans (usually
1–2, and only up to `num_shards` for a fully relaxed seed).

| Operation | Time | Space |
|---|---|---|
| `enqueue` | `O(log N)` (one `BTreeMap` insert) | `O(1)` |
| `try_match` (one attempt) | `O(log N + k·m)` — bounded range scans + `m log m` sort + 126 balance iters | `O(k·m)` scratch |
| Atomic eviction of 10 | `O(m · log N)` (`BTreeMap` removes) | `O(1)` |
| Metrics update | `O(1)` atomic | `O(1)` |
| Pool | — | `O(N)` players + `O(keep_recent)` retained matches |

The thing I care about most is that the per-attempt cost doesn't depend on how many
players a shard happens to hold. That's what keeps it fast as the pool grows into the
hundreds of thousands. Balancing is constant work. Draining all `N` players is
`O((N/m)·(log N + k·m))` total, spread across the worker pool.

## Scaling and trade-offs

- **Contention follows the population.** Skill is roughly a bell curve, so the central
  shards run hot and workers pile into them while the outer shards sit idle (you can
  see this in the "empty-seed attempts" metric). Options if it became a problem: more
  and finer shards, biasing seed selection toward the populated shards, or sharding
  the worker set itself by skill band so each worker owns a region. Going
  sharded-by-region or lock-free would drop the central `Mutex` entirely.
- **Single process, in memory, not durable.** Everything lives in RAM, so a crash
  loses the queue. To scale out horizontally I'd shard players across nodes by region
  first and then skill (cross-region play is rare and latency-sensitive anyway), with
  a thin gateway routing the `enqueue`s. A persistent log or a Redis-style store would
  add durability and backpressure.
- **Fairness vs. quality vs. cost.** `base_window`, `relax_per_sec`, `max_window`,
  `num_shards` and the worker count are the knobs. Drain faster and wait drops but the
  candidate pool shrinks and matches loosen; let the queue sit deeper and quality
  tightens at the cost of latency. All of these live in `engine::Config`.
- **The sparse tail.** Any finite batch leaves a few skill-extreme players who can't
  find 10 peers. Real systems backfill from the steady stream of new arrivals, relax
  further, or drop in bots. The sim just reports them as "still waiting".
- **Richer quality signals.** Right now quality means skill proximity plus balanced
  sums. A production version would also weigh latency/ping, roles and queue
  composition, party grouping (keep premades together), and anti-rematch / anti-smurf
  signals. Each is another dimension on the candidate filter and the balance objective.

## What the tests cover

```bash
cargo test --release
```

- `balance::perfect_split_is_found` / `optimal_imbalance_for_skewed_roster` — the
  partition is optimal, cross-checked against brute force.
- `engine::forms_balanced_matches_and_evicts` — 50 players turn into exactly 5
  matches, everyone gets evicted, every team has 5 players.
- `engine::no_player_is_double_matched_under_concurrency` — players are conserved
  (`matched + waiting == enqueued`) under 8 concurrent workers, which is the proof
  that eviction is atomic.
