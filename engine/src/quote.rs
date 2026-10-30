//! What a series is worth, in the two number systems this venue has.
//!
//! `surface::pricing` says the mark is 0.15213 of a point; `core::fees` says the
//! trader pays 152_730 units of token for it. The engine is the only place those
//! two are allowed to meet, and it meets them by handing the float's *output* to
//! the integer code rather than inventing its own rounding. What the chain later
//! charges for the same `mid_units` is therefore this function's answer, digit
//! for digit — which is what lets the screen show a cost the contract cannot
//! contradict.

use bounded_core::accounting::max_payout;
use bounded_core::fees::Terms;
use surface::pricing::{self, Kind, DEFAULT_SKEW};

/// The half-spread the venue shows: 4% of the premium, with a floor so a
/// near-worthless deep wing still quotes a two-sided market instead of 0.00.
pub const EDGE_OF_MARK: f64 = 0.04;
pub const MIN_HALF_SPREAD: f64 = 0.02;
pub const MIN_BID: f64 = 0.01;
/// Below this sensitivity to vol, an implied vol is a search artifact rather
/// than a reading. A pinned wing's vega is around 1e-15; a near-money hour-long
/// spread's is above 0.1, so nothing meaningful sits between them.
pub const MIN_VEGA_FOR_IV: f64 = 1e-6;

/// A mark has to move this much of itself, plus a cent of noise, before the
/// venue re-prices. Same reason a maker does not cancel and repost every tick,
/// and every repost here is a transaction.
pub const REQUOTE_SHARE_OF_MARK: f64 = 0.0025;
pub const REQUOTE_NOISE: f64 = 1e-5;
/// A quiet market still gets checked, so a stale quote cannot sit forever.
pub const REQUOTE_GRACE_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quote {
    /// Model value of one contract, in price points.
    pub mark: f64,
    pub bid: f64,
    pub ask: f64,
    pub delta: f64,
    /// The rest of the desk's risk table. One number per contract, in price
    /// points, straight from `surface::pricing` — the screen shows them, so they
    /// belong to the quote rather than to a JavaScript model in the browser.
    pub gamma: f64,
    pub vega: f64,
    pub theta: f64,
    pub mark_iv: Option<f64>,
    pub bid_iv: Option<f64>,
    pub ask_iv: Option<f64>,
    /// The vol at the two prices the vault actually charges — the loaded entry a
    /// buyer pays and the haircut exit a holder is paid. The screen's ask column
    /// shows the vault's price, so an IV solved at the model's ask described a
    /// number nobody could trade at.
    pub entry_iv: Option<f64>,
    pub exit_iv: Option<f64>,
    /// The integer the venue trades on: `mid_units` is what `pool_buy` prices
    /// from, and everything below is what it costs.
    pub mid_units: u128,
    pub entry_units: u128,
    pub exit_units: u128,
    pub venue_cut_units: u128,
    pub lp_notional_units: u128,
    pub width_units: u128,
    pub max_payout_units: u128,
    pub quantity_units: u128,
}

impl Quote {
    /// The margin the venue kept, as a share of what the trader paid.
    pub fn effective_margin_bps(&self) -> f64 {
        if self.entry_units == 0 {
            return 0.0;
        }
        (self.venue_cut_units as f64 / self.entry_units as f64) * 10_000.0
    }
}

/// One instrument: the index and the two legs of a vertical. Constructed
/// through `new`, because every pricing function below is only total on a cell
/// that the model can actually price — a NaN index or an inverted pair of legs
/// has a price of nothing, and deciding that once is better than guarding twice.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub spot: f64,
    pub low: f64,
    pub high: f64,
    pub kind: Kind,
}

impl Cell {
    pub fn new(spot: f64, low: f64, high: f64, kind: Kind) -> Option<Cell> {
        if !spot.is_finite() || !low.is_finite() || !high.is_finite() {
            return None;
        }
        if spot <= 0.0 || low <= 0.0 || high <= low {
            return None;
        }
        Some(Cell { spot, low, high, kind })
    }

