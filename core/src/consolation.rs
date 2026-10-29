//! The consolation draw: what a losing trader is given back, and why it cannot
//! be farmed.
//!
//! When a spread expires worthless the holder loses the premium. A share of that
//! loss is routed to a bucket, and at the end of an epoch the bucket is drawn
//! among the losers — weighted by how much they lost, with a pity multiplier for
//! anyone who has been unlucky several epochs running. One winner takes the pot.
//!
//! This is *not* a rebate and it is not compensation, and the difference is the
//! whole design problem: the moment giving money back to losers can beat holding
//! a winning position, the venue is paying users to lose and the economics
//! invert. Three things keep it honest, and all three are integers the chain can
//! check:
//!
//! - **Pari-mutuel.** The pot is the bucket and one account takes it, so the sum
//!   of expectations is exactly the pot. It cannot promise more than it holds —
//!   unlike an "insurance fund" that books a payout it has not collected.
//! - **Funded from yield, never collateral.** The bucket is filled from fee and
//!   float income outside `Pool::equity`; a draw can never be paid with money
//!   that is standing behind a worst case.
//! - **Deliberately −EV.** `Rules::keeps_losing_unprofitable` is the constraint
//!   that makes losing on purpose worth less than not losing, and it is checked
//!   at the boundary rather than assumed.
//!
//! Participants are opaque `u64` handles. The contract owns the mapping to
//! accounts; this crate only needs to add and compare numbers.

/// One whole, in basis points.
pub const BASIS: u128 = 10_000;

/// The payout policy for one epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rules {
    /// Share of each worthless settlement that joins the bucket, in bps.
    pub fund_bps: u32,
    /// Extra weight per consecutive lost epoch, in bps of par. `2_000` means a
    /// streak of one adds 20%, two adds 40%, and so on.
    pub pity_bps: u32,
    /// Ceiling on the multiplier, in bps. `40_000` is 4x par.
    pub pity_cap_bps: u32,
}

impl Rules {
    /// 5% of every worthless expiry, +20% per lost streak, capped at 4x. The
    /// product is 0.2, so the best a farmer can hope for is one fifth of what
    /// they threw away.
    pub const STANDARD: Rules = Rules {
        fund_bps: 500,
        pity_bps: 2_000,
        pity_cap_bps: 40_000,
    };

    /// Nothing funded: no draw, no pity, no reason to lose.
    pub const OFF: Rules = Rules {
        fund_bps: 0,
        pity_bps: 0,
        pity_cap_bps: BASIS as u32,
    };

    /// The weight multiplier for someone who has lost `streak` epochs in a row.
    /// Saturating rather than wrapping: a long-lost account must not become a
    /// number larger than the pool.
    pub fn multiplier_bps(&self, streak: u32) -> u128 {
        let grown = BASIS.saturating_add(u128::from(self.pity_bps).saturating_mul(u128::from(streak)));
        grown.min(u128::from(self.pity_cap_bps).max(BASIS))
    }

    /// What one loss is worth in the draw. Floored, so a fractional weight can
    /// never inflate the total past what the pot can pay.
    pub fn weight(&self, loss: u128, streak: u32) -> Option<u128> {
        loss.checked_mul(self.multiplier_bps(streak))?.checked_div(BASIS)
    }

    /// What joins the bucket from one worthless settlement. Floored for the same
    /// reason: the bucket may be short by an atom, never over-promised by one.
    pub fn contribution(&self, loss: u128) -> Option<u128> {
        loss.checked_mul(u128::from(self.fund_bps))?.checked_div(BASIS)
    }

    /// A participant's share of the pot in expectation. Pari-mutuel with one
    /// winner, so this is *exactly* the expectation, not an approximation of it.
    pub fn expected(&self, pot: u128, weight: u128, total_weight: u128) -> u128 {
        if total_weight == 0 {
            return 0;
        }
        pot.saturating_mul(weight) / total_weight
    }

    /// The condition the whole product rests on.
    ///
    /// For a farmer with a trivial loss sitting inside a large pool, the
    /// expectation approaches `fund × multiplier`, so losing beats not losing
    /// exactly when that product reaches par. Everything else in this module is
    /// a consequence; this is the one line a deployment has to get right.
    pub fn keeps_losing_unprofitable(&self) -> bool {
        let product = u128::from(self.fund_bps).saturating_mul(self.multiplier_bps(u32::MAX));
        product < BASIS.saturating_mul(BASIS)
    }

