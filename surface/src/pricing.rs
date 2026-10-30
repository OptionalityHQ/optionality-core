//! Black-Scholes over a skewed vol surface, in `f64`.
//!
//! Ported term for term from the JavaScript pricing module this crate replaced,
//! including the Abramowitz & Stegun error-function approximation, so a quote
//! produced here lands on the same number the old code produced rather than a
//! nearby one. That matters because `testdata/surface-vectors.json` was printed
//! by that JavaScript and is now the migration's frozen proof: the generator
//! went with the code it measured, so a failing vector is adjudicated by the
//! Rust, not by re-running the thing being retired.
//!
//! Nothing here is safe to run inside the contract: floats are not guaranteed to
//! agree across validator builds. The chain sees the integer premium this model
//! was used to ask for, and enforces bounds on it with `core::accounting`.

/// A spread is either side of the same bounded payoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Put,
    Call,
}

impl Kind {
    pub fn is_put(self) -> bool {
        self == Kind::Put
    }
}

pub const SECONDS_PER_YEAR: f64 = 365.0 * 24.0 * 60.0 * 60.0;

/// Log-moneyness slope of the smile: lower strikes carry more implied vol, the
/// way listed crypto options are quoted.
pub const DEFAULT_SKEW: f64 = 0.35;

/// Abramowitz & Stegun 7.1.26, the same approximation the JavaScript used.
fn erf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let value = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * value);
    let y = 1.0
        - ((((1.061405429 * t - 1.453152027) * t + 1.421413741) * t - 0.284496736) * t
            + 0.254829592)
            * t
            * (-value * value).exp();
    sign * y
}

pub fn normal_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

fn pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// Black-Scholes put with r = 0, in price points. A prototype never discounts
/// over a week; at or past expiry the value is intrinsic.
pub fn put_value(spot: f64, strike: f64, vol: f64, years: f64) -> f64 {
    if years <= 0.0 || vol <= 0.0 || spot <= 0.0 || strike <= 0.0 {
        return (strike - spot).max(0.0);
    }
    let sqrt_t = years.sqrt();
    let d1 = ((spot / strike).ln() + 0.5 * vol * vol * sqrt_t * sqrt_t) / (vol * sqrt_t);
    let d2 = d1 - vol * sqrt_t;
    strike * normal_cdf(-d2) - spot * normal_cdf(-d1)
}

/// Call by parity, which keeps one error function rather than two.
pub fn call_price(spot: f64, strike: f64, vol: f64, years: f64) -> f64 {
    put_value(spot, strike, vol, years) - strike + spot
}

/// Implied vol at a strike, floored so a deep wing cannot go negative.
pub fn smile_vol(spot: f64, strike: f64, atm_vol: f64, skew: f64) -> f64 {
    let ratio = (spot / strike).ln();
    (atm_vol * (1.0 + skew * ratio)).max(0.05)
}

