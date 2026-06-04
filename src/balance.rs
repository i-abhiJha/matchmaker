//! Team balancing.
//!
//! Once 10 compatible players are gathered, they must be split into two teams of
//! five as fairly as possible. Minimizing the difference of the two teams' summed
//! skill is the balanced-partition problem (NP-hard in general), but for a fixed
//! 10-player match the search space is tiny: choosing 5 of 10 is C(10,5) = 252
//! subsets, halved to 126 once we pin player 0 to team A to remove mirror
//! duplicates. We brute-force all of them and pick the optimal split. This is a
//! constant amount of work per match (~126 iterations) and yields a provably
//! optimal balance for the chosen 10.

use crate::player::Player;

/// Split players into two equal halves minimizing |sum(A) - sum(B)|.
///
/// Returns `(team_a, team_b, imbalance)`. Assumes `players.len()` is even
/// (the engine always passes exactly `team_size * 2`).
pub fn balance_teams(players: &[Player]) -> (Vec<Player>, Vec<Player>, u32) {
    let n = players.len();
    debug_assert!(n % 2 == 0 && n <= 20, "balance_teams expects an even, small roster");
    let half = n / 2;
    let total: u64 = players.iter().map(|p| p.skill as u64).sum();

    let mut best_mask: u32 = 0;
    let mut best_diff = u64::MAX;

    // Enumerate all subsets; keep those with exactly `half` members and player 0
    // pinned to team A (bit 0 set) to skip the symmetric A<->B duplicates.
    for mask in 0u32..(1u32 << n) {
        if mask.count_ones() as usize != half {
            continue;
        }
        if mask & 1 == 0 {
            continue;
        }
        let mut sum_a: u64 = 0;
        for i in 0..n {
            if mask & (1 << i) != 0 {
                sum_a += players[i].skill as u64;
            }
        }
        let sum_b = total - sum_a;
        let diff = sum_a.abs_diff(sum_b);
        if diff < best_diff {
            best_diff = diff;
            best_mask = mask;
            if diff == 0 {
                break; // can't do better than a perfect split
            }
        }
    }

    let mut a = Vec::with_capacity(half);
    let mut b = Vec::with_capacity(n - half);
    for i in 0..n {
        if best_mask & (1 << i) != 0 {
            a.push(players[i]);
        } else {
            b.push(players[i]);
        }
    }
    (a, b, best_diff as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::Player;

    fn roster(skills: &[u32]) -> Vec<Player> {
        skills.iter().enumerate().map(|(i, &s)| Player::new(i as u64, s)).collect()
    }

    #[test]
    fn perfect_split_is_found() {
        let players = roster(&[100, 100, 100, 100, 100, 100, 100, 100, 100, 100]);
        let (a, b, imb) = balance_teams(&players);
        assert_eq!(a.len(), 5);
        assert_eq!(b.len(), 5);
        assert_eq!(imb, 0);
    }

    #[test]
    fn optimal_imbalance_for_skewed_roster() {
        // Sums: total = 5500. Best achievable split difference is 0 here:
        // {1000,1000,100,100,100}=2300 vs {1000,1000,1000,100,100}=3200 ... let's
        // just assert the brute force matches an exhaustive recompute.
        let players = roster(&[1000, 1000, 1000, 1000, 100, 100, 100, 100, 50, 50]);
        let (_a, _b, imb) = balance_teams(&players);

        // Independently compute the true optimum.
        let n = players.len();
        let total: i64 = players.iter().map(|p| p.skill as i64).sum();
        let mut best = i64::MAX;
        for mask in 0u32..(1 << n) {
            if mask.count_ones() != (n as u32 / 2) {
                continue;
            }
            let mut sa = 0i64;
            for i in 0..n {
                if mask & (1 << i) != 0 {
                    sa += players[i].skill as i64;
                }
            }
            best = best.min((sa - (total - sa)).abs());
        }
        assert_eq!(imb as i64, best);
    }
}
