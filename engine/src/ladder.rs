//! Which series the venue lists, and what they are struck at.
//!
//! This is the part of the market that has never lived in Rust: the JavaScript
//! daemon decided the strike grid and created its ledgers in memory, and the
//! chain was told about a series only when a demo script happened to call
//! `create_series`. Here the ladder is a pure function of the index and the
//! expiry, and its output is the argument list for a real transaction — so the
//! grid the screen quotes from and the grid the contract enforces are produced
//! by the same call.

use serde_json::{json, Value};
use surface::pricing::Kind;

/// Six decimals, the prototype's "one price point is one token" scale.
pub const SCALE: u128 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asset {
    Eth,
    Btc,
}

impl Asset {
    pub fn symbol(self) -> &'static str {
        match self {
            Asset::Eth => "ETH",
            Asset::Btc => "BTC",
        }
    }

    pub fn feed(self) -> &'static str {
        match self {
            Asset::Eth => "ETH-USD",
            Asset::Btc => "BTC-USD",
        }
    }

    /// Strike spacing, from the grid actually traded on these underlyings: $25
    /// on ETH, $1,000 on BTC.
    pub fn step(self) -> f64 {
        match self {
            Asset::Eth => 25.0,
            Asset::Btc => 1_000.0,
        }
    }

    pub fn parse(value: &str) -> Option<Asset> {
        match value.to_uppercase().as_str() {
            "ETH" => Some(Asset::Eth),
            "BTC" => Some(Asset::Btc),
            _ => None,
        }
    }
}

/// The shape of one expiry's chain. Defaults are the numbers the demo runs on.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Legs sit this many rungs apart, so the width is `width_rungs × step`.
    pub width_rungs: i32,
    /// Rungs each side of at-the-money, before both kinds are expanded.
    pub ladder_count: i32,
    pub min_confidence_bps: u16,
    /// How long after expiry the oracle may still deliver a print. Five minutes,
    /// not the minute the demo started with: one stalled reporter pass past the
    /// window voids every ticket on the series, and a void pays nobody anything.
    /// The contract allows a day.
    pub accept_window_ns: u64,
    /// How long after that the venue may force a settlement.
    pub finalize_window_ns: u64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            width_rungs: 4,
            ladder_count: 18,
            min_confidence_bps: 3_000,
            accept_window_ns: 300_000_000_000,
            finalize_window_ns: 900_000_000_000,
        }
    }
}

impl Params {
    pub fn width(&self, asset: Asset) -> f64 {
        (self.width_rungs as f64) * asset.step()
    }
}

/// The two legs of a vertical spread struck at `strike`. A put is long the high
/// leg and short the low; a call the other way round. `None` is a rung the
/// venue must not list — a negative strike has no meaning and the contract
/// would reject the series anyway.
pub fn legs(asset: Asset, kind: Kind, strike: f64, params: &Params) -> Option<(f64, f64)> {
    let width = params.width(asset);
    let (low, high) = match kind {
        Kind::Put => (strike - width, strike),
        Kind::Call => (strike, strike + width),
    };
    // `high > low` is width arithmetic, not a check: what can fail is a
    // non-finite strike, a leg at or below zero, and a zero width.
    if !strike.is_finite() || low <= 0.0 || high <= low {
        return None;
    }
    Some((low, high))
}

/// The strike grid around the current index. Cached by the caller against a
/// bucket of spot, because recomputing it on every print would let the ladder
/// jitter one rung every tick and the chain would see churn as intent.
pub fn ladder(asset: Asset, spot: f64, params: &Params) -> Vec<f64> {
    if !(spot.is_finite()) || spot <= 0.0 {
        return Vec::new();
    }
    surface::pricing::strike_ladder(spot, asset.step(), 0, params.ladder_count)
}

/// One listed series, in the units the contract takes.
#[derive(Clone, Debug, PartialEq)]
pub struct SeriesSpec {
    pub asset: Asset,
    pub kind: Kind,
    pub low_units: u128,
    pub high_units: u128,
    pub expiry_ns: u64,
    pub accept_until_ns: u64,
    pub finalize_until_ns: u64,
    pub price_scale: u128,
    pub min_confidence_bps: u16,
}

impl SeriesSpec {
    pub fn width_units(&self) -> u128 {
        self.high_units - self.low_units
    }

    /// The strike a trader reads on the ladder: a put is named by its high leg,
    /// a call by its low. Same rule the gateway uses to find a cell.
    pub fn strike_units(&self) -> u128 {
        match self.kind {
            Kind::Put => self.high_units,
            Kind::Call => self.low_units,
        }
    }

    pub fn label(&self) -> String {
        let low = self.low_units as f64 / self.price_scale as f64;
        let high = self.high_units as f64 / self.price_scale as f64;
        let letter = if self.kind == Kind::Put { "P" } else { "C" };
        format!(
            "{} {}{}/{}",
            self.asset.symbol(),
            letter,
            trim(low),
            trim(high)
        )
    }

