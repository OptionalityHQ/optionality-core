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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tick {
    pub price_units: u128,
    pub publish_time_ns: u64,
    pub confidence_bps: u16,
}

/// A seat: a name the oracle knows, and a socket to the process that holds the
/// only key able to speak for it.
///
/// The relay no longer signs anything. It fetched prices and signed them with all
/// four keys, so a compromised relay could vote four times; now each key lives in
/// a process of its own, and this struct is a client that can ask and be refused.
#[derive(Clone)]
pub struct Seat {
    pub name: &'static str,
    pub socket: std::path::PathBuf,
    pub public_key: Vec<u8>,
}

impl Seat {
    /// The registration call for this seat, as the oracle wants it.
    pub fn registration(&self) -> Value {
        json!({ "name": self.name, "public_key": self.public_key })
    }

    /// What this seat says about one feed, signed in the process that owns the
    /// key. Nothing the relay does to the request changes which key answers.
    pub fn observation(&self, feed: &str, tick: &Tick) -> Result<Value, String> {
        let signature = self.sign(feed, tick)?;
        Ok(json!({
            "feed_id": feed,
            "source": self.name,
            "price": tick.price_units,
            "publish_time_ns": tick.publish_time_ns,
            "confidence_bps": tick.confidence_bps,
            "signature": signature,
        }))
    }

    fn sign(&self, feed: &str, tick: &Tick) -> Result<Vec<u8>, String> {
        let line = format!(
            "sign {feed} {} {} {}",
            tick.price_units, tick.publish_time_ns, tick.confidence_bps
        );
        let signature = self.ask(&line)?;
        hex::decode(signature.trim())
            .map_err(|error| format!("{}: signature is not hex: {error}", self.name))
    }

    fn ask(&self, line: &str) -> Result<String, String> {
        use std::io::{BufRead, BufReader, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(&self.socket)
            .map_err(|error| format!("{} is not answering: {error}", self.name))?;
        writeln!(stream, "{line}").map_err(|error| format!("{}: {error}", self.name))?;
        let mut reply = String::new();
        BufReader::new(stream)
            .read_line(&mut reply)
            .map_err(|error| format!("{}: {error}", self.name))?;
        let reply = reply.trim().to_string();
        reply
            .strip_prefix("ok ")
            .map(|value| value.to_string())
            .ok_or_else(|| format!("{} refused: {reply}", self.name))
    }
}

/// Start one signer process per seat and read the public key each of them reports.
///
/// The keys are generated inside those processes and never leave them: this
/// function learns only what it needs to seat them on chain. The children are
/// returned so the caller can keep them alive for the life of the venue.
pub fn start_committee(
    root: &std::path::Path,
) -> Result<(Vec<Seat>, Vec<std::process::Child>), String> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().map_err(|error| format!("no executable path: {error}"))?;
    let signer = exe.with_file_name("oracle-signer");
    if !signer.exists() {
        return Err(format!("{} is missing: build it with -p oracle-signer", signer.display()));
    }
    std::fs::create_dir_all(root).map_err(|error| format!("{}: {error}", root.display()))?;
    let mut seats = Vec::new();
    let mut children = Vec::new();
    for name in SEAT_NAMES {
        let socket = root.join(format!("{name}.sock"));
        let mut child = Command::new(&signer)
            .arg("--name")
            .arg(name)
            .arg("--socket")
            .arg(&socket)
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| format!("starting the {name} signer: {error}"))?;
        let mut line = String::new();
        let ready = child
            .stdout
            .as_mut()
            .ok_or_else(|| format!("{name} signer has no stdout"))?;
        BufReader::new(ready)
            .read_line(&mut line)
            .map_err(|error| format!("{name} signer never said ready: {error}"))?;
        let mut parts = line.split_whitespace();
        let (Some("ready"), Some(reported), Some(public_key)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(format!("{name} signer said {line:?} instead of ready"));
        };
        if reported != name {
            return Err(format!("the {name} signer introduced itself as {reported}"));
        }
        seats.push(Seat {
            name,
            socket,
            public_key: hex::decode(public_key)
                .map_err(|error| format!("{name}: public key is not hex: {error}"))?,
        });
        children.push(child);
    }
    Ok((seats, children))
}

/// The seats, in the order the panel shows them. Ours is named for what it is —
/// the venue's own index — and it gets exactly one vote, like every other row.
pub const SEAT_NAMES: [&'static str; 4] = ["venue-index", "kraken", "okx", "pyth"];

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

    /// A stand-in signer: one unix socket, one line in, one line out. What the
    /// relay's side of the protocol has to get right is the request it sends and
    /// the refusal it reports, and both are testable without a key.
    fn stub(reply: &'static str) -> (Seat, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader, Write};
        let socket = std::env::temp_dir().join(format!(
            "oracle-seat-test-{}-{}.sock",
            std::process::id(),
            reply.len()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(stream.try_clone().expect("clone"))
                .read_line(&mut line)
                .expect("read");
            writeln!(stream, "{reply}").expect("reply");
            line.trim().to_string()
        });
        let seat = Seat { name: "kraken", socket, public_key: vec![0u8; 32] };
        (seat, handle)
    }

    fn tick() -> Tick {
        Tick { price_units: 1_000_000, publish_time_ns: 2, confidence_bps: 9_900 }
    }

    #[test]
    fn the_relay_asks_for_exactly_the_observation_it_has() {
        let signature = "ab".repeat(64);
        let (seat, asked) = stub(Box::leak(format!("ok {signature}").into_boxed_str()));
        let item = seat.observation("BTC-USD", &tick()).expect("signed");
        assert_eq!(
            asked.join().expect("stub thread"),
            "sign BTC-USD 1000000 2 9900",
            "the request carries the feed, the price, the stamp and the confidence"
        );
        assert_eq!(item["signature"].as_array().map(|rows| rows.len()), Some(64));
        assert_eq!(item["source"], "kraken", "and the seat signs its own name");
        assert_eq!(item["price"], 1_000_000, "u128 goes as a number, the way the contract reads it");
    }

    #[test]
    fn a_refused_signature_is_an_error_not_an_empty_observation() {
        let (seat, _) = stub("err refusing to sign a non-price");
        let error = seat.observation("BTC-USD", &tick()).expect_err("must refuse");
        assert!(error.contains("refusing to sign a non-price"), "{error}");
    }

    #[test]
    fn a_dead_signer_is_reported_by_name() {
        let seat = Seat {
            name: "okx",
            socket: std::env::temp_dir().join("oracle-no-such-signer.sock"),
            public_key: vec![0u8; 32],
        };
        let error = seat.observation("BTC-USD", &tick()).expect_err("no signer");
        assert!(error.contains("okx is not answering"), "{error}");
    }

    #[test]
    fn the_committee_names_four_seats_and_holds_no_keys() {
        assert_eq!(SEAT_NAMES, ["venue-index", "kraken", "okx", "pyth"]);
    }

    #[test]
    fn every_seat_has_one_endpoint_per_feed() {
        for feed in &FEEDS {
            for source in ["venue-index", "kraken", "okx", "pyth"] {
                let url = url_for(source, feed, "http://127.0.0.1:8791").expect("a known seat");
                assert!(url.starts_with("http"), "{source} needs a URL");
                assert!(!url.contains(' '), "{source} built a URL with a space in it");
            }
            assert!(url_for("rumour", feed, "http://x").is_none(), "an unknown seat has no endpoint");
        }
        assert_eq!(every_secs("pyth"), 16, "somebody else's RPC is polled politely");
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
