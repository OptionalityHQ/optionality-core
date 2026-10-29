//! Pure integer payoff and settlement accounting.
//!
//! All values are integers in collateral-token units. `scale` is the price
//! scale (for example 1_000_000 for six decimals); quantities use the same
//! scale for the prototype. Overflow is rejected rather than wrapped.

/// Maximum liability of a put spread: quantity * (high - low), rounded up.
pub fn max_payout(quantity: u128, width: u128, scale: u128) -> Option<u128> {
    if scale == 0 {
        return None;
    }
    quantity
        .checked_mul(width)?
        .checked_add(scale.checked_sub(1)?)?
        .checked_div(scale)
}

/// Cash payoff at expiry for a put spread with strikes `low < high`.
///
/// Rounded down in the maker's favour. It can never exceed `max_payout`.
pub fn payout(quantity: u128, scale: u128, high: u128, low: u128, price: u128) -> Option<u128> {
    if scale == 0 || high <= low {
        return None;
    }
    let width = high - low;
    let intrinsic = high.saturating_sub(price);
    let capped = intrinsic.min(width);
    quantity.checked_mul(capped)?.checked_div(scale)
}

/// Cash payoff at expiry for a call spread with strikes `low < high`, the
/// mirror of the put spread: bounded by the same width and rounded down.
pub fn payout_call(quantity: u128, scale: u128, high: u128, low: u128, price: u128) -> Option<u128> {
    if scale == 0 || high <= low {
        return None;
    }
    let width = high - low;
    let intrinsic = price.saturating_sub(low);
    let capped = intrinsic.min(width);
    quantity.checked_mul(capped)?.checked_div(scale)
}