    pub fn width(&self) -> f64 {
        self.high - self.low
    }
}

/// What the model is told: an ATM vol and a time to expiry. Negative time is
/// refused, zero is allowed — a series between expiry and the print is still a
/// market, and it prices at its intrinsic value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Model {
    pub vol: f64,
    pub years: f64,
}

impl Model {
    pub fn new(vol: f64, years: f64) -> Option<Model> {
        if !vol.is_finite() || !years.is_finite() || vol <= 0.0 || years < 0.0 {
            return None;
        }
        Some(Model { vol, years })
    }
}

/// Half of the two-sided spread the ladder shows.
pub fn half_spread(mark: f64) -> f64 {
    (mark * EDGE_OF_MARK).max(MIN_HALF_SPREAD)
}

/// Vol that reproduces `price` on this spread, with the smile flattened so the
/// solved number means one thing.
///
/// The direction is not fixed: a vertical whose strikes straddle the index loses
/// value as vol rises, because its out-of-the-money leg gains more than the
/// in-the-money one loses. Assuming the usual increasing case is what made the
/// JavaScript column print 300% on every near-money spread — that was the end of
/// its own search bracket, not a volatility. So this finds the slope, solves
/// against it, and says `None` when a price has no vol behind it.
/// A ticket's integer premium, back to one contract's price in dollars.
fn unit_price(units: u128, quantity_units: u128, scale: u128) -> f64 {
    let quantity = quantity_units.max(1) as f64;
    units as f64 / quantity / scale as f64
}

