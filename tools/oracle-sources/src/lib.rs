//! What each seat reads, and how.
//!
//! These are the four price sources the venue settles on, each read by the party
//! that signs it. That is the whole point of the split: a seat's signature means
//! "I fetched this myself", not "somebody told me a number and asked me to vouch
//! for it". The relay chooses when to ask and what to deliver; it never touches a
//! price a seat did not read.
//!
//! Pyth is the one source that is not somebody's page: its own contract on Base is
//! asked for a price over a public RPC, so the number is the one Pyth's nodes signed
//! and that contract verified.

use serde_json::{json, Value};

/// One feed, and what each source calls it.
pub struct Feed {
    pub id: &'static str,
    pub asset: &'static str,
    pub kraken: &'static str,
    pub okx: &'static str,
    /// Pyth's feed id, the key its own on-chain contract is asked for.
    pub pyth: &'static str,
}

pub const FEEDS: [Feed; 2] = [
    Feed {
        id: "BTC-USD",
        asset: "BTC",
        kraken: "XBTUSD",
        okx: "BTC-USDT",
        pyth: "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
    },
    Feed {
        id: "ETH-USD",
        asset: "ETH",
        kraken: "ETHUSD",
        okx: "ETH-USDT",
        pyth: "ff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace",
    },
];

/// Pyth's price contract. The seat reads a contract rather than an API: the
/// number is the one Pyth's own nodes signed and the contract verified, and any
/// reviewer can make the same `eth_call` and get the same bytes. That is the
/// difference between "we asked somebody" and "it is on chain".
///
/// Base is the deployment this was measured against: its BTC/USD answers to the
/// second, while Arbitrum's answered 23 seconds fresh for BTC and *28 days* stale
/// for ETH, and Base's own ETH/USD was 39 minutes old when checked. A stale feed
/// is not a broken seat — the oracle drops a price whose stamp is older than the
/// age limit — it just abstains until the feed moves again.
pub const PYTH_CONTRACT: &str = "0x8250f4aF4B972684F7b336503E2D6dFeDeB1487a";
/// A public Base RPC. No key, no account: the read is the one a wallet makes.
pub const PYTH_RPC: &str = "https://mainnet.base.org";

/// A seat's price for one feed, in the contract's own units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tick {
    pub price_units: u128,
    pub publish_time_ns: u64,
    pub confidence_bps: u16,
}

/// The host's wall clock, held back by a second and a half. These sources stamp
/// their own time or none at all; a stamp a little behind the block is a stamp the
/// contract will accept, while one ahead of it is a print from the future.
pub fn wall_ns() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or_default();
    now.saturating_sub(1_500_000_000)
}

/// How far apart the two sides of a live book are, as the contract reads
/// confidence: a wide market is a less certain price.
fn confidence_from_spread(bid: f64, ask: f64) -> u16 {
    if bid <= 0.0 || ask < bid {
        return 5_000;
    }
    let mid = (bid + ask) / 2.0;
    let bps = ((ask - bid) / mid * 10_000.0).round().clamp(0.0, 10_000.0);
    (10_000.0 - bps) as u16
}

fn dollars_to_units(value: f64) -> Option<u128> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some((value * SCALE as f64).round() as u128)
}

const SCALE: f64 = 1_000_000.0;

fn field(row: &Value, key: &str, index: usize) -> Option<f64> {
    row.get(key)?.get(index)?.as_str()?.parse::<f64>().ok()
}

