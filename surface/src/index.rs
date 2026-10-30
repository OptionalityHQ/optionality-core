//! One price out of many venues, and the confidence the chain should give it.
//!
//! Ported from `sim/feed.mjs`, which stays only as the WebSocket shell: a
//! browser or Node socket delivers prints and nothing else. Every decision —
//! which prints count, what the index is, how much to trust it, whether a
//! settlement print is acceptable — is made here, in the same crate the engine
//! quotes from.
//!
//! The rule the whole module exists to enforce is that a single source is never
//! an index. One lagging exchange must not be able to fix a series.

pub const DEFAULT_STALE_MS: u64 = 15_000;
/// Above this spread across venues the composite is not an index, it is an
/// argument between sources.
pub const DEFAULT_DEGENERATE_SPREAD_BPS: f64 = 20.0;
/// Confidence lost per basis point of cross-venue spread.
pub const CONFIDENCE_PENALTY_PER_BP: f64 = 20.0;
/// A two-source agreement can never read as a high-confidence oracle.
pub const SINGLE_SOURCE_CAP_BPS: f64 = 6_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Two or more fresh sources inside the degenerate spread.
    Live,
    /// A number exists, but it is one source or a disagreement.
    Degraded,
    /// Nothing fresh: the newest print is older than the stale window.
    Stale,
    /// No prints at all.
    Offline,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Live => "live",
            Status::Degraded => "degraded",
            Status::Stale => "stale",
            Status::Offline => "offline",
        }
    }

    /// Only a live index may quote or fix. Degraded is shown to the trader and
    /// otherwise ignored, so a thin tape narrows the venue instead of misleading
    /// it.
    pub fn is_quotable(self) -> bool {
        self == Status::Live
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub stale_ms: u64,
    pub degenerate_spread_bps: f64,
    pub confidence_penalty_per_bp: f64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            stale_ms: DEFAULT_STALE_MS,
            degenerate_spread_bps: DEFAULT_DEGENERATE_SPREAD_BPS,
            confidence_penalty_per_bp: CONFIDENCE_PENALTY_PER_BP,
        }
    }
}

/// The newest print from one venue.
#[derive(Clone, Copy, Debug)]
pub struct Print {
    pub price: f64,
    pub ts_ms: u64,
    pub ticks: u32,
}

/// One venue's standing on the tape, for the source table the screen shows.
#[derive(Clone, Copy, Debug)]
pub struct Source<'a> {
    pub name: &'a str,
    pub price: f64,
    pub age_ms: u64,
    pub ticks: u32,
    pub fresh: bool,
}

/// The per-venue table, sorted by name so two runs of the same tape cannot look
/// different because a socket delivered its frames in another order.
pub fn sources<'a>(prints: &[(&'a str, Print)], now_ms: u64, options: &Options) -> Vec<Source<'a>> {
    let mut list: Vec<Source<'a>> = prints
        .iter()
        .map(|(name, print)| Source {
            name,
            price: print.price,
            age_ms: now_ms.saturating_sub(print.ts_ms),
            ticks: print.ticks,
            fresh: now_ms.saturating_sub(print.ts_ms) <= options.stale_ms,
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(b.name));
    list
}

#[derive(Clone, Copy, Debug)]
pub struct Summary {
    pub status: Status,
    pub median: Option<f64>,
    pub spread_bps: Option<f64>,
    pub confidence_bps: f64,
    /// Age of the newest print, fresh or not.
    pub age_ms: Option<u64>,
    pub fresh_sources: u8,
    pub sources: u8,
}

/// Median of the prints. An even count has no middle element, so the two
/// central prints are averaged: taking the upper one would hold the index on a
/// stale quote until the ordering itself changed.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        Some(sorted[mid])
    } else {
        Some((sorted[mid - 1] + sorted[mid]) / 2.0)
    }
}