    /// The exact `create_series` argument object. `U128` wrappers go as strings
    /// and `u64`s as numbers, which is the one place the SDK's JSON convention
    /// has bitten every demo so far.
    pub fn create_series_args(&self) -> Value {
        json!({
            "underlying": self.asset.symbol(),
            "kind": if self.kind == Kind::Put { "Put" } else { "Call" },
            "expiry_ns": self.expiry_ns,
            "accept_until_ns": self.accept_until_ns,
            "finalize_until_ns": self.finalize_until_ns,
            "low_strike": self.low_units.to_string(),
            "high_strike": self.high_units.to_string(),
            "price_scale": self.price_scale.to_string(),
            "min_confidence_bps": self.min_confidence_bps,
            "oracle_feed": self.asset.feed(),
        })
    }
}

fn trim(value: f64) -> String {
    if value % 1.0 == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{:.2}", value)
    }
}

/// Every series one expiry should hold: the grid, both kinds, in the order the
/// operator lists them. Rungs too close to zero to have two legs are skipped
/// rather than clamped — a 0/25 put on a 20 index is a different product.
pub fn plan(asset: Asset, spot: f64, expiry_ns: u64, params: &Params) -> Vec<SeriesSpec> {
    let mut specs = Vec::new();
    for strike in ladder(asset, spot, params) {
        for kind in [Kind::Put, Kind::Call] {
            let Some((low, high)) = legs(asset, kind, strike, params) else {
                continue;
            };
            let low_units = units(low, params);
            let high_units = units(high, params);
            if high_units <= low_units || low_units == 0 {
                continue;
            }
            specs.push(SeriesSpec {
                asset,
                kind,
                low_units,
                high_units,
                expiry_ns,
                accept_until_ns: expiry_ns.saturating_add(params.accept_window_ns),
                finalize_until_ns: expiry_ns.saturating_add(params.accept_window_ns + params.finalize_window_ns),
                price_scale: SCALE,
                min_confidence_bps: params.min_confidence_bps,
            });
        }
    }
    specs
}