/// Split escrowed collateral and premium after a fixed price or a void.
///
/// The two credits always sum to `collateral + premium`.
pub fn split_settlement(
    collateral: u128,
    premium: u128,
    payout: u128,
    voided: bool,
) -> Option<(u128, u128)> {
    if payout > collateral {
        return None;
    }
    if voided {
        // Buyer gets the premium back; maker recovers the full reservation.
        Some((premium, collateral))
    } else {
        let maker_collateral = collateral - payout;
        premium
            .checked_add(maker_collateral)
            .map(|maker| (payout, maker))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALE: u128 = 1_000_000;

    #[test]
    fn payoff_is_capped_on_both_sides() {
        assert_eq!(payout(1, SCALE, 110 * SCALE, 100 * SCALE, 120 * SCALE), Some(0));
        assert_eq!(payout(1, SCALE, 110 * SCALE, 100 * SCALE, 110 * SCALE), Some(0));
        assert_eq!(payout(1, SCALE, 110 * SCALE, 100 * SCALE, 105 * SCALE), Some(5));
        assert_eq!(payout(1, SCALE, 110 * SCALE, 100 * SCALE, 100 * SCALE), Some(10));
        assert_eq!(payout(1, SCALE, 110 * SCALE, 100 * SCALE, 90 * SCALE), Some(10));
    }

    #[test]
    fn max_payout_rounds_up_and_covers_every_payout() {
        let m = max_payout(3, 10 * SCALE, SCALE).unwrap();
        assert_eq!(m, 30);
        let m = max_payout(3, 10, 6).unwrap();
        assert_eq!(m, 5); // ceil(30 / 6)
        for price in [0, 99, 100, 105, 110, 120] {
            let p = payout(3, 6, 110, 100, price).unwrap();
            assert!(p <= m);
        }
    }

    #[test]
    fn settlement_conserves_value() {
        for voided in [false, true] {
            for p in [0, 1, 5, 10] {
                let (buyer, maker) = split_settlement(10, 3, p, voided).unwrap();
                assert_eq!(buyer + maker, 13);
            }
        }
        assert_eq!(split_settlement(10, 3, 0, false), Some((0, 13)));
        assert_eq!(split_settlement(10, 3, 10, false), Some((10, 3)));
        assert_eq!(split_settlement(10, 3, 0, true), Some((3, 10)));
    }

    #[test]
    fn rejects_overflow_and_bad_inputs() {
        assert_eq!(max_payout(1, 1, 0), None);
        assert_eq!(max_payout(u128::MAX, 2, 1), None);
        assert_eq!(payout(1, 0, 110, 100, 90), None);
        assert_eq!(payout(1, SCALE, 100, 100, 90), None);
        assert_eq!(split_settlement(10, 3, 11, false), None);
        assert_eq!(payout(u128::MAX, 1, 3, 1, 0), None);
    }

    /// `(quantity, scale, low, high, price, premium)` for the shared vectors.
    fn vector_cases() -> Vec<(u128, u128, u128, u128, u128, u128)> {
        let scale = 1_000_000u128;
        let mut rows = Vec::new();
        for (quantity, premium) in [
            (1_000_000u128, 2_000_000u128),
            (2_000_000, 5_000_000),
            (1_500_000, 3_000_000),
            (3, 10),
            (7, 1),
            (1, 7),
        ] {
            for price in [
                0u128, 99_999_999, 100_000_000, 103_000_000, 105_000_000, 109_999_999,
                110_000_000, 120_000_000,
            ] {
                rows.push((quantity, scale, 100 * scale, 110 * scale, price, premium));
            }
        }
        // An awkward scale exercises the ceil/floor rounding directly.
        for price in [0u128, 100, 103, 105, 110] {
            rows.push((3, 7, 100, 110, price, 2));
            rows.push((1_000, 7, 100, 110, price, 1));
        }
        // Rejected inputs.
        rows.push((1, 0, 100, 110, 105, 1));
        rows.push((1, 7, 110, 100, 105, 1));
        rows.push((1, 7, 100, 110, 90, 1));
        rows.push((u128::MAX, 1, 100, 110, 105, 1));
        rows
    }

    fn vector_row(
        quantity: u128,
        scale: u128,
        low: u128,
        high: u128,
        price: u128,
        premium: u128,
    ) -> serde_json::Value {
        use serde_json::json;
        let width = high.checked_sub(low);
        let collateral = match width {
            Some(w) => max_payout(quantity, w, scale),
            None => None,
        };
        let payout = payout(quantity, scale, high, low, price);
        let valid = collateral.is_some() && payout.is_some();
        let splits = || match (collateral, payout) {
            (Some(c), Some(p)) => (
                split_settlement(c, premium, p, false)
                    .map(|(b, m)| json!({ "buyer": b.to_string(), "maker": m.to_string() })),
                split_settlement(c, premium, p, true)
                    .map(|(b, m)| json!({ "buyer": b.to_string(), "maker": m.to_string() })),
            ),
            _ => (None, None),
        };
        let (fixed, voided) = splits();
        json!({
            "input": {
                "quantity": quantity.to_string(),
                "scale": scale.to_string(),
                "low_strike": low.to_string(),
                "high_strike": high.to_string(),
                "price": price.to_string(),
                "premium": premium.to_string()
            },
            "collateral": collateral.map(|v| v.to_string()),
            "payout": payout.map(|v| v.to_string()),
            "valid": valid,
            "fixed": fixed,
            "voided": voided
        })
    }

    /// The same JSON drives the Rust tests and the JavaScript simulator, so
    /// the demo UI cannot drift from the contract's numbers.
    /// Regenerate with `GENERATE_VECTORS=1 cargo test -p bounded-options`.
    #[test]
    fn accounting_vectors_stay_reproducible() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/accounting-vectors.json");
        let rows: Vec<_> = vector_cases()
            .into_iter()
            .map(|(q, s, l, h, p, prem)| vector_row(q, s, l, h, p, prem))
            .collect();
        let document = serde_json::json!({
            "comment": "Shared contract/simulator accounting vectors.",
            "generated_by": "bounded-options accounting.rs",
            "rows": rows
        });
        if std::env::var("GENERATE_VECTORS").is_ok() {
            std::fs::write(
                path,
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(&document).unwrap()
                ),
            )
            .expect("write vectors");
        }

        let fixture = std::fs::read_to_string(path).expect("run with GENERATE_VECTORS=1 once");
        let parsed: serde_json::Value =
            serde_json::from_str(&fixture).expect("valid json");
        let expected = parsed["rows"]
            .as_array()
            .expect("rows array")
            .clone();
        assert_eq!(expected.len(), rows.len(), "vector count drifted");
        for (index, (actual, want)) in rows.iter().zip(expected.iter()).enumerate() {
            assert_eq!(actual, want, "vector {} differs", index);
        }

        // Every valid row must conserve value.
        for row in &rows {
            if row["valid"].as_bool().unwrap() {
                let collateral: u128 = row["collateral"].as_str().unwrap().parse().unwrap();
                let premium: u128 = row["input"]["premium"].as_str().unwrap().parse().unwrap();
                let payout: u128 = row["payout"].as_str().unwrap().parse().unwrap();
                assert!(payout <= collateral, "payout exceeds reservation");
                let buyer: u128 = row["fixed"]["buyer"].as_str().unwrap().parse().unwrap();
                let maker: u128 = row["fixed"]["maker"].as_str().unwrap().parse().unwrap();
                assert_eq!(buyer + maker, collateral + premium);
                let void_buyer: u128 = row["voided"]["buyer"].as_str().unwrap().parse().unwrap();
                let void_maker: u128 = row["voided"]["maker"].as_str().unwrap().parse().unwrap();
                assert_eq!(void_buyer + void_maker, collateral + premium);
            } else {
                assert!(row["fixed"].is_null(), "invalid input must not settle");
            }
        }
    }
}
