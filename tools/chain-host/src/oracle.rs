//! The committee that signs settlement prices, and the third-party pages it
//! reads them from.
//!
//! The host is a relay here, not an authority: each seat has its own ed25519 key,
//! generated when the chain boots and registered on the oracle, and the host signs
//! with that seat's key rather than its own. The prices come from venues the
//! engine does not already read — Kraken and OKX trade their own books, CoinGecko
//! aggregates somebody else's — so a settlement needs two of them to agree before
//! the venue pays anyone. Killing this process's engine socket is no longer enough
//! to decide who wins an option.

use ed25519_dalek::{Signer, SigningKey};
use bounded_core::oracle::signing_message;
use serde_json::{json, Value};

/// One feed, and what each source calls it.
pub struct Feed {
    pub id: &'static str,
    pub asset: &'static str,
    pub kraken: &'static str,
    pub okx: &'static str,
    pub coingecko: &'static str,
}

pub const FEEDS: [Feed; 2] = [
    Feed { id: "BTC-USD", asset: "BTC", kraken: "XBTUSD", okx: "BTC-USDT", coingecko: "bitcoin" },
    Feed { id: "ETH-USD", asset: "ETH", kraken: "ETHUSD", okx: "ETH-USDT", coingecko: "ethereum" },
];

/// A seat's price for one feed, in the contract's own units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tick {
    pub price_units: u128,
    pub publish_time_ns: u64,
    pub confidence_bps: u16,
}

/// A seat: a name the oracle knows, and the only key that can speak for it.
#[derive(Clone)]
pub struct Seat {
    pub name: &'static str,
    pub key: SigningKey,
}

impl Seat {
    pub fn public_key(&self) -> Vec<u8> {
        self.key.verifying_key().to_bytes().to_vec()
    }

    /// The registration call for this seat, as the oracle wants it.
    pub fn registration(&self) -> Value {
        json!({ "name": self.name, "public_key": self.public_key() })
    }

    /// What this seat says about one feed, signed so that the oracle can tell it
    /// did not come from the relay's own hand.
    pub fn observation(&self, feed: &str, tick: &Tick) -> Value {
        let message = signing_message(
            feed,
            self.name,
            tick.price_units,
            tick.publish_time_ns,
            tick.confidence_bps,
        );
        let signature: [u8; 64] = self.key.sign(&message).to_bytes();
        json!({
            "feed_id": feed,
            "source": self.name,
            "price": tick.price_units,
            "publish_time_ns": tick.publish_time_ns,
            "confidence_bps": tick.confidence_bps,
            "signature": signature.to_vec(),
        })
    }
}

/// 32 honest random bytes. The sandbox chain only lives as long as this process,
/// so the keys do not need to survive a restart — which is also why nothing here
/// ever writes one to disk.
fn random_seed() -> [u8; 32] {
    use std::io::Read;
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .expect("no randomness to sign settlement prices with")
        .read_exact(&mut seed)
        .expect("could not read randomness");
    seed
}

/// The four seats. Ours is named for what it is — the venue's own index — and it
/// gets exactly one vote, like every other row on the page.
pub fn committee() -> Vec<Seat> {
    ["venue-index", "kraken", "okx", "coingecko"]
        .iter()
        .map(|name| Seat { name, key: SigningKey::from_bytes(&random_seed()) })
        .collect()
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

/// `GET https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd`
///
/// An aggregate of somebody else's books, published without a bid or an ask, so
/// its certainty is taken as a flat one percent rather than pretended at.
pub fn parse_coingecko(body: &str, coin: &str, stamp_ns: u64) -> Result<Tick, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("coingecko: {e}"))?;
    let usd = value
        .get(coin)
        .and_then(|coin| coin.get("usd"))
        .and_then(|usd| usd.as_f64())
        .ok_or("coingecko has no usd price for this coin")?;
    let price_units = dollars_to_units(usd).ok_or("coingecko's price is not a price")?;
    Ok(Tick { price_units, publish_time_ns: stamp_ns, confidence_bps: 9_900 })
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

/// One seat's URL for one feed.
pub fn url_for(source: &str, feed: &Feed, engine: &str) -> Option<String> {
    let engine = engine.trim_end_matches('/');
    match source {
        "venue-index" => Some(format!("{engine}/index?asset={}", feed.asset)),
        "kraken" => Some(format!("https://api.kraken.com/0/public/Ticker?pair={}", feed.kraken)),
        "okx" => Some(format!("https://www.okx.com/api/v5/market/ticker?instId={}", feed.okx)),
        "coingecko" => Some(format!(
            "https://api.coingecko.com/api/v3/simple/price?ids={}&vs_currencies=usd",
            feed.coingecko
        )),
        _ => None,
    }
}

/// How often each seat is worth asking. CoinGecko's free page is rate limited, and
/// an aggregate that moves once a minute does not need polling every five seconds.
pub fn every_secs(source: &str) -> u64 {
    match source {
        "coingecko" => 60,
        _ => 5,
    }
}