/// Value of one spread contract with each leg priced off the smile. The long leg
/// is `high` for a put and `low` for a call, matching the contract's payoff.
pub fn spread_value(
    spot: f64,
    low: f64,
    high: f64,
    kind: Kind,
    atm_vol: f64,
    years: f64,
    skew: f64,
) -> f64 {
    if years <= 0.0 {
        let intrinsic = if kind.is_put() {
            (high - spot).max(0.0)
        } else {
            (spot - low).max(0.0)
        };
        return intrinsic.min(high - low);
    }
    let vol_low = smile_vol(spot, low, atm_vol, skew);
    let vol_high = smile_vol(spot, high, atm_vol, skew);
    if kind.is_put() {
        let value_low = put_value(spot, low, vol_low, years);
        let value_high = put_value(spot, high, vol_high, years);
        (value_high - value_low).max(0.0)
    } else {
        let value_low = call_price(spot, low, vol_low, years);
        let value_high = call_price(spot, high, vol_high, years);
        (value_low - value_high).max(0.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Greeks {
    pub delta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub theta: f64,
}

/// Greeks for one put, r = 0, per contract in price points.
pub fn put_greeks(spot: f64, strike: f64, vol: f64, years: f64) -> (Greeks, f64) {
    if years <= 0.0 || vol <= 0.0 {
        let intrinsic = (strike - spot).max(0.0);
        let greeks = Greeks {
            delta: if spot < strike { -1.0 } else { 0.0 },
            gamma: 0.0,
            vega: 0.0,
            theta: 0.0,
        };
        return (greeks, intrinsic);
    }
    let sqrt_t = years.sqrt();
    let d1 = ((spot / strike).ln() + 0.5 * vol * vol * years) / (vol * sqrt_t);
    let price = put_value(spot, strike, vol, years);
    (
        Greeks {
            delta: -normal_cdf(-d1),
            gamma: pdf(d1) / (spot * vol * sqrt_t),
            vega: spot * pdf(d1) * sqrt_t / 100.0,
            theta: -(spot * pdf(d1) * vol) / (2.0 * sqrt_t) / 365.0,
        },
        price,
    )
}

/// Greeks of the ticket: long one leg, short the other, both off the smile.
pub fn spread_greeks(
    spot: f64,
    low: f64,
    high: f64,
    kind: Kind,
    atm_vol: f64,
    years: f64,
    skew: f64,
) -> Greeks {
    if years <= 0.0 {
        return Greeks::default();
    }
    let vol_low = smile_vol(spot, low, atm_vol, skew);
    let vol_high = smile_vol(spot, high, atm_vol, skew);
    let (long_strike, long_vol, short_strike, short_vol) = if kind.is_put() {
        (high, vol_high, low, vol_low)
    } else {
        (low, vol_low, high, vol_high)
    };
    let (long, _) = put_greeks(spot, long_strike, long_vol, years);
    let (short, _) = put_greeks(spot, short_strike, short_vol, years);
    // A call's delta is the put's delta plus one, and the spread cancels the
    // shift, so the same difference serves both kinds.
    Greeks {
        delta: long.delta - short.delta,
        gamma: long.gamma - short.gamma,
        vega: long.vega - short.vega,
        theta: long.theta - short.theta,
    }
}

/// Round strikes with tighter spacing near the money, mirroring the grid seen in
/// Derive's shipped chain ($25 on ETH, $1,000 on BTC).
pub fn strike_ladder(spot: f64, step: f64, near_steps: i32, count: i32) -> Vec<f64> {
    let half = step / 2.0;
    let atm = (spot / half).round() * half;
    let mut strikes: Vec<f64> = Vec::new();
    for index in -count..=count {
        let raw = atm + (index as f64) * half;
        let candidate = (raw * 100.0).round() / 100.0;
        let near = (candidate - spot).abs() <= (near_steps as f64) * step;
        let on_grid = (candidate % step).abs() < 1e-9;
        if (near || on_grid) && !strikes.iter().any(|seen| (seen - candidate).abs() < 1e-12) {
            strikes.push(candidate);
        }
    }
    strikes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    strikes
}

/// Fair value of one offer in integer token units, with the maker's edge.
/// `quantity` is already scaled, which is also the prototype's "1 price point =
/// 1 token" collateral equivalence. This is the one place the float model is
/// allowed to touch the integers the chain enforces.
#[allow(clippy::too_many_arguments)]
pub fn quote_premium(
    quantity: u128,
    scale: u128,
    spot: f64,
    low: u128,
    high: u128,
    vol: f64,
    years: f64,
    edge_bps: u32,
) -> u128 {
    let scale_f64 = scale as f64;
    let value = spread_value(
        spot,
        low as f64 / scale_f64,
        high as f64 / scale_f64,
        Kind::Put,
        vol,
        years,
        0.0,
    );
    let gross = quantity as f64 * value * (1.0 + edge_bps as f64 / 10_000.0);
    let rounded = gross.ceil().max(0.0) as u128;
    if rounded > 0 {
        rounded
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both sides of the migration use the same approximation of `erf`, but V8
    /// and the system libm may round the last ulp of `ln`/`exp` differently, so
    /// agreement is relative rather than bit-exact.
    fn close(got: f64, want: f64, label: &str) {
        let tolerance = 1e-9 * want.abs().max(1.0);
        assert!((got - want).abs() <= tolerance, "{label}: {got} vs {want}");
    }

    #[test]
    fn reproduces_the_javascript_pricing() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/surface-vectors.json");
        let document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("testdata/surface-vectors.json is missing"))
                .expect("valid json");
        let rows = document["rows"].as_array().expect("rows");
        for row in rows {
            let input = &row["input"];
            let spot = input["spot"].as_f64().unwrap();
            let low = input["low"].as_f64().unwrap();
            let high = input["high"].as_f64().unwrap();
            let vol = input["vol"].as_f64().unwrap();
            let years = input["years"].as_f64().unwrap();

            for (index, want) in row["normalCdf"].as_array().unwrap().iter().enumerate() {
                close(normal_cdf([-3.0, -0.5, 0.0, 1.7][index]), want.as_f64().unwrap(), "cdf");
            }
            close(put_value(spot, high, vol, years), row["put"].as_f64().unwrap(), "put");
            close(call_price(spot, high, vol, years), row["call"].as_f64().unwrap(), "call");
            close(smile_vol(spot, low, vol, DEFAULT_SKEW), row["smileLow"].as_f64().unwrap(), "smile");
            close(
                spread_value(spot, low, high, Kind::Put, vol, years, DEFAULT_SKEW),
                row["putSpread"].as_f64().unwrap(),
                "put spread",
            );
            close(
                spread_value(spot, low, high, Kind::Call, vol, years, DEFAULT_SKEW),
                row["callSpread"].as_f64().unwrap(),
                "call spread",
            );
            close(
                spread_value(spot, low, high, Kind::Put, vol, years, 0.0),
                row["putSpreadFlat"].as_f64().unwrap(),
                "flat-surface spread",
            );

            let (greeks, price) = put_greeks(spot, high, vol, years);
            let want = &row["putGreeks"];
            close(greeks.delta, want["delta"].as_f64().unwrap(), "put delta");
            close(greeks.gamma, want["gamma"].as_f64().unwrap(), "put gamma");
            close(greeks.vega, want["vega"].as_f64().unwrap(), "put vega");
            close(greeks.theta, want["theta"].as_f64().unwrap(), "put theta");
            close(price, want["price"].as_f64().unwrap(), "put greek price");

            for (field, want) in [
                ("delta", &row["putGreeksSpread"]),
                ("gamma", &row["putGreeksSpread"]),
                ("vega", &row["putGreeksSpread"]),
                ("theta", &row["putGreeksSpread"]),
            ] {
                let got = spread_greeks(spot, low, high, Kind::Put, vol, years, DEFAULT_SKEW);
                let value = match field {
                    "delta" => got.delta,
                    "gamma" => got.gamma,
                    "vega" => got.vega,
                    _ => got.theta,
                };
                close(value, want[field].as_f64().unwrap(), "put spread greeks");
            }
            let call_greeks = spread_greeks(spot, low, high, Kind::Call, vol, years, DEFAULT_SKEW);
            close(
                call_greeks.delta,
                row["callGreeksSpread"]["delta"].as_f64().unwrap(),
                "call spread delta",
            );

            let want_ladder = row["ladder"].as_array().unwrap();
            let got_ladder = strike_ladder(spot, if spot >= 10_000.0 { 1000.0 } else { 25.0 }, 2, 14);
            assert_eq!(got_ladder.len(), want_ladder.len(), "ladder length at {spot}");
            for (got, want) in got_ladder.iter().zip(want_ladder) {
                close(*got, want.as_f64().unwrap(), "ladder strike");
            }

            let premium = quote_premium(
                3_000_000,
                SCALE,
                spot,
                (low * 1e6) as u128,
                (high * 1e6) as u128,
                vol,
                years,
                25,
            );
            let want: u128 = row["premium"].as_str().unwrap().parse().unwrap();
            assert_eq!(premium, want, "integer premium at {spot} {vol} {years}");
        }
    }

    const SCALE: u128 = 1_000_000;

    #[test]
    fn a_spread_is_worth_between_zero_and_its_width() {
        for spot in [50.0, 100.0, 110.0, 500.0] {
            for vol in [0.05, 0.5, 2.0] {
                for years in [0.0, 0.01, 0.25] {
                    for kind in [Kind::Put, Kind::Call] {
                        let value = spread_value(spot, 100.0, 110.0, kind, vol, years, DEFAULT_SKEW);
                        assert!((0.0..=10.0).contains(&value), "{kind:?} {spot} {vol} {years} -> {value}");
                    }
                }
            }
        }
    }

    #[test]
    fn put_parity_holds_and_the_smile_is_monotone() {
        // call - put == spot - strike, the identity the call leg is derived from.
        for spot in [900.0, 1000.0, 1100.0] {
            let difference = call_price(spot, 1000.0, 0.5, 0.1) - put_value(spot, 1000.0, 0.5, 0.1);
            close(difference, spot - 1000.0, "parity");
        }
        // Lower strikes carry more implied vol: that is the skew's whole point.
        assert!(smile_vol(1000.0, 900.0, 0.5, DEFAULT_SKEW) > smile_vol(1000.0, 1100.0, 0.5, DEFAULT_SKEW));
        assert!((smile_vol(1000.0, 1e9, 0.5, DEFAULT_SKEW) - 0.05).abs() < 1e-12, "wing floor");
    }

    #[test]
    fn the_ladder_is_sorted_unique_and_covers_the_money() {
        let ladder = strike_ladder(2350.0, 25.0, 2, 14);
        assert!(ladder.windows(2).all(|pair| pair[0] < pair[1]), "sorted and unique");
        assert!(ladder.iter().any(|strike| (*strike - 2350.0).abs() < 13.0), "nothing near spot");
        assert!(ladder.iter().any(|strike| *strike > 2400.0), "no upside wing");
    }
}
