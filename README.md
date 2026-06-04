# 5v5 Real-Time Competitive Matchmaker

A high-performance, thread-safe matchmaking engine in **Rust** that holds waiting
players in memory and continuously groups them into **balanced 5v5 matches** (two
teams of five). It is built around a skill-sharded, lock-striped player pool that
multiple worker threads scan in parallel, with **atomic eviction**, **wait-time
based constraint relaxation**, **optimal team balancing**, and **lock-free health
metrics**.

> **Zero external dependencies** — pure `std`. It builds and runs offline.

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
    ├── rng.rs          # tiny SplitMix64 PRNG (no `rand` crate)
    ├── sim.rs          # load-generator / simulation harness
    ├── main.rs         # demo "service" binary
    └── bin/simulate.rs # stress-test binary
```

## Quick start

```bash
# Run the unit + concurrency tests
cargo test --release

# Run the demo service (injects 50k players, prints sample balanced matches)
cargo run --release --bin matchmaker

# Stress test: inject N players with P producer threads and W matching workers
cargo run --release --bin simulate -- 1000000 16 8
#                                      ^players  ^producers ^workers
```

### Representative results (Apple Silicon, 8 workers)

| Workload | Player throughput | Avg queue wait | Avg team imbalance | Avg skill spread |
|---|---|---|---|---|
| 500k players | ~1.0 M players/s | 2–90 ms | < 5 skill pts | 9–81 skill pts |
| 1M players | ~0.9 M players/s | ~80 ms | ~5 skill pts | ~39 skill pts |

Sample formed matches (note the near-perfect skill-sum balance):

```
match #4927  [3315,3325,3333,3350,3363] (Σ16686) | [3325,3332,3337,3345,3348] (Σ16687) | Δ1  spread 48
match #4929  [1939,1931,1920,1960,1905] (Σ9655)  | [1934,1930,1918,1964,1909] (Σ9655)  | Δ0  spread 59
```

---

## How each engineering challenge was tackled

### 1. The core algorithm — latency vs. match quality

Matchmaking is a tug-of-war: wait longer to assemble a *tighter*, fairer lobby, or
ship a *looser* one *now*. We make this an explicit, per-player dial:

```
band(wait) = min(base_window + relax_per_sec * wait, max_window)
```

A player's acceptable skill band starts narrow (`base_window = 80` rating points →
high quality) and **widens the longer they wait**. A match is only accepted when 10
players fit inside the band of the at-risk (oldest) member. The result is an
emergent, self-tuning policy:

- **Fresh / dense skill regions** match almost instantly inside a tiny band → top quality.
- **Isolated high/low-skill players** progressively relax until they match → bounded latency.

The simulation makes the tradeoff visible: with a **deep backlog** (1 worker, ~90 ms
wait) the matcher has a large candidate pool and produces **very tight** matches
(spread ≈ 9). When the queue is **drained as fast as it fills** (8 workers, ~2 ms
wait) there are fewer candidates near any seed, so matches are looser (spread ≈ 81)
but still well within one base band. Lower latency, slightly lower quality — exactly
the expected curve.

### 2. Thread-safe state & atomic eviction

The pool is a **skill-sharded, lock-striped** structure: the skill axis
(`0..skill_max`) is split into `num_shards` contiguous buckets, each a
`Mutex<BTreeMap<(skill, id), Instant>>`. Because the map is keyed by skill, every
operation a worker needs — "find waiters near skill *s*" — is a bounded `BTreeMap`
range query (`O(log n + m)`); a bucket is **never cloned or fully scanned**.

A match attempt:

1. **Peeks** the seed shard (one short lock) to size the seed's band.
2. **Locks the contiguous shard range** `[lo..=hi]` the band spans, **always in
   ascending index order**.
3. **Gathers** up to `m` nearest candidates on each side of the seed and **removes
   the chosen 10** — all while the range locks are held.

Steps 2–3 make eviction **atomic**: the gather and the removal are one critical
section over exactly the shards involved, so two workers can never claim the same
player. The bundled test `no_player_is_double_matched_under_concurrency` asserts the
invariant `matched + waiting == enqueued` under 8 concurrent workers.

**Deadlock freedom:** every worker acquires multi-shard ranges in the *same*
(ascending) order, and never holds a lock across the Phase-1 peek. With a global
acquisition order, no cyclic wait is possible. Lock striping means workers on
disjoint skill ranges proceed fully in parallel; contention only appears where the
population (and thus the matches) concentrate.

### 3. Time-based constraint relaxation

Handled by the same `band(wait)` function, and crucially the band reaches
**symmetrically in both skill directions**. A skill-isolated player (e.g. a lone
high-MMR queuer) becomes the lowest-skill seed of its own shard, and its widening
band reaches *down* across neighbouring shards to pull in teammates. Acceptance is
gauged off the **oldest** member, so the player most at risk of starving is the one
whose tolerance is relaxed — never a fresh joiner vetoing the lobby. Past
`max_window` the band stops growing (a quality floor); the only players left unmatched
are a genuinely sparse tail with fewer than 10 compatible peers in the entire pool —
in production these simply wait for the next arrivals (or fill with bots).

### 4. Team balance optimization

Splitting 10 gathered players into two fair teams of five is a balanced-partition
problem (NP-hard in general), but the roster is fixed and tiny. We **brute-force all
`C(10,5)/2 = 126` partitions** (pinning player 0 to team A removes mirror
duplicates) and pick the split minimizing `|Σ skill_A − Σ skill_B|`. This is a
constant ~126 iterations per match and is **provably optimal** for the chosen 10 —
the samples above show imbalances of 0–2 rating points. `balance.rs` includes a test
that cross-checks the result against an independent exhaustive optimum.

### 5. Low-latency health metrics

Every counter is a plain atomic (`AtomicU64` / `AtomicUsize`) updated with `Relaxed`
ordering; the matching hot path only ever does `fetch_add`s — **no locks, no
allocation**. The queue-depth high-water mark uses a lock-free CAS loop. A monitor
reads a consistent-enough `snapshot()` off-thread to derive rates, average wait,
average imbalance and spread. Observability therefore never serializes against, or
slows down, the matchmaking loop.

---

## Complexity analysis

Let `N` = players waiting, `m` = match size (10), `k` = shards a band spans
(typically 1–2; up to `num_shards` only for maximally-relaxed seeds).

| Operation | Time | Space |
|---|---|---|
| `enqueue` | `O(log N)` (one `BTreeMap` insert) | `O(1)` |
| `try_match` (one attempt) | `O(log N + k·m)` — bounded range scans + `m log m` sort + **126** balance iters | `O(k·m)` scratch |
| Atomic eviction of 10 | `O(m · log N)` (`BTreeMap` removes) | `O(1)` |
| Metrics update | `O(1)` atomic | `O(1)` |
| **Pool** | — | `O(N)` players + `O(keep_recent)` retained matches |

Per-attempt cost is **independent of how many players a shard holds** — the key
property that lets the engine stay fast as the pool grows into the hundreds of
thousands. Balancing is constant work. The work to fully drain `N` players is
`O((N/m)·(log N + k·m))` spread across the worker pool.

## Scaling challenges & trade-offs

- **Lock contention is population-shaped.** Skill follows a bell curve, so central
  shards are hot and workers funnel there; outer shards sit idle (visible as the
  "empty-seed attempts" metric). Mitigations: more/finer shards, biasing seed
  selection toward populated shards, or sharding the *worker set* by skill band so a
  worker owns a region. A lock-free / sharded-by-region design removes the central
  `Mutex` entirely.
- **Single-process, in-memory, non-durable.** State lives in RAM; a crash loses the
  queue. Horizontal scale-out shards players across nodes by **region first, then
  skill** (cross-region play is rare and latency-sensitive), with a thin gateway
  routing `enqueue`s. A persistent log / Redis-style store adds durability and
  backpressure.
- **Fairness vs. quality vs. cost.** `base_window`, `relax_per_sec`, `max_window`,
  `num_shards`, and worker count are the tuning surface. Faster draining lowers wait
  but shrinks the candidate pool and loosens matches; a deeper queue tightens quality
  at the cost of latency. These are config knobs in `engine::Config`.
- **The sparse tail.** A finite batch always leaves a handful of skill-extreme
  players who can't gather 10 peers. Real systems backfill from continuous arrivals,
  widen further, or use bots; the simulation reports them as "still waiting".
- **Richer match quality.** Today quality = skill proximity + balanced sums.
  Production would fold in latency/ping, role/queue composition, party grouping
  (keep premades together), and anti-rematch/anti-smurf signals — each an added
  dimension on the candidate filter and the balance objective.

## What the tests cover

```bash
cargo test --release
```

- `balance::perfect_split_is_found` / `optimal_imbalance_for_skewed_roster` — the
  partition is optimal (cross-checked against brute force).
- `engine::forms_balanced_matches_and_evicts` — 50 players → exactly 5 matches, all
  evicted, every team has 5 players.
- `engine::no_player_is_double_matched_under_concurrency` — players are conserved
  (`matched + waiting == enqueued`) under 8 concurrent workers, proving atomic eviction.
