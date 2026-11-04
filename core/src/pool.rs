//! The vault: one balance sheet standing behind every trade.
//!
//! Replaces the book. There is no resting order to match against; a player buys
//! from this and the pool is the counterparty to the end. Two numbers make that
//! safe, and both are integers so the chain can enforce them:
//!
//! - **A spread's worst case is known before it is written.** `max_payout` is
//!   arithmetic, not a forecast, so the pool reserves exactly what it could owe.
//!   A perpetual pool cannot say this, which is why perp vaults get liquidated
//!   and this one does not need to.
//! - **The reservation is the whole worst case, not the loss after the
//!   premium.** At expiry the pool pays `payout` out of equity, and the premium
//!   is already in equity, so the full `max_payout` has to stay covered. The
//!   premium lowers how much equity the write consumes; it does not lower what
//!   the vault must hold. Reserving only `max_payout - premium` let LPs withdraw
//!   the premium a payout still needed, which is insolvency with extra steps.
//!
//! The invariant everything else depends on is `reserved ≤ equity`. Every method
//! here either preserves it or refuses.
//!
//! Four buckets, and their sum is the token balance the contract holds:
//! `equity` (the LPs' money, part of it committed), `venue_locked` (the margin
//! collected on tickets that are still open, which a void has to hand back),
//! `venue` (margin on tickets that can no longer be voided, so the venue may
//! take it), `consolation` (the lottery bucket), and nothing else.
//!
//! The margin is split in two because it has two different owners over the life
//! of a ticket. While the ticket is open the venue's slice is collateral against
//! a refund, and sweeping it would make that ticket unsettled-able — a fee
//! collection that breaks settlement liveness is not a fee, it is a leak. Only
//! once a ticket is settled, closed or voided is the slice actually earned.
//!
//! It also never belongs to the LPs, so it sits outside `equity` entirely: a
//! sweep must not dilute the net asset value a share is priced at.

use crate::accounting;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pool {
    /// LP principal plus pool money earned, net of what has been paid out.
    /// The only bucket share prices are read against.
    pub equity: u128,
    /// Worst case still owed across open positions: the full max payout, since
    /// that is what the vault pays at expiry out of this equity.
    pub reserved: u128,
    /// Margin collected on open tickets. Encumbered: a void returns it.
    pub venue_locked: u128,
    /// Margin the venue may take now that its ticket has stopped being voidable.
    pub venue: u128,
    /// Consolation bucket: funded from yield and fee, never from collateral.
    pub consolation: u128,
    /// Share token supply.
    pub shares: u128,
}

/// What one position costs to hold, and what it can return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    pub max_payout: u128,
    pub lp_notional: u128,
    pub venue_cut: u128,
}

impl Written {
    /// The part of the worst case the premium has not already paid for.
    pub fn exposure(&self) -> u128 {
        self.max_payout.saturating_sub(self.lp_notional)
    }
}

impl Pool {
    pub const fn empty() -> Pool {
        Pool {
            equity: 0,
            reserved: 0,
            venue_locked: 0,
            venue: 0,
            consolation: 0,
            shares: 0,
        }
    }

    /// Equity not yet committed to a worst case.
    pub fn free_equity(&self) -> u128 {
        self.equity.saturating_sub(self.reserved)
    }

    /// Committed over total, in basis points — the number the UI shows as
    /// "pool is X% full".
    pub fn utilisation_bps(&self) -> u128 {
        if self.equity == 0 {
            return 0;
        }
        self.reserved.saturating_mul(10_000) / self.equity
    }

    /// The whole token balance the contract must be holding.
    pub fn expected_custody(&self) -> u128 {
        self.equity
            .saturating_add(self.venue_locked)
            .saturating_add(self.venue)
            .saturating_add(self.consolation)
    }

    /// Could the pool take this without breaking its own invariant?
    ///
    /// The new position's whole worst case has to fit inside today's equity plus
    /// the premium it is about to collect. Checking only the net-of-premium
    /// exposure against `equity + premium` counts the premium twice and lets a
    /// position through that the vault cannot pay at expiry — a ticket that
    /// cannot settle, which is worse than refusing it.
    pub fn can_write(&self, max_payout: u128, lp_notional: u128) -> bool {
        self.reserved.saturating_add(max_payout) <= self.equity.saturating_add(lp_notional)
    }

