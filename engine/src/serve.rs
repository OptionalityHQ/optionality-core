//! One snapshot of the venue, in the exact shape the gateway reads.
//!
//! The sockets live in `main.rs`; everything that decides *what to say* is here,
//! because a sentence about what a trade costs is not a transport concern. The
//! engine is also the only place the pool's loaded price is written down: the
//! ladder's `ask` is what opening a contract costs and its `bid` is what closing
//! one pays, both computed by `core::fees` — the same code the contract runs —
//! so nothing between the screen and the chain can disagree about the number.

use std::collections::BTreeMap;

use bounded_core::fees::Terms;
use serde_json::{json, Value};
use surface::index::{self, Options, Print, Status, Summary};
use surface::pricing::Kind;

use crate::ladder::{self, Asset, Params, SeriesSpec};
use crate::quote;

/// How the venue is configured. `vol` is the model's input, not a promise: a
/// prototype quotes a flat ATM vol and skews it by strike.
#[derive(Clone, Copy, Debug)]
pub struct Venue {
    pub params: Params,
    pub terms: Terms,
    pub atm_vol: f64,
    pub stale: Options,
}

impl Default for Venue {
    fn default() -> Self {
        Venue {
            params: Params::default(),
            terms: Terms::STANDARD,
            atm_vol: 0.6,
            stale: Options::default(),
        }
    }
}

impl Venue {
    /// Time to expiry in years, never negative: a series past its expiry still
    /// has to be quotable while the print arrives.
    pub fn years_to(expiry_ns: u64, now_ns: u64) -> f64 {
        let remaining = expiry_ns.saturating_sub(now_ns) as f64;
        (remaining / 1e9) / surface::pricing::SECONDS_PER_YEAR
    }
}

/// One exchange print on its way in.
#[derive(Clone, Debug, PartialEq)]
pub struct Tick {
    pub source: String,
    pub asset: String,
    pub price: f64,
    pub ts_ms: u64,
}

/// Parse one JSON line from the feed shell. Anything malformed or irrelevant is
/// dropped without ceremony: a socket that emits garbage must not stop the tape.
pub fn parse_tick(line: &str) -> Option<Tick> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let source = value.get("source")?.as_str()?.to_string();
    let asset = value.get("asset")?.as_str()?.to_uppercase();
    let price = value.get("price")?.as_f64()?;
    let ts_ms = value
        .get("tsMs")
        .and_then(Value::as_u64)
        .or_else(|| value.get("ts").and_then(Value::as_f64).map(|t| t as u64))
        .unwrap_or(0);
    if !price.is_finite() || price <= 0.0 || asset.is_empty() {
        return None;
    }
    Some(Tick { source, asset, price, ts_ms })
}

/// Last prints per source per asset, with the count each one has delivered.
#[derive(Clone, Default, Debug)]
pub struct Tape {
    prints: BTreeMap<String, BTreeMap<String, Print>>,
    ticks: u64,
}

impl Tape {
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    pub fn ingest(&mut self, tick: &Tick) {
        let entry = self
            .prints
            .entry(tick.asset.clone())
            .or_default()
            .entry(tick.source.clone())
            .or_insert(Print { price: tick.price, ts_ms: tick.ts_ms, ticks: 0 });
        entry.ticks += 1;
        self.ticks += 1;
        // A print older than the one held is a late frame off a slow socket: it
        // counts as traffic but must not move the index backwards.
        if tick.ts_ms >= entry.ts_ms {
            entry.price = tick.price;
            entry.ts_ms = tick.ts_ms;
        }
    }

    /// The last print from every source for one asset.
    pub fn prints(&self, asset: Asset) -> Vec<(&str, Print)> {
        self.prints
            .get(asset.symbol())
            .map(|sources| sources.iter().map(|(name, print)| (name.as_str(), *print)).collect())
            .unwrap_or_default()
    }

    pub fn summary(&self, asset: Asset, now_ms: u64, options: &Options) -> Summary {
        index::summarise(&self.prints(asset), now_ms, options)
    }
}

