//! The operator's three jobs: read what the engine says the market should be,
//! tell what the chain already has, and name the series that need a print.
//!
//! Listing and fixing are the engine's decisions (`GET /plan` and `GET /index`
//! are arithmetic on the live index). Signing them is this host's, because the
//! host is the only thing here holding an admin key — and the oracle's `report`
//! only answers to the reporter it was configured with, which is this account.
//!
//! Nothing in here talks to NEAR, so all of it is testable without a sandbox.

use serde_json::{json, Value};

/// One `create_series` call, as the engine emitted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub underlying: String,
    pub kind: String,
    pub expiry_ns: u64,
    pub accept_until_ns: u64,
    pub finalize_until_ns: u64,
    pub low_units: u128,
    pub high_units: u128,
    pub price_scale: u128,
    pub min_confidence_bps: u64,
    pub oracle_feed: String,
}

impl Plan {
    /// The identity a series is listed under. Two plans that agree on this are
    /// the same market, whoever created them.
    pub fn key(&self) -> (String, String, u64, u128, u128) {
        (
            self.underlying.clone(),
            self.kind.clone(),
            self.expiry_ns,
            self.low_units,
            self.high_units,
        )
    }

    /// The argument object, in the SDK's JSON dialect: `U128` wrappers as
    /// strings, `u64` and `u16` as numbers. Getting this wrong is a panic inside
    /// the contract rather than a validation error, so it is written once here.
    pub fn args(&self) -> Value {
        json!({
            "underlying": self.underlying,
            "kind": self.kind,
            "expiry_ns": self.expiry_ns,
            "accept_until_ns": self.accept_until_ns,
            "finalize_until_ns": self.finalize_until_ns,
            "low_strike": self.low_units.to_string(),
            "high_strike": self.high_units.to_string(),
            "price_scale": self.price_scale.to_string(),
            "min_confidence_bps": self.min_confidence_bps as u16,
            "oracle_feed": self.oracle_feed,
        })
    }
}

fn integer(value: &Value, field: &str) -> Result<u64, String> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{field} is not a number"))
}

/// An amount in either dialect the SDK may hand back: a quoted `U128` wrapper
/// or a bare number. Above 2^53 a JSON number is already lossy, which is why the
/// engine writes strings; reading both means a pass cannot fail on format alone.
fn amount(raw: &Value) -> Option<u128> {
    if let Some(text) = raw.as_str() {
        return text.parse::<u128>().ok();
    }
    raw.as_u64().map(u128::from)
}

fn units(value: &Value, field: &str) -> Result<u128, String> {
    let raw = value.get(field).ok_or_else(|| format!("{field} is missing"))?;
    amount(raw).ok_or_else(|| format!("{field} is not an integer"))
}

/// Read the engine's `/plan` body. A plan that does not parse is an error, not
/// an empty list: silently listing nothing would look like a settled market.
pub fn parse_plan(body: &str) -> Result<Vec<Plan>, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("plan is not JSON: {e}"))?;
    let rows = value
        .get("series")
        .and_then(Value::as_array)
        .ok_or_else(|| "plan has no series array".to_string())?;
    let mut plans = Vec::with_capacity(rows.len());
    for row in rows {
        let plan = Plan {
            underlying: row
                .get("underlying")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or("plan row has no underlying")?
                .to_string(),
            kind: row
                .get("kind")
                .and_then(Value::as_str)
                .filter(|kind| *kind == "Put" || *kind == "Call")
                .ok_or("plan row has an unknown kind")?
                .to_string(),
            expiry_ns: integer(row, "expiry_ns")?,
            accept_until_ns: integer(row, "accept_until_ns")?,
            finalize_until_ns: integer(row, "finalize_until_ns")?,
            low_units: units(row, "low_strike")?,
            high_units: units(row, "high_strike")?,
            price_scale: units(row, "price_scale")?,
            min_confidence_bps: integer(row, "min_confidence_bps")?,
            oracle_feed: row
                .get("oracle_feed")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        };
        if plan.high_units <= plan.low_units || plan.low_units == 0 || plan.price_scale == 0 {
            return Err(format!("plan row {} has broken strikes", plan.key().0));
        }
        plans.push(plan);
    }
    Ok(plans)
}