/// `GET https://api.kraken.com/0/public/Ticker?pair=XBTUSD`
///
/// Kraken returns its pair under its own symbol (`XXBTZUSD`), so the answer is
/// read from the single entry rather than from the name we asked with.
pub fn parse_kraken(body: &str, stamp_ns: u64) -> Result<Tick, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("kraken: {e}"))?;
    let errors = value.get("error").and_then(Value::as_array).cloned().unwrap_or_default();
    if !errors.is_empty() {
        return Err(format!("kraken refused: {errors:?}"));
    }
    let row = value
        .get("result")
        .and_then(Value::as_object)
        .and_then(|result| result.values().next())
        .ok_or("kraken returned no pair")?;
    let last = field(row, "c", 0).ok_or("kraken has no last trade")?;
    let price_units = dollars_to_units(last).ok_or("kraken's last trade is not a price")?;
    let bid = field(row, "b", 0).unwrap_or(last);
    let ask = field(row, "a", 0).unwrap_or(last);
    Ok(Tick { price_units, publish_time_ns: stamp_ns, confidence_bps: confidence_from_spread(bid, ask) })
}

/// `GET https://www.okx.com/api/v5/market/ticker?instId=BTC-USDT`
///
/// OKX stamps its own response time, so this seat carries the market's clock
/// rather than the host's.
pub fn parse_okx(body: &str) -> Result<Tick, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("okx: {e}"))?;
    if value.get("code").and_then(Value::as_str) != Some("0") {
        return Err(format!(
            "okx refused: {}",
            value.get("msg").and_then(Value::as_str).unwrap_or("?")
        ));
    }
    let row = value
        .get("data")
        .and_then(Value::as_array)
        .and_then(|rows| rows.first())
        .ok_or("okx returned no ticker")?;
    let number = |key: &str| row.get(key).and_then(Value::as_str).and_then(|v| v.parse::<f64>().ok());
    let last = number("last").ok_or("okx's last is not a number")?;
    let price_units = dollars_to_units(last).ok_or("okx's last is not a price")?;
    let stamp = row
        .get("ts")
        .and_then(Value::as_str)
        .and_then(|ts| ts.parse::<u64>().ok())
        .ok_or("okx has no timestamp")?;
    let bid = number("bidPx").unwrap_or(last);
    let ask = number("askPx").unwrap_or(last);
    Ok(Tick {
        price_units,
        publish_time_ns: stamp.saturating_mul(1_000_000),
        confidence_bps: confidence_from_spread(bid, ask),
    })
}

/// `eth_call` to Pyth's contract: `getPriceUnsafe(bytes32 id)`.
///
/// The answer is four words — price, confidence, exponent, publish time. The
/// price arrives as an integer scaled by ten to the exponent, and this relay is
/// the only place that turns it into the contract's micro-dollars, so the scale
/// is asserted rather than assumed.
pub fn parse_pyth(body: &str) -> Result<Tick, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("pyth rpc: {e}"))?;
    if let Some(error) = value.get("error") {
        return Err(format!("pyth rpc refused: {error}"));
    }
    let hex = value
        .get("result")
        .and_then(Value::as_str)
        .ok_or("pyth rpc returned no result")?
        .trim_start_matches("0x");
    if hex.len() != 4 * 64 {
        return Err(format!("pyth answered {} bytes, not four words", hex.len() / 2));
    }
    // Each word is 32 bytes of two's complement. The values are int64/int32, so
    // the top bytes are sign extension: read the low eight bytes and refuse
    // anything that is not zero- or sign-padded, rather than silently truncating
    // a number that does not fit.
    let word = |index: usize| -> Result<(u64, bool), String> {
        let raw = &hex[index * 64..(index + 1) * 64];
        let low = u64::from_str_radix(&raw[48..], 16)
            .map_err(|e| format!("pyth word {index}: {e}"))?;
        let padding = &raw[..48];
        let negative = padding.bytes().all(|byte| byte == b'f');
        let empty = padding.bytes().all(|byte| byte == b'0');
        if !negative && !empty {
            return Err(format!("pyth word {index} is wider than 64 bits"));
        }
        Ok((low, negative))
    };
    // The low eight bytes of a sign-extended word *are* the two's complement
    // value, so the cast is the conversion — the padding check above is what
    // makes it safe.
    let (price_word, _) = word(0)?;
    let (confidence_word, _) = word(1)?;
    let (exponent_word, _) = word(2)?;
    let (publish_time, _) = word(3)?;
    let price = price_word as i64 as i128;
    let confidence = confidence_word as i64 as i128;
    let exponent = exponent_word as u32 as i32;
    let price_units = scale_to_units(price, exponent).ok_or("pyth's scale is out of range")?;
    if price_units == 0 {
        return Err("pyth's price rounds to nothing at this scale".to_string());
    }
    // A price is only as good as the band Pyth puts around it: the confidence
    // interval becomes the contract's confidence, the same way a book's spread
    // does for an exchange seat.
    let band_bps = (confidence.max(0) as u128)
        .saturating_mul(10_000)
        .checked_div(price as u128)
        .unwrap_or(10_000)
        .min(10_000);
    Ok(Tick {
        price_units,
        publish_time_ns: publish_time.saturating_mul(1_000_000_000),
        confidence_bps: (10_000 - band_bps) as u16,
    })
}