/// The cell a ladder row shows for one kind. Per contract, in price points, plus
/// the integer units a trade would actually move.
fn side(
    spot: f64,
    spec: &SeriesSpec,
    vol: f64,
    years: f64,
    terms: &Terms,
) -> Option<Value> {
    let scale = spec.price_scale;
    let low = spec.low_units as f64 / scale as f64;
    let high = spec.high_units as f64 / scale as f64;
    let cell = quote::Cell::new(spot, low, high, spec.kind)?;
    let model = quote::Model::new(vol, years)?;
    let q = quote::quote(&cell, &model, scale, scale, terms)?;
    let points = |units: u128| units as f64 / scale as f64;
    // A wing worth less than one unit rounds to a mid of zero, and the contract
    // refuses `pool_buy` at a zero mid. So it is not a price of 0.00 — it is no
    // price at all, and saying 0 here would put a number on the screen that a
    // click cannot fill. Nor is a spread that costs more than it can ever pay
    // out a price: the loaded premium on a deep in-the-money rung passes its own
    // width, and a ticket bought there loses money at every print.
    let openable = q.mid_units > 0 && q.entry_units > 0 && q.entry_units < q.max_payout_units;
    // Opening and closing are not the same question. A spread whose loaded entry
    // costs more than it can ever pay is not for sale, but a ticket already held
    // still has an exit value, and hiding it would strand the holder with a
    // position the screen says cannot be sold.
    let exitable = q.mid_units > 0 && q.exit_units > 0;
    let money = |units: u128| json!(points(units));
    let shown = |units: u128, allowed: bool| if allowed { json!(units.to_string()) } else { Value::Null };
    Some(json!({
        "mark": q.mark,
        // The pool's own two-sided price: entry is loaded, exit is haircut.
        "ask": if openable { money(q.entry_units) } else { Value::Null },
        "bid": if exitable { money(q.exit_units) } else { Value::Null },
        "markIv": q.mark_iv,
        "bidIv": q.bid_iv,
        "askIv": q.ask_iv,
        // The vol at the two prices the vault actually charges. The ask column on
        // the screen shows the entry, so that is the number it carries; the exit is
        // what closes a held ticket, and it is not a bid anybody can write into.
        "entryIv": if openable { json!(q.entry_iv) } else { Value::Null },
        "exitIv": if exitable { json!(q.exit_iv) } else { Value::Null },
        "delta": q.delta,
        "gamma": q.gamma,
        "vega": q.vega,
        "theta": q.theta,
        "midUnits": if openable { q.mid_units.to_string() } else { "0".to_string() },
        "entryUnits": shown(q.entry_units, openable),
        "exitUnits": shown(q.exit_units, exitable),
        "venueCutUnits": shown(q.venue_cut_units, openable),
        "maxPayoutUnits": q.max_payout_units.to_string(),
        "openable": openable,
        "low": spec.low_units.to_string(),
        "high": spec.high_units.to_string(),
        "seriesLabel": spec.label(),
    }))
}

/// Group the plan by strike so one ladder row carries both its put and its call,
/// which is how the gateway addresses a cell.
pub fn chain_json(specs: &[SeriesSpec], spot: f64, vol: f64, years: f64, terms: &Terms) -> Vec<Value> {
    let mut rows: BTreeMap<u128, Value> = BTreeMap::new();
    for spec in specs {
        let Some(cell) = side(spot, spec, vol, years, terms) else {
            continue;
        };
        let key = spec.strike_units();
        let field = if spec.kind == Kind::Put { "put" } else { "call" };
        let row = rows.entry(key).or_insert_with(|| json!({ "strike": key as f64 / spec.price_scale as f64 }));
        row[field] = cell;
    }
    rows.into_values().collect()
}

/// `GET /state?asset=ETH` — the whole answer, index and chain together.
pub fn state_json(asset: Asset, summary: &Summary, specs: &[SeriesSpec], expiry_ns: u64, now_ms: u64, venue: &Venue) -> String {
    let years = Venue::years_to(expiry_ns, now_ms.saturating_mul(1_000_000));
    let body = json!({
        "asset": asset.symbol(),
        "status": summary.status.label(),
        "spot": summary.median,
        "confidenceBps": summary.confidence_bps,
        "spreadBps": summary.spread_bps,
        "sources": summary.sources,
        "freshSources": summary.fresh_sources,
        "ageMs": summary.age_ms,
        "expiryNs": expiry_ns.to_string(),
        "years": years,
        "atmVol": venue.atm_vol,
        "marginBps": venue.terms.margin_bps,
        "impactBps": venue.terms.impact_bps,
        "quotable": summary.status == Status::Live,
        "chain": chain_json(specs, summary.median.unwrap_or(0.0), venue.atm_vol, years, &venue.terms),
    });
    body.to_string()
}

/// `GET /plan?asset=ETH&expiry_ns=…` — the `create_series` arguments the operator
/// has to send for this ladder to exist on chain. Listing is the engine's
/// decision; signing it is the host's.
pub fn plan_json(asset: Asset, spot: f64, expiry_ns: u64, venue: &Venue) -> String {
    let specs = ladder::plan(asset, spot, expiry_ns, &venue.params);
    let body = json!({
        "asset": asset.symbol(),
        "spot": spot,
        "expiryNs": expiry_ns.to_string(),
        "count": specs.len(),
        "series": specs.iter().map(|spec| spec.create_series_args()).collect::<Vec<_>>(),
    });
    body.to_string()
}


/// `GET /smile?asset=ETH` — implied vol against strike, one curve per tenor.
///
/// These are the engine's own vols, from the same smile function that prices the
/// legs of every spread on the ladder: `σ(K) = σ_atm · (1 + skew · ln(S/K))`. The
/// smile built from *spread* prices is a different and much noisier object — a
/// spread has two strikes and its vol explodes at the wings — so the ladder's IV
/// column and this chart are answered by the same model rather than by inverting a
/// number that was never a single option's price.
pub fn smile_json(
    asset: Asset,
    spot: Option<f64>,
    atm_vol: f64,
    now_ns: u64,
    expiries: &[u64],
    tenors: usize,
) -> String {
    const YEAR_NS: f64 = 365.0 * 24.0 * 3_600.0 * 1e9;
    let Some(spot) = spot.filter(|value| *value > 0.0) else {
        return json!({ "asset": asset.symbol(), "spot": null, "atmVol": atm_vol, "expiries": [] })
            .to_string();
    };
    let curves: Vec<serde_json::Value> = expiries
        .iter()
        .copied()
        .filter(|expiry| *expiry > now_ns)
        .take(tenors)
        .map(|expiry| {
            let years = (expiry - now_ns) as f64 / YEAR_NS;
            let points: Vec<serde_json::Value> = (0..=SMILE_POINTS)
                .map(|step| {
                    let fraction = 0.85 + (0.30 * step as f64 / SMILE_POINTS as f64);
                    let strike = spot * fraction;
                    let vol = surface::pricing::smile_vol(spot, strike, atm_vol, surface::pricing::DEFAULT_SKEW)
                        .max(0.01);
                    json!({ "strike": (strike * 100.0).round() / 100.0, "vol": vol })
                })
                .collect();
            json!({ "expiryNs": expiry.to_string(), "years": years, "atmVol": atm_vol, "points": points })
        })
        .collect();
    json!({
        "asset": asset.symbol(),
        "spot": spot,
        "atmVol": atm_vol,
        // The centre of every curve, so a reader can see the skew is the same
        // function at every tenor and only the time to expiry differs.
        "skew": surface::pricing::DEFAULT_SKEW,
        "expiries": curves,
    })
    .to_string()
}