    /// The most any single participant's expectation can be, as a fraction of
    /// their own loss, in bps. Reported so a screen can show the edge rather
    /// than hide it.
    pub fn worst_case_ev_bps(&self) -> u128 {
        u128::from(self.fund_bps).saturating_mul(self.multiplier_bps(u32::MAX)) / BASIS
    }
}

/// One account's standing in the current epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub participant: u64,
    pub loss: u128,
    /// Epochs lost in a row *before* this one. The current epoch's loss is in
    /// `loss`; counting it here too would double the pity.
    pub streak: u32,
}

/// The epoch's enrolments. `Book` holds no money: the bucket lives in the
/// contract's custody, and a draw reads this to decide who takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Book {
    pub entries: Vec<Entry>,
}

impl Book {
    pub fn empty() -> Book {
        Book { entries: Vec::new() }
    }

    /// Enrol a loss. Repeat entries within one epoch merge into a single weight,
    /// because the draw is against the epoch, not against each ticket.
    pub fn record(&mut self, participant: u64, loss: u128) -> Option<()> {
        if loss == 0 {
            return Some(());
        }
        if let Some(entry) = self.entries.iter_mut().find(|row| row.participant == participant) {
            entry.loss = entry.loss.checked_add(loss)?;
            return Some(());
        }
        self.entries.push(Entry {
            participant,
            loss,
            streak: 0,
        });
        Some(())
    }

    /// Carry the pity of previous epochs onto the accounts still enrolled.
    pub fn roll_streaks(&mut self) -> Option<()> {
        for entry in self.entries.iter_mut() {
            entry.streak = entry.streak.checked_add(1)?;
        }
        Some(())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every weight, in enrolment order.
    pub fn weights(&self, rules: &Rules) -> Option<Vec<u128>> {
        let mut out = Vec::with_capacity(self.entries.len());
        for entry in self.entries.iter() {
            out.push(rules.weight(entry.loss, entry.streak)?);
        }
        Some(out)
    }

    pub fn total_weight(&self, rules: &Rules) -> Option<u128> {
        let mut total = 0u128;
        for weight in self.weights(rules)?.into_iter() {
            total = total.checked_add(weight)?;
        }
        Some(total)
    }

    /// Pick the winner from a roll in `0..total_weight`. Cumulative and
    /// first-past-the-post, so the same roll on the same book always names the
    /// same account — the draw is reproducible, which is the only way anyone
    /// downstream can check it.
    pub fn pick(&self, rules: &Rules, roll: u128) -> Option<u64> {
        let total = self.total_weight(rules)?;
        if total == 0 || roll >= total {
            return None;
        }
        let mut cumulative = 0u128;
        for (index, weight) in self.weights(rules)?.into_iter().enumerate() {
            cumulative = cumulative.checked_add(weight)?;
            if roll < cumulative {
                return Some(self.entries[index].participant);
            }
        }
        None
    }

    /// Close the epoch: the winner's pity resets, everyone else carries one more
    /// lost streak, and every enrolled loss is consumed. Burning the loss is what
    /// stops the same defeat from entering two draws.
    pub fn finish(&mut self, winner: Option<u64>) {
        for entry in self.entries.iter_mut() {
            if Some(entry.participant) == winner {
                entry.streak = 0;
            } else {
                entry.streak = entry.streak.saturating_add(1);
            }
        }
        self.entries.clear();
    }

    /// The full draw: the winner and what they take. `None` when nobody is
    /// enrolled or the pot is empty, which leaves the bucket where it is.
    pub fn draw(&self, rules: &Rules, pot: u128, roll: u128) -> Option<Draw> {
        if pot == 0 || self.is_empty() {
            return None;
        }
        let winner = self.pick(rules, roll)?;
        Some(Draw { winner, pot })
    }
}

/// A completed draw: one account, the whole bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Draw {
    pub winner: u64,
    pub pot: u128,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALE: u128 = 1_000_000;

    fn loss(tokens: u128) -> u128 {
        tokens * SCALE
    }

    #[test]
    fn pity_climbs_and_then_stops() {
        let rules = Rules::STANDARD;
        assert_eq!(rules.multiplier_bps(0), 10_000, "no streak is par");
        assert_eq!(rules.multiplier_bps(1), 12_000);
        assert_eq!(rules.multiplier_bps(5), 20_000);
        assert_eq!(rules.multiplier_bps(15), 40_000, "capped at 4x");
        assert_eq!(rules.multiplier_bps(u32::MAX), 40_000, "and never wraps");
        // A cap below par would be a policy bug, not a smaller payout.
        let broken = Rules { fund_bps: 500, pity_bps: 0, pity_cap_bps: 5_000 };
        assert_eq!(broken.multiplier_bps(0), 10_000, "never below par");
    }

