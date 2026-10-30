//! The operator's three jobs: read what the engine says the market should be,
//! tell what the chain already has, and name what still needs signing — a series
//! to list, a print to fix it, a quote for the vault to trade at.
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

/// How long the contract lets a venue quote stay payable, for a series on this
/// ladder. It mirrors the contract's own two tiers: a market inside a day is
/// quoted as if it were live, and a week-out market can be renewed less often
/// because its price barely moves in a minute. An operator renewing on a different
/// clock than the one that ages quotes out is a vault that stops trading on
/// schedule, and says so with "the venue has not quoted this series".
pub const QUOTE_AGE_NEAR_NS: u64 = 60_000_000_000;
pub const QUOTE_AGE_FAR_NS: u64 = 300_000_000_000;
const DAY_NS: u64 = 24 * 3_600_000_000_000;

pub fn quote_age(expiry_ns: u64, now_ns: u64) -> u64 {
    if expiry_ns.saturating_sub(now_ns) <= DAY_NS {
        QUOTE_AGE_NEAR_NS
    } else {
        QUOTE_AGE_FAR_NS
    }
}

/// Which ladders a pass should quote, soonest-overdue first and capped, so a pass
/// stays finite while no ladder is left past its renewal point. Renewing at half
/// the age limit is the slack that keeps a slow pass — or a transaction still in
/// flight — from landing a quote past its own expiry.
pub fn ladders_due(
    expiries: &[u64],
    now_ns: u64,
    last_quoted: &std::collections::HashMap<u64, u64>,
    limit: usize,
) -> Vec<u64> {
    let mut scored: Vec<(u64, u64)> = Vec::new();
    for expiry_ns in expiries {
        // Renew at a third of the limit, not half. The sandbox clock runs faster
        // than the wall clock, so sixty seconds of chain time is nearer twenty of
        // ours — half a limit is no slack at all there.
        let renew_ns = quote_age(*expiry_ns, now_ns) / 3;
        let overdue = match last_quoted.get(expiry_ns) {
            None => u64::MAX,
            Some(&at) => now_ns.saturating_sub(at).saturating_sub(renew_ns),
        };
        if overdue > 0 {
            scored.push((overdue, *expiry_ns));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, expiry)| expiry).collect()
}

/// A series whose expiry has passed and whose accept window has not: the ones a
/// print can still turn into a payout this pass.
pub fn due_for_fix(rows: &[(u64, Value)], now_ns: u64) -> Vec<u64> {
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
        due.push(*series_id);
    }
    due
}

/// One side of one ladder row, as the engine marked it. `mid_units` is the
/// number the vault trades on: the contract does the fee arithmetic itself, so
/// the host's job stops at naming the mid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotePlan {
    pub kind: String,
    pub low_units: u128,
    pub high_units: u128,
    pub mid_units: u128,
}

/// The quote a series already has on chain, from `get_pool_quote`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
    pub mid_units: u128,
    pub stamp_ns: u64,
}

/// Read a `get_pool_quote` reply.
///
/// The mid is a `U128`, which the contract's serializer writes as a *string*, and
/// the stamp is a plain `u64`. Reading the string as a number returns nothing, so
/// a host that does that believes no series has a quote and re-posts all of them
/// on every tick — a still index costing a transaction per cell per tick.
pub fn standing_quote(raw: &Value) -> Option<Standing> {
    Some(Standing {
        mid_units: amount(raw.get("mid_units")?)?,
        stamp_ns: raw.get("stamp_ns").and_then(Value::as_u64)?,
    })
}

/// Read `GET /state`: the expiry its cells were marked against, and the mids.
///
/// A cell the engine will not sell has no mid and is left out rather than
/// carried as zero — the vault has no price for it, which is a different fact
/// from a price of nothing, and `set_pool_quote` refuses the second one.
pub fn parse_state(body: &str) -> Result<(u64, Vec<QuotePlan>), String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("state is not JSON: {e}"))?;
    let expiry_ns = amount(value.get("expiryNs").ok_or("state has no expiryNs")?)
        .ok_or("expiryNs is not an integer")?;
    let rows = value
        .get("chain")
        .and_then(Value::as_array)
        .ok_or_else(|| "state has no chain array".to_string())?;
    let mut plans = Vec::new();
    for row in rows {
        for (field, kind) in [("put", "Put"), ("call", "Call")] {
            let Some(cell) = row.get(field) else { continue };
            plans.push(QuotePlan {
                kind: kind.to_string(),
                low_units: units(cell, "low")?,
                high_units: units(cell, "high")?,
                mid_units: units(cell, "midUnits")?,
            });
        }
    }
    Ok((expiry_ns as u64, plans))
}

