//! The venue's cut, computed in integers.
//!
//! Every fee in this product lives inside the entry premium: the buyer pays
//! `mid × (1 + margin)` as one number, and nothing is itemised on the ticket.
//! That makes the margin the entire revenue model, so it has to be split from
//! the premium after the fact — the part that belongs to the venue, and the part
//! that is the pool's own money and therefore counts against what the pool can
//! afford to write.
//!
//! Rounding is never neutral. The entry premium rounds up and the exit value
//! rounds down, so both of the cases someone could argue about favour the pool —
//! the same convention `accounting::payout` uses when it rounds down.

pub const BASIS: u128 = 10_000;

fn ceil_div(numerator: u128, divisor: u128) -> Option<u128> {
    if divisor == 0 {
        return None;
    }
    numerator.checked_add(divisor - 1)?.checked_div(divisor)
}

fn floor_div(numerator: u128, divisor: u128) -> Option<u128> {
    if divisor == 0 {
        return None;
    }
    numerator.checked_div(divisor)
}

/// A quote policy: how much the pool charges above mid, and how much it charges
/// to let a position out early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terms {
    /// Venue margin inside the entry premium, basis points of mid.
    pub margin_bps: u32,
    /// Haircut on selling back before expiry, basis points of mid.
    pub impact_bps: u32,
}

impl Terms {
    /// Mid plus 4% to open, mid minus 10% to close early.
    pub const STANDARD: Terms = Terms {
        margin_bps: 400,
        impact_bps: 1_000,
    };

    /// Nothing charged, for the fixture series and anything quoted at model
    /// value.
    pub const FLAT: Terms = Terms {
        margin_bps: 0,
        impact_bps: 0,
    };

    /// What the buyer pays for `quantity` scaled contracts at `mid_units` scaled
    /// points per contract, both against `scale`. Rounded up.
    pub fn entry_premium(&self, quantity: u128, mid_units: u128, scale: u128) -> Option<u128> {
        let loaded = BASIS.checked_add(u128::from(self.margin_bps))?;
        let numerator = quantity.checked_mul(mid_units)?.checked_mul(loaded)?;
        ceil_div(numerator, scale.checked_mul(BASIS)?)
    }

    /// The venue's share of a collected premium: `margin ÷ (1 + margin)` of it.
    /// Floored, so the two shares always add back to the premium.
    ///
    /// Written as a quotient and a remainder rather than `premium × margin ÷
    /// loaded`, because the product would overflow a `u128` on a premium the
    /// token could legitimately hold.
    pub fn venue_cut(&self, premium: u128) -> Option<u128> {
        let margin = u128::from(self.margin_bps);
        let loaded = BASIS.checked_add(margin)?;
        let whole = premium / loaded;
        let rest = premium % loaded;
        Some(whole.checked_mul(margin)? + rest.checked_mul(margin)? / loaded)
    }

    /// The part of the premium that is the pool's own money, and so reduces what
    /// it must hold against the worst case.
    pub fn lp_notional(&self, premium: u128) -> Option<u128> {
        premium.checked_sub(self.venue_cut(premium)?)
    }

    /// What the pool pays to take a position back before expiry: mid less the
    /// impact haircut, rounded down. An haircut of a full basis or more would
    /// mean the pool pays nothing or negative, so it is refused.
    pub fn exit_value(&self, quantity: u128, mid_units: u128, scale: u128) -> Option<u128> {
        let kept = BASIS.checked_sub(u128::from(self.impact_bps))?;
        if kept == 0 {
            return None;
        }
        let numerator = quantity.checked_mul(mid_units)?.checked_mul(kept)?;
        floor_div(numerator, scale.checked_mul(BASIS)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALE: u128 = 1_000_000;
    const THREE: u128 = 3 * SCALE;
    /// 22.62 per contract, scaled.
    const MID: u128 = 22_620_000;

    #[test]
    fn the_margin_lands_inside_the_premium() {
        // 3 × 22.62 = 67.86 mid; plus 4% = 70.5744.
        assert_eq!(
            Terms::STANDARD.entry_premium(THREE, MID, SCALE).unwrap(),
            70_574_400
        );
        assert_eq!(Terms::FLAT.entry_premium(THREE, MID, SCALE).unwrap(), 67_860_000);
    }

    #[test]
    fn the_premium_splits_into_the_two_shares_exactly() {
        let premium = Terms::STANDARD.entry_premium(THREE, MID, SCALE).unwrap();
        assert_eq!(Terms::STANDARD.venue_cut(premium).unwrap(), 2_714_400);
        // The pool's share is the mid it would have charged with no margin, so
        // the venue takes the uplift and nothing else.
        assert_eq!(Terms::STANDARD.lp_notional(premium).unwrap(), 67_860_000);
    }

    #[test]
    fn the_two_shares_always_add_up() {
        for premium in [0u128, 1, 7, 999_999, 3_000_000, 67_860_001, u128::MAX / 3] {
            let terms = Terms::STANDARD;
            assert_eq!(
                terms.venue_cut(premium).unwrap() + terms.lp_notional(premium).unwrap(),
                premium,
                "premium {premium}"
            );
        }
        assert_eq!(Terms::FLAT.venue_cut(5_000_000).unwrap(), 0);
        assert_eq!(Terms::FLAT.lp_notional(5_000_000).unwrap(), 5_000_000);
    }

    #[test]
    fn closing_early_pays_the_haircut_and_never_more_than_mid() {
        assert_eq!(
            Terms::STANDARD.exit_value(THREE, MID, SCALE).unwrap(),
            61_074_000
        );
        let mid_total = THREE * MID / SCALE;
        assert_eq!(Terms::FLAT.exit_value(THREE, MID, SCALE).unwrap(), mid_total);
        assert!(Terms::STANDARD.exit_value(THREE, MID, SCALE).unwrap() < mid_total);
    }

    #[test]
    fn a_haircut_that_would_pay_nothing_is_refused() {
        let terms = Terms { margin_bps: 0, impact_bps: 10_000 };
        assert_eq!(terms.exit_value(THREE, MID, SCALE), None);
    }

    #[test]
    fn overflow_is_rejected_rather_than_wrapped() {
        assert_eq!(Terms::STANDARD.entry_premium(u128::MAX, MID, SCALE), None);
        assert_eq!(Terms::STANDARD.entry_premium(THREE, MID, 0), None);
    }
}