/// Reduce per-venue last prints to one index. Any source older than the stale
/// window is dropped before the median, so one dead venue is a lost vote rather
/// than a wrong price.
pub fn summarise(prints: &[(&str, Print)], now_ms: u64, options: &Options) -> Summary {
    let table = sources(prints, now_ms, options);
    let newest = table.iter().map(|s| s.age_ms).min();
    let fresh: Vec<f64> = table.iter().filter(|s| s.fresh).map(|s| s.price).collect();

    if fresh.is_empty() {
        return Summary {
            status: if table.is_empty() { Status::Offline } else { Status::Stale },
            median: median(&table.iter().map(|s| s.price).collect::<Vec<_>>()),
            spread_bps: None,
            confidence_bps: 0.0,
            age_ms: newest,
            fresh_sources: 0,
            sources: table.len() as u8,
        };
    }

    let centre = median(&fresh).unwrap_or(fresh[0]);
    let high = fresh.iter().copied().fold(f64::MIN, f64::max);
    let low = fresh.iter().copied().fold(f64::MAX, f64::min);
    let spread_bps = if centre > 0.0 {
        (high - low) / centre * 10_000.0
    } else {
        0.0
    };
    let raw = (10_000.0 - spread_bps * options.confidence_penalty_per_bp).clamp(0.0, 10_000.0);
    let confidence_bps = if fresh.len() < 2 {
        raw.min(SINGLE_SOURCE_CAP_BPS)
    } else {
        raw
    };
    let degraded = fresh.len() < 2 || spread_bps > options.degenerate_spread_bps;

    Summary {
        status: if degraded { Status::Degraded } else { Status::Live },
        median: Some(centre),
        spread_bps: Some(spread_bps),
        confidence_bps,
        age_ms: newest,
        fresh_sources: fresh.len() as u8,
        sources: table.len() as u8,
    }
}

/// A print in the shape the contract's oracle callback expects.
#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub price: f64,
    pub publish_time_ns: u64,
    pub confidence_bps: u64,
    pub spread_bps: f64,
    pub sources: u8,
}

impl Summary {
    /// The observation the engine may submit for settlement.
    ///
    /// One source is not an index and a stale one is not a price, so neither can
    /// fix a series. Two sources that disagree *are* an index: the disagreement
    /// is exactly what `confidence_bps` measures, and the contract's per-series
    /// floor is where it gets refused. Withholding the print instead made a
    /// 25-basis-point minute unfixable even though the chain would have accepted
    /// the number at 95% confidence. Quoting stays live-only — that is a risk
    /// appetite, not a fact about the print.
    pub fn observation(&self, now_ms: u64) -> Option<Observation> {
        if self.fresh_sources < 2 || !matches!(self.status, Status::Live | Status::Degraded) {
            return None;
        }
        let price = self.median?;
        Some(Observation {
            price,
            publish_time_ns: now_ms.saturating_mul(1_000_000),
            confidence_bps: self.confidence_bps.round() as u64,
            spread_bps: self.spread_bps.unwrap_or(0.0),
            sources: self.fresh_sources,
        })
    }
}

/// The series' own acceptance rules, as the contract applies them.
#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub scale: u128,
    pub low: u128,
    pub high: u128,
    pub expiry_ns: u64,
    pub accept_until_ns: u64,
    pub min_confidence_bps: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    NoPrint,
    NotPositive,
    OutsideBand,
    PredatesExpiry,
    AfterAcceptWindow,
    InFuture,
    LowConfidence,
}

impl Reject {
    pub fn label(self) -> &'static str {
        match self {
            Reject::NoPrint => "no live print",
            Reject::NotPositive => "price must be positive",
            Reject::OutsideBand => "price outside band",
            Reject::PredatesExpiry => "print predates expiry",
            Reject::AfterAcceptWindow => "print after accept window",
            Reject::InFuture => "print is in the future",
            Reject::LowConfidence => "confidence below floor",
        }
    }
}