/// The index the engine marked a ladder with, in the same units as the strikes.
///
/// The maker needs it to know which strike is "nearest the money"; it is the same
/// number the screen shows, so the cells a person sees at the top of the ladder
/// are the cells a person's offers land on.
pub fn parse_spot(body: &str) -> Option<u128> {
    let value: Value = serde_json::from_str(body).ok()?;
    let spot = value.get("spot").and_then(Value::as_f64)?;
    if !spot.is_finite() || spot <= 0.0 {
        return None;
    }
    Some((spot * 1_000_000.0).round() as u128)
}

/// Series whose accept window closed while still Open.
///
/// A series can only be fixed inside that window, so once it has passed, every
/// ticket in it is stuck: `settle_ticket` refuses an unfixed series and the vault
/// will not buy it back. Voiding is the only exit left, and it is permissionless
/// on purpose — the operator is the party that can see the window close.
pub fn voidable(rows: &[(u64, Value)], now_ns: u64) -> Vec<u64> {
    let mut out = Vec::new();
    for (series_id, row) in rows {
        if row.get("settlement").and_then(Value::as_str) != Some("Open") {
            continue;
        }
        let Some(finalize_until_ns) = row.get("finalize_until_ns").and_then(Value::as_u64) else {
            continue;
        };
        if now_ns >= finalize_until_ns {
            out.push(*series_id);
        }
    }
    out
}

