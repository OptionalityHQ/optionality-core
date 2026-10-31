//! A seeded maker, not a market.
//!
//! The vault is the counterparty to every ladder click, which makes a book
//! optional — and an optional book stays empty. This posts a few real resting
//! offers on the cells nearest the index so the order book has something to
//! show, `post-only` has somebody to meet, and the screen's "book" price is a
//! number that came from a person's escrowed collateral rather than from the
//! swap being empty.
//!
//! It quotes a couple of percent *above* the model's mid: it is a maker with a
//! spread, not a subsidy, and every offer it posts can lose money like any other
//! seller's. The interesting arithmetic is in `plan`, which is pure and tested;
//! the orders themselves are the operator's, signed with the role's key.

use serde_json::Value;

/// One series the maker wants to be quoting, and at what unit premium.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Want {
    pub series_id: u64,
    pub premium_units: u128,
}

/// What the maker has to do to get from where it is to where it wants to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Cancel { offer_id: u64 },
    Post { series_id: u64, premium_units: u128 },
}

/// How far over the model's mid the maker rests.
///
/// It has to sit *inside* the venue's own entry (the contract's margin, 50 bps at
/// boot) or the ladder will always prefer the vault and the book is decoration —
/// which is exactly what happened when the fee dropped from 400 to 50 and this
/// stayed at 200. Twenty bps keeps it a genuine maker with a spread, and the first
/// one on the book.
pub const EDGE_BPS: u128 = 20;

/// The cell a series occupies: a put is struck at its high leg and a call at its
/// low one. This is the same rule the gateway addresses cells with, and the two
/// copies have to agree (the gateway's is `seriesStrike`).
pub fn cell_strike(kind: &str, low: u128, high: u128) -> u128 {
    if kind == "Put" {
        high
    } else {
        low
    }
}

/// The two cells a seeded maker quotes on one ladder: the call and the put whose
/// cell strike sits closest to the index. One each, nearest the money, because a
/// demo book only has to be legible.
pub fn wants(
    index_units: u128,
    plans: &[crate::operator::QuotePlan],
    rows: &[(u64, Value)],
    expiry_ns: u64,
) -> Vec<Want> {
    let mut chosen: Vec<(String, u128, u128)> = Vec::new(); // (kind, cell strike, mid)
    for kind in ["Put", "Call"] {
        let nearest = plans
            .iter()
            .filter(|plan| plan.kind == kind)
            .filter(|plan| plan.low_units > 0 && plan.high_units > 0)
            .filter(|plan| cell_strike(kind, plan.low_units, plan.high_units) > 0)
            .min_by_key(|plan| {
                let strike = cell_strike(kind, plan.low_units, plan.high_units);
                strike.abs_diff(index_units)
            });
        let Some(plan) = nearest else { continue };
        chosen.push((
            kind.to_string(),
            cell_strike(kind, plan.low_units, plan.high_units),
            plan.mid_units,
        ));
    }

    let mut wants = Vec::new();
    for (kind, strike, mid) in chosen {
        let Some((series_id, _)) = rows.iter().find(|(_, row)| {
            // The stamp crosses JSON as a float and loses a few nanoseconds on the
            // way back; an exact comparison finds no ladder at all.
            row.get("expiry_ns")
                .and_then(Value::as_u64)
                .is_some_and(|ns| ns.abs_diff(expiry_ns) <= 1_000_000)
                && row.get("kind").and_then(Value::as_str) == Some(kind.as_str())
                && row.get("settlement").and_then(Value::as_str) == Some("Open")
                && cell_strike(
                    &kind,
                    row.get("low_strike").and_then(Value::as_u64).unwrap_or(0) as u128,
                    row.get("high_strike").and_then(Value::as_u64).unwrap_or(0) as u128,
                ) == strike
        }) else {
            continue;
        };
        wants.push(Want { series_id: *series_id, premium_units: mid + mid * EDGE_BPS / 10_000 });
    }
    wants
}

