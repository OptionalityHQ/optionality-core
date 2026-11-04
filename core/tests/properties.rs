//! Property tests for the money.
//!
//! The unit tests next to each function pin examples. These do the opposite:
//! they assert the rules that must hold for *every* input, over a deterministic
//! pseudo-random sequence, because a venue's failure mode is not "this example
//! is wrong" — it is "this arithmetic was correct for the sizes we happened to
//! test and wrong for one we did not".
//!
//! The rules, in the order a trade meets them:
//!
//! - a premium always splits exactly into the pool's share and the venue's;
//! - the entry premium is the ceiling of mid plus margin, and the exit value is
//!   the floor of mid less impact, so neither rounding creates money;
//! - a payoff never exceeds the worst case that was reserved for it, and is
//!   monotone in the settlement price in the direction the spread should be;
//! - the pool never reserves more than its equity, never pays more than a
//!   reservation, and its custody sum is conserved by every operation;
//! - share prices never let the LP set redeem more than the equity it owns.
//!
//! No external proptest crate: the workspace is deliberately dependency-free,
//! and a stable xorshift keeps a failure reproducible from the seed printed in
//! the assertion.

use bounded_core::{accounting, fees, oracle, pool::Pool};

const SCALE: u128 = 1_000_000;
const BASIS: u128 = 10_000;

/// xorshift64* — deterministic, fast, and good enough for input coverage. The
/// seed is carried in every failure message so a red run can be replayed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next() % bound
        }
    }

    fn range(&mut self, low: u128, high: u128) -> u128 {
        debug_assert!(low <= high);
        low + (self.next() as u128) % (high - low + 1)
    }

    /// Mostly ordinary values, occasionally the awkward ones: zero, one, the
    /// scale boundary, and the top of the type.
    fn amount(&mut self, high: u128) -> u128 {
        match self.below(8) {
            0 => 0,
            1 => 1,
            2 => SCALE.saturating_sub(1),
            3 => SCALE,
            4 => SCALE + 1,
            5 => high,
            _ => self.range(0, high),
        }
    }
}

fn terms(rng: &mut Rng) -> fees::Terms {
    fees::Terms {
        margin_bps: rng.below(5_001) as u32,
        impact_bps: rng.below(10_000) as u32,
    }
}

fn ceil_div(n: u128, d: u128) -> u128 {
    n / d + u128::from(n % d != 0)
}

#[test]
fn every_premium_splits_exactly_between_pool_and_venue() {
    let mut rng = Rng::new(0xFEE5);
    for _ in 0..20_000 {
        let terms = terms(&mut rng);
        let premium = rng.amount(u128::MAX / 4);
        let cut = terms.venue_cut(premium);
        let lp = terms.lp_notional(premium);
        assert_eq!(
            cut.is_some(),
            lp.is_some(),
            "a premium that can be split one way must be splittable the other"
        );
        if let (Some(cut), Some(lp)) = (cut, lp) {
            assert_eq!(cut + lp, premium, "terms {terms:?} premium {premium}");
            assert!(cut <= premium);
            assert!(lp <= premium);
        }
        // Zero margin means the venue's share is exactly zero, never a rounding
        // crumb of somebody else's mid.
        if terms.margin_bps == 0 {
            assert_eq!(cut, Some(0));
            assert_eq!(lp, Some(premium));
        }
    }
}

#[test]
fn the_entry_is_mid_plus_margin_rounded_up() {
    let mut rng = Rng::new(0xE271);
    for _ in 0..20_000 {
        let quantity = rng.range(1, 1_000_000_000);
        let mid = rng.range(0, 1_000_000_000);
        let scale = rng.range(1, 1_000_000_000);
        let terms = terms(&mut rng);
        let loaded = BASIS + u128::from(terms.margin_bps);
        let numerator = quantity * mid * loaded;
        let denominator = scale * BASIS;
        let entry = terms
            .entry_premium(quantity, mid, scale)
            .expect("bounded inputs cannot overflow");
        assert_eq!(
            entry,
            ceil_div(numerator, denominator),
            "q {quantity} mid {mid} scale {scale} terms {terms:?}"
        );
        // The two properties a buyer is promised: never below mid, and never
        // more than one unit above the exact fraction.
        assert!(entry * denominator >= numerator);
        assert!(entry * denominator - numerator < denominator);
        // More size never gets cheaper, and a higher mid never gets cheaper.
        let bigger = terms
            .entry_premium(quantity + 1, mid, scale)
            .expect("bounded");
        let higher = terms
            .entry_premium(quantity, mid + 1, scale)
            .expect("bounded");
        assert!(bigger >= entry, "size made the premium cheaper");
        assert!(higher >= entry, "a higher mid made the premium cheaper");
        // A flat schedule is the same arithmetic with a zero margin, which is
        // what the held ticket relies on.
        let flat = fees::Terms::FLAT
            .entry_premium(quantity, mid, scale)
            .expect("bounded");
        assert_eq!(flat, ceil_div(quantity * mid, scale));
        assert!(flat <= entry);
    }
}