    /// Take a position. The buyer's premium arrives split into the pool's share,
    /// which joins equity, and the venue's, which joins the encumbered bucket.
    pub fn write(
        &mut self,
        quantity: u128,
        width: u128,
        scale: u128,
        lp_notional: u128,
        venue_cut: u128,
    ) -> Option<Written> {
        let max_payout = accounting::max_payout(quantity, width, scale)?;
        if !self.can_write(max_payout, lp_notional) {
            return None;
        }
        self.equity = self.equity.checked_add(lp_notional)?;
        self.venue_locked = self.venue_locked.checked_add(venue_cut)?;
        self.reserved = self.reserved.checked_add(max_payout)?;
        Some(Written {
            max_payout,
            lp_notional,
            venue_cut,
        })
    }

    /// A ticket has stopped being voidable, so its margin is finally the
    /// venue's. Every terminal path calls this with the same slice `write` put
    /// in the encumbered bucket.
    fn earn(&mut self, venue_cut: u128) -> Option<()> {
        self.venue_locked = self.venue_locked.checked_sub(venue_cut)?;
        self.venue = self.venue.checked_add(venue_cut)?;
        Some(())
    }

    /// Settle at expiry. The pool pays `payout`, which `accounting::payout` has
    /// already bounded by `max_payout`, and gives back the reservation.
    pub fn settle(&mut self, position: &Written, payout: u128) -> Option<()> {
        if payout > position.max_payout {
            return None;
        }
        self.equity = self.equity.checked_sub(payout)?;
        self.reserved = self.reserved.checked_sub(position.max_payout)?;
        self.earn(position.venue_cut)
    }

    /// Close early: the pool buys the position back and releases the reservation.
    /// The buyer keeps their whole premium as consideration, so so does the fee.
    pub fn close(&mut self, position: &Written, exit_value: u128) -> Option<()> {
        if exit_value > position.max_payout {
            return None;
        }
        self.equity = self.equity.checked_sub(exit_value)?;
        self.reserved = self.reserved.checked_sub(position.max_payout)?;
        self.earn(position.venue_cut)
    }

    /// An outage: the buyer's premium goes back, so the pool gives up what it
    /// kept and the encumbered margin is handed over rather than earned. Nothing
    /// is owed on the payoff, and the pool ends exactly where it started.
    pub fn void_out(&mut self, position: &Written) -> Option<()> {
        self.equity = self.equity.checked_sub(position.lp_notional)?;
        self.venue_locked = self.venue_locked.checked_sub(position.venue_cut)?;
        self.reserved = self.reserved.checked_sub(position.max_payout)?;
        Some(())
    }

    /// LP deposit. Shares are minted at the current value of a share, so an
    /// entrant cannot dilute the people already exposed.
    pub fn deposit(&mut self, amount: u128) -> Option<u128> {
        if amount == 0 {
            return None;
        }
        let minted = if self.shares == 0 || self.equity == 0 {
            amount
        } else {
            amount.checked_mul(self.shares)?.checked_div(self.equity)?
        };
        if minted == 0 {
            return None;
        }
        self.equity = self.equity.checked_add(amount)?;
        self.shares = self.shares.checked_add(minted)?;
        Some(minted)
    }

    /// What a share balance is worth at the current NAV, priced exactly the way
    /// an exit prices it — integer floor, same operands. Read-only so a gateway
    /// can show an LP what leaving would pay before it asks to leave.
    pub fn exit_value(&self, shares: u128) -> u128 {
        if shares == 0 || shares > self.shares {
            return 0;
        }
        shares
            .checked_mul(self.equity)
            .and_then(|value| value.checked_div(self.shares))
            .unwrap_or(0)
    }

    /// LP exit, priced at the same share value. Refused when it would leave less
    /// free equity than the open worst cases need — otherwise an LP could walk
    /// out in front of a settlement and leave the rest to cover it.
    pub fn withdraw(&mut self, shares: u128) -> Option<u128> {
        let amount = self.exit_value(shares);
        if amount == 0 {
            return None;
        }
        if self.equity.checked_sub(amount)? < self.reserved {
            return None;
        }
        self.equity -= amount;
        self.shares -= shares;
        Some(amount)
    }

