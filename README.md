# 5v5 Real-Time Competitive Matchmaker — Write-up

A matchmaking engine in Rust that keeps waiting players in memory and continuously
pulls them into balanced 5v5 matches. The notes below cover how I tackled the main
engineering problems, the algorithmic trade-offs, the time/space complexity, and how
I'd scale it.

## How to run

No external crates, just `std`, so it builds and runs offline.

```bash
# Unit + concurrency tests
cargo test --release

# Demo service: injects 50k players and prints a few sample balanced matches
cargo run --release --bin matchmaker

# Load test: inject N players with P producer threads and W matching workers
cargo run --release --bin simulate -- 1000000 16 8
#                                      ^players  ^producers ^workers
```

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
most at risk). Two behaviours fall out of the same rule:

- Players in fresh or crowded skill regions match almost immediately inside a tiny
  band, which means high quality.
- Players off on their own at the top or bottom of the ladder keep relaxing until
  they find a game, which keeps their wait bounded.

The simulation shows the trade-off clearly. With a deep backlog (1 worker, ~90 ms
wait) the matcher has a big candidate pool and forms very tight matches (spread around
9). Drain the queue as fast as it fills (8 workers, ~2 ms wait) and there are fewer
candidates near any seed, so matches come out looser (spread around 81), though still
inside one base band. Lower latency, slightly lower quality. That's the curve I'd
expect.

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
same player. A test, `no_player_is_double_matched_under_concurrency`, checks the
invariant `matched + waiting == enqueued` with 8 workers hammering it.

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
anywhere in the pool. In production they'd wait for the next batch of arrivals, or get
filled out with bots.

### 4. Team balancing

Splitting 10 gathered players into two fair teams of five is a balanced-partition
problem, which is NP-hard in general, but here the roster is fixed and tiny so it
doesn't matter. I brute-force all `C(10,5)/2 = 126` partitions (pinning player 0 to
team A drops the mirror-image duplicates) and keep the one that minimises
`|Σ skill_A − Σ skill_B|`. It's a flat ~126 iterations per match and it's optimal for
those 10 players, which is why matches land 0–2 rating points apart. `balance.rs` has
a test that re-derives the optimum independently and checks they agree.

### 5. Health metrics that don't slow anything down

Every counter is a plain atomic (`AtomicU64` / `AtomicUsize`) bumped with `Relaxed`
ordering, so the matching hot path only ever does `fetch_add`s. No locks, no
allocation. The queue-depth high-water mark is a lock-free CAS loop. A separate
monitor thread reads a good-enough `snapshot()` off to the side to work out rates,
average wait, average imbalance and spread. So observability never serialises against
the matchmaking loop or slows it down.

## Algorithmic trade-offs

- **Per-player relaxing band vs. a fixed skill threshold.** A fixed threshold either
  starves the tails or floods the middle with bad matches. The widening band lets the
  same rule serve both, and acceptance keyed off the oldest member means latency is
  bounded for whoever needs it most.
- **Brute-force balancing vs. a heuristic.** With only 126 partitions, exact is
  cheaper and simpler than any approximation, and it's provably the best split for
  that roster. The trade-off only appears if match size grows large, where I'd switch
  to a greedy or DP partition.
- **Lock striping vs. one global lock vs. lock-free.** Striping is the middle ground:
  most of the parallelism of a lock-free design with a fraction of the complexity, at
  the cost of contention in the hot central shards (see scaling, below).

## Complexity

Let `N` = players waiting, `m` = match size (10), `k` = shards a band spans (usually
1–2, up to `num_shards` only for a fully relaxed seed).

| Operation | Time | Space |
|---|---|---|
| `enqueue` | `O(log N)` (one `BTreeMap` insert) | `O(1)` |
| `try_match` (one attempt) | `O(log N + k·m)` — bounded range scans + `m log m` sort + 126 balance iters | `O(k·m)` scratch |
| Atomic eviction of 10 | `O(m · log N)` (`BTreeMap` removes) | `O(1)` |
| Metrics update | `O(1)` atomic | `O(1)` |
| Pool | — | `O(N)` players + `O(keep_recent)` retained matches |

The property I care about most is that the per-attempt cost doesn't depend on how many
players a shard holds. That's what keeps it fast as the pool grows into the hundreds
of thousands. Balancing is constant work. Draining all `N` players is
`O((N/m)·(log N + k·m))` total, spread across the worker pool.

## Scaling challenges

- **Contention follows the population.** Skill is roughly a bell curve, so the central
  shards run hot and workers pile into them while the outer shards sit idle (visible in
  the "empty-seed attempts" metric). Options if it became a problem: more and finer
  shards, biasing seed selection toward the populated shards, or sharding the worker
  set itself by skill band so each worker owns a region. Going sharded-by-region or
  lock-free would drop the central `Mutex` entirely.
- **Single process, in memory, not durable.** Everything lives in RAM, so a crash
  loses the queue. To scale out horizontally I'd shard players across nodes by region
  first and then skill (cross-region play is rare and latency-sensitive anyway), with a
  thin gateway routing the `enqueue`s. A persistent log or a Redis-style store would
  add durability and backpressure.
- **Fairness vs. quality vs. cost.** `base_window`, `relax_per_sec`, `max_window`,
  `num_shards` and the worker count are the knobs. Drain faster and wait drops but the
  candidate pool shrinks and matches loosen; let the queue sit deeper and quality
  tightens at the cost of latency. They all live in `engine::Config`.
- **The sparse tail.** Any finite batch leaves a few skill-extreme players who can't
  find 10 peers. Real systems backfill from the steady stream of new arrivals, relax
  further, or drop in bots.
- **Richer quality signals.** Right now quality means skill proximity plus balanced
  sums. A production version would also weigh latency/ping, roles and queue
  composition, party grouping (keep premades together), and anti-rematch / anti-smurf
  signals. Each is another dimension on the candidate filter and the balance objective.