/// An integer scaled by ten to the exponent, in the venue's six-decimal dollars.
fn scale_to_units(value: i128, exponent: i32) -> Option<u128> {
    let shift = exponent + 6;
    let value = u128::try_from(value).ok()?;
    if shift >= 0 {
        value.checked_mul(10u128.checked_pow(shift as u32)?)
    } else {
        Some(value / 10u128.checked_pow((-shift) as u32)?)
    }
}

/// The JSON-RPC body that asks Pyth for one feed.
pub fn pyth_request(feed_id: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [
            { "to": PYTH_CONTRACT, "data": format!("0x96834ad3{feed_id}") },
            "latest"
        ]
    })
    .to_string()
}

/// `GET /index` — the venue's own three-venue median, as the engine wrote it.
///
/// A body without a price is the engine saying it has no index — no prints, or
/// none fresh — and its `reason` travels with the error. A body *with* a price is
/// read whatever its `quotable` flag says: quotable is the venue's appetite for
/// making markets on the number, while the series' `min_confidence_bps` is the
/// chain's rule for settling on it.
pub fn parse_engine_index(body: &str) -> Result<Tick, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("index is not JSON: {e}"))?;
    if value.get("priceUnits").is_none() {
        let reason = value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        return Err(format!("engine has no print: {reason}"));
    }
    let price_units = units_of(value.get("priceUnits").ok_or("priceUnits is missing")?)
        .ok_or("priceUnits is not an integer")?;
    let publish_time_ns = units_of(value.get("publishTimeNs").ok_or("publishTimeNs is missing")?)
        .ok_or("publishTimeNs is not an integer")? as u64;
    let confidence_bps = units_of(value.get("confidenceBps").ok_or("confidenceBps is missing")?)
        .ok_or("confidenceBps is not an integer")?;
    if price_units == 0 {
        return Err("a print of zero units is not a price".to_string());
    }
    if confidence_bps > 10_000 {
        return Err(format!("confidence {confidence_bps} bps is not a fraction"));
    }
    Ok(Tick {
        price_units,
        publish_time_ns,
        confidence_bps: confidence_bps as u16,
    })
}

/// A JSON number or a numeric string, whichever dialect the writer chose.
fn units_of(raw: &Value) -> Option<u128> {
    raw.as_u64()
        .map(u128::from)
        .or_else(|| raw.as_str().and_then(|text| text.parse().ok()))
        .or_else(|| raw.as_f64().filter(|value| *value >= 0.0).map(|value| value as u128))
}

/// An HTTPS read. The engine sits on loopback and is fetched with plain HTTP by
/// the operator module; everything in the committee is somebody else's server and
/// has to be reached over TLS.
pub fn https_get(url: &str) -> Result<String, String> {
    ureq::get(url)
        .call()
        .map_err(|error| format!("{url}: {error}"))
        .map(|mut response| response.body_mut().read_to_string().unwrap_or_default())
}

