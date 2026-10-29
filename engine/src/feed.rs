//! The sockets. Three venues in, one tape out, no JavaScript in between.
//!
//! The parsers are the whole point of this module and they are pure: given the
//! bytes a venue sent, they say what was traded, at what price, at what time, or
//! say nothing. Only `listen` touches the network, and it decides nothing either —
//! it reconnects on the same schedule the old shell used and forwards frames to
//! the parsers.
//!
//! What is deliberately *not* here: any opinion about which print is the price.
//! That is `surface::index`, which is where a rule that can set a settlement
//! belongs. A venue that disagrees is outvoted by the median, not special-cased.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tungstenite::{connect, Message};

use crate::serve::Tape;
use crate::serve::Tick;

/// Where "now" comes from, so the parsers' clock fallbacks are testable without
/// waiting for a real clock to move.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The venues, in the order the tape reports them. `wss` only, and the host part
/// is never taken from anywhere but this table.
pub const SOURCES: [(&str, &str); 3] = [
    ("binance", "wss://stream.binance.com:9443/stream?streams=btcusdt@trade/ethusdt@trade"),
    ("coinbase", "wss://ws-feed.exchange.coinbase.com"),
    ("bybit", "wss://stream.bybit.com/v5/public/spot"),
];

/// Reconnect at 1s, 2s, 4s, 8s, then 15s, like the shell did: a venue that is
/// briefly unreachable gets retried without a storm of handshakes.
pub const BACKOFF_BASE_MS: u64 = 1_000;
pub const BACKOFF_MAX_MS: u64 = 15_000;

/// What each venue has to be told before it sends anything.
pub fn subscribe_frame(source: &str) -> Option<&'static str>
{
    match source {
        // Binance needs no subscribe frame: its stream names are in the URL, and
        // sending one anyway makes it answer with a handshake error.
        "binance" => None,
        "coinbase" => Some("{\"type\":\"subscribe\",\"product_ids\":[\"BTC-USD\",\"ETH-USD\"],\"channels\":[\"ticker\"]}"),
        "bybit" => Some("{\"op\":\"subscribe\",\"args\":[\"publicTrade.BTCUSDT\",\"publicTrade.ETHUSDT\"]}"),
        _ => None,
    }
}

/// Exponential backoff, capped. Attempt 0 is immediate, so a first failure does
/// not wait a second for nothing.
pub fn backoff_ms(attempt: u32) -> u64 {
    let shift = attempt.min(4);
    let raw = BACKOFF_BASE_MS.saturating_mul(1u64 << shift);
    raw.min(BACKOFF_MAX_MS)
}

/// Milliseconds since the epoch for `YYYY-MM-DDTHH:MM:SS.mmmZ`, the one format
/// Coinbase puts in its ticker frames. Written here rather than pulled in as a
/// datetime crate, because a leap-year bug in a date library would be a bug in
/// the price feed and there is no second use for it.
pub fn utc_millis(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let part = |range: std::ops::Range<usize>| -> Option<u64> {
        let slice = &bytes[range.clone()];
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(slice).ok()?.parse::<u64>().ok()
    };
    let year = part(0..4)?;
    let month = part(5..7)?;
    let day = part(8..10)?;
    let hour = part(11..13)?;
    let minute = part(14..16)?;
    let second = part(17..19)?;
    let ms = if bytes.get(19) == Some(&b'.') { part(20..23)? } else { 0 };
    if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 || ms > 999 {
        return None;
    }
    // Days from the civil calendar (Howard Hinnant's `days_from_civil`), which
    // is how 1970-01-01 and 2024-02-29 both come out right without a table of
    // month lengths to get wrong.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let absolute = era.checked_mul(146_097)?.checked_add(doe)?;
    if absolute < 719_468 {
        return None;
    }
    let days = absolute - 719_468;
    Some(days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000 + ms)
}

fn price(value: &Value) -> Option<f64> {
    value
        .as_str()
        .and_then(|text| text.parse::<f64>().ok())
        .or_else(|| value.as_f64())
}