/// How many points a smile is drawn from. Enough to look like a curve, few enough
/// that the payload stays a chart rather than a series.
pub const SMILE_POINTS: usize = 24;

/// `GET /index?asset=ETH` — the print a reporter is allowed to put on chain.
///
/// The scaling happens here rather than in the caller because this is the last
/// place the price is still a number the venue derived: `price_units` is exactly
/// what `report` takes, and the contract's own band check runs against it. A
/// degraded or stale index answers nothing, so a stalled socket cannot produce a
/// settlement print that pays the wrong holder.
pub fn index_json(asset: Asset, summary: &Summary, now_ms: u64, scale: u128) -> String {
    let Some(observation) = summary.observation(now_ms) else {
        return json!({
            "asset": asset.symbol(),
            "status": summary.status.label(),
            "quotable": false,
            "reason": match summary.status {
                Status::Offline => "no prints from any venue",
                Status::Stale => "every print is older than the stale window",
                Status::Degraded => if summary.fresh_sources < 2 {
                    "one source is not an index"
                } else {
                    "the venues disagree past the degenerate spread"
                },
                Status::Live => "no median",
            },
        })
        .to_string();
    };
    json!({
        "asset": asset.symbol(),
        "status": summary.status.label(),
        // A degraded index is still a print. Whether the venue will *quote* on it
        // is a separate question, and answered by this flag.
        "quotable": summary.status == Status::Live,
        "price": observation.price,
        // Raw u128 for the oracle's `price`, which the SDK reads as a number.
        "priceUnits": (observation.price * scale as f64).round() as u128,
        "publishTimeNs": observation.publish_time_ns,
        "confidenceBps": observation.confidence_bps,
        "spreadBps": observation.spread_bps,
        "sources": observation.sources,
        "scale": scale,
    })
    .to_string()
}

/// `GET /feed` — the index with the venues that produced it.
///
/// The banner is the one place a visitor learns that the number is a median of
/// three exchanges rather than a price this app invented, so the engine names
/// the sources instead of only counting them. `/index` stays minimal: it answers
/// the oracle, not a screen.
pub fn feed_json(tape: &Tape, assets: &[Asset], now_ms: u64, options: &Options, sockets: u64, reconnects: u64) -> String {
    let mut entries = serde_json::Map::new();
    let mut any_fresh = false;
    let mut all_live = !assets.is_empty();
    for asset in assets {
        let prints = tape.prints(*asset);
        let summary = index::summarise(&prints, now_ms, options);
        all_live &= summary.status == Status::Live;
        any_fresh |= summary.fresh_sources > 0;
        entries.insert(
            asset.symbol().to_string(),
            json!({
                "status": summary.status.label(),
                "median": summary.median,
                "spreadBps": summary.spread_bps,
                "confidenceBps": summary.confidence_bps,
                "ageMs": summary.age_ms,
                "freshSources": summary.fresh_sources,
                "sources": index::sources(&prints, now_ms, options)
                    .into_iter()
                    .map(|source| json!({
                        "name": source.name,
                        "price": source.price,
                        "ageMs": source.age_ms,
                        "ticks": source.ticks,
                        "fresh": source.fresh,
                    }))
                    .collect::<Vec<_>>(),
            }),
        );
    }
    json!({
        "status": if all_live { "live" } else if any_fresh { "degraded" } else { "offline" },
        "connectedSources": sockets,
        "ticks": tape.ticks(),
        "reconnects": reconnects,
        "assets": Value::Object(entries),
    })
    .to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Health,
    State(Asset, Option<u64>),
    Smile(Asset),
    Plan(Asset, u64),
    Index(Asset, u128),
    Feed,
    Unknown,
}

