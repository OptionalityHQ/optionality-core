//! A little flow, so the venue looks alive while somebody is looking at it.
//!
//! This is not a user and not a market maker: it is one seeded account buying one
//! contract near the money on the ladder the engine is marking, and sometimes
//! closing it again. The point is that the tape, the P&L line and the vault's
//! reserved equity move without the visitor having to click first — a demo where
//! every number is zero until you press a button reads as a dead venue.
//!
//! It trades at the vault's own price through `pool_buy`, so it cannot do anything
//! a person could not: it is the same path, from the same kind of account, with the
//! same budget discipline.

use serde_json::Value;

/// One simulated purchase: which series, and what the taker will pay at most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Play {
    pub series_id: u64,
    pub budget_units: u128,
}

/// How much over the model's mid a simulated taker will accept. The vault charges
/// the mid plus its own margin (400 bps today); a budget of 110% of the mid leaves
/// room for that and still refuses a price that has run away between polls.
const BUDGET_BPS: u128 = 1_100;

/// The cell the simulated taker buys: the one the maker quotes, which is the cell
/// nearest the money on the ladder being marked. Deliberately the same choice — a
/// taker who always bought the cheapest wing would fill the tape with trades no
/// person would make.
pub fn pick(
    index_units: u128,
    plans: &[crate::operator::QuotePlan],
    rows: &[(u64, Value)],
    expiry_ns: u64,
) -> Option<Play> {
    let want = crate::maker::wants(index_units, plans, rows, expiry_ns).into_iter().next()?;
    Some(Play { series_id: want.series_id, budget_units: want.premium_units * BUDGET_BPS / 1_020 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator::QuotePlan;
    use serde_json::json;

    const EXPIRY: u64 = 1_793_300_000_000_000_000;

    fn row(id: u64, kind: &str, low: u128, high: u128) -> (u64, Value) {
        (
            id,
            json!({ "kind": kind, "low_strike": low, "high_strike": high, "expiry_ns": EXPIRY, "settlement": "Open" }),
        )
    }

    fn plan(kind: &str, low: u128, high: u128, mid: u128) -> QuotePlan {
        QuotePlan { kind: kind.to_string(), low_units: low, high_units: high, mid_units: mid }
    }

    #[test]
    fn a_simulated_taker_buys_near_the_money_with_a_budget_the_vault_can_meet() {
        let plans = vec![plan("Put", 2_300, 2_400, 55_000), plan("Call", 2_400, 2_500, 70_000)];
        let rows = vec![row(2, "Put", 2_300, 2_400), row(3, "Call", 2_400, 2_500)];
        let play = pick(2_360, &plans, &rows, EXPIRY).expect("a cell to buy");
        assert_eq!(play.series_id, 2, "the nearest put, same as the maker");
        // The mid plus the vault's 400 bps margin has to fit inside the budget, or
        // the simulated trade is one the contract would refuse.
        assert!(play.budget_units >= 55_000 * 104 / 100, "budget covers entry: {}", play.budget_units);
        assert!(play.budget_units < 55_000 * 120 / 100, "and is not open-ended");
    }

    #[test]
    fn with_nothing_listed_there_is_nothing_to_buy() {
        assert!(pick(2_360, &[plan("Call", 3_000, 3_100, 5)], &[], EXPIRY).is_none());
    }
}