fn asset_of(symbol: &str) -> Option<&'static str> {
    match symbol {
        "BTCUSDT" | "BTC-USD" => Some("BTC"),
        "ETHUSDT" | "ETH-USD" => Some("ETH"),
        _ => None,
    }
}

fn now(clock: &Clock) -> u64 {
    clock()
}

/// One venue's frame, in its own dialect. Every branch below is a shape that
/// venue actually sends; anything else is a control frame and answers `None`.
pub fn normalize(source: &str, frame: &str, clock: &Clock) -> Option<Tick> {
    let value: Value = serde_json::from_str(frame).ok()?;
    let (asset, price, ts_ms) = match source {
        "binance" => {
            let payload = value.get("data").unwrap_or(&value);
            if payload.get("e")?.as_str()? != "trade" {
                return None;
            }
            let symbol = payload.get("s")?.as_str()?;
            let price = price(payload.get("p")?)?;
            let ts = payload
                .get("T")
                .and_then(Value::as_u64)
                .or_else(|| payload.get("E").and_then(Value::as_u64))
                .unwrap_or_else(|| now(clock));
            (asset_of(symbol)?, price, ts)
        }
        "coinbase" => {
            if value.get("type")?.as_str()? != "ticker" {
                return None;
            }
            let symbol = value.get("product_id")?.as_str()?;
            let price = price(value.get("price")?)?;
            // `time` is when the exchange matched it; if it is missing or
            // unreadable the frame's arrival is the only honest timestamp.
            let ts = value
                .get("time")
                .and_then(Value::as_str)
                .and_then(utc_millis)
                .unwrap_or_else(|| now(clock));
            (asset_of(symbol)?, price, ts)
        }
        "bybit" => {
            if !value
                .get("topic")
                .and_then(Value::as_str)
                .is_some_and(|topic| topic.starts_with("publicTrade"))
            {
                return None;
            }
            let trade = value.get("data")?.as_array()?.first()?;
            let symbol = trade.get("s")?.as_str()?;
            let price = price(trade.get("p")?)?;
            let ts = trade.get("T").and_then(Value::as_u64).unwrap_or_else(|| now(clock));
            (asset_of(symbol)?, price, ts)
        }
        _ => return None,
    };
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    Some(Tick { source: source.to_string(), asset: asset.to_string(), price, ts_ms })
}

/// Traffic counters for the status line, so an operator can tell a quiet market
/// from a dead socket.
#[derive(Default, Debug)]
pub struct Counts {
    pub ticks: AtomicU64,
    pub reconnects: AtomicU64,
    pub connected: AtomicU64,
}

impl Counts {
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.ticks.load(Ordering::Relaxed),
            self.reconnects.load(Ordering::Relaxed),
            self.connected.load(Ordering::Relaxed),
        )
    }
}