    #[test]
    fn weights_and_contributions_are_floored_not_rounded() {
        let rules = Rules::STANDARD;
        assert_eq!(rules.weight(loss(100), 0).unwrap(), loss(100));
        assert_eq!(rules.weight(loss(100), 1).unwrap(), loss(120));
        assert_eq!(rules.weight(0, 9).unwrap(), 0, "no loss carries no weight");
        assert_eq!(rules.contribution(loss(100)).unwrap(), loss(5));
        assert_eq!(rules.contribution(19_999).unwrap(), 999, "an atom short stays short");
        assert_eq!(rules.contribution(u128::MAX), None, "overflow is refused, not wrapped");
    }

    #[test]
    fn the_expectation_is_the_pot_times_the_share() {
        let rules = Rules::STANDARD;
        // Two equal losers: each expects half the pot, and that is all it is.
        assert_eq!(rules.expected(loss(10), 5, 10), loss(5));
        assert_eq!(rules.expected(loss(10), 0, 10), 0);
        assert_eq!(rules.expected(loss(10), 10, 10), loss(10));
        assert_eq!(rules.expected(loss(10), 7, 0), 0, "an empty book pays nothing");
        let book = Book {
            entries: vec![
                Entry { participant: 1, loss: loss(100), streak: 0 },
                Entry { participant: 2, loss: loss(100), streak: 0 },
            ],
        };
        let pot = rules.contribution(loss(200)).unwrap();
        let total = book.total_weight(&rules).unwrap();
        for entry in book.entries.iter() {
            let weight = rules.weight(entry.loss, entry.streak).unwrap();
            assert_eq!(book.weights(&rules).unwrap()[entry.participant as usize - 1], weight);
            assert_eq!(rules.expected(pot, weight, total), pot / 2);
        }
        // The two expectations add to the pot: pari-mutuel cannot over-promise.
        assert_eq!(
            rules.expected(pot, total / 2, total) + rules.expected(pot, total - total / 2, total),
            pot
        );
    }

    #[test]
    fn losing_stays_worth_less_than_not_losing() {
        assert!(Rules::STANDARD.keeps_losing_unprofitable());
        assert!(Rules::OFF.keeps_losing_unprofitable());
        assert_eq!(Rules::STANDARD.worst_case_ev_bps(), 2_000, "20% of the loss, at the very best");
        // The boundary: fund at 25% with a 4x cap is exactly par, and that is
        // the moment losing becomes a strategy.
        let at_par = Rules { fund_bps: 2_500, pity_bps: 10_000, pity_cap_bps: 40_000 };
        assert!(!at_par.keeps_losing_unprofitable());
        assert_eq!(at_par.worst_case_ev_bps(), 10_000);
        let just_under = Rules { fund_bps: 2_499, pity_bps: 10_000, pity_cap_bps: 40_000 };
        assert!(just_under.keeps_losing_unprofitable());
    }