/// A series the chain already has, in the same shape as a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub underlying: String,
    pub kind: String,
    pub expiry_ns: u64,
    pub low_units: u128,
    pub high_units: u128,
}

impl Listed {
    pub fn key(&self) -> (String, String, u64, u128, u128) {
        (
            self.underlying.clone(),
            self.kind.clone(),
            self.expiry_ns,
            self.low_units,
            self.high_units,
        )
    }

    /// Read one `get_series` view result. `None` is a gap in the id range, which
    /// is what a chain that pruned or never created a series looks like.
    pub fn from_view(value: &Value) -> Option<Listed> {
        let kind = value.get("kind").and_then(Value::as_str)?;
        Some(Listed {
            underlying: value.get("underlying").and_then(Value::as_str)?.to_string(),
            kind: kind.to_string(),
            expiry_ns: value.get("expiry_ns").and_then(Value::as_u64)?,
            low_units: value.get("low_strike").and_then(amount)?,
            high_units: value.get("high_strike").and_then(amount)?,
        })
    }
}

/// The plans the chain does not have, in plan order, each at most once.
pub fn missing<'a>(planned: &'a [Plan], existing: &[Listed]) -> Vec<&'a Plan> {
    let have: std::collections::HashSet<_> = existing.iter().map(Listed::key).collect();
    let mut seen: std::collections::HashSet<_> = std::collections::HashSet::new();
    planned
        .iter()
        .filter(|plan| !have.contains(&plan.key()))
        .filter(|plan| seen.insert(plan.key()))
        .collect()
}

/// How many series the chain holds, which is also the ids to read back: they are
/// assigned from zero and never reused.
pub fn series_count(stats: &Value) -> Option<u64> {
    stats.get("series_count").and_then(Value::as_u64)
}

/// A print the engine says is reportable, in the oracle's own units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Print {
    pub price_units: u128,
    pub publish_time_ns: u64,
    pub confidence_bps: u64,
}

/// Read `GET /index`. A print that is not quotable is an error carrying the
/// engine's own reason, because the alternative would be reporting a price the
/// venue has already told its traders it does not trust.
pub fn parse_print(body: &str) -> Result<Print, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("index is not JSON: {e}"))?;
    if value.get("quotable").and_then(Value::as_bool) != Some(true) {
        let reason = value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        return Err(format!("engine will not stake a print: {reason}"));
    }
    let price_units = amount(value.get("priceUnits").ok_or("priceUnits is missing")?)
        .ok_or("priceUnits is not an integer")?;
    let publish_time_ns = integer(&value, "publishTimeNs")?;
    let confidence_bps = integer(&value, "confidenceBps")?;
    if price_units == 0 {
        return Err("a print of zero units is not a price".to_string());
    }
    if confidence_bps > 10_000 {
        return Err(format!("confidence {confidence_bps} bps is not a fraction"));
    }
    Ok(Print { price_units, publish_time_ns, confidence_bps })
}

/// A series whose expiry has passed and whose accept window has not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    pub series_id: u64,
    pub feed: String,
    pub expiry_ns: u64,
    pub accept_until_ns: u64,
}

/// ETH-USD → ETH: which engine index can fix a series on this feed.
pub fn feed_asset(feed: &str) -> Option<&str> {
    feed.split('-').next().filter(|asset| !asset.is_empty())
}

impl Due {
    pub fn asset(&self) -> Option<&str> {
        feed_asset(&self.feed)
    }
}