/// What it takes to hold the positions we want.
///
/// `mine` is what the maker currently has resting, as `(offer_id, series_id,
/// premium)`. An offer within `tolerance_bps` of its target is left alone — a
/// book that re-posts on every tick is noise in the tape and a transaction fee on
/// every pass — and anything the maker no longer wants is taken back, because a
/// resting offer is escrowed collateral.
pub fn plan(
    mine: &[(u64, u64, u128)],
    wants: &[Want],
    tolerance_bps: u32,
) -> Vec<Action> {
    let mut actions = Vec::new();
    for want in wants {
        let resting: Vec<&(u64, u64, u128)> =
            mine.iter().filter(|(_, series_id, _)| *series_id == want.series_id).collect();
        if resting.is_empty() {
            actions.push(Action::Post {
                series_id: want.series_id,
                premium_units: want.premium_units,
            });
            continue;
        }
        let close_enough = resting.iter().any(|(_, _, premium)| {
            let drift = premium.abs_diff(want.premium_units);
            drift.saturating_mul(10_000) <= want.premium_units.saturating_mul(tolerance_bps as u128)
        });
        if !close_enough {
            for (offer_id, _, _) in resting {
                actions.push(Action::Cancel { offer_id: *offer_id });
            }
            actions.push(Action::Post {
                series_id: want.series_id,
                premium_units: want.premium_units,
            });
        }
    }
    for (offer_id, series_id, _) in mine {
        if !wants.iter().any(|want| want.series_id == *series_id) {
            actions.push(Action::Cancel { offer_id: *offer_id });
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator::QuotePlan;
    use serde_json::json;

    const EXPIRY: u64 = 1_793_300_000_000_000_000;

    fn row(id: u64, kind: &str, low: u128, high: u128, expiry: u64) -> (u64, Value) {
        (
            id,
            json!({
                "kind": kind,
                "low_strike": low,
                "high_strike": high,
                "expiry_ns": expiry,
                "settlement": "Open",
            }),
        )
    }

    fn plan_row(kind: &str, low: u128, high: u128, mid: u128) -> QuotePlan {
        QuotePlan { kind: kind.to_string(), low_units: low, high_units: high, mid_units: mid }
    }

    #[test]
    fn a_put_is_read_at_its_high_leg_and_a_call_at_its_low() {
        assert_eq!(cell_strike("Put", 2_300, 2_400), 2_400);
        assert_eq!(cell_strike("Call", 2_300, 2_400), 2_300);
    }

    #[test]
    fn the_maker_quotes_the_cell_nearest_the_money() {
        let plans = vec![
            plan_row("Put", 2_200, 2_300, 10),
            plan_row("Put", 2_300, 2_400, 55_000),
            plan_row("Call", 2_400, 2_500, 70_000),
            plan_row("Call", 2_500, 2_600, 20_000),
        ];
        let rows = vec![
            row(1, "Put", 2_200, 2_300, EXPIRY),
            row(2, "Put", 2_300, 2_400, EXPIRY),
            row(3, "Call", 2_400, 2_500, EXPIRY),
            row(4, "Call", 2_500, 2_600, EXPIRY),
            row(5, "Call", 2_400, 2_500, EXPIRY + 86_400_000_000_000), // another day
        ];
        // Index 2,360: the nearest put cell is 2,400 and the nearest call is 2,400.
        let wants = wants(2_360, &plans, &rows, EXPIRY);
        assert_eq!(wants.len(), 2, "one put and one call, no more");
        assert_eq!(wants[0].series_id, 2, "the 2300/2400 put");
        assert_eq!(wants[1].series_id, 3, "the 2400/2500 call, not next week's");
        assert_eq!(wants[1].premium_units, 70_000 + 70_000 * 20 / 10_000, "the model's mid plus the edge");
    }

    #[test]
    fn the_expiry_stamp_survives_the_json_float() {
        // The engine's `expiryNs` comes back as a string, the chain's as a number,
        // and the number has lost a few nanoseconds to a float on the way.
        let plans = vec![plan_row("Call", 2_400, 2_500, 70_000)];
        let rows = vec![row(3, "Call", 2_400, 2_500, EXPIRY - 192)];
        assert_eq!(wants(2_450, &plans, &rows, EXPIRY).len(), 1, "a 192 ns drift is the same ladder");
    }

    #[test]
    fn a_series_the_chain_does_not_list_is_not_quoted() {
        let plans = vec![plan_row("Call", 3_000, 3_100, 5)];
        let rows = vec![row(1, "Call", 2_400, 2_500, EXPIRY)];
        assert!(wants(2_450, &plans, &rows, EXPIRY).is_empty(), "no series, no quote");
    }

    #[test]
    fn the_maker_quotes_inside_the_venue_so_the_book_can_win() {
        // The venue's entry is mid × (1 + margin). A maker priced above that is
        // never the best ask, which makes the whole book decorative.
        let venue_entry_bps = 50u128;
        assert!(
            EDGE_BPS < venue_entry_bps,
            "a maker at {EDGE_BPS} bps is wider than the venue at {venue_entry_bps}"
        );
    }

    #[test]
    fn a_resting_offer_within_tolerance_is_left_alone() {
        let wants = vec![Want { series_id: 7, premium_units: 100_000 }];
        let mine = vec![(11, 7, 100_050u128)];
        assert!(plan(&mine, &wants, 25).is_empty(), "five basis points is not worth a transaction");
    }

    #[test]
    fn a_moved_market_reprices_rather_than_stacking() {
        let wants = vec![Want { series_id: 7, premium_units: 120_000 }];
        let mine = vec![(11, 7, 100_000u128), (12, 7, 100_010u128)];
        let actions = plan(&mine, &wants, 25);
        assert_eq!(
            actions,
            vec![
                Action::Cancel { offer_id: 11 },
                Action::Cancel { offer_id: 12 },
                Action::Post { series_id: 7, premium_units: 120_000 },
            ],
            "take the old ones back before posting the new one"
        );
    }

    #[test]
    fn an_offer_the_maker_no_longer_wants_is_taken_back() {
        let wants = vec![Want { series_id: 7, premium_units: 100_000 }];
        let mine = vec![(11, 7, 100_000u128), (12, 9, 50_000u128)];
        assert_eq!(plan(&mine, &wants, 25), vec![Action::Cancel { offer_id: 12 }]);
    }

    #[test]
    fn nothing_resting_is_one_post() {
        let wants = vec![Want { series_id: 7, premium_units: 100_000 }];
        assert_eq!(
            plan(&[], &wants, 25),
            vec![Action::Post { series_id: 7, premium_units: 100_000 }]
        );
    }
}