/// The contract's acceptance filter, run locally before a print is sent. The
/// band is wide (`low/10 .. high*10`) so a real settlement passes and a fat
/// finger does not; reproducing it here means the engine never submits a
/// transaction the chain is going to refuse.
pub fn accept(print: Option<&Observation>, band: &Band, now_ns: u64) -> Result<u128, Reject> {
    let Some(observation) = print else {
        return Err(Reject::NoPrint);
    };
    if observation.price <= 0.0 || !observation.price.is_finite() {
        return Err(Reject::NotPositive);
    }
    let scaled = (observation.price * band.scale as f64).round() as u128;
    if scaled == 0 {
        return Err(Reject::NotPositive);
    }
    let lower = band.low / 10;
    let upper = band.high.saturating_mul(10);
    if scaled < lower || scaled > upper {
        return Err(Reject::OutsideBand);
    }
    if observation.publish_time_ns < band.expiry_ns {
        return Err(Reject::PredatesExpiry);
    }
    if observation.publish_time_ns > band.accept_until_ns {
        return Err(Reject::AfterAcceptWindow);
    }
    if observation.publish_time_ns > now_ns {
        return Err(Reject::InFuture);
    }
    if observation.confidence_bps < band.min_confidence_bps {
        return Err(Reject::LowConfidence);
    }
    Ok(scaled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prints(items: &[(&'static str, f64, u64)]) -> Vec<(&'static str, Print)> {
        items
            .iter()
            .map(|(name, price, ts)| (*name, Print { price: *price, ts_ms: *ts, ticks: 1 }))
            .collect()
    }

    #[test]
    fn median_takes_the_centre_of_an_odd_set_and_averages_an_even_one() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0]), Some(2.5));
        assert_eq!(median(&[] as &[f64]), None);
    }

    #[test]
    fn three_agreeing_sources_are_live_and_nearly_full_confidence() {
        let now = 100_000;
        let fresh = prints(&[
            ("binance", 2350.0, now - 500),
            ("coinbase", 2350.4, now - 800),
            ("bybit", 2349.8, now - 200),
        ]);
        let summary = summarise(&fresh, now, &Options::default());
        assert_eq!(summary.status, Status::Live);
        assert_eq!(summary.fresh_sources, 3);
        // 0.6 across 2350 is 2.55 bps of disagreement, so 20 bps per bp of
        // spread costs 51 confidence points and nothing more.
        assert!((summary.confidence_bps - 9_949.0).abs() < 1.0, "{:?}", summary.confidence_bps);
        assert!(summary.observation(now).is_some());
    }

    #[test]
    fn one_source_is_capped_and_never_quotable() {
        let now = 100_000;
        let single = prints(&[("binance", 2350.0, now - 100)]);
        let summary = summarise(&single, now, &Options::default());
        assert_eq!(summary.status, Status::Degraded);
        assert!(
            (summary.confidence_bps - SINGLE_SOURCE_CAP_BPS).abs() < 1e-9,
            "a lone source must read as capped, not as certain"
        );
        assert!(summary.observation(now).is_none(), "a degraded index may not fix a series");
    }

    #[test]
    fn two_disagreeing_sources_are_a_print_with_a_lower_confidence() {
        let now = 100_000;
        let strict = prints(&[("binance", 2350.0, now - 100), ("coinbase", 2356.0, now - 100)]);
        let summary = summarise(&strict, now, &Options::default());
        assert_eq!(summary.status, Status::Degraded, "60 bps is past the degenerate spread");
        let print = summary.observation(now).expect("two sources are an index");
        assert_eq!(print.price, 2353.0, "the median, not either venue");
        // 25 bps of disagreement costs 20 bps of confidence apiece, so the number
        // lands just under 9,500 and stays well above the 3,000 floor a series
        // is created with.
        assert!(print.confidence_bps < 9_500 && print.confidence_bps > 9_400, "{}", print.confidence_bps);
    }

    #[test]
    fn a_diverging_source_widens_the_spread_and_buys_down_confidence() {
        let now = 100_000;
        let wide = prints(&[
            ("binance", 2350.0, now - 100),
            ("coinbase", 2350.2, now - 100),
            ("bybit", 2400.0, now - 100),
        ]);
        let summary = summarise(&wide, now, &Options::default());
        assert_eq!(summary.status, Status::Degraded);
        // The median refuses to follow the outlier, which is the point of it.
        assert!((summary.median.unwrap() - 2350.2).abs() < 1e-9);
        assert!(summary.confidence_bps < 6_000.0, "{:?}", summary.confidence_bps);
    }

    #[test]
    fn a_stale_source_is_dropped_before_the_median() {
        let now = 100_000;
        let mixed = prints(&[
            ("binance", 2350.0, now - 1_000),
            ("coinbase", 2350.0, now - 1_000),
            ("bybit", 1900.0, now - 40_000),
        ]);
        let summary = summarise(&mixed, now, &Options::default());
        assert_eq!(summary.status, Status::Live);
        assert_eq!(summary.fresh_sources, 2);
        assert_eq!(summary.median, Some(2350.0));
        assert_eq!(summary.sources, 3, "the dead venue is still counted");
    }

    #[test]
    fn all_stale_is_stale_and_nothing_is_offline() {
        let now = 100_000;
        let old = prints(&[("binance", 2350.0, now - 60_000)]);
        let summary = summarise(&old, now, &Options::default());
        assert_eq!(summary.status, Status::Stale);
        assert_eq!(summary.confidence_bps, 0.0);
        assert_eq!(summary.spread_bps, None);
        assert_eq!(summary.age_ms, Some(60_000));
        assert_eq!(
            summarise(&[], now, &Options::default()).status,
            Status::Offline,
            "an empty tape is not the same failure as a late one"
        );
    }

    #[test]
    fn sources_are_named_in_order_so_two_tapes_cannot_look_different_by_luck() {
        let now = 100_000;
        let a = prints(&[("bybit", 100.0, now - 100), ("binance", 101.0, now - 100)]);
        let b = prints(&[("binance", 101.0, now - 100), ("bybit", 100.0, now - 100)]);
        let one = summarise(&a, now, &Options::default());
        let two = summarise(&b, now, &Options::default());
        assert_eq!(one.median, two.median);
        assert_eq!(one.confidence_bps, two.confidence_bps);
        let table = sources(&a, now, &Options::default());
        assert_eq!(table[0].name, "binance", "sorted by name, not by arrival");
        assert_eq!(table[0].age_ms, 100);
        assert!(table[0].fresh);
    }

    fn band() -> Band {
        Band {
            scale: 1_000_000,
            low: 2_300_000_000,
            high: 2_400_000_000,
            expiry_ns: 1_000_000_000_000,
            accept_until_ns: 1_060_000_000_000,
            min_confidence_bps: 3_000,
        }
    }

    fn observation(price: f64, publish_ns: u64, confidence: u64) -> Observation {
        Observation {
            price,
            publish_time_ns: publish_ns,
            confidence_bps: confidence,
            spread_bps: 1.0,
            sources: 3,
        }
    }

    #[test]
    fn an_ordinary_print_scales_and_passes() {
        let band = band();
        let print = observation(2350.0, 1_020_000_000_000, 9_000);
        let scaled = accept(Some(&print), &band, 1_030_000_000_000).expect("accepted");
        assert_eq!(scaled, 2_350_000_000);
    }

    #[test]
    fn every_rejection_is_named_because_the_chain_refuses_without_a_reason() {
        let band = band();
        let inside = 1_020_000_000_000;
        let cases = [
            (None, Reject::NoPrint),
            (
                Some(observation(200.0, inside, 9_000)),
                Reject::OutsideBand,
            ),
            (Some(observation(30_000.0, inside, 9_000)), Reject::OutsideBand),
            (Some(observation(2350.0, 900_000_000_000, 9_000)), Reject::PredatesExpiry),
            (Some(observation(2350.0, 1_070_000_000_000, 9_000)), Reject::AfterAcceptWindow),
            (Some(observation(2350.0, inside, 2_999)), Reject::LowConfidence),
        ];
        for (print, want) in cases {
            let got = accept(print.as_ref(), &band, 1_030_000_000_000);
            assert_eq!(got.map(|_| 0).err(), Some(want), "{}", want.label());
        }
        assert_eq!(
            accept(Some(&observation(f64::NAN, inside, 9_000)), &band, 1_030_000_000_000).err(),
            Some(Reject::NotPositive),
            "a NaN price must never reach a scaling multiply"
        );
        // A print from ahead of the clock is still inside a long accept window:
        // only the now comparison catches it.
        let wide = Band { accept_until_ns: 3_000_000_000_000, ..band };
        assert_eq!(
            accept(Some(&observation(2350.0, 2_000_000_000_000, 9_000)), &wide, 1_500_000_000_000).err(),
            Some(Reject::InFuture)
        );
    }

    #[test]
    fn a_price_that_rounds_to_zero_is_not_a_price() {
        let band = band();
        let dust = observation(0.0000004, 1_020_000_000_000, 9_000);
        assert_eq!(
            accept(Some(&dust), &band, 1_030_000_000_000).err(),
            Some(Reject::NotPositive),
            "a price that rounds to zero units cannot settle anything"
        );
    }
}