#[test]
fn the_exit_is_mid_less_impact_rounded_down_and_never_beats_entry() {
    let mut rng = Rng::new(0xE217);
    for _ in 0..20_000 {
        let quantity = rng.range(1, 1_000_000_000);
        let mid = rng.range(0, 1_000_000_000);
        let scale = rng.range(1, 1_000_000_000);
        let mut terms = terms(&mut rng);
        if terms.impact_bps == 10_000 {
            terms.impact_bps = 9_999;
        }
        let kept = BASIS - u128::from(terms.impact_bps);
        let exit = terms
            .exit_value(quantity, mid, scale)
            .expect("a positive kept fraction always prices");
        assert_eq!(
            exit,
            quantity * mid * kept / (scale * BASIS),
            "q {quantity} mid {mid} scale {scale} terms {terms:?}"
        );
        let mid_total = quantity * mid / scale;
        assert!(exit <= mid_total, "the pool paid above mid to close");
        let entry = terms
            .entry_premium(quantity, mid, scale)
            .expect("bounded");
        assert!(exit <= entry, "a round trip could be worth money for free");
        // A zero impact haircut is the same floor, and therefore never exceeds
        // the ceiling charged on entry.
        let flat = fees::Terms::FLAT
            .exit_value(quantity, mid, scale)
            .expect("bounded");
        assert_eq!(flat, mid_total);
    }
}

#[test]
fn a_payoff_never_exceeds_its_reservation_and_moves_the_right_way() {
    let mut rng = Rng::new(0x9A40);
    for _ in 0..20_000 {
        let quantity = rng.range(1, 10_000_000);
        let scale = rng.range(1, 10_000_000);
        let low = rng.range(0, 1_000_000_000);
        let high = low + rng.range(1, 1_000_000_000);
        let width = high - low;
        let reserved = accounting::max_payout(quantity, width, scale).unwrap();

        let mut previous_put = u128::MAX;
        let mut previous_call = 0;
        // Walk prices across and beyond the strikes, including the exact edges.
        for step in 0..12u128 {
            let price = (low / 12) * step + (width / 6) * step;
            let put = accounting::payout(quantity, scale, high, low, price).unwrap();
            let call = accounting::payout_call(quantity, scale, high, low, price).unwrap();
            assert!(put <= reserved, "put payoff {put} > reservation {reserved}");
            assert!(call <= reserved, "call payoff {call} > reservation {reserved}");
            assert!(put <= previous_put, "the put payoff rose as spot rose");
            assert!(call >= previous_call, "the call payoff fell as spot rose");
            previous_put = put;
            previous_call = call;
            if price >= high {
                assert_eq!(put, 0, "a put above its high strike pays nothing");
                assert_eq!(
                    call,
                    quantity * width / scale,
                    "a call above its high strike pays the whole width"
                );
            }
            if price <= low {
                assert_eq!(call, 0, "a call below its low strike pays nothing");
                assert_eq!(
                    put,
                    quantity * width / scale,
                    "a put below its low strike pays the whole width"
                );
            }
        }
    }
}

#[test]
fn settlement_splits_conserve_value_on_every_path() {
    let mut rng = Rng::new(0xC0FFEE);
    for _ in 0..20_000 {
        let collateral = rng.amount(1_000_000_000_000);
        let premium = rng.amount(1_000_000_000_000);
        let payout = rng.range(0, collateral);
        let (buyer, maker) = accounting::split_settlement(collateral, premium, payout, false)
            .expect("payout inside collateral");
        assert_eq!(buyer + maker, collateral + premium);
        assert_eq!(buyer, payout);
        assert!(payout <= collateral, "a ticket paid more than it reserved");

        let (buyer, maker) =
            accounting::split_settlement(collateral, premium, payout, true).unwrap();
        assert_eq!(buyer, premium, "a void returns the whole premium");
        assert_eq!(maker, collateral, "and the maker keeps the whole reservation");
        assert_eq!(buyer + maker, collateral + premium);
    }
}

/// One open position plus what the test knows the vault's token balance must be.
struct Open {
    written: bounded_core::pool::Written,
}

