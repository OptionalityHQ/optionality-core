//! Golden trade vectors, printed by the same code the contracts execute.
//!
//! The browser plans an order in TypeScript; the contract accepts or refuses it
//! in Rust. Neither language re-implements the other, so the only way to know
//! the plan is fundable is to check it against numbers Rust actually produced.
//! These vectors are that bridge: `entry_units` and `max_payout_units` come from
//! `bounded-core`, and `api/test/vectors.test.ts` feeds them through the real
//! order planner.
//!
//! Regenerate with `GENERATE_VECTORS=1 cargo test -p bounded-core --test vectors`.

use bounded_core::{accounting, fees};
use serde_json::{json, Value};

const SCALE: u128 = 1_000_000;
const LOW: u128 = 100;
const HIGH: u128 = 200;
const WIDTH: u128 = HIGH - LOW;
const PRICES: [u128; 5] = [0, 90, 110, 200, 220];

fn row(contracts: u128, mid: u128, terms: fees::Terms) -> Value {
    let quantity = contracts * SCALE;
    let mid_units = mid * SCALE;
    let entry = terms
        .entry_premium(quantity, mid_units, SCALE)
        .expect("bounded mid");
    let exit = terms
        .exit_value(quantity, mid_units, SCALE)
        .expect("positive kept fraction");
    let hold = fees::Terms::FLAT
        .entry_premium(quantity, mid_units, SCALE)
        .expect("bounded mid");
    let cut = terms.venue_cut(entry).expect("split");
    let lp = terms.lp_notional(entry).expect("split");
    let max_payout = accounting::max_payout(quantity, WIDTH * SCALE, SCALE).expect("bounded");
    let payoffs = |call: bool| -> Vec<Value> {
        PRICES
            .iter()
            .map(|price| {
                let payout = if call {
                    accounting::payout_call(
                        quantity,
                        SCALE,
                        HIGH * SCALE,
                        LOW * SCALE,
                        price * SCALE,
                    )
                } else {
                    accounting::payout(quantity, SCALE, HIGH * SCALE, LOW * SCALE, price * SCALE)
                }
                .expect("bounded");
                json!({
                    "price_units": (price * SCALE).to_string(),
                    "payout_units": payout.to_string(),
                })
            })
            .collect()
    };
    json!({
        "contracts": contracts,
        "quantity_units": quantity.to_string(),
        "mid_units": mid_units.to_string(),
        "scale": SCALE.to_string(),
        "margin_bps": terms.margin_bps,
        "impact_bps": terms.impact_bps,
        "entry_units": entry.to_string(),
        "hold_entry_units": hold.to_string(),
        "exit_units": exit.to_string(),
        "cut_units": cut.to_string(),
        "lp_units": lp.to_string(),
        "max_payout_units": max_payout.to_string(),
        "put_payouts": payoffs(false),
        "call_payouts": payoffs(true),
    })
}

#[test]
fn trade_vectors_stay_reproducible() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../testdata/trade-vectors.json"
    );
    let mut rows = Vec::new();
    for contracts in [1u128, 3, 10] {
        for mid in [1u128, 5, 22, 95] {
            for terms in [fees::Terms::FLAT, fees::Terms::VENUE, fees::Terms::STANDARD] {
                rows.push(row(contracts, mid, terms));
            }
        }
    }
    let document = json!({
        "comment": "Trade vectors from bounded-core, consumed by the TypeScript order planner.",
        "generated_by": "core/tests/vectors.rs",
        "strikes": { "low": LOW.to_string(), "high": HIGH.to_string() },
        "rows": rows,
    });
    if std::env::var("GENERATE_VECTORS").is_ok() {
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&document).unwrap()),
        )
        .expect("write vectors");
    }

    let fixture = std::fs::read_to_string(path).expect("run with GENERATE_VECTORS=1 once");
    let parsed: Value = serde_json::from_str(&fixture).expect("valid json");
    assert_eq!(
        parsed["rows"], document["rows"],
        "the checked-in vectors differ from what the money code produces"
    );

    // Every vector obeys the two rules the planner depends on, so a broken
    // generator cannot bless a broken plan.
    for entry in parsed["rows"].as_array().expect("rows") {
        let entry_units: u128 = entry["entry_units"].as_str().unwrap().parse().unwrap();
        let max: u128 = entry["max_payout_units"].as_str().unwrap().parse().unwrap();
        let cut: u128 = entry["cut_units"].as_str().unwrap().parse().unwrap();
        let lp: u128 = entry["lp_units"].as_str().unwrap().parse().unwrap();
        assert!(entry_units <= max, "a ticket may never cost more than it pays");
        assert_eq!(cut + lp, entry_units, "the split conserves the premium");
        for payoff in entry["put_payouts"].as_array().unwrap() {
            let payout: u128 = payoff["payout_units"].as_str().unwrap().parse().unwrap();
            assert!(payout <= max, "a payoff may never exceed its reservation");
        }
    }
}