/// Points to scaled integer units. Rounding rather than truncating, because a
/// grid point like 2337.50 must not land a leg half a cent off its own strike.
fn units(points: f64, _params: &Params) -> u128 {
    (points * SCALE as f64).round().max(0.0) as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPIRY: u64 = 1_700_000_000_000_000_000;

    #[test]
    fn assets_are_their_own_grids() {
        assert_eq!(Asset::parse("eth"), Some(Asset::Eth));
        assert_eq!(Asset::parse("BTC"), Some(Asset::Btc));
        assert_eq!(Asset::parse("sol"), None);
        assert_eq!(Asset::Eth.step(), 25.0);
        assert_eq!(Asset::Btc.step(), 1_000.0);
        assert_eq!(Asset::Btc.feed(), "BTC-USD");
        assert_eq!(Params::default().width(Asset::Eth), 100.0);
    }

    #[test]
    fn a_put_and_a_call_on_the_same_strike_face_the_other_way() {
        let p = Params::default();
        assert_eq!(legs(Asset::Eth, Kind::Put, 2400.0, &p), Some((2300.0, 2400.0)));
        assert_eq!(legs(Asset::Eth, Kind::Call, 2400.0, &p), Some((2400.0, 2500.0)));
    }

    #[test]
    fn a_leg_below_zero_is_not_a_series() {
        let p = Params { width_rungs: 4, ..Params::default() };
        assert_eq!(legs(Asset::Eth, Kind::Put, 50.0, &p), None);
        // The boundary itself: a 100 put with a 100-wide chain wants a leg at 0.
        assert_eq!(legs(Asset::Eth, Kind::Put, 100.0, &p), None);
        assert_eq!(legs(Asset::Eth, Kind::Call, f64::NAN, &p), None);
    }

    #[test]
    fn the_ladder_is_a_round_grid_centred_on_the_index() {
        let p = Params::default();
        let grid = ladder(Asset::Eth, 2350.0, &p);
        assert!(!grid.is_empty());
        assert!(grid.windows(2).all(|pair| pair[0] < pair[1]), "sorted");
        assert!(grid.iter().all(|strike| (strike / 25.0).fract() == 0.0), "all on the 25 grid");
        assert!(grid.iter().any(|strike| (*strike - 2350.0).abs() <= 12.5), "spans the money");
        assert!(ladder(Asset::Eth, f64::NAN, &p).is_empty(), "a broken index lists nothing");
        assert!(ladder(Asset::Eth, 0.0, &p).is_empty());
    }

    #[test]
    fn a_plan_is_the_grid_times_both_kinds_minus_the_ones_with_no_room() {
        let p = Params::default();
        let specs = plan(Asset::Eth, 2350.0, EXPIRY, &p);
        assert_eq!(specs.len(), ladder(Asset::Eth, 2350.0, &p).len() * 2);
        assert!(specs.iter().all(|s| s.high_units - s.low_units == 100 * SCALE));
        let puts = specs.iter().filter(|s| s.kind == Kind::Put).count();
        let calls = specs.len() - puts;
        assert!(puts == calls, "both sides of the same grid");
        assert!(specs.iter().all(|s| s.expiry_ns == EXPIRY));
        assert!(specs.iter().all(|s| s.asset == Asset::Eth));
    }

    #[test]
    fn windows_open_at_expiry_and_close_in_order() {
        let p = Params::default();
        let spec = plan(Asset::Btc, 60_000.0, EXPIRY, &p)
            .into_iter()
            .find(|s| s.kind == Kind::Call)
            .expect("a call exists");
        assert_eq!(spec.accept_until_ns, EXPIRY + 300_000_000_000, "five minutes to fix");
        assert_eq!(spec.finalize_until_ns, spec.accept_until_ns + 900_000_000_000);
        assert!(spec.finalize_until_ns > spec.accept_until_ns);
    }

    #[test]
    fn a_deep_put_on_a_low_index_loses_its_rungs_instead_of_listing_a_negative_leg() {
        let p = Params::default();
        let specs = plan(Asset::Eth, 120.0, EXPIRY, &p);
        assert!(
            specs.iter().all(|s| s.low_units > 0),
            "every listed series has two positive legs"
        );
        let puts = specs.iter().filter(|s| s.kind == Kind::Put).count();
        let calls = specs.iter().filter(|s| s.kind == Kind::Call).count();
        assert!(puts < calls, "the put wing that cannot exist is skipped: {puts} vs {calls}");
    }

    #[test]
    fn a_cell_is_named_by_the_leg_a_trader_uses() {
        let p = Params::default();
        let specs = plan(Asset::Eth, 2350.0, EXPIRY, &p);
        let put = specs.iter().find(|s| s.kind == Kind::Put && s.low_units == 2300 * SCALE).expect("put");
        let call = specs.iter().find(|s| s.kind == Kind::Call && s.low_units == 2300 * SCALE).expect("call");
        assert_eq!(put.strike_units(), 2400 * SCALE);
        assert_eq!(call.strike_units(), 2300 * SCALE);
        assert_eq!(put.width_units(), 100 * SCALE);
        assert_eq!(call.label(), "ETH C2300/2400");
        assert_eq!(put.label(), "ETH P2300/2400");
    }

    #[test]
    fn create_series_args_follow_the_sds_json_convention() {
        let p = Params::default();
        let spec = plan(Asset::Eth, 2350.0, EXPIRY, &p)
            .into_iter()
            .find(|s| s.kind == Kind::Call)
            .expect("a call");
        let args = spec.create_series_args();
        // U128 wrappers are strings; u64 and u16 are numbers. Getting this wrong
        // is a panic inside the contract, not a validation error.
        assert!(args["low_strike"].is_string());
        assert!(args["high_strike"].is_string());
        assert!(args["price_scale"].is_string());
        assert!(args["expiry_ns"].is_number());
        assert!(args["accept_until_ns"].is_number());
        assert!(args["finalize_until_ns"].is_number());
        assert!(args["min_confidence_bps"].is_number());
        assert_eq!(args["underlying"], "ETH");
        assert_eq!(args["oracle_feed"], "ETH-USD");
        assert_eq!(args["kind"], "Call");
        assert_eq!(args["low_strike"], "2125000000");
    }

    #[test]
    fn the_grid_stays_on_the_grid_when_the_index_does_not() {
        let p = Params::default();
        let grid = ladder(Asset::Eth, 2337.5, &p);
        // Every rung is on the 25 grid, except the one the index itself sits on
        // — a grid that ignored that would list nothing at 2337.50, and a venue
        // that followed it rung-for-rung would re-list the whole chain per tick.
        assert!(
            grid.iter()
                .all(|strike| (strike / 25.0).fract() == 0.0 || (*strike - 2337.5).abs() < 1e-9),
            "off-grid rung: {grid:?}"
        );
        assert!(grid.contains(&2350.0) && grid.contains(&2325.0));
        assert_eq!(grid.iter().filter(|s| (**s / 25.0).fract() != 0.0).count(), 1);
        // Rounding, not truncating: a scaled leg that lands on .5 must keep it,
        // or the series sits 50 yocto away from the strike it is named for.
        assert_eq!(units(2337.5, &p), 2_337_500_000);
        assert_eq!(units(-1.0, &p), 0, "nothing scales to a negative leg");
    }
}