/// A POST with a JSON body, for the one seat that has to ask a contract a
/// question rather than fetch a page.
pub fn https_post(url: &str, body: &str) -> Result<String, String> {
    ureq::post(url)
        .header("content-type", "application/json")
        .send(body)
        .map_err(|error| format!("{url}: {error}"))
        .map(|mut response| response.body_mut().read_to_string().unwrap_or_default())
}

/// One seat's URL for one feed.
pub fn url_for(source: &str, feed: &Feed, engine: &str) -> Option<String> {
    let engine = engine.trim_end_matches('/');
    match source {
        "venue-index" => Some(format!("{engine}/index?asset={}", feed.asset)),
        "kraken" => Some(format!("https://api.kraken.com/0/public/Ticker?pair={}", feed.kraken)),
        "okx" => Some(format!("https://www.okx.com/api/v5/market/ticker?instId={}", feed.okx)),
        "pyth" => Some(PYTH_RPC.to_string()),
        _ => None,
    }
}

/// How often each seat is worth asking. CoinGecko's free page is rate limited, and
/// an aggregate that moves once a minute does not need polling every five seconds.
pub fn every_secs(source: &str) -> u64 {
    match source {
        // Somebody else's RPC, polled politely: the answer moves every second but
        // a settlement does not need it more often than this.
        "pyth" => 16,
        _ => 5,
    }
}