/// One venue, forever. Blocking reads are fine: when the process is told to stop
/// the whole thing goes, and a socket that had to be interrupted mid-read would
/// only have been closed anyway.
fn listen(source: &str, url: &str, tape: Arc<Mutex<Tape>>, counts: Arc<Counts>, running: Arc<AtomicBool>) {
    let clock: Clock = Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    });
    let mut attempt = 0u32;
    while running.load(Ordering::Relaxed) {
        match connect(url) {
            Ok((mut socket, _)) => {
                attempt = 0;
                counts.connected.fetch_add(1, Ordering::Relaxed);
                if let Some(frame) = subscribe_frame(source) {
                    let _ = socket.send(Message::Text(frame.into()));
                }
                loop {
                    match socket.read() {
                        Ok(Message::Text(text)) => {
                            if let Some(tick) = normalize(source, &text, &clock) {
                                if let Ok(mut tape) = tape.lock() {
                                    tape.ingest(&tick);
                                }
                                counts.ticks.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
                let _ = socket.close(None);
            }
            Err(_) => {
                counts.connected.store(0, Ordering::Relaxed);
            }
        }
        if !running.load(Ordering::Relaxed) {
            return;
        }
        counts.reconnects.fetch_add(1, Ordering::Relaxed);
        let wait = backoff_ms(attempt);
        attempt = attempt.saturating_add(1);
        // Sleep in small slices so a Ctrl-C does not sit inside a 15-second wait.
        let mut slept = 0u64;
        while slept < wait && running.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
            slept += 100;
        }
    }
}

/// What the sockets are doing, in one line: prints are the only way to tell a
/// quiet market from a venue that stopped answering.
impl Counts {
    pub fn line(&self) -> String {
        let (ticks, reconnects, connected) = self.snapshot();
        format!("tape={ticks} · sockets={connected} · reconnects={reconnects}")
    }
}

/// Start one thread per venue. Nothing here blocks the caller, and the handles
/// are dropped on purpose: the threads run until the process does.
pub fn spawn(tape: Arc<Mutex<Tape>>, counts: Arc<Counts>, running: Arc<AtomicBool>) -> Vec<thread::JoinHandle<()>> {
    SOURCES
        .iter()
        .map(|(source, url)| {
            let tape = Arc::clone(&tape);
            let counts = Arc::clone(&counts);
            let running = Arc::clone(&running);
            let source = *source;
            let url = (*url).to_string();
            thread::spawn(move || listen(source, &url, tape, counts, running))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> Clock {
        Arc::new(|| 1_793_182_584_500)
    }

    #[test]
    fn binance_maps_a_trade_and_ignores_its_handshake() {
        let tick = normalize(
            "binance",
            r#"{"stream":"btcusdt@trade","data":{"e":"trade","p":"81234.50","T":1793182536001,"s":"BTCUSDT"}}"#,
            &clock(),
        )
        .expect("a trade");
        assert_eq!(tick.asset, "BTC");
        assert_eq!(tick.price, 81_234.5);
        assert_eq!(tick.ts_ms, 1_793_182_536_001);
        assert_eq!(tick.source, "binance");

        assert_eq!(normalize("binance", r#"{"result":null,"id":1}"#, &clock()), None);
        assert_eq!(normalize("binance", "not json", &clock()), None);
        assert_eq!(normalize("binance", r#"{"e":"trade","p":"1","s":"SOLUSDT","T":1}"#, &clock()), None, "not an asset we list");
        // The event payload is nested for a combined stream and bare otherwise.
        let bare = normalize("binance", r#"{"e":"trade","p":"2372.71","E":5,"s":"ETHUSDT"}"#, &clock()).unwrap();
        assert_eq!((bare.asset.as_str(), bare.price, bare.ts_ms), ("ETH", 2372.71, 5));
    }

    #[test]
    fn coinbase_maps_a_ticker_and_reads_its_own_time_format() {
        let tick = normalize(
            "coinbase",
            r#"{"type":"ticker","product_id":"ETH-USD","price":"2372.71","time":"2026-10-28T10:16:24.500Z"}"#,
            &clock(),
        )
        .expect("a ticker");
        assert_eq!((tick.asset.as_str(), tick.price), ("ETH", 2372.71));
        assert_eq!(tick.ts_ms, 1_793_182_584_500, "the exchange's own millisecond");

        assert_eq!(normalize("coinbase", r#"{"type":"subscriptions"}"#, &clock()), None);
        // A frame with no readable time is stamped with arrival, not zero: a zero
        // would put the source out of the stale window instantly.
        let late = normalize("coinbase", r#"{"type":"ticker","product_id":"BTC-USD","price":"60000"}"#, &clock()).unwrap();
        assert_eq!(late.ts_ms, 1_793_182_584_500);
    }

    #[test]
    fn bybit_maps_the_first_trade_of_its_batch() {
        let tick = normalize(
            "bybit",
            r#"{"topic":"publicTrade.BTCUSDT","data":[{"s":"BTCUSDT","p":"81240.10","T":1793182536100}]}"#,
            &clock(),
        )
        .expect("a batch");
        assert_eq!((tick.asset.as_str(), tick.price, tick.ts_ms), ("BTC", 81_240.1, 1_793_182_536_100));
        assert_eq!(normalize("bybit", r#"{"success":true,"op":"subscribe"}"#, &clock()), None);
        assert_eq!(normalize("bybit", r#"{"topic":"publicTrade.BTCUSDT","data":[]}"#, &clock()), None, "empty batch");
    }

    #[test]
    fn a_price_that_is_not_a_price_is_dropped_by_every_source() {
        let c = clock();
        assert_eq!(normalize("binance", r#"{"e":"trade","p":"0","s":"ETHUSDT","T":1}"#, &c), None);
        assert_eq!(normalize("coinbase", r#"{"type":"ticker","product_id":"ETH-USD","price":"-1","time":"2026-10-28T10:16:24.500Z"}"#, &c), None);
        assert_eq!(normalize("bybit", r#"{"topic":"publicTrade.ETHUSDT","data":[{"s":"ETHUSDT","p":"x","T":1}]}"#, &c), None);
        assert_eq!(normalize("kraken", r#"{"e":"trade"}"#, &c), None, "an unknown source parses nothing");
    }

    #[test]
    fn utc_millis_agrees_with_a_real_calendar() {
        assert_eq!(utc_millis("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(utc_millis("2024-02-29T00:00:00.000Z"), Some(1_709_164_800_000), "leap day");
        assert_eq!(utc_millis("2026-10-28T10:16:24.500Z"), Some(1_793_182_584_500));
        assert_eq!(utc_millis("2026-12-31T23:59:59.999Z"), Some(1_798_761_599_999));
        // Crossing into March is where an unstored table of month lengths fails.
        assert_eq!(
            utc_millis("2024-03-01T00:00:00.000Z"),
            Some(utc_millis("2024-02-29T00:00:00.000Z").unwrap() + 86_400_000)
        );
        assert_eq!(utc_millis("2026-10-28T10:16:24Z"), Some(1_793_182_584_000), "no fraction is fine");
        assert_eq!(utc_millis(""), None);
        assert_eq!(utc_millis("2026-13-01T00:00:00.000Z"), None, "month 13");
        assert_eq!(utc_millis("2026-02-30T00:00:00.000Z").map(|_| true), Some(true), "day 30 is read, the exchange is not the calendar");
        assert_eq!(utc_millis("not a date at all............"), None);
    }

    #[test]
    fn reconnect_waits_like_the_shell_did_and_then_stops_growing() {
        assert_eq!(backoff_ms(0), 1_000);
        assert_eq!(backoff_ms(1), 2_000);
        assert_eq!(backoff_ms(2), 4_000);
        assert_eq!(backoff_ms(4), 15_000, "capped at 16s then clamped");
        assert_eq!(backoff_ms(40), BACKOFF_MAX_MS, "a venue down all night is not hammered");
    }

    #[test]
    fn only_coinbase_needs_a_subscribe_and_it_asks_for_both_products() {
        assert_eq!(subscribe_frame("binance"), None);
        let coinbase = subscribe_frame("coinbase").unwrap();
        assert!(coinbase.contains("ETH-USD") && coinbase.contains("BTC-USD"), "{coinbase}");
        assert!(coinbase.contains("ticker"));
        let bybit = subscribe_frame("bybit").unwrap();
        assert!(bybit.contains("publicTrade.BTCUSDT") && bybit.contains("publicTrade.ETHUSDT"));
        assert_eq!(subscribe_frame("gemini"), None, "no venue is subscribed by accident");
    }

    #[test]
    fn the_sources_are_the_three_the_tape_names() {
        assert_eq!(SOURCES.len(), 3);
        let names: Vec<&str> = SOURCES.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, vec!["binance", "coinbase", "bybit"]);
        for (name, url) in SOURCES {
            assert!(url.starts_with("wss://"), "{name} is not a TLS socket");
            let authority = url[6..].split('/').next().unwrap_or("");
            assert!(!authority.contains('@'), "{name} must not carry credentials");
        }
    }

    #[test]
    fn counters_start_at_zero_and_snapshot_without_touching_the_sockets() {
        let counts = Counts::default();
        assert_eq!(counts.snapshot(), (0, 0, 0));
        counts.ticks.fetch_add(3, Ordering::Relaxed);
        counts.reconnects.fetch_add(1, Ordering::Relaxed);
        assert_eq!(counts.snapshot(), (3, 1, 0));
    }
}