/// Which of the chain's series need a print *now*. Anything already Fixed,
/// Pending or Void is not due, and anything past its accept window is no longer
/// fixable — that one is a void for the holder, and reporting into it would only
/// fail on chain.
pub fn due_for_fix(rows: &[(u64, Value)], now_ns: u64) -> Vec<Due> {
    let mut due = Vec::new();
    for (series_id, row) in rows {
        if row.get("settlement").and_then(Value::as_str) != Some("Open") {
            continue;
        }
        let (Some(expiry_ns), Some(accept_until_ns)) = (
            row.get("expiry_ns").and_then(Value::as_u64),
            row.get("accept_until_ns").and_then(Value::as_u64),
        ) else {
            continue;
        };
        if expiry_ns > now_ns || now_ns > accept_until_ns {
            continue;
        }
        due.push(Due {
            series_id: *series_id,
            feed: row
                .get("oracle_feed")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            expiry_ns,
            accept_until_ns,
        });
    }
    due
}

/// The feeds that need a print, once each, as `(feed, asset)` in the order first
/// seen. One report per feed per pass covers every series on it, and the oracle
/// rejects a second observation that does not advance time — which a per-series
/// pass would trip, since a rung's put and call expire on the same clock. A feed
/// the engine cannot name is dropped: there is no index to ask for it.
pub fn feeds_needing_prints(due: &[Due]) -> Vec<(String, String)> {
    let mut seen: Vec<(String, String)> = Vec::new();
    for item in due {
        let Some(asset) = item.asset() else { continue };
        let pair = (item.feed.clone(), asset.to_string());
        if !pair.0.is_empty() && !seen.iter().any(|seen| seen.0 == pair.0) {
            seen.push(pair);
        }
    }
    seen
}

/// Where a URL points, for the plain-HTTP loopback reads the engine answers.
/// No TLS here on purpose: the engine is a local process on 127.0.0.1, and a
/// host that could reach out over the internet is a wider door than this tool
/// needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub fn parse_url(url: &str) -> Result<Target, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("{url} is not an http:// URL"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err("no host in URL".to_string());
    }
    // A bare IPv6 literal would need brackets, and no local engine uses one.
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, raw)) => (
            host.to_string(),
            raw.parse::<u16>().map_err(|_| format!("bad port in {authority}"))?,
        ),
        None => (authority.to_string(), 80),
    };
    Ok(Target { host, port, path })
}