/// A seat's answer for a feed, or the reason it did not answer.
pub fn fetch(source: &str, feed: &Feed, engine: &str) -> Result<Tick, String> {
    let url = url_for(source, feed, engine).ok_or("no endpoint for this seat")?;
    if source == "pyth" {
        return parse_pyth(&https_post(&url, &pyth_request(feed.pyth))?);
    }
    let body = https_get(&url)?;
    match source {
        "venue-index" => parse_engine_index(&body),
        "kraken" => parse_kraken(&body, wall_ns()),
        "okx" => parse_okx(&body),
        _ => Err("no parser for this seat".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KRAKEN: &str = r#"{"error":[],"result":{"XXBTZUSD":{"a":["77485.80000","1","1.000"],"b":["77485.70000","1","1.000"],"c":["77485.75000","0.011","0.852"],"v":["12713567.1","16154.5"],"t":[2,23415],"l":["76519.00000","76519.00000"],"h":["81970.40000","81970.40000"],"o":"80160.00000"}}}"#;
    const OKX: &str = r#"{"code":"0","msg":"","data":[{"instType":"SPOT","instId":"BTC-USDT","last":"77485.7","lastSz":"0.00007","askPx":"77485.8","askSz":"1.629","bidPx":"77485.6","bidSz":"0.259","ts":"1793360000123","sodUtc0":"79000.0"}]}"#;
    // A real `eth_call` answer from Pyth's Arbitrum contract, kept verbatim: four
    // words of price, confidence, exponent and publish time. If the decoder ever
    // stops agreeing with this, every settlement the Pyth seat feeds is wrong.
    const PYTH: &str = r#"{"jsonrpc":"2.0","id":1,"result":"0x0000000000000000000000000000000000000000000000000000070c52859dd000000000000000000000000000000000000000000000000000000001b27d16f0fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff8000000000000000000000000000000000000000000000000000000006ae48b86"}"#;

    #[test]
    fn kraken_reads_its_own_symbol_not_the_pair_we_asked_for() {
        let tick = parse_kraken(KRAKEN, 1_000).expect("kraken answered");
        assert_eq!(tick.price_units, 77_485_750_000, "micro-dollars, like every other price here");
        assert_eq!(tick.publish_time_ns, 1_000, "the host's clock, held back");
        // One tenth of a cent across a 77k book is a fifth of a basis point.
        assert!(tick.confidence_bps >= 9_900, "a tight book reads as a confident one");
    }

    #[test]
    fn okx_carries_the_markets_own_timestamp() {
        let tick = parse_okx(OKX).expect("okx answered");
        assert_eq!(tick.price_units, 77_485_700_000);
        assert_eq!(tick.publish_time_ns, 1_793_360_000_123_000_000, "its milliseconds, as nanoseconds");
        assert!(tick.confidence_bps > 9_000);
    }

    #[test]
    fn pyth_is_read_from_its_own_contract_not_an_api() {
        let tick = parse_pyth(PYTH).expect("pyth answered");
        // 7749505490384 at an exponent of -8 is 77,495.05 in the venue's units.
        assert_eq!(tick.price_units, 77_495_054_903);
        assert_eq!(
            tick.publish_time_ns, 1_793_362_822_000_000_000,
            "the publish time Pyth signed, in nanoseconds"
        );
        // A 72.90 dollar band on a 77,495 dollar price is nine basis points, and
        // that is what the seat's confidence is made of.
        assert_eq!(tick.confidence_bps, 9_991);
        let request = pyth_request(FEEDS[0].pyth);
        assert!(request.contains("0x96834ad3"), "getPriceUnsafe(bytes32)");
        assert!(request.contains(FEEDS[0].pyth), "for the feed we asked about");
        assert!(request.contains(PYTH_CONTRACT), "at Pyth's own contract");
    }

    #[test]
    fn a_pyth_answer_of_the_wrong_shape_is_an_error() {
        assert!(parse_pyth(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"execution reverted"}}"#).is_err());
        assert!(parse_pyth(r#"{"jsonrpc":"2.0","id":1,"result":"0x"}"#).is_err());
        assert!(parse_pyth(r#"{"jsonrpc":"2.0","id":1}"#).is_err());
        // A zero price is what an uninitialised feed returns, not a free option.
        let zero = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"0x{}{}{}{}\"}}",
            "00".repeat(32), "00".repeat(32), format!("{:064x}", (-8i32) as u32 as u64).chars().collect::<String>(), "00".repeat(32));
        assert!(parse_pyth(&zero).is_err());
    }

    #[test]
    fn the_engine_print_is_read_in_either_dialect() {
        let numeric = json!({
            "quotable": true, "priceUnits": 2_350_400_000u64,
            "publishTimeNs": 1_793_182_584_500_000_000u64, "confidenceBps": 9_790u64
        });
        let tick = parse_engine_index(&numeric.to_string()).expect("a print");
        assert_eq!(tick.price_units, 2_350_400_000);
        assert_eq!(tick.confidence_bps, 9_790);
        let strings = json!({
            "priceUnits": "2350400000", "publishTimeNs": "1793182584500000000", "confidenceBps": "9790"
        });
        assert_eq!(parse_engine_index(&strings.to_string()).unwrap(), tick, "same numbers, other dialect");
        // A degraded index is still a print: quotable is the venue's appetite, not
        // whether the number exists.
        let degraded = json!({ "quotable": false, "priceUnits": 1u64, "publishTimeNs": 1u64, "confidenceBps": 1u64 });
        assert!(parse_engine_index(&degraded.to_string()).is_ok());
        assert!(parse_engine_index(&json!({"reason": "no prints"}).to_string())
            .unwrap_err()
            .contains("no prints"));
    }

    #[test]
    fn a_refusal_is_an_error_not_a_price_of_zero() {
        assert!(parse_kraken(r#"{"error":["Invalid pair"],"result":{}}"#, 1).is_err());
        assert!(parse_okx(r#"{"code":"51001","msg":"instrument does not exist","data":[]}"#).is_err());
        assert!(parse_kraken(r#"{"error":[],"result":{"XXBTZUSD":{"c":["0.0000"]}}}"#, 1).is_err());
    }

    #[test]
    fn a_wide_book_reads_as_a_less_confident_one() {
        assert_eq!(confidence_from_spread(100.0, 100.0), 10_000);
        assert_eq!(confidence_from_spread(100.0, 101.0), 9_900, "one dollar across a 100.5 mid");
        assert_eq!(confidence_from_spread(0.0, 100.0), 5_000, "no bid is not a tight market");
    }

}