pub fn implied_vol(price: f64, cell: &Cell, model: &Model) -> Option<f64> {
    if !price.is_finite() || price <= 0.0 || model.years <= 0.0 {
        return None;
    }
    // A price that hardly responds to vol has no volatility in it to read off.
    // A wing pinned at its width answers any vol with the same number, and the
    // column would print whatever the search landed on — 289% is not a fact
    // about the market, it is the shape of the bracket.
    let vega = pricing::spread_greeks(cell.spot, cell.low, cell.high, cell.kind, model.vol, model.years, 0.0).vega;
    if vega.abs() < MIN_VEGA_FOR_IV {
        return None;
    }
    let value_at = |vol: f64| {
        pricing::spread_value(cell.spot, cell.low, cell.high, cell.kind, vol, model.years, 0.0)
    };
    let (floor_vol, ceiling_vol) = (0.03_f64, 3.0_f64);
    let at_floor = value_at(floor_vol);
    let at_ceiling = value_at(ceiling_vol);
    let slope = at_ceiling - at_floor;
    if slope.abs() < 1e-12 {
        return None;
    }
    let increasing = slope > 0.0;
    let (cheap, rich) = if increasing { (at_floor, at_ceiling) } else { (at_ceiling, at_floor) };
    if price <= cheap || price >= rich {
        return None;
    }
    let mut lo = floor_vol;
    let mut hi = ceiling_vol;
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        let value = value_at(mid);
        let still_too_cheap = if increasing { value < price } else { value > price };
        if still_too_cheap {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some((lo + hi) / 2.0)
}

/// One quote for one series at one size. `low`, `high` and `spot` are price
/// points; `quantity_units` is already scaled, so one contract is `scale`.
pub fn quote(cell: &Cell, model: &Model, scale: u128, quantity_units: u128, terms: &Terms) -> Option<Quote> {
    if quantity_units == 0 || scale == 0 {
        return None;
    }
    let Cell { spot, low, high, kind } = *cell;
    let (atm_vol, years) = (model.vol, model.years);
    let mark = pricing::spread_value(spot, low, high, kind, atm_vol, years, DEFAULT_SKEW);
    let half = half_spread(mark);
    let bid = (mark - half).max(MIN_BID);
    let ask = mark + half;
    let greeks = pricing::spread_greeks(spot, low, high, kind, atm_vol, years, DEFAULT_SKEW);

    let mid_units = (mark * scale as f64).round().max(0.0) as u128;
    let width_units = (cell.width() * scale as f64).round().max(0.0) as u128;
    let entry_units = terms.entry_premium(quantity_units, mid_units, scale)?;
    let exit_units = terms.exit_value(quantity_units, mid_units, scale)?;
    let venue_cut_units = terms.venue_cut(entry_units)?;
    let lp_notional_units = terms.lp_notional(entry_units)?;
    let max_payout_units = max_payout(quantity_units, width_units, scale)?;

    Some(Quote {
        mark,
        bid,
        ask,
        delta: greeks.delta,
        gamma: greeks.gamma,
        vega: greeks.vega,
        theta: greeks.theta,
        mark_iv: implied_vol(mark, cell, model),
        bid_iv: implied_vol(bid, cell, model),
        ask_iv: implied_vol(ask, cell, model),
        // The vault does not trade at the model's ask: it charges the mid plus its
        // margin and pays the mid minus its haircut. Those are the prices on the
        // screen, so those are the prices whose vol the columns should name.
        entry_iv: implied_vol(unit_price(entry_units, quantity_units, scale), cell, model),
        exit_iv: implied_vol(unit_price(exit_units, quantity_units, scale), cell, model),
        mid_units,
        entry_units,
        exit_units,
        venue_cut_units,
        lp_notional_units,
        width_units,
        max_payout_units,
        quantity_units,
    })
}

/// Whether the ladder's quote for a series has to be re-sent.
pub fn needs_requote(last_mark: Option<f64>, last_ms: u64, mark: f64, now_ms: u64) -> bool {
    let threshold = |reference: f64| reference.abs() * REQUOTE_SHARE_OF_MARK + REQUOTE_NOISE;
    match last_mark {
        None => true,
        Some(last) => (mark - last).abs() >= threshold(last) || now_ms.saturating_sub(last_ms) >= REQUOTE_GRACE_MS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALE: u128 = 1_000_000;
    /// Three contracts, the size the venue quotes at.
    const THREE: u128 = 3 * SCALE;

    /// The old positional shape, kept only so a test can say "this input is
    /// refused" without repeating two constructor calls in every assertion.
    #[allow(clippy::too_many_arguments)]
    fn priced(
        spot: f64,
        low: f64,
        high: f64,
        kind: Kind,
        vol: f64,
        years: f64,
        scale: u128,
        quantity: u128,
        terms: &Terms,
    ) -> Option<Quote> {
        let cell = Cell::new(spot, low, high, kind)?;
        let model = Model::new(vol, years)?;
        quote(&cell, &model, scale, quantity, terms)
    }

    fn solved_vol(price: f64, spot: f64, low: f64, high: f64, kind: Kind, years: f64) -> Option<f64> {
        let cell = Cell::new(spot, low, high, kind)?;
        let model = Model::new(0.6, years)?;
        implied_vol(price, &cell, &model)
    }

    fn eth_quote() -> Quote {
        priced(
            2350.0,
            2300.0,
            2400.0,
            Kind::Call,
            0.6,
            7.0 / 365.0,
            SCALE,
            THREE,
            &Terms::STANDARD,
        )
        .expect("a sane market quotes")
    }

    #[test]
    fn the_ladder_is_two_sided_around_the_mark() {
        let q = eth_quote();
        assert!(q.mark > 0.0, "a 2300/2400 call across the money has value");
        assert!(q.ask > q.mark, "{:?}", (q.mark, q.ask));
        assert!(q.bid < q.mark);
        assert!((q.ask - q.mark - half_spread(q.mark)).abs() < 1e-12);
        assert!(q.bid > 0.0);
        assert!(q.delta > 0.0 && q.delta < 1.0, "call spread delta: {}", q.delta);
    }

    #[test]
    fn a_wing_pinned_by_the_index_has_no_implied_vol() {
        // The rung the live screen printed as 289.8%: 2175/2275 calls with the
        // index at 2338 and 39 minutes left. The price barely answers to vol, so
        // a solve lands wherever its bracket happens to end.
        let q = priced(
            2_338.0,
            2_175.0,
            2_275.0,
            Kind::Call,
            0.6,
            39.0 / 60.0 / 24.0 / 365.0,
            SCALE,
            SCALE,
            &Terms::STANDARD,
        )
        .expect("a pinned spread still has an intrinsic value");
        assert!(q.vega.abs() < MIN_VEGA_FOR_IV, "pinned: vega {}", q.vega);
        assert_eq!(q.mark_iv, None, "a pinned price has no vol in it to read");
        assert_eq!(q.bid_iv, None);
    }

    #[test]
    fn a_quote_carries_the_desk_s_risk_table() {
        // An out-of-the-money put spread: long the nearer strike, short the
        // further one, so vega is positive and time costs money.
        let q = priced(
            2500.0,
            2200.0,
            2300.0,
            Kind::Put,
            0.6,
            7.0 / 365.0,
            SCALE,
            SCALE,
            &Terms::STANDARD,
        )
        .expect("an otm put spread prices");
        assert!(q.vega > 0.0, "vega: {}", q.vega);
        assert!(q.theta < 0.0, "theta: {}", q.theta);
        assert!(q.gamma.is_finite() && q.delta < 0.0, "{:?}", (q.gamma, q.delta));
    }

    #[test]
    fn the_venue_charge_is_inside_the_entry_and_adds_up() {
        let q = eth_quote();
        assert!(q.entry_units > q.mid_units * THREE / SCALE, "loaded above the mark");
        assert_eq!(
            q.venue_cut_units + q.lp_notional_units,
            q.entry_units,
            "the split is the premium, not an extra line"
        );
        // 400 bps of margin on the price is 400/10400 = 3.85% of the premium:
        // the fee the trader sees and the share the venue keeps are not the same
        // number, and the UI must quote the second one.
        assert!((q.effective_margin_bps() - 384.6).abs() < 1.0, "{:?}", q.effective_margin_bps());
    }

    #[test]
    fn a_worse_mark_never_costs_more_than_the_ticket_can_pay() {
        // The entry ceiling is the whole reason a pool can underwrite this: the
        // premium cannot exceed a worst case the arithmetic already knows.
        let q = eth_quote();
        assert!(q.entry_units <= q.max_payout_units, "{:?}", (q.entry_units, q.max_payout_units));
        assert_eq!(q.max_payout_units, 3 * 100 * SCALE);
        assert!(q.exit_units <= q.entry_units, "closing early cannot be free money");
    }

    #[test]
    fn a_deep_wing_still_quotes_a_spread() {
        // 4% of a 0.004 premium is nothing; the floor is what keeps the ladder
        // two-sided instead of printing 0.00 / 0.00.
        let q = priced(2350.0, 4000.0, 4100.0, Kind::Call, 0.6, 7.0 / 365.0, SCALE, THREE, &Terms::STANDARD)
            .expect("far OTM still lists");
        assert!(q.mark < 0.001, "the wing is nearly worthless: {}", q.mark);
        // The floor is on the half-spread, not on the pair: a bid that cannot be
        // zero eats part of the width rather than printing a 0.00 quote.
        assert!((q.ask - q.mark - MIN_HALF_SPREAD).abs() < 1e-12, "{:?}", (q.mark, q.ask));
        assert_eq!(q.bid, MIN_BID, "{:?}", q.bid);
        assert!(q.ask > q.bid && q.bid > 0.0, "{:?}", (q.bid, q.ask));
    }

    #[test]
    fn expired_and_nonsense_inputs_quote_nothing() {
        let flat = Terms::STANDARD;
        assert!(priced(2350.0, 2300.0, 2400.0, Kind::Call, 0.6, 0.0, SCALE, THREE, &flat).is_some(),
            "at expiry a spread still has an intrinsic value to quote");
        assert!(priced(0.0, 2300.0, 2400.0, Kind::Call, 0.6, 0.01, SCALE, THREE, &flat).is_none(), "no index");
        assert!(priced(2350.0, 2400.0, 2300.0, Kind::Call, 0.6, 0.01, SCALE, THREE, &flat).is_none(), "inverted legs");
        assert!(priced(2350.0, 2300.0, 2400.0, Kind::Call, 0.6, 0.01, SCALE, 0, &flat).is_none(), "no size");
        assert!(priced(2350.0, -25.0, 2400.0, Kind::Call, 0.6, 0.01, SCALE, THREE, &flat).is_none(), "negative leg");
    }

    #[test]
    fn implied_vol_round_trips_through_the_model() {
        // An out-of-the-money wing: more vol has to mean more value.
        let (spot, low, high, years, vol) = (2350.0, 2600.0, 2700.0, 7.0 / 365.0, 0.6);
        let price = pricing::spread_value(spot, low, high, Kind::Call, vol, years, 0.0);
        let solved = solved_vol(price, spot, low, high, Kind::Call, years).expect("solvable");
        assert!((solved - vol).abs() < 1e-3, "{solved} vs {vol}");
        let richer = solved_vol(price * 1.5, spot, low, high, Kind::Call, years).expect("solvable");
        assert!(richer > solved, "a dearer wing is a higher vol, not a lower one");
        // A near-money spread loses value as vol rises, and still has to solve.
        let mid = (2350.0, 2300.0, 2400.0);
        let at_the_money = pricing::spread_value(mid.0, mid.1, mid.2, Kind::Call, vol, years, 0.0);
        let flat_vol = pricing::spread_value(mid.0, mid.1, mid.2, Kind::Call, 1.5, years, 0.0);
        assert!(flat_vol < at_the_money, "the decreasing case is real, not a rounding");
        let solved_mid = solved_vol(at_the_money, mid.0, mid.1, mid.2, Kind::Call, years).expect("solvable");
        assert!((solved_mid - vol).abs() < 1e-3, "{solved_mid} vs {vol}");
    }

    #[test]
    fn implied_vol_refuses_the_impossible() {
        let (spot, low, high) = (2350.0, 2300.0, 2400.0);
        assert_eq!(solved_vol(0.0, spot, low, high, Kind::Call, 0.02), None);
        assert_eq!(solved_vol(100.0, spot, low, high, Kind::Call, 0.02), None, "above the width");
        assert_eq!(solved_vol(1.0, spot, low, high, Kind::Call, 0.0), None, "expired");
        assert_eq!(solved_vol(1.0, spot, high, low, Kind::Call, 0.02), None, "inverted");
    }

    #[test]
    fn the_iv_columns_are_solvable_on_a_near_money_spread() {
        let q = eth_quote();
        let bid = q.bid_iv.expect("the bid is a price the model explains");
        let mark = q.mark_iv.expect("the mark solves");
        // The straddled cell is worth most at zero vol, so a cheaper spread means
        // *less* vol, not more: the column runs the other way from a wing.
        assert!(bid > mark, "{:?}", (bid, mark));
        assert!(mark > 0.03 && mark < 3.0, "inside the bracket: {mark}");
        // The ask is the mark plus the venue's edge. Above the mark there is no
        // volatility that reproduces it, so the column must say "—" rather than
        // print the end of the search bracket, which is what the old solver did.
        assert_eq!(q.ask_iv, None, "{:?}", (q.mark, q.ask, q.ask_iv));
    }

    #[test]
    fn the_iv_columns_name_the_price_they_sit_beside() {
        // A wing the model can solve: the entry is dearer than the exit, and for a
        // call that means a higher vol — so the two columns must differ, and the
        // dearer one must be the one on the ask side.
        let wing = priced(
            2350.0,
            2600.0,
            2700.0,
            Kind::Call,
            0.6,
            7.0 / 365.0,
            SCALE,
            THREE,
            &Terms::STANDARD,
        )
        .expect("a wing quotes");
        let entry = wing.entry_iv.expect("the entry price solves");
        let exit = wing.exit_iv.expect("the exit price solves");
        assert!(entry > exit, "the loaded entry is dearer, so its vol is higher: {entry} vs {exit}");
        assert!((entry - wing.ask_iv.unwrap()).abs() > 1e-6, "and it is not the model's ask either");

        // The ticket's size changes the integer premium, not the price per
        // contract, so it cannot change the vol a column reports.
        let five = priced(
            2350.0,
            2600.0,
            2700.0,
            Kind::Call,
            0.6,
            7.0 / 365.0,
            SCALE,
            5 * THREE,
            &Terms::STANDARD,
        )
        .expect("a wing quotes");
        assert!((five.entry_iv.unwrap() - entry).abs() < 1e-9, "five tickets, one price");
    }

    #[test]
    fn requoting_follows_the_index_not_the_clock() {
        assert!(needs_requote(None, 0, 0.5, 100), "first quote always goes");
        assert!(!needs_requote(Some(0.5), 0, 0.5001, 100), "a tenth of a basis point is noise");
        assert!(needs_requote(Some(0.5), 0, 0.51, 100), "2% of the premium is a market move");
        assert!(needs_requote(Some(0.5), 0, 0.50001, REQUOTE_GRACE_MS + 1), "stale gets re-sent anyway");
        assert!(
            needs_requote(Some(0.0), 0, 0.0001, 100),
            "a worthless mark still moves the quote by 0.0001, its own noise floor"
        );
    }

    #[test]
    fn a_flat_fee_schedule_prices_the_mark_and_nothing_more() {
        let loaded = priced(2350.0, 2300.0, 2400.0, Kind::Call, 0.6, 7.0 / 365.0, SCALE, THREE, &Terms::STANDARD)
            .expect("loaded");
        let flat = priced(2350.0, 2300.0, 2400.0, Kind::Call, 0.6, 7.0 / 365.0, SCALE, THREE, &Terms::FLAT)
            .expect("flat");
        assert_eq!(flat.entry_units, flat.mid_units * THREE / SCALE, "no margin, no haircut");
        assert_eq!(flat.venue_cut_units, 0);
        assert_eq!(flat.exit_units, flat.entry_units, "exit at the same money as entry");
        assert!(loaded.entry_units > flat.entry_units, "the margin is the entire revenue line");
        assert_eq!(
            loaded.exit_units,
            flat.entry_units * 9 / 10,
            "a 1,000 bps impact haircut is exactly 10% off the same mid"
        );
    }

    #[test]
    fn puts_and_calls_on_the_same_rung_are_priced_on_their_own_legs() {
        let call = priced(2350.0, 2300.0, 2400.0, Kind::Call, 0.6, 7.0 / 365.0, SCALE, THREE, &Terms::STANDARD).unwrap();
        let put = priced(2350.0, 2300.0, 2400.0, Kind::Put, 0.6, 7.0 / 365.0, SCALE, THREE, &Terms::STANDARD).unwrap();
        // Parity, at r = 0: the two sides of one rung add up to the width. The
        // venue can price a call ladder and read the put ladder off it, and a
        // number that breaks this identity is a bug in the model, not a market.
        assert!((call.mark + put.mark - 100.0).abs() < 0.01, "{:?}", (call.mark, put.mark));
        assert!(call.delta > 0.0 && put.delta < 0.0, "{:?}", (call.delta, put.delta));
        assert!((call.delta + put.delta).abs() < 0.01, "and the two deltas cancel: {:?}", (call.delta, put.delta));
    }
}