/// Where a request path goes. The asset defaults to ETH so a bare `/state` is
/// still a page, and an unparseable expiry is refused rather than guessed at.
pub fn route(path: &str) -> Route {
    let (head, query) = match path.split_once('?') {
        Some(pair) => pair,
        None => (path, ""),
    };
    let params = |query: &str| -> BTreeMap<String, String> {
        query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>()
    };
    let found = params(query);
    let asset = found
        .get("asset")
        .and_then(|value| Asset::parse(value))
        .unwrap_or(Asset::Eth);
    match head {
        "/health" => Route::Health,
        // A state read names the expiry it is about; without one it is the
        // nearest the venue lists, which is the ladder a screen opens on.
        "/state" => Route::State(
            asset,
            found.get("expiry_ns").and_then(|value| value.parse::<u64>().ok()),
        ),
        "/smile" => Route::Smile(asset),
        "/plan" => match found.get("expiry_ns").and_then(|value| value.parse::<u64>().ok()) {
            Some(expiry_ns) => Route::Plan(asset, expiry_ns),
            None => Route::Unknown,
        },
        "/index" => {
            let scale = found
                .get("scale")
                .and_then(|value| value.parse::<u128>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(ladder::SCALE);
            Route::Index(asset, scale)
        }
        "/feed" => Route::Feed,
        _ => Route::Unknown,
    }
}

/// A one-line status for the terminal the engine runs in, so an operator can see
/// whether the tape is the problem or the model is.
pub fn status_line(asset: Asset, summary: &Summary, listed: usize, feed: &str) -> String {
    format!(
        "{} {} spot={} conf={:.0}bps sources={}/{} listed={} {}",
        asset.symbol(),
        summary.status.label(),
        summary
            .median
            .map(|value| format!("{value:.2}"))
            .unwrap_or_else(|| "—".to_string()),
        summary.confidence_bps,
        summary.fresh_sources,
        summary.sources,
        listed,
        feed
    )
}

/// The expiries still ahead, nearest first.
///
/// A ladder that has run off is no longer a market: the operator stops listing
/// it, the screen stops defaulting to it, and its tickets are settled or voided
/// by the same clock. The full schedule comes back only when every expiry has
/// passed, so a stopped venue still answers with the last ladder it had.
pub fn upcoming(expiries: &[u64], now_ns: u64) -> Vec<u64> {
    let live: Vec<u64> = expiries.iter().copied().filter(|ns| *ns > now_ns).collect();
    if live.is_empty() {
        expiries.to_vec()
    } else {
        live
    }
}

/// The expiries this venue lists.
///
/// Two inside the day, so there is always something hours away, then a daily
/// close at 08:00 UTC out to a week. Five of the seven land inside three days,
/// which is where a spread actually trades; the last two are there so the strip
/// reads like a venue rather than like a single market.
/// The ladders the venue lists: the near tenor, `hours` whole-hour markets, a
/// half-day, and a daily close out to a week.
///
/// `hours` is a knob rather than a constant because every extra ladder is another
/// quote the operator has to renew inside the contract's sixty seconds, and a
/// sandbox clock that runs faster than the wall clock turns that sixty seconds into
/// about twenty. Zero is a perfectly good schedule; two is a better demo.
pub fn schedule(now_ns: u64, near_ns: u64, hours: u64) -> Vec<u64> {
    const HOUR: u64 = 3_600_000_000_000;
    const DAY: u64 = 24 * HOUR;
    let near = near_ns.max(HOUR / 60);
    let mut expiries = vec![now_ns + near, now_ns + near.max(12 * HOUR)];
    // The whole hours. A trader watching an intraday market wants the next few
    // hours on the strip, not one tenor and then a week. These sit in front of the
    // daily closes and behind the near tenor, which is the ladder the screen opens
    // on.
    let next_hour = (now_ns / HOUR + 1) * HOUR;
    for step in 0..hours {
        let expiry = next_hour + step * HOUR;
        if expiry > now_ns + near {
            expiries.push(expiry);
        }
    }
    // The daily close: 00:00 UTC (12:00 AM UTC+0), the hour a listed daily option expires.
    let close = (now_ns / DAY) * DAY;
    for day in [1u64, 2, 3, 5, 7] {
        let mut expiry = close + day * DAY;
        while expiry <= now_ns {
            expiry += DAY;
        }
        expiries.push(expiry);
    }
    expiries.sort_unstable();
    expiries.dedup();
    expiries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ladder::SCALE;

    fn venue() -> Venue {
        Venue::default()
    }

    fn tick(source: &str, asset: &str, price: f64, ts_ms: u64) -> Tick {
        Tick { source: source.to_string(), asset: asset.to_string(), price, ts_ms }
    }

    #[test]
    fn an_expiry_that_has_passed_is_no_longer_offered() {
        let hour = 3_600_000_000_000u64;
        let now = 1_000 * hour;
        let all = vec![now - hour, now + hour, now + 3 * hour];
        assert_eq!(upcoming(&all, now), vec![now + hour, now + 3 * hour]);
        // Every expiry past: the schedule still answers, so a stopped venue
        // reports the last ladder rather than an empty one.
        assert_eq!(upcoming(&all, now + 4 * hour), all);
    }

    #[test]
    fn the_smile_is_the_engines_own_function_at_every_tenor() {
        let hour = 3_600_000_000_000u64;
        let now = 1_793_280_000 * 1_000_000_000;
        let expiries = vec![now + hour, now + 12 * hour, now + 30 * hour, now + 60 * hour];
        let body: serde_json::Value =
            serde_json::from_str(&smile_json(Asset::Eth, Some(2_300.0), 0.6, now, &expiries, 3))
                .expect("a smile");
        assert_eq!(body["spot"], 2300.0);
        let curves = body["expiries"].as_array().expect("curves");
        assert_eq!(curves.len(), 3, "three tenors, the fourth is not drawn");
        assert!(curves[0]["expiryNs"].as_str().unwrap().parse::<u64>().unwrap() < now + 12 * hour);
        let points = curves[0]["points"].as_array().expect("points");
        assert_eq!(points.len(), SMILE_POINTS + 1, "an even sweep of strikes");
        let strikes: Vec<f64> = points.iter().map(|p| p["strike"].as_f64().unwrap()).collect();
        assert!(strikes.windows(2).all(|pair| pair[0] < pair[1]), "strike ascending");
        // At the money the smile is the model's atm vol, by definition.
        let at_index = points
            .iter()
            .min_by(|a, b| {
                (a["strike"].as_f64().unwrap() - 2_300.0)
                    .abs()
                    .partial_cmp(&(b["strike"].as_f64().unwrap() - 2_300.0).abs())
                    .unwrap()
            })
            .unwrap();
        assert!((at_index["vol"].as_f64().unwrap() - 0.6).abs() < 0.02);
        // And it skews the way the model does: lower strikes carry more vol.
        assert!(points.first().unwrap()["vol"].as_f64().unwrap() > points.last().unwrap()["vol"].as_f64().unwrap());
        // No index, no curve: a smile with no money in it is not a chart.
        let empty: serde_json::Value =
            serde_json::from_str(&smile_json(Asset::Eth, None, 0.6, now, &expiries, 3)).unwrap();
        assert_eq!(empty["expiries"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn the_intraday_grid_sits_in_front_of_the_daily_closes() {
        let hour = 3_600_000_000_000u64;
        let day = 24 * hour;
        let now = 1_793_251_200 * 1_000_000_000; // 00:00 UTC (12:00 AM UTC+0)
        let expiries = schedule(now, 30 * 60 * 1_000_000_000, 2);
        assert_eq!(expiries[0], now + hour / 2, "the tenor asked for comes first");
        let hours: Vec<u64> = expiries.iter().copied().filter(|ns| ns % hour == 0).take(2).collect();
        assert_eq!(hours.len(), 2, "two whole hours behind it: {expiries:?}");
        assert!(hours.windows(2).all(|pair| pair[1] - pair[0] == hour), "consecutive");
        assert!(hours[0] > now + hour / 2, "and all of them after the near tenor");
        let closes = expiries.iter().filter(|ns| **ns % day == 0).count();
        assert_eq!(closes, 5, "five 00:00 UTC closes are still listed behind them");
        assert!(expiries.windows(2).all(|pair| pair[0] < pair[1]));

        // An hour is added even when the near tenor already lands on it: two ways
        // to reach the same instant is one ladder, not two.
        let on_the_hour = schedule(now, hour, 2);
        assert_eq!(on_the_hour.iter().filter(|ns| **ns == now + hour).count(), 1);
        assert_eq!(on_the_hour[0], now + hour, "and the near tenor is that ladder");
    }

    #[test]
    fn the_venue_lists_a_week_with_most_of_it_inside_three_days() {
        let hour = 3_600_000_000_000u64;
        let now = 1_793_251_200 * 1_000_000_000; // 00:00 UTC (12:00 AM UTC+0), the daily close
        let expiries = schedule(now, 4 * hour, 2);
        assert_eq!(expiries.len(), 7, "{expiries:?}");
        assert!(expiries.windows(2).all(|pair| pair[0] < pair[1]), "sorted and distinct");
        assert_eq!(expiries[0], now + 4 * hour, "the near tenor is the one asked for");
        let inside_three_days = expiries.iter().filter(|ns| **ns <= now + 3 * 24 * hour).count();
        assert_eq!(inside_three_days, 5, "most of the ladder is inside three days");
        // The last close is 00:00 UTC on day seven, which from a mid-afternoon
        // start is six and a half days out rather than exactly seven.
        let furthest = *expiries.last().unwrap();
        assert!(furthest >= now + 6 * 24 * hour, "and it reaches into next week");

        // A near tenor shorter than an hour keeps its own slot and the whole hours
        // follow it: the settle demo runs a three-minute expiry, not a day, and a
        // trader who wants the next few hours gets them.
        let short = schedule(now, 3 * 60 * 1_000_000_000, 2);
        assert_eq!(short[0], now + 3 * 60 * 1_000_000_000);
        let hours: Vec<u64> = short.iter().copied().filter(|ns| ns % hour == 0).take(2).collect();
        assert_eq!(hours.len(), 2, "two whole hours on the strip: {short:?}");
        assert!(
            hours.windows(2).all(|pair| pair[1] - pair[0] == hour),
            "and they are the next two, not scattered: {hours:?}"
        );
        assert!(short.contains(&(now + 12 * hour)), "and the half-day tenor is still there");

        // A minute is the floor: nothing is listed with an expiry already gone
        // by the time the operator reads it.
        assert!(schedule(now, 0, 2)[0] > now);
    }

    #[test]
    fn parses_the_feed_shells_own_line_format() {
        let line = r#"{"source":"binance","asset":"ETH","price":2350.25,"tsMs":1730000000000}"#;
        let parsed = parse_tick(line).expect("a print");
        assert_eq!(parsed.source, "binance");
        assert_eq!(parsed.asset, "ETH");
        assert_eq!(parsed.price, 2350.25);
        assert_eq!(parsed.ts_ms, 1_730_000_000_000);
    }

    #[test]
    fn garbage_and_irrelevant_lines_are_dropped_not_stored() {
        assert_eq!(parse_tick(""), None);
        assert_eq!(parse_tick("not json"), None);
        assert_eq!(parse_tick("{}"), None);
        assert_eq!(parse_tick(r#"{"source":"x","asset":"ETH","price":0,"tsMs":1}"#), None);
        assert_eq!(parse_tick(r#"{"source":"x","asset":"ETH","price":"2350","tsMs":1}"#), None, "a string price");
        // An uppercase asset is the same asset; a lowercase one is what the shell
        // prints, so both have to land on the same tape.
        assert_eq!(parse_tick(r#"{"source":"x","asset":"eth","price":2350.0,"tsMs":1}"#).unwrap().asset, "ETH");
        // A missing timestamp is not a reason to lose a price.
        assert_eq!(parse_tick(r#"{"source":"x","asset":"eth","price":2350.0}"#).unwrap().ts_ms, 0);
    }

    #[test]
    fn a_tape_holds_one_print_per_source_and_counts_them() {
        let mut tape = Tape::default();
        tape.ingest(&tick("binance", "ETH", 2350.0, 1000));
        tape.ingest(&tick("binance", "ETH", 2351.0, 2000));
        tape.ingest(&tick("coinbase", "ETH", 2352.0, 1500));
        tape.ingest(&tick("binance", "BTC", 60_000.0, 1500));
        assert_eq!(tape.ticks(), 4);
        let summary = tape.summary(Asset::Eth, 2000, &Options::default());
        assert_eq!(summary.sources, 2);
        assert_eq!(summary.median, Some(2351.5), "the newest of two sources");
        assert_eq!(tape.summary(Asset::Btc, 2000, &Options::default()).sources, 1);
        // A late frame from a slow socket is traffic, not a price.
        tape.ingest(&tick("binance", "ETH", 999.0, 500));
        assert_eq!(tape.summary(Asset::Eth, 2000, &Options::default()).median, Some(2351.5));
        assert_eq!(tape.ticks(), 5, "but it is still counted as received");
    }

    #[test]
    fn a_dead_tape_reports_offline_and_quotes_nothing() {
        let tape = Tape::default();
        let summary = tape.summary(Asset::Eth, 1000, &Options::default());
        assert_eq!(summary.status, Status::Offline);
        let body: Value =
            serde_json::from_str(&state_json(Asset::Eth, &summary, &[], 2_000_000_000_000, 1000, &venue())).unwrap();
        assert_eq!(body["status"], "offline");
        assert!(body["spot"].is_null());
        assert_eq!(body["quotable"], false);
        assert_eq!(body["chain"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_live_tape_lists_the_whole_ladder_with_both_sides() {
        let mut tape = Tape::default();
        for (source, price) in [("binance", 2350.0), ("coinbase", 2350.2), ("bybit", 2349.9)] {
            tape.ingest(&tick(source, "ETH", price, 100_000));
        }
        let summary = tape.summary(Asset::Eth, 100_000, &Options::default());
        assert_eq!(summary.status, Status::Live);
        let expiry_ns = 100_000_000_000_000 + 3_600_000_000_000;
        let specs = ladder::plan(Asset::Eth, 2350.0, expiry_ns, &Params::default());
        let text = state_json(Asset::Eth, &summary, &specs, expiry_ns, 100_000, &venue());
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["quotable"], true);
        assert_eq!(body["spot"], 2350.0);
        let rows = body["chain"].as_array().unwrap();
        assert_eq!(rows.len(), ladder::ladder(Asset::Eth, 2350.0, &Params::default()).len());
        let row = &rows[rows.len() / 2];
        assert!(row["put"].is_object() && row["call"].is_object(), "{row}");
        assert_eq!(row["put"]["high"], "2350000000", "the middle rung is the money");
        assert_eq!(row["call"]["low"], "2350000000");
    }

    #[test]
    fn the_ladder_shows_what_a_trade_actually_costs() {
        let expiry_ns = 3_600_000_000_000_000;
        let specs = ladder::plan(Asset::Eth, 2350.0, expiry_ns, &Params::default());
        let one = specs
            .iter()
            .find(|spec| spec.kind == Kind::Call && spec.low_units == 2350 * SCALE)
            .expect("an at-the-money call");
        let cell = side(2350.0, one, 0.6, Venue::years_to(expiry_ns, 0), &Terms::STANDARD).expect("a quote");
        let mark = cell["mark"].as_f64().unwrap();
        let ask = cell["ask"].as_f64().unwrap();
        let bid = cell["bid"].as_f64().unwrap();
        // Loaded entry above the mark, haircut exit below it: this is the pool's
        // price, not a model mid with a spread drawn around it.
        assert!(ask > mark, "{ask} <= {mark}");
        assert!(bid < mark, "{bid} >= {mark}");
        assert!((ask - mark - mark * 400.0 / 10_000.0).abs() < 0.01, "4% margin: {:?}", (ask, mark));
        assert!((mark - bid - mark * 1_000.0 / 10_000.0).abs() < 0.01, "10% impact: {:?}", (bid, mark));
        assert_eq!(cell["venueCutUnits"], (cell["entryUnits"].as_str().unwrap().parse::<u128>().unwrap() * 400 / 10400).to_string());
        assert_eq!(cell["maxPayoutUnits"], "100000000", "one contract of a 100-wide chain pays at most 100");
    }

    #[test]
    fn a_wing_worth_less_than_a_unit_is_not_offered_at_zero() {
        // Deep OTM at 0.6 vol over an hour: the mark rounds the mid to nothing,
        // and `pool_buy` would reject a zero mid on chain. The row has to read as
        // "no price", not as "free".
        let specs = ladder::plan(Asset::Eth, 2350.0, 3_600_000_000_000_000, &Params::default());
        let wing = specs
            .iter()
            .find(|spec| spec.kind == Kind::Call && spec.low_units == 2550 * SCALE)
            .expect("the top call");
        // One hour out, which is what the demo expiry is: 200 above the index at
        // 0.6 vol is worth less than one unit of the token.
        let hours = 1.0 / (365.0 * 24.0);
        let cell = side(2350.0, wing, 0.6, hours, &Terms::STANDARD).expect("the wing is still shown");
        assert_eq!(cell["openable"], json!(false), "mark {:?}", cell["mark"]);
        assert!(cell["ask"].is_null() && cell["entryUnits"].is_null(), "{cell}");
        assert!(cell["mark"].as_f64().unwrap() >= 0.0, "the mark is still reported");
        assert_eq!(cell["maxPayoutUnits"], "100000000", "and the worst case is still known");
        let money = side(2350.0, &specs[specs.len() / 2], 0.6, 0.11, &Terms::STANDARD).expect("a real cell");
        assert_eq!(money["openable"], json!(true));
        assert!(money["ask"].as_f64().unwrap() > 0.0, "{money}");
    }

    #[test]
    fn years_to_expiry_never_goes_negative() {
        assert!(Venue::years_to(1_000, 2_000) >= 0.0, "an expired series still quotes at intrinsic");
        assert!((Venue::years_to(365 * 86_400_000_000_000, 0) - 1.0).abs() < 1e-9);
        assert_eq!(Venue::years_to(0, 0), 0.0);
    }

    #[test]
    fn an_index_print_is_reportable_and_scaled() {
        let now = 100_000;
        let mut tape = Tape::default();
        for (source, price) in [("binance", 2350.0), ("coinbase", 2350.4), ("bybit", 2349.8)] {
            tape.ingest(&tick(source, "ETH", price, now - 200));
        }
        let summary = tape.summary(Asset::Eth, now, &Options::default());
        let body: Value = serde_json::from_str(&index_json(Asset::Eth, &summary, now, SCALE)).unwrap();
        assert_eq!(body["quotable"], true);
        assert_eq!(body["priceUnits"], 2_350_000_000u128.to_string().parse::<u64>().unwrap());
        assert_eq!(body["publishTimeNs"], 100_000_000_000u64, "the report must advance time");
        assert!(body["confidenceBps"].as_u64().unwrap() >= 3_000, "the floor the series will accept");
        assert_eq!(body["sources"], 3);
    }

    #[test]
    fn a_degraded_index_answers_with_a_reason_and_no_price() {
        // The difference between these two is the difference between a venue that
        // is quiet and a venue that is lying, and a reporter must not guess.
        let body: Value =
            serde_json::from_str(&index_json(Asset::Eth, &Summary {
                status: Status::Degraded,
                median: Some(2350.0),
                spread_bps: Some(40.0),
                confidence_bps: 100.0,
                age_ms: Some(10),
                fresh_sources: 1,
                sources: 3,
            }, 100_000, SCALE)).unwrap();
        assert_eq!(body["quotable"], false);
        assert!(body["price"].is_null(), "no price may be reported");
        assert_eq!(body["reason"], "one source is not an index");

        let offline: Value = serde_json::from_str(&index_json(
            Asset::Eth,
            &tape_empty().summary(Asset::Eth, 100_000, &Options::default()),
            100_000,
            SCALE,
        ))
        .unwrap();
        assert_eq!(offline["reason"], "no prints from any venue");
    }

    #[test]
    fn the_feed_names_the_venues_behind_the_index() {
        let now = 100_000;
        let mut tape = Tape::default();
        for (source, price) in [("binance", 2350.0), ("coinbase", 2350.4), ("bybit", 2349.8)] {
            tape.ingest(&tick(source, "ETH", price, now - 200));
        }
        tape.ingest(&tick("binance", "BTC", 60_000.0, now - 200));
        let body: Value = serde_json::from_str(&feed_json(
            &tape,
            &[Asset::Eth, Asset::Btc],
            now,
            &Options::default(),
            3,
            7,
        ))
        .unwrap();
        assert_eq!(body["status"], "degraded", "one thin asset degrades the whole banner");
        assert_eq!(body["assets"]["ETH"]["status"], "live", "and says which one it is");
        assert_eq!(body["connectedSources"], 3);
        assert_eq!(body["reconnects"], 7);
        assert_eq!(body["ticks"], 4);
        let eth = &body["assets"]["ETH"];
        assert_eq!(eth["freshSources"], 3);
        let names = eth["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|source| source["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["binance", "bybit", "coinbase"], "sorted, so two runs look alike");
        // One source is a number, not an index: the screen has to say so.
        assert_eq!(body["assets"]["BTC"]["status"], "degraded");

        let quiet: Value =
            serde_json::from_str(&feed_json(&tape_empty(), &[Asset::Eth], now, &Options::default(), 0, 0)).unwrap();
        assert_eq!(quiet["status"], "offline");
        assert!(quiet["assets"]["ETH"]["median"].is_null());
    }

    fn tape_empty() -> Tape {
        Tape::default()
    }

    #[test]
    fn an_expired_series_is_still_listed_and_priced() {
        // The venue must keep showing a series between expiry and the print, or
        // the trader sees a row disappear on them.
        let specs = ladder::plan(Asset::Eth, 2350.0, 1_000_000_000, &Params::default());
        let in_the_money = specs
            .iter()
            .find(|spec| spec.kind == Kind::Call && spec.low_units == 2350 * SCALE)
            .expect("the money call");
        let cell = side(2360.0, in_the_money, 0.6, 0.0, &Terms::STANDARD).expect("priced at intrinsic");
        assert!((cell["mark"].as_f64().unwrap() - 10.0).abs() < 1e-9, "{:?}", cell["mark"]);
        assert_eq!(cell["markIv"], Value::Null, "nothing to solve for at expiry");
    }

    #[test]
    fn a_spread_costing_more_than_it_can_pay_is_not_openable() {
        // The loaded premium on a deep in-the-money rung passes the ticket's own
        // width, so buying it loses money at every print. A venue that shows the
        // ask is inviting the trade, which is not what a quote is for.
        let specs = ladder::plan(Asset::Eth, 2_338.0, 2_000_000_000_000_000_000, &Params::default());
        let pinned = specs
            .iter()
            .find(|spec| spec.kind == Kind::Call && spec.low_units == 2_175 * SCALE)
            .expect("the in-the-money call rung");
        let years = 39.0 / 60.0 / 24.0 / 365.0;
        let cell = side(2_338.0, pinned, 0.6, years, &Terms::STANDARD).expect("priced");
        assert_eq!(cell["openable"], json!(false), "{:?}", cell["openable"]);
        assert!(cell["ask"].is_null(), "no ask is offered for a certain loss");
        assert_eq!(cell["midUnits"], json!("0"), "and the entry arithmetic is withheld with it");
        assert!(
            cell["exitUnits"].as_str().is_some(),
            "a ticket already held still has an exit value: the close is not gated on the open"
        );
        assert!(cell["bid"].is_number(), "and the screen can show it");
        assert!(cell["mark"].as_f64().unwrap() > 99.0, "the mark itself stays honest");
    }

    #[test]
    fn routes_name_the_page_and_the_asset() {
        assert_eq!(route("/health"), Route::Health);
        assert_eq!(route("/state"), Route::State(Asset::Eth, None));
        assert_eq!(route("/state?asset=BTC"), Route::State(Asset::Btc, None));
        assert_eq!(
            route("/state?asset=btc&expiry_ns=1"),
            Route::State(Asset::Btc, Some(1)),
            "a state read can name the expiry it is about"
        );
        assert_eq!(route("/plan?asset=ETH&expiry_ns=12345"), Route::Plan(Asset::Eth, 12345));
        assert_eq!(route("/plan"), Route::Unknown, "a plan without an expiry is not a request");
        assert_eq!(route("/index?asset=BTC"), Route::Index(Asset::Btc, SCALE));
        assert_eq!(route("/index"), Route::Index(Asset::Eth, SCALE));
        assert_eq!(route("/index?scale=0"), Route::Index(Asset::Eth, SCALE), "zero scale is a bug, not a unit");
        assert_eq!(route("/index?scale=100000000"), Route::Index(Asset::Eth, 100_000_000));
        assert_eq!(route("/plan?asset=ETH&expiry_ns=abc"), Route::Unknown);
        assert_eq!(route("/feed"), Route::Feed);
        assert_eq!(route("/settle"), Route::Unknown, "the engine never takes trades over HTTP");
    }

    #[test]
    fn a_plan_is_the_argument_list_for_every_series_listed() {
        let expiry_ns = 1_700_000_000_000_000_000;
        let body: Value = serde_json::from_str(&plan_json(Asset::Eth, 2350.0, expiry_ns, &venue())).unwrap();
        assert_eq!(body["asset"], "ETH");
        assert_eq!(body["count"].as_u64().unwrap() as usize, body["series"].as_array().unwrap().len());
        assert!(body["count"].as_u64().unwrap() > 20, "a full chain is listed");
        let first = &body["series"][0];
        assert!(first["low_strike"].is_string());
        assert_eq!(first["expiry_ns"].as_u64().unwrap() as u64, expiry_ns);
        assert_eq!(first["oracle_feed"], "ETH-USD");
        // Both kinds on the same strike appear, as two create_series calls.
        assert_eq!(body["series"].as_array().unwrap().iter().filter(|s| s["kind"] == "Put").count(),
                   body["series"].as_array().unwrap().iter().filter(|s| s["kind"] == "Call").count());
    }

    #[test]
    fn a_status_line_says_why_nothing_is_quoting() {
        let mut tape = Tape::default();
        tape.ingest(&tick("binance", "ETH", 2350.0, 0));
        let summary = tape.summary(Asset::Eth, 60_000, &Options::default());
        let line = status_line(Asset::Eth, &summary, 38, "tape=1 · sockets=2 · reconnects=3");
        assert!(line.contains("stale"), "{line}");
        assert!(line.contains("sources=0/1"), "{line}");
        assert!(line.contains("listed=38"), "{line}");
        assert!(line.contains("reconnects=3"), "the socket count belongs on this line: {line}");
    }
}