/// One GET, read to end of body. The engine answers with `Connection: close`,
/// so the socket ending *is* the body ending — no chunked decoding needed.
pub fn http_get(url: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let target = parse_url(url)?;
    let mut stream = TcpStream::connect((target.host.as_str(), target.port))
        .map_err(|e| format!("connect {}:{}: {e}", target.host, target.port))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    write!(
        stream,
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        target.path, target.host, target.port
    )
    .map_err(|e| e.to_string())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).map_err(|e| format!("read: {e}"))?;
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| "engine replied without a header block".to_string())?;
    if !head.starts_with("HTTP/1.1 200") {
        return Err(format!("engine replied {}", head.lines().next().unwrap_or("nothing")));
    }
    if body.trim().is_empty() {
        return Err("engine replied with an empty body".to_string());
    }
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_row(low: u128, high: u128, kind: &str) -> Value {
        json!({
            "underlying": "ETH",
            "kind": kind,
            "expiry_ns": 1_700_000_000_000_000_000u64,
            "accept_until_ns": 1_700_000_060_000_000_000u64,
            "finalize_until_ns": 1_700_000_960_000_000_000u64,
            "low_strike": low.to_string(),
            "high_strike": high.to_string(),
            "price_scale": "1000000",
            "min_confidence_bps": 3000,
            "oracle_feed": "ETH-USD",
        })
    }

    fn body(rows: &[Value]) -> String {
        json!({ "asset": "ETH", "series": rows }).to_string()
    }

    fn listed(low: u128, high: u128, kind: &str, expiry: u64) -> Listed {
        Listed {
            underlying: "ETH".to_string(),
            kind: kind.to_string(),
            expiry_ns: expiry,
            low_units: low,
            high_units: high,
        }
    }

    fn row(id: u64, settlement: Value, expiry: u64, accept: u64, feed: &str) -> (u64, Value) {
        (
            id,
            json!({
                "underlying": "ETH", "kind": "Call", "settlement": settlement,
                "expiry_ns": expiry, "accept_until_ns": accept, "oracle_feed": feed,
            }),
        )
    }

    #[test]
    fn reads_the_plan_the_engine_actually_writes() {
        let plans = parse_plan(&body(&[plan_row(2_300_000_000, 2_400_000_000, "Call")])).expect("a plan");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].low_units, 2_300_000_000);
        assert_eq!(plans[0].high_units, 2_400_000_000);
        assert_eq!(plans[0].expiry_ns, 1_700_000_000_000_000_000);
        assert_eq!(plans[0].min_confidence_bps, 3000);
        assert_eq!(plans[0].oracle_feed, "ETH-USD");
    }

    #[test]
    fn args_keep_the_sds_json_convention() {
        let plans = parse_plan(&body(&[plan_row(2_300_000_000, 2_400_000_000, "Put")])).unwrap();
        let args = plans[0].args();
        assert!(args["low_strike"].is_string(), "U128 wrappers go as strings");
        assert!(args["price_scale"].is_string());
        assert!(args["expiry_ns"].is_number(), "u64 goes as a number");
        assert!(args["min_confidence_bps"].is_number(), "u16 goes as a number");
        assert_eq!(args["kind"], "Put");
        assert_eq!(args["low_strike"].as_str().unwrap().parse::<u128>().unwrap(), 2_300_000_000);
    }

    #[test]
    fn a_broken_plan_is_an_error_not_an_empty_market() {
        assert!(parse_plan("not json").unwrap_err().contains("not JSON"));
        assert!(parse_plan("{}").unwrap_err().contains("no series array"));
        assert!(parse_plan(&body(&[json!({"underlying": "ETH"})])).is_err(), "no kind");
        let inverted = plan_row(2_400_000_000, 2_300_000_000, "Call");
        assert!(parse_plan(&body(&[inverted])).unwrap_err().contains("broken strikes"));
        let zero = plan_row(0, 100, "Call");
        assert!(parse_plan(&body(&[zero])).unwrap_err().contains("broken strikes"));
        let weird = plan_row(1, 2, "Future");
        assert!(parse_plan(&body(&[weird])).unwrap_err().contains("unknown kind"), "only verticals");
    }

    #[test]
    fn an_amount_may_arrive_as_a_number_or_a_string() {
        let text = json!({"series": [{
            "underlying": "BTC", "kind": "Call",
            "expiry_ns": 1u64, "accept_until_ns": 2u64, "finalize_until_ns": 3u64,
            "low_strike": 1000, "high_strike": 2000, "price_scale": 1000000,
            "min_confidence_bps": 3000, "oracle_feed": "BTC-USD"
        }]})
        .to_string();
        let plans = parse_plan(&text).expect("numbers parse too");
        assert_eq!(plans[0].low_units, 1000);
        assert!(plans[0].args()["low_strike"].is_string(), "out always follows the SDK");
    }

    #[test]
    fn only_the_series_the_chain_lacks_are_sent() {
        let text = body(&[
            plan_row(2_300_000_000, 2_400_000_000, "Call"),
            plan_row(2_200_000_000, 2_300_000_000, "Put"),
        ]);
        let plans = parse_plan(&text).unwrap();
        let existing = vec![listed(2_200_000_000, 2_300_000_000, "Put", 1_700_000_000_000_000_000)];
        let need = missing(&plans, &existing);
        assert_eq!(need.len(), 1);
        assert_eq!(need[0].kind, "Call");
        assert_eq!(missing(&plans, &[]).len(), 2, "a fresh chain lists the whole plan");
    }

    #[test]
    fn a_different_expiry_or_side_is_a_different_series() {
        let plans = parse_plan(&body(&[plan_row(2_300_000_000, 2_400_000_000, "Call")])).unwrap();
        let other_time = vec![listed(2_300_000_000, 2_400_000_000, "Call", 1_600_000_000_000_000_000)];
        assert_eq!(missing(&plans, &other_time).len(), 1, "the old expiry is not this one");
        let other_side = vec![listed(2_300_000_000, 2_400_000_000, "Put", 1_700_000_000_000_000_000)];
        assert_eq!(missing(&plans, &other_side).len(), 1, "a put is not a call");
        let same = vec![listed(2_300_000_000, 2_400_000_000, "Call", 1_700_000_000_000_000_000)];
        assert!(missing(&plans, &same).is_empty(), "and a repeat is not a new market");
    }

    #[test]
    fn a_plan_that_lists_itself_twice_lists_it_once() {
        let row = plan_row(2_300_000_000, 2_400_000_000, "Call");
        let plans = parse_plan(&body(&[row.clone(), row])).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(missing(&plans, &[]).len(), 1);
    }

    #[test]
    fn reads_a_series_back_from_the_view() {
        let view = json!({
            "underlying": "ETH", "kind": "Call", "expiry_ns": 5u64,
            "low_strike": 2_300_000_000u64, "high_strike": 2_400_000_000u64,
        });
        let read = Listed::from_view(&view).expect("a series");
        assert_eq!(read.kind, "Call");
        assert_eq!(read.low_units, 2_300_000_000);
        // A view that quotes its amounts is the other dialect the same helper reads.
        let quoted = json!({"underlying": "ETH", "kind": "Call", "expiry_ns": 5u64,
            "low_strike": "2300000000", "high_strike": "2400000000"});
        assert_eq!(Listed::from_view(&quoted).unwrap().low_units, 2_300_000_000);
        assert!(Listed::from_view(&json!({"kind": {"Call": null}})).is_none(), "a tagged enum is not a string");
        assert!(Listed::from_view(&json!({"underlying": "ETH"})).is_none());
        assert!(Listed::from_view(&Value::Null).is_none());
    }

    #[test]
    fn stats_says_how_many_ids_to_read() {
        assert_eq!(series_count(&json!({"series_count": 9, "tickets": 3})), Some(9));
        assert_eq!(series_count(&json!({"series_count": 0})), Some(0), "a fresh chain");
        assert_eq!(series_count(&json!({})), None, "no field is not zero");
        assert_eq!(series_count(&Value::Null), None);
    }

    #[test]
    fn a_local_url_becomes_a_host_a_port_and_a_path() {
        let target = parse_url("http://127.0.0.1:8791/plan?asset=ETH&expiry_ns=5").unwrap();
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, 8791);
        assert_eq!(target.path, "/plan?asset=ETH&expiry_ns=5");
        assert_eq!(parse_url("http://localhost:80").unwrap().path, "/");
        assert_eq!(parse_url("http://127.0.0.1/state").unwrap().port, 80, "http default");
        assert!(parse_url("https://127.0.0.1/state").unwrap_err().contains("not an http"), "no TLS here");
        assert!(parse_url("127.0.0.1:8791").unwrap_err().contains("not an http"), "no scheme, no deal");
        assert!(parse_url("http://host:notaport/x").unwrap_err().contains("bad port"));
        assert!(parse_url("http:///x").unwrap_err().contains("no host"));
    }

    #[test]
    fn an_unreachable_engine_says_so_instead_of_listing_nothing_quietly() {
        // Port 1 on loopback refuses; what matters is that the failure names the
        // address, because the alternative is a boot that looks fine and lists 0.
        let error = http_get("http://127.0.0.1:1/health").unwrap_err();
        assert!(error.contains("127.0.0.1:1"), "{error}");
    }

    #[test]
    fn a_series_is_due_only_inside_its_own_window() {
        let rows = vec![
            row(7, json!("Open"), 1_000, 2_000, "ETH-USD"),
            row(8, json!("Open"), 5_000, 6_000, "ETH-USD"),
            row(9, json!("Open"), 1_000, 1_500, "BTC-USD"),
            row(10, json!({"Fixed": {"price": 1}}), 1_000, 2_000, "ETH-USD"),
            row(11, json!("Pending"), 1_000, 2_000, "ETH-USD"),
            row(12, json!("Void"), 1_000, 2_000, "ETH-USD"),
        ];
        let due = due_for_fix(&rows, 1_800);
        assert_eq!(
            due.iter().map(|d| d.series_id).collect::<Vec<_>>(),
            vec![7],
            "past expiry, inside the window, still Open: {due:?}"
        );
        assert!(due_for_fix(&rows, 900).is_empty(), "nothing is due before its expiry");
        assert!(
            due_for_fix(&rows, 2_500).iter().all(|d| d.series_id != 7),
            "and the window closes: a late print is a void, not a fix"
        );
    }

    #[test]
    fn a_broken_view_row_is_skipped_rather_than_panicking_the_pass() {
        let rows = vec![
            (1, json!({"settlement": "Open"})),
            (2, json!({"settlement": "Open", "expiry_ns": 1u64})),
            (3, Value::Null),
        ];
        assert!(due_for_fix(&rows, 10_000).is_empty(), "no expiry is not an expired series");
    }

    #[test]
    fn one_print_per_feed_no_matter_how_many_series_expire_together() {
        // The whole grid expires on the same clock, so a pass that reported per
        // series would trip the oracle's advance-time rule on the second one.
        let due = vec![
            Due { series_id: 1, feed: "ETH-USD".into(), expiry_ns: 1, accept_until_ns: 9 },
            Due { series_id: 2, feed: "ETH-USD".into(), expiry_ns: 1, accept_until_ns: 9 },
            Due { series_id: 3, feed: "BTC-USD".into(), expiry_ns: 1, accept_until_ns: 9 },
            Due { series_id: 4, feed: String::new(), expiry_ns: 1, accept_until_ns: 9 },
        ];
        assert_eq!(
            feeds_needing_prints(&due),
            vec![
                ("ETH-USD".to_string(), "ETH".to_string()),
                ("BTC-USD".to_string(), "BTC".to_string()),
            ],
            "the feedless series is dropped, not asked of an asset named \"\""
        );
        assert_eq!(due[0].asset(), Some("ETH"));
        assert_eq!(due[3].asset(), None, "a series with no feed cannot be fixed");
        assert_eq!(feed_asset("-USD"), None, "an empty asset is not a name");
    }

    #[test]
    fn a_reportable_print_carries_the_oracles_own_types() {
        let print = parse_print(
            &json!({
                "quotable": true, "price": 2350.4, "priceUnits": 2_350_400_000u64,
                "publishTimeNs": 1_793_182_584_500_000_000u64, "confidenceBps": 9_790u64,
            })
            .to_string(),
        )
        .expect("a print");
        assert_eq!(print.price_units, 2_350_400_000);
        assert_eq!(print.publish_time_ns, 1_793_182_584_500_000_000);
        assert_eq!(print.confidence_bps, 9_790);
    }

    #[test]
    fn an_unreportable_index_is_refused_with_its_reason() {
        let error =
            parse_print(&json!({"quotable": false, "reason": "one source is not an index"}).to_string()).unwrap_err();
        assert!(error.contains("one source is not an index"), "{error}");
        assert!(parse_print("{}").unwrap_err().contains("no reason given"), "a bare body is not a print");
        assert!(parse_print(&json!({"quotable": false, "reason": "x"}).to_string())
            .unwrap_err()
            .contains("x"));
        assert!(parse_print(&json!({"quotable": true}).to_string()).unwrap_err().contains("priceUnits"));
        assert!(parse_print(
            &json!({"quotable": true, "priceUnits": 0u64, "publishTimeNs": 1u64, "confidenceBps": 1u64}).to_string()
        )
        .unwrap_err()
        .contains("zero units"));
        assert!(parse_print(
            &json!({"quotable": true, "priceUnits": 1u64, "publishTimeNs": 1u64, "confidenceBps": 10_001u64}).to_string()
        )
        .unwrap_err()
        .contains("not a fraction"));
        assert!(parse_print("garbage").unwrap_err().contains("not JSON"));
    }
}