#[test]
fn the_vault_is_solvent_and_custody_is_conserved_under_random_lifecycles() {
    let mut rng = Rng::new(0xB0A7);
    let mut pool = Pool::empty();
    let mut custody = 0u128;
    let mut open: Vec<Open> = Vec::new();
    let width = 100 * SCALE;

    for step in 0..2_000 {
        match rng.below(9) {
            // Deposit, sometimes tiny, sometimes the whole float.
            0 | 1 => {
                let amount = rng.range(1, 1_000_000 * SCALE);
                if let Some(minted) = pool.deposit(amount) {
                    assert!(minted > 0);
                    custody += amount;
                }
            }
            // An LP leaves, but never ahead of a settlement.
            2 => {
                if pool.shares > 0 {
                    let shares = rng.range(1, pool.shares);
                    if let Some(amount) = pool.withdraw(shares) {
                        custody -= amount;
                    }
                }
            }
            // Write a position. A refusal is a legitimate outcome; the test is
            // that a refusal leaves the balance sheet exactly where it was.
            3 | 4 | 5 | 6 => {
                let before = pool;
                let quantity = rng.range(1, 50) * SCALE;
                // The contract refuses a premium above the maximum payoff, so
                // the generator must too, or it would be testing a write the
                // chain can never make.
                let max = quantity * width / SCALE;
                let lp = rng.amount(max);
                let cut = rng.amount(lp);
                if let Some(written) = pool.write(quantity, width, SCALE, lp, cut) {
                    custody += lp + cut;
                    open.push(Open { written });
                    assert_eq!(written.exposure(), written.max_payout - lp);
                } else {
                    assert_eq!(pool, before, "a refused write moved the balance sheet");
                }
            }
            // Settle, close or void whatever is open.
            _ => {
                if open.is_empty() {
                    continue;
                }
                let index = rng.below(open.len() as u64) as usize;
                let held = open.swap_remove(index);
                match rng.below(3) {
                    0 => {
                        let payout = rng.range(0, held.written.max_payout);
                        assert!(pool.settle(&held.written, payout).is_some());
                        custody -= payout;
                    }
                    1 => {
                        let exit = rng.range(0, held.written.max_payout);
                        assert!(pool.close(&held.written, exit).is_some());
                        custody -= exit;
                    }
                    _ => {
                        assert!(pool.void_out(&held.written).is_some());
                        custody -= held.written.lp_notional + held.written.venue_cut;
                    }
                }
            }
        }

        assert!(
            pool.reserved <= pool.equity,
            "step {step}: reserved {} > equity {}",
            pool.reserved,
            pool.equity
        );
        assert_eq!(
            pool.expected_custody(),
            custody,
            "step {step}: custody drifted from the operations that moved it"
        );
        assert_eq!(
            pool.equity + pool.venue_locked + pool.venue + pool.consolation,
            custody,
            "step {step}: the buckets stopped adding up"
        );
        let locked: u128 = open.iter().map(|o| o.written.venue_cut).sum();
        assert!(
            pool.venue_locked >= locked,
            "step {step}: an open ticket's refundable margin was already swept"
        );
    }

    // Every remaining position can be resolved at its worst case, and the books
    // still balance afterwards.
    while let Some(held) = open.pop() {
        assert!(pool.settle(&held.written, held.written.max_payout).is_some());
        custody -= held.written.max_payout;
        assert!(pool.reserved <= pool.equity);
        assert_eq!(pool.expected_custody(), custody);
    }
    // The venue's earned accrual is the only money left outside the buckets,
    // and sweeping it is the last legal move.
    custody -= pool.sweep_venue();
    assert_eq!(pool.expected_custody(), custody);
    assert_eq!(pool.reserved, 0);
}

#[test]
fn a_write_must_fit_the_equity_it_has_not_the_premium_it_is_about_to_collect() {
    // The sharpest edge in the whole balance sheet: 3 contracts of a 100-wide
    // spread (worst case 300) sold for a 100 premium against 100 of LP equity.
    //
    // If the write is allowed, the vault ends up holding 100 of premium + 100 of
    // equity and owing 300 at the worst case: the ticket is written but cannot
    // be settled, which is the one thing a vault must never do. The premium is
    // the buyer's offset, not extra capital the pool can lean on twice.
    let mut pool = Pool::empty();
    pool.deposit(100 * SCALE).unwrap();
    assert_eq!(
        pool.write(3 * SCALE, 100 * SCALE, SCALE, 100 * SCALE, 0),
        None,
        "the net exposure (200) is larger than the equity (100)"
    );

    // The same ticket fits once the equity actually covers the net exposure.
    pool.deposit(100 * SCALE).unwrap();
    let written = pool
        .write(3 * SCALE, 100 * SCALE, SCALE, 100 * SCALE, 0)
        .expect("200 of equity covers a 200 net exposure");
    assert!(pool.settle(&written, written.max_payout).is_some());
    assert_eq!(pool.equity, 0, "the worst case takes exactly the equity left");
}