    #[test]
    fn no_farmer_beats_their_own_loss_however_losses_are_split() {
        let rules = Rules::STANDARD;
        // Deterministic pseudo-random loss vectors, including the farmer's
        // favourtie shape: one tiny loss against a crowd of big ones.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..500 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let count = 1 + (state % 9) as usize;
            let mut book = Book::empty();
            let mut total_loss = 0u128;
            for index in 0..count {
                let tokens = 1 + (state >> (index % 7)) % 4_000;
                let streak = ((state >> index as u64) % 20) as u32;
                book.record(index as u64, loss(tokens as u128)).unwrap();
                book.entries[index].streak = streak;
                total_loss += loss(tokens as u128);
            }
            let pot = rules.contribution(total_loss).unwrap();
            let total_weight = book.total_weight(&rules).unwrap();
            for entry in book.entries.iter() {
                let weight = rules.weight(entry.loss, entry.streak).unwrap();
                assert!(
                    rules.expected(pot, weight, total_weight) <= entry.loss,
                    "participant {} expects {} against a loss of {}",
                    entry.participant,
                    rules.expected(pot, weight, total_weight),
                    entry.loss
                );
            }
        }
    }

    #[test]
    fn a_loss_is_enrolled_once_and_merges_within_the_epoch() {
        let rules = Rules::STANDARD;
        let mut book = Book::empty();
        assert!(book.is_empty());
        book.record(7, loss(30)).unwrap();
        book.record(7, loss(20)).unwrap();
        book.record(9, loss(50)).unwrap();
        book.record(3, 0).unwrap();
        assert_eq!(book.entries.len(), 2, "one row per account, and a zero buys nothing");
        assert_eq!(book.entries[0].loss, loss(50));
        assert_eq!(book.total_weight(&rules).unwrap(), loss(100));
    }

    #[test]
    fn the_roll_maps_onto_weights_and_is_reproducible() {
        let rules = Rules::STANDARD;
        let book = Book {
            entries: vec![
                Entry { participant: 1, loss: loss(100), streak: 0 },
                Entry { participant: 2, loss: loss(300), streak: 0 },
            ],
        };
        // Weights 100 and 300: the first quarter of the roll space is p1.
        assert_eq!(book.pick(&rules, 0).unwrap(), 1);
        assert_eq!(book.pick(&rules, loss(99)).unwrap(), 1);
        assert_eq!(book.pick(&rules, loss(100)).unwrap(), 2, "the boundary belongs to the next entry");
        assert_eq!(book.pick(&rules, loss(399)).unwrap(), 2);
        assert_eq!(book.pick(&rules, loss(400)), None, "a roll outside the book names nobody");
        assert_eq!(book.pick(&rules, u128::MAX), None);
        // Pity reshapes the map without changing anyone's loss.
        let pitying = Book {
            entries: vec![
                Entry { participant: 1, loss: loss(100), streak: 3 },
                Entry { participant: 2, loss: loss(300), streak: 0 },
            ],
        };
        assert_eq!(pitying.total_weight(&rules).unwrap(), loss(160 + 300));
        assert_eq!(pitying.pick(&rules, loss(159)).unwrap(), 1);
        assert_eq!(pitying.pick(&rules, loss(160)).unwrap(), 2);
        assert_eq!(Book::empty().pick(&rules, 0), None);
    }

    #[test]
    fn a_draw_pays_the_pot_to_one_account_or_nobody() {
        let rules = Rules::STANDARD;
        let mut book = Book::empty();
        book.record(1, loss(100)).unwrap();
        book.record(2, loss(100)).unwrap();
        let pot = rules.contribution(loss(200)).unwrap();
        assert_eq!(book.draw(&rules, pot, 0).unwrap(), Draw { winner: 1, pot });
        assert_eq!(book.draw(&rules, pot, loss(100)).unwrap(), Draw { winner: 2, pot });
        assert_eq!(book.draw(&rules, 0, 0), None, "an empty bucket is not a prize");
        assert_eq!(Book::empty().draw(&rules, pot, 0), None);
        assert_eq!(book.draw(&rules, pot, loss(200)), None);
    }

    #[test]
    fn finishing_resets_the_winner_and_burns_every_loss() {
        let rules = Rules::STANDARD;
        let mut book = Book::empty();
        book.record(1, loss(100)).unwrap();
        book.record(2, loss(200)).unwrap();
        book.entries[0].streak = 4;
        book.entries[1].streak = 2;
        book.finish(Some(2));
        assert!(book.is_empty(), "the epoch's defeats cannot be entered twice");

        // Next epoch, player 2 wins again immediately: their pity is gone, so a
        // fresh loss carries par weight, while player 1's has climbed.
        book.record(1, loss(100)).unwrap();
        book.record(2, loss(100)).unwrap();
        assert_eq!(book.entries[0].streak, 0, "a fresh enrolment starts at par");
        book.roll_streaks().unwrap();
        assert_eq!(
            (book.entries[0].loss, book.entries[1].loss),
            (loss(100), loss(100)),
            "carrying pity changes weight, never amount"
        );
        assert_eq!(book.weights(&rules).unwrap()[0], rules.weight(loss(100), 1).unwrap());
        book.finish(None);
        assert!(book.is_empty());
    }

    #[test]
    fn the_bucket_never_becomes_collateral() {
        // The draw reads the pot it is handed and pays at most that. Whether the
        // pot is a yield sweep or a fee rebate is the contract's business, and
        // this module has no way to reach `Pool::equity` at all.
        let rules = Rules::STANDARD;
        let mut book = Book::empty();
        book.record(1, loss(1_000)).unwrap();
        let pot = rules.contribution(loss(1_000)).unwrap();
        assert_eq!(pot, loss(50), "5% of the loss, no more");
        let draw = book.draw(&rules, pot, 0).unwrap();
        assert_eq!(draw.pot, pot);
        assert!(
            book.total_weight(&rules).unwrap() > 0,
            "the book is a ranking, not a liability"
        );
        assert_eq!(pot, rules.contribution(loss(1_000)).unwrap(), "and it is not recomputed from weights");
    }
}