/// A seat's answer for a feed, or the reason it did not answer.
pub fn fetch(source: &str, feed: &Feed, engine: &str) -> Result<Tick, String> {
    let url = url_for(source, feed, engine).ok_or("no endpoint for this seat")?;
    let body = https_get(&url)?;
    match source {
        "venue-index" => {
            let print = crate::operator::parse_print(&body)?;
            Ok(Tick {
                price_units: print.price_units,
                publish_time_ns: print.publish_time_ns,
                confidence_bps: print.confidence_bps.min(10_000) as u16,
            })
        }
        "kraken" => parse_kraken(&body, wall_ns()),
        "okx" => parse_okx(&body),
        "coingecko" => parse_coingecko(&body, feed.coingecko, wall_ns()),
        _ => Err("no parser for this seat".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::VerifyingKey;

    const KRAKEN: &str = r#"{"error":[],"result":{"XXBTZUSD":{"a":["77485.80000","1","1.000"],"b":["77485.70000","1","1.000"],"c":["77485.75000","0.011","0.852"],"v":["12713567.1","16154.5"],"t":[2,23415],"l":["76519.00000","76519.00000"],"h":["81970.40000","81970.40000"],"o":"80160.00000"}}}"#;
    const OKX: &str = r#"{"code":"0","msg":"","data":[{"instType":"SPOT","instId":"BTC-USDT","last":"77485.7","lastSz":"0.00007","askPx":"77485.8","askSz":"1.629","bidPx":"77485.6","bidSz":"0.259","ts":"1793360000123","sodUtc0":"79000.0"}]}"#;
    const COINGECKO: &str = r#"{"bitcoin":{"usd":77313},"ethereum":{"usd":2221.51}}"#;

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
    fn coingecko_is_taken_at_a_flat_confidence() {
        let tick = parse_coingecko(COINGECKO, "bitcoin", 7).expect("coingecko answered");
        assert_eq!(tick.price_units, 77_313_000_000);
        assert_eq!(tick.confidence_bps, 9_900, "an index without a book does not claim a spread");
        let eth = parse_coingecko(COINGECKO, "ethereum", 7).expect("both coins in one page");
        assert_eq!(eth.price_units, 2_221_510_000);
        assert!(parse_coingecko(COINGECKO, "solana", 7).is_err(), "an absent coin is not a zero price");
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

    #[test]
    fn the_signed_layout_is_the_contracts_own() {
        // The message comes from the oracle crate, so the relay cannot drift from
        // it by construction. Pinned here anyway: a change to the layout is a
        // change to what every seat signed, and should not arrive quietly.
        let message = signing_message("BTC-USD", "kraken", 7, 8, 9);
        assert_eq!(message.len(), 30 + 2 + 7 + 6 + 16 + 8 + 2);
        assert_eq!(&message[..7], b"near-bo");
        assert_eq!(message[30], 7);
    }

    #[test]
    fn a_seat_signs_only_for_itself() {
        let seat = Seat { name: "kraken", key: SigningKey::from_bytes(&[3u8; 32]) };
        let other = VerifyingKey::from_bytes(&[9u8; 32].into()).unwrap();
        let tick = Tick { price_units: 1, publish_time_ns: 2, confidence_bps: 3 };
        let item = seat.observation("BTC-USD", &tick);
        let signature: [u8; 64] = item["signature"]
            .as_array()
            .unwrap()
            .iter()
            .map(|byte| byte.as_u64().unwrap() as u8)
            .collect::<Vec<u8>>()
            .try_into()
            .unwrap();
        let mine = signing_message("BTC-USD", "kraken", 1, 2, 3);
        use ed25519_dalek::Verifier;
        assert!(seat.key.verifying_key().verify(&mine, &ed25519_dalek::Signature::from_bytes(&signature)).is_ok());
        assert!(
            other.verify(&mine, &ed25519_dalek::Signature::from_bytes(&signature)).is_err(),
            "another seat's key does not vouch for this one"
        );
        assert_eq!(item["price"], 1, "u128 goes as a number, the way the contract reads it");
        assert_eq!(item["source"], "kraken");
    }

    #[test]
    fn the_committee_seats_four_distinct_keys() {
        let seats = committee();
        assert_eq!(seats.len(), 4);
        let keys: Vec<Vec<u8>> = seats.iter().map(Seat::public_key).collect();
        assert_eq!(seats.iter().map(|s| s.name).collect::<Vec<_>>(), vec!["venue-index", "kraken", "okx", "coingecko"]);
        for pair in keys.windows(2) {
            assert_ne!(pair[0], pair[1], "two seats cannot share a key");
        }
        assert_eq!(seats[0].registration()["name"], "venue-index");
        assert_eq!(seats[0].registration()["public_key"].as_array().unwrap().len(), 32);
    }

    #[test]
    fn every_seat_has_one_endpoint_per_feed() {
        for feed in &FEEDS {
            for source in ["venue-index", "kraken", "okx", "coingecko"] {
                let url = url_for(source, feed, "http://127.0.0.1:8791").expect("a known seat");
                assert!(url.starts_with("http"), "{source} needs a URL");
                assert!(!url.contains(' '), "{source} built a URL with a space in it");
            }
            assert!(url_for("rumour", feed, "http://x").is_none(), "an unknown seat has no endpoint");
        }
        assert_eq!(every_secs("coingecko"), 60);
        assert_eq!(every_secs("kraken"), 5);
    }

    #[test]
    fn the_wall_clock_stamp_is_behind_the_block() {
        let stamp = wall_ns();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        assert!(stamp < now, "a settlement stamp must not arrive from the future");
        assert!(now - stamp < 3_000_000_000, "nor be so old that it reads as stale");
    }
}