#[test]
fn a_settlement_beyond_its_reservation_is_always_refused() {
    let mut rng = Rng::new(0x0DDB);
    for _ in 0..5_000 {
        let mut pool = Pool::empty();
        pool.deposit(rng.range(1_000, 1_000_000 * SCALE));
        let quantity = rng.range(1, 100) * SCALE;
        let lp = rng.amount(5_000 * SCALE);
        let cut = rng.amount(200 * SCALE);
        let Some(written) = pool.write(quantity, 100 * SCALE, SCALE, lp, cut) else {
            continue;
        };
        let before = pool;
        assert_eq!(pool.settle(&written, written.max_payout + 1), None);
        assert_eq!(pool.close(&written, written.max_payout + 1), None);
        assert_eq!(pool, before, "a refusal partially applied a payout");
    }
}

#[test]
fn share_prices_never_let_the_set_redeem_more_than_equity() {
    let mut rng = Rng::new(0x5417);
    for _ in 0..5_000 {
        let mut pool = Pool::empty();
        let mut holders: Vec<u128> = Vec::new();
        for _ in 0..rng.below(8) + 1 {
            let amount = rng.range(1, 1_000_000 * SCALE);
            if let Some(minted) = pool.deposit(amount) {
                holders.push(minted);
            }
        }
        // Random P&L: the pool can win or lose, but shares outstanding do not change.
        let gain = rng.range(0, pool.equity / 2);
        pool.equity = if rng.below(2) == 0 {
            pool.equity + gain
        } else {
            pool.equity - gain
        };
        let redeemable: u128 = holders.iter().map(|s| pool.exit_value(*s)).sum();
        assert!(
            redeemable <= pool.equity,
            "holders could redeem {redeemable} from {} of equity",
            pool.equity
        );
        // A share balance that does not exist is worth nothing, not a panic.
        assert_eq!(pool.exit_value(pool.shares + 1), 0);
        assert_eq!(pool.exit_value(0), 0);
    }
}

#[test]
fn an_signed_observation_cannot_be_confused_with_another() {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ-_.0123456789";
    let mut rng = Rng::new(0x5153);
    let word = |rng: &mut Rng, min: usize, max: usize| {
        let len = min + rng.below((max - min + 1) as u64) as usize;
        (0..len)
            .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize] as char)
            .collect::<String>()
    };
    for _ in 0..10_000 {
        let feed = word(&mut rng, 1, 24);
        let source = word(&mut rng, 1, 24);
        let price = rng.amount(1_000_000_000_000_000);
        let publish = rng.next();
        let confidence = rng.below(10_001) as u16;
        let message = oracle::signing_message(&feed, &source, price, publish, confidence);
        assert_eq!(
            message.len(),
            oracle::DOMAIN.len() + 2 + feed.len() + source.len() + 16 + 8 + 2,
            "the length prefix must account for every byte"
        );
        assert_eq!(&message[..oracle::DOMAIN.len()], oracle::DOMAIN);
        // A different feed, source, price, stamp or confidence is a different
        // signature target.
        let other = oracle::signing_message(
            &feed,
            &(source.clone() + "X"),
            price,
            publish,
            confidence,
        );
        assert_ne!(message, other);
    }
    // The boundary case the length prefix exists for.
    assert_ne!(
        oracle::signing_message("BTC-US", "DKRAKEN", 1, 2, 3),
        oracle::signing_message("BTC-USD", "KRAKEN", 1, 2, 3)
    );
}

#[test]
fn overflow_is_refused_instead_of_manufacturing_a_number() {
    let max = u128::MAX;
    for terms in [fees::Terms::FLAT, fees::Terms::VENUE, fees::Terms::STANDARD] {
        // Entry and exit refuse inputs whose exact product does not fit.
        assert_eq!(terms.entry_premium(max, max, 1), None);
        // A zero scale is never a price.
        assert_eq!(terms.entry_premium(1, 1, 0), None);
        assert_eq!(terms.exit_value(1, 1, 0), None);
        // The split may refuse an absurd premium, but never panics and never
        // returns one half without the other.
        let cut = terms.venue_cut(max);
        let lp = terms.lp_notional(max);
        assert_eq!(cut.is_some(), lp.is_some());
        if let (Some(cut), Some(lp)) = (cut, lp) {
            assert_eq!(cut + lp, max);
        }
    }
    assert_eq!(accounting::max_payout(1, 1, 0), None);
    assert_eq!(accounting::max_payout(max, 2, 1), None);
    assert_eq!(accounting::payout(max, 1, 3, 1, 0), None);
    assert_eq!(accounting::payout(1, SCALE, 100, 100, 90), None);
    assert_eq!(accounting::split_settlement(10, 3, 11, false), None);
}