    /// Sweep the venue's earned accrual. Everything in `venue` is free, because
    /// the tickets it came from have already run off and cannot be voided — and
    /// nothing here touches `equity`, so a sweep never dilutes a share.
    pub fn sweep_venue(&mut self) -> u128 {
        let claimable = self.venue;
        self.venue = 0;
        claimable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALE: u128 = 1_000_000;
    const ONE: u128 = SCALE;
    const WIDTH: u128 = 100 * SCALE;

    fn funded() -> Pool {
        let mut pool = Pool::empty();
        pool.deposit(20_000 * SCALE).unwrap();
        pool
    }

    #[test]
    fn a_premium_is_not_counted_twice_when_reserving() {
        // 3 contracts of a 100-wide spread, worst case 300, sold for 100 against
        // 100 of equity. The net exposure is 200 and the equity is 100, so the
        // ticket must be refused: allowing it would mean writing a payoff the
        // vault cannot make at expiry.
        let mut thin = Pool::empty();
        thin.deposit(100 * SCALE).unwrap();
        assert!(thin.write(3 * ONE, WIDTH, SCALE, 100 * SCALE, 0).is_none());
        assert_eq!(thin.reserved, 0);

        // The same ticket fits once the equity covers the net exposure, and the
        // worst case then takes exactly that equity.
        thin.deposit(100 * SCALE).unwrap();
        let written = thin.write(3 * ONE, WIDTH, SCALE, 100 * SCALE, 0).unwrap();
        assert!(thin.settle(&written, written.max_payout).is_some());
        assert_eq!(thin.equity, 0);
    }

    #[test]
    fn the_whole_worst_case_is_reserved_even_after_the_premium() {
        let mut pool = funded();
        // Buyer pays 3,000 for a 10,000 max payout: 2,900 pool, 100 venue.
        let written = pool
            .write(100 * ONE, WIDTH, SCALE, 2_900 * SCALE, 100 * SCALE)
            .unwrap();
        assert_eq!(written.max_payout, 10_000 * SCALE);
        assert_eq!(written.exposure(), 7_100 * SCALE, "the net risk, for reporting");
        assert_eq!(
            pool.reserved, 10_000 * SCALE,
            "but the vault reserves the gross payout it must actually make"
        );
        assert_eq!(pool.equity, 22_900 * SCALE);
        assert_eq!(pool.venue_locked, 100 * SCALE);
        assert_eq!(pool.venue, 0, "not earned until the ticket runs off");
        assert!(pool.reserved <= pool.equity);
        assert_eq!(pool.free_equity(), 12_900 * SCALE);
    }

    #[test]
    fn the_pool_refuses_what_it_cannot_pay() {
        let mut pool = funded();
        // Two full-width worst cases with no premium kept: 20,000 covers them
        // exactly, a third does not fit.
        assert!(pool.write(100 * ONE, WIDTH, SCALE, 0, 0).is_some());
        assert!(pool.write(100 * ONE, WIDTH, SCALE, 0, 0).is_some());
        assert!(pool.write(100 * ONE, WIDTH, SCALE, 0, 0).is_none());
        assert_eq!(pool.reserved, 20_000 * SCALE);
        assert_eq!(pool.free_equity(), 0);
        assert_eq!(pool.utilisation_bps(), 10_000);
    }

    #[test]
    fn every_settlement_path_returns_the_reservation() {
        // Worst case, best case, an early exit and an outage all release.
        for payout in [0u128, 5_000 * SCALE, 10_000 * SCALE] {
            let mut pool = funded();
            let written = pool
                .write(100 * ONE, WIDTH, SCALE, 2_900 * SCALE, 100 * SCALE)
                .unwrap();
            pool.settle(&written, payout).unwrap();
            assert_eq!(pool.reserved, 0, "payout {payout}");
            assert_eq!(pool.equity, (20_000 + 2_900) * SCALE - payout);
            assert_eq!(pool.venue, 100 * SCALE, "the fee is earned at settlement");
            assert_eq!(pool.venue_locked, 0);
        }
        let mut pool = funded();
        let written = pool
            .write(100 * ONE, WIDTH, SCALE, 2_900 * SCALE, 100 * SCALE)
            .unwrap();
        pool.close(&written, 2_000 * SCALE).unwrap();
        assert_eq!(
            (pool.reserved, pool.equity, pool.venue),
            (0, 20_900 * SCALE, 100 * SCALE)
        );

        let mut pool = funded();
        let written = pool
            .write(100 * ONE, WIDTH, SCALE, 2_900 * SCALE, 100 * SCALE)
            .unwrap();
        pool.void_out(&written).unwrap();
        // Nothing kept, nothing owed, and the margin went back to the buyer
        // rather than being earned: the pool is exactly as it started.
        assert_eq!(
            (pool.reserved, pool.equity, pool.venue_locked, pool.venue),
            (0, 20_000 * SCALE, 0, 0)
        );
    }

    #[test]
    fn a_payout_beyond_the_worst_case_is_refused() {
        let mut pool = funded();
        let written = pool.write(100 * ONE, WIDTH, SCALE, 2_900 * SCALE, 100 * SCALE).unwrap();
        assert_eq!(pool.settle(&written, 10_000 * SCALE + 1), None);
        assert_eq!(pool.close(&written, 10_000 * SCALE + 1), None);
        assert_eq!(pool.reserved, written.max_payout);
    }

    #[test]
    fn shares_are_minted_at_value_and_a_full_exit_returns_the_equity() {
        let mut pool = Pool::empty();
        let first = pool.deposit(10_000 * SCALE).unwrap();
        // Second entrant at the same NAV, so a deposit buys its face in shares.
        let second = pool.deposit(5_000 * SCALE).unwrap();
        assert_eq!((first, second), (10_000 * SCALE, 5_000 * SCALE));

        // The pool earns 1,500 on 15,000 of equity, so shares redeem at 1.1x par.
        pool.equity += 1_500 * SCALE;
        assert_eq!(pool.withdraw(second).unwrap(), 5_500 * SCALE);
        assert_eq!(pool.withdraw(first).unwrap(), 11_000 * SCALE);
        assert_eq!((pool.equity, pool.shares), (0, 0));
    }

    #[test]
    fn a_quoted_exit_price_is_the_price_of_the_real_one() {
        let mut pool = Pool::empty();
        let mine = pool.deposit(10_000 * SCALE).unwrap();
        pool.deposit(5_000 * SCALE).unwrap();
        pool.equity += 1_500 * SCALE;
        // The number a screen shows before the click has to be the number the
        // click pays: same floor, same operands, no second rounding.
        let quoted = pool.exit_value(mine);
        assert_eq!(quoted, 11_000 * SCALE);
        assert_eq!(pool.withdraw(mine).unwrap(), quoted);
        assert_eq!(pool.exit_value(pool.shares + 1), 0, "shares that do not exist are worth nothing");
    }

    #[test]
    fn an_lp_cannot_leave_in_front_of_a_worst_case() {
        let mut pool = funded();
        let shares = pool.shares;
        pool.write(100 * ONE, WIDTH, SCALE, 0, 0).unwrap();
        // 10,000 of the 20,000 is committed, so half the shares can still leave
        // and all of them cannot.
        assert_eq!(pool.withdraw(shares / 2), Some(10_000 * SCALE));
        assert_eq!(pool.withdraw(shares / 2), None);
        assert_eq!(pool.reserved, 10_000 * SCALE);
    }

    #[test]
    fn the_venue_cannot_sweep_a_margin_a_void_can_still_call_for() {
        let mut pool = funded();
        let written = pool
            .write(100 * ONE, WIDTH, SCALE, 2_900 * SCALE, 100 * SCALE)
            .unwrap();
        // Free equity is 22,900 − 10,000, so a sweep limited only by slack would
        // happily hand the 100 back. It must not: the ticket is still voidable
        // and the refund needs that slice.
        assert_eq!(pool.free_equity(), 12_900 * SCALE);
        assert_eq!(pool.sweep_venue(), 0);
        assert_eq!(pool.venue_locked, 100 * SCALE);

        pool.settle(&written, 4_000 * SCALE).unwrap();
        assert_eq!(pool.sweep_venue(), 100 * SCALE);
        // The sweep leaves the balance sheet alone: a sweep is not a withdrawal
        // by LPs, and must never move the price of a share.
        let equity_after_settle = 22_900 * SCALE - 4_000 * SCALE;
        assert_eq!(pool.equity, equity_after_settle);
        let shares = pool.shares;
        assert_eq!(pool.withdraw(shares).unwrap(), equity_after_settle);
        assert_eq!(pool.equity, 0);
    }

    #[test]
    fn custody_always_equals_the_sum_of_the_buckets() {
        let mut pool = funded();
        let written = pool
            .write(100 * ONE, WIDTH, SCALE, 2_900 * SCALE, 100 * SCALE)
            .unwrap();
        assert_eq!(pool.expected_custody(), 23_000 * SCALE);
        pool.consolation += 10 * SCALE;
        assert_eq!(pool.expected_custody(), 23_010 * SCALE);
        pool.settle(&written, 4_000 * SCALE).unwrap();
        assert_eq!(pool.expected_custody(), 19_010 * SCALE);
        assert_eq!(
            pool.equity + pool.venue_locked + pool.venue + pool.consolation,
            19_010 * SCALE
        );
        // Sweeping moves money between buckets and out of the vault's custody
        // sum, which is what the contract's FT balance then reflects.
        assert_eq!(pool.sweep_venue(), 100 * SCALE);
        assert_eq!(pool.expected_custody(), 18_910 * SCALE);
    }

    /// Deterministic pseudo-random sequence of writes, settlements, early exits
    /// and sweeps: the invariant and the custody sum must hold after every
    /// single mutation.
    #[test]
    fn the_invariant_holds_under_any_fill_sequence() {
        let mut pool = funded();
        let mut custody = pool.expected_custody();
        let mut state = 0x1234_5678_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut open: Vec<Written> = Vec::new();
        for _ in 0..500 {
            let roll = next() % 6;
            if roll < 3 || open.is_empty() {
                let contracts = (1 + next() % 60) as u128;
                let kept = (next() % 4_000) as u128;
                let cut = (next() % 200) as u128;
                if let Some(written) =
                    pool.write(contracts * ONE, WIDTH, SCALE, kept * SCALE, cut * SCALE)
                {
                    custody += kept * SCALE + cut * SCALE;
                    open.push(written);
                }
            } else if roll == 3 {
                let index = (next() as usize) % open.len();
                let written = open.swap_remove(index);
                let payout = (next() as u128) % (written.max_payout / SCALE + 1);
                pool.settle(&written, payout * SCALE).unwrap();
                custody -= payout * SCALE;
                // A sweep hands the margin to the venue, so it leaves the vault's
                // own custody sum even though the contract still holds it.
                custody -= pool.sweep_venue();
            } else if roll == 4 {
                // An early exit the pool can actually afford: never more than
                // the worst case it is holding for.
                let index = (next() as usize) % open.len();
                let written = open.swap_remove(index);
                let exit = (next() as u128) % (written.max_payout / SCALE + 1);
                pool.close(&written, exit * SCALE).unwrap();
                custody -= exit * SCALE;
                custody -= pool.sweep_venue();
            } else {
                // An outage refunds the whole premium, which is only possible
                // because the margin never left the encumbered bucket.
                let index = (next() as usize) % open.len();
                let written = open.swap_remove(index);
                let refund = written.lp_notional + written.venue_cut;
                pool.void_out(&written).unwrap();
                custody -= refund;
            }
            assert!(
                pool.reserved <= pool.equity,
                "reserved {} > equity {}",
                pool.reserved,
                pool.equity
            );
            assert_eq!(
                pool.expected_custody(),
                custody,
                "money left the vault without a payout"
            );
        }
        // Everything still open can be unwound, and no sweep leaves a void
        // unable to pay: the encumbered slice always covers the open tickets.
        let locked: u128 = open.iter().map(|written| written.venue_cut).sum();
        assert!(pool.venue_locked >= locked);
    }
}