/// Which open series need a quote posted, as `(series_id, mid_units)`.
///
/// A quote is reposted when the price moved or when it is older than
/// `renew_ns`, so a running index costs a transaction per move and a still one
/// costs almost nothing — and the vault never has to reach for a price the
/// contract would call expired. Series the engine has no rung for are left
/// alone: their standing quote simply ages out and the vault stops trading them,
/// which is the correct outcome when the venue has nothing to say.
pub fn quotes_needed(
    plans: &[QuotePlan],
    expiry_ns: u64,
    rows: &[(u64, Value)],
    live: &std::collections::HashMap<u64, Standing>,
    now_ns: u64,
    renew_ns: u64,
) -> Vec<(u64, u128)> {
    let mut needed: Vec<(u64, u128)> = Vec::new();
    for plan in plans.iter().filter(|plan| plan.mid_units > 0) {
        let found = rows.iter().find(|(_, row)| {
            row.get("settlement").and_then(Value::as_str) == Some("Open")
                && row.get("expiry_ns").and_then(Value::as_u64) == Some(expiry_ns)
                && Listed::from_view(row)
                    .is_some_and(|listed| {
                        listed.kind == plan.kind
                            && listed.low_units == plan.low_units
                            && listed.high_units == plan.high_units
                    })
        });
        let Some((series_id, _)) = found else { continue };
        if !needed.iter().any(|(id, _)| id == series_id) {
            let repost = match live.get(series_id) {
                None => true,
                Some(standing) => {
                    standing.mid_units != plan.mid_units
                        || now_ns.saturating_sub(standing.stamp_ns) > renew_ns
                }
            };
            if repost {
                needed.push((*series_id, plan.mid_units));
            }
        }
    }
    needed
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
    use std::collections::HashMap;

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
    fn a_near_ladder_is_renewed_more_often_than_a_far_one() {
        let hour = 3_600_000_000_000u64;
        let near = 1_000 + hour;
        let far = 1_000 + 7 * 24 * hour;
        let mut last = std::collections::HashMap::new();
        last.insert(near, 1_000u64);
        last.insert(far, 1_000u64);
        // Forty seconds after both were quoted: the near one is past half its
        // sixty-second life, the far one has minutes to spare.
        assert_eq!(
            ladders_due(&[near, far], 1_000 + 40 * 1_000_000_000, &last, 4),
            vec![near],
            "the ladder a trader can click today is the one that gets the next pass"
        );
        assert_eq!(quote_age(near, 1_000), QUOTE_AGE_NEAR_NS);
        assert_eq!(quote_age(far, 1_000), QUOTE_AGE_FAR_NS);
    }

    #[test]
    fn a_ladder_never_quoted_is_due_and_the_most_overdue_goes_first() {
        let hour = 3_600_000_000_000u64;
        let a = 1_000 + hour;
        let b = 1_000 + 2 * hour;
        let c = 1_000 + 3 * hour;
        let mut last = std::collections::HashMap::new();
        last.insert(a, 1_000u64);
        last.insert(b, 1_000 + 50 * 1_000_000_000);
        let now = 1_000 + 70 * 1_000_000_000;
        assert_eq!(
            ladders_due(&[b, a, c], now, &last, 4),
            vec![c, a],
            "the unquoted ladder first, then the one whose quote is oldest"
        );
        assert_eq!(ladders_due(&[b, a, c], now, &last, 1).len(), 1, "and a pass stays bounded");
        let fresh: std::collections::HashMap<u64, u64> =
            [(a, now), (b, now), (c, now)].into_iter().collect();
        assert!(ladders_due(&[a, b, c], now, &fresh, 4).is_empty(), "nothing renewed this pass");
    }

    #[test]
    fn the_index_comes_back_in_the_units_the_strikes_use() {
        let body = json!({ "spot": 2360.42, "expiryNs": "1", "chain": [] });
        assert_eq!(parse_spot(&body.to_string()), Some(2_360_420_000));
        assert_eq!(parse_spot(&json!({ "spot": null }).to_string()), None);
        assert_eq!(parse_spot("not json"), None);
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
        assert_eq!(due, vec![7], "past expiry, inside the window, still Open");
        assert!(due_for_fix(&rows, 900).is_empty(), "nothing is due before its expiry");
        assert!(
            !due_for_fix(&rows, 2_500).contains(&7),
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

    fn cell(low: u128, high: u128, mid: u128) -> Value {
        json!({
            "low": low.to_string(), "high": high.to_string(),
            "midUnits": mid.to_string(), "openable": mid > 0,
        })
    }

    fn state(expiry: u64, chain: &[Value]) -> String {
        json!({ "asset": "ETH", "expiryNs": expiry.to_string(), "chain": chain }).to_string()
    }

    fn open_row(expiry: u64, low: u128, high: u128, kind: &str) -> Value {
        json!({
            "underlying": "ETH", "kind": kind, "settlement": "Open", "expiry_ns": expiry,
            "low_strike": low.to_string(), "high_strike": high.to_string(),
        })
    }

    #[test]
    fn reads_both_sides_of_a_ladder_row() {
        let (expiry, plans) = parse_state(&state(
            900,
            &[json!({ "strike": 2_400.0, "put": cell(2_300, 2_400, 55_000), "call": cell(2_400, 2_500, 71_000) })],
        ))
        .expect("a state");
        assert_eq!(expiry, 900);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].kind, "Put");
        assert_eq!(plans[0].mid_units, 55_000);
        assert_eq!(plans[1].kind, "Call");
        assert_eq!(plans[1].low_units, 2_400);
    }

    #[test]
    fn a_cell_the_engine_wont_sell_is_no_quote_rather_than_a_free_one() {
        // midUnits is "0" when a wing rounds away or a spread costs more than it
        // can pay. Posting that would ask the contract to price the vault at
        // nothing; leaving it out lets its last quote expire instead.
        let (_, plans) = parse_state(&state(900, &[json!({ "put": cell(1, 2, 0) })])).unwrap();
        assert_eq!(plans[0].mid_units, 0);
        let rows = [(0u64, open_row(900, 1, 2, "Put"))];
        assert!(
            quotes_needed(&plans, 900, &rows, &HashMap::new(), 901, 30).is_empty(),
            "a zero mid never reaches set_pool_quote"
        );
    }

    #[test]
    fn state_that_is_not_a_ladder_is_an_error_not_an_empty_quote_run() {
        assert!(parse_state("garbage").unwrap_err().contains("not JSON"));
        assert!(parse_state("{}").unwrap_err().contains("expiryNs"));
        assert!(parse_state(&json!({"expiryNs": "9"}).to_string())
            .unwrap_err()
            .contains("no chain array"));
        assert!(parse_state(&state(9, &[json!({ "put": {"low": "1"} })])).is_err(), "a half cell");
    }

    #[test]
    fn quotes_are_posted_for_the_series_the_chain_actually_has_open() {
        let plans = vec![
            QuotePlan { kind: "Put".into(), low_units: 2_300, high_units: 2_400, mid_units: 55_000 },
            QuotePlan { kind: "Call".into(), low_units: 2_400, high_units: 2_500, mid_units: 71_000 },
        ];
        let rows = vec![
            (0, open_row(900, 2_300, 2_400, "Put")),
            // Same strikes, different side: the rung is addressed by both.
            (1, open_row(900, 2_300, 2_400, "Call")),
            // Last week's series on the same strikes must not be re-quoted.
            (2, open_row(800, 2_300, 2_400, "Put")),
        ];
        let need = quotes_needed(&plans, 900, &rows, &HashMap::new(), 901, 30);
        assert_eq!(need, vec![(0, 55_000)], "one series, the price the engine marked");
        // A call at 2_400/2_500 has no rung on this chain, so nothing is posted.
        assert_eq!(need.len(), 1);

        let mut settled = open_row(900, 2_300, 2_400, "Put");
        settled["settlement"] = json!({"Fixed": {"price": 2_350}});
        assert!(
            quotes_needed(&plans, 900, &[(0, settled)], &HashMap::new(), 901, 30).is_empty(),
            "a fixed series has no spread left to sell"
        );
    }

    #[test]
    fn a_quote_is_reposted_when_it_moves_or_goes_stale_not_every_pass() {
        let plans = vec![QuotePlan {
            kind: "Put".into(),
            low_units: 2_300,
            high_units: 2_400,
            mid_units: 55_000,
        }];
        let rows = vec![(0, open_row(900, 2_300, 2_400, "Put"))];
        let fresh = HashMap::from([(0, Standing { mid_units: 55_000, stamp_ns: 900 })]);
        assert!(
            quotes_needed(&plans, 900, &rows, &fresh, 910, 30).is_empty(),
            "unchanged and young: no transaction"
        );
        // The price moved off the index, so the old number must be replaced.
        let moved = quotes_needed(&plans, 900, &rows, &HashMap::from([(0, Standing {
            mid_units: 50_000,
            stamp_ns: 910,
        })]), 911, 30);
        assert_eq!(moved, vec![(0, 55_000)]);
        // And a price that has simply aged out is refreshed before the contract
        // would refuse it.
        let aged = quotes_needed(&plans, 900, &rows, &fresh, 931, 30);
        assert_eq!(aged, vec![(0, 55_000)], "31 ns past a 30 ns renew window");
        assert!(quotes_needed(&plans, 900, &rows, &fresh, 929, 30).is_empty());
    }

    #[test]
    fn a_series_that_ran_out_of_window_is_voidable() {
        let mut row = open_row(900, 2_300, 2_400, "Put");
        row["finalize_until_ns"] = json!(1_000u64);
        let rows = vec![(7, row.clone())];
        assert!(voidable(&rows, 999).is_empty(), "the window is still open");
        assert_eq!(voidable(&rows, 1_000), vec![7], "the deadline itself counts");
        row["settlement"] = json!("Fixed");
        assert!(voidable(&vec![(7, row)], 2_000).is_empty(), "a fixed series is settled, not voided");
    }

    #[test]
    fn a_standing_quote_is_read_in_the_dialect_the_contract_writes() {
        // `U128` comes back as a string and a raw `u64` as a number. Reading the
        // string as a number gives nothing, which the host would take for "no
        // quote yet" and answer by re-posting every cell on every pass.
        let live = standing_quote(&json!({
            "mid_units": "62187841",
            "stamp_ns": 1_793_322_190_933_147_000u64,
        }))
        .expect("the venue's own reply");
        assert_eq!(live.mid_units, 62_187_841);
        assert_eq!(live.stamp_ns, 1_793_322_190_933_147_000);
        // An unquoted series answers null, and a reply missing either half is not
        // an age the renewal clock can be measured against.
        assert!(standing_quote(&Value::Null).is_none());
        assert!(standing_quote(&json!({ "mid_units": "5" })).is_none());
        assert!(standing_quote(&json!({ "stamp_ns": 1 })).is_none());
    }
}
