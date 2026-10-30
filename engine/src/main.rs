//! The venue's process: a feed in, a market out.
//!
//! Deliberately thin. Everything that decides a number is in the library
//! (`ladder`, `quote`, `serve`, `feed`) and therefore under test; what is left
//! here is process wiring: start the tape, answer HTTP, print a status line.
//!
//! ```text
//!   cargo run -p engine -- --port 8791                    # the three venue sockets
//!   cargo run -p engine -- --replay data/feed-sample.jsonl  # a recorded tape
//!   curl 127.0.0.1:8791/plan?asset=ETH&expiry_ns=…
//! ```
//! `/plan` is the answer to "which series has to exist on chain for this ladder"
//! and is the only place the engine and the chain meet without a trade in between.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use bounded_core::fees::Terms;
use engine::feed::{self, Counts};
use engine::ladder::{self, Asset};
use engine::serve::{self, Route, Tape, Venue};

/// What the process was told to do. Every field has a default that runs the
/// demo, so `engine` with no arguments is a working market on one port.
struct Config {
    port: u16,
    assets: Vec<Asset>,
    /// The nearest expiry the venue lists. Everything after it comes from the
    /// schedule, so a three-minute tenor is a demo knob and a four-hour one is
    /// the day it ships with.
    expiry_hours: f64,
    /// Whole-hour markets in front of the daily closes. Each one is another quote
    /// to renew inside the contract's sixty seconds, so it is a knob: zero is a
    /// clean schedule, one or two is a nicer strip.
    hours: u64,
    venue: Venue,
    replay: Option<PathBuf>,
    no_feed: bool,
    quiet: bool,
    counts: Arc<Counts>,
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let index = args.iter().position(|a| a == name)?;
    args.get(index + 1).map(|v| v.to_string())
}

fn parse(args: &[String]) -> Config {
    let mut venue = Venue::default();
    if let Some(vol) = flag_value(args, "--vol").and_then(|v| v.parse::<f64>().ok()) {
        venue.atm_vol = vol;
    }
    let mut terms = Terms::STANDARD;
    if let Some(margin) = flag_value(args, "--margin").and_then(|v| v.parse::<u32>().ok()) {
        terms.margin_bps = margin;
    }
    if let Some(impact) = flag_value(args, "--impact").and_then(|v| v.parse::<u32>().ok()) {
        terms.impact_bps = impact;
    }
    venue.terms = terms;
    let assets = flag_value(args, "--assets")
        .map(|list| {
            list.split(',')
                .filter_map(Asset::parse)
                .collect::<Vec<_>>()
        })
        .filter(|list| !list.is_empty())
        .unwrap_or_else(|| vec![Asset::Eth, Asset::Btc]);
    Config {
        port: flag_value(args, "--port").and_then(|v| v.parse().ok()).unwrap_or(8791),
        assets,
        expiry_hours: flag_value(args, "--expiry-hours")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|hours| *hours > 0.0)
            .unwrap_or(4.0),
        hours: flag_value(args, "--hours")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|hours| *hours <= 4)
            .unwrap_or(0),
        venue,
        replay: flag_value(args, "--replay").map(PathBuf::from),
        no_feed: args.iter().any(|a| a == "--no-feed"),
        quiet: args.iter().any(|a| a == "--quiet"),
        counts: Arc::new(Counts::default()),
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    now_ns() / 1_000_000
}

/// Replay a recorded tape instead of holding sockets: the same lines the live
/// feed would have produced, read as fast as the file gives them. The thread
/// ends with the file and the engine keeps serving the last index it saw, which
/// reads as `stale` after the window rather than as a live market.
fn read_tape(path: &std::path::Path, tape: &Arc<Mutex<Tape>>, counts: &Arc<Counts>) {
    let Ok(file) = std::fs::File::open(path) else { return };
    let mut ingested = 0u64;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if let Some(tick) = serve::parse_tick(&line) {
            if let Ok(mut tape) = tape.lock() {
                tape.ingest(&tick);
            }
            counts.ticks.fetch_add(1, Ordering::Relaxed);
            ingested += 1;
        }
    }
    println!("tape replayed · {} prints from {}", ingested, path.display());
}

fn respond(stream: &mut TcpStream, body: &str, status: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\n\
         Access-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// One request, one answer. Sequential is enough: a GET here is a few kilobytes
/// of numbers and the venue has one screen and one gateway pointed at it.
fn handle(stream: &mut TcpStream, tape: &Arc<Mutex<Tape>>, config: &Config, expiries: &[u64]) {
    let Ok(mut peer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(&mut peer);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    // The headers are read and thrown away. Answering while part of the request
    // is still in the kernel's buffer turns the close into a reset, and a reset
    // the client sees mid-read looks exactly like an engine that is not running.
    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        if header.trim().is_empty() {
            break;
        }
    }
    drop(reader);
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
    match serve::route(&path) {
        // `expiryNs` is a JSON number on purpose: the operator reads it back to ask
        // for that exact ladder, and a nanosecond epoch that has been through a
        // float is no longer the expiry the chain stored.
        Route::Health => {
            let (ticks, reconnects, sockets) = config.counts.snapshot();
            let upcoming = serve::upcoming(expiries, now_ns());
            let body = serde_json::json!({
                "ok": true,
                "engine": "bounded-options",
                // The nearest expiry stays at the top level for callers that only
                // want one, and the whole ladder is listed beside it.
                "expiryNs": upcoming[0],
                "expiries": upcoming,
                "assets": config
                    .assets
                    .iter()
                    .map(|asset| asset.symbol())
                    .collect::<Vec<_>>(),
                "atmVol": config.venue.atm_vol,
                "marginBps": config.venue.terms.margin_bps,
                "impactBps": config.venue.terms.impact_bps,
                "feed": { "ticks": ticks, "reconnects": reconnects, "sockets": sockets },
            });
            respond(stream, &body.to_string(), "200 OK");
        }
        Route::Smile(asset) => {
            let (spot, summary) = {
                let guard = tape.lock().expect("the tape is only held while copying prints out of it");
                let summary = guard.summary(asset, now_ms(), &config.venue.stale);
                (summary.observation(now_ms()).map(|observation| observation.price), summary)
            };
            let _ = summary;
            let body = serve::smile_json(
                asset,
                spot,
                config.venue.atm_vol,
                now_ns(),
                &serve::upcoming(expiries, now_ns()),
                // One curve: the model carries a single σ_atm, so a second tenor
                // would be the same line drawn again. When the surface grows a term
                // structure, this number is the only thing that changes.
                1,
            );
            respond(stream, &body, "200 OK");
        }
        Route::State(asset, wanted) => {
            let expiry_ns = wanted.unwrap_or_else(|| serve::upcoming(expiries, now_ns())[0]);
            let (body, listed, summary) = {
                let guard = tape.lock().expect("the tape is only held while copying prints out of it");
                let summary = guard.summary(asset, now_ms(), &config.venue.stale);
                let specs = summary
                    .median
                    .map(|spot| ladder::plan(asset, spot, expiry_ns, &config.venue.params))
                    .unwrap_or_default();
                (
                    serve::state_json(asset, &summary, &specs, expiry_ns, now_ms(), &config.venue),
                    specs.len(),
                    summary,
                )
            };
            // One line per poll, so an operator can see whether the tape went
            // quiet or the model went away — the two failures look the same on
            // a screen that only shows blanks.
            println_if(
                !config.quiet,
                &serve::status_line(asset, &summary, listed, &config.counts.line()),
            );
            respond(stream, &body, "200 OK");
        }
        // A print the venue would not stake a settlement on answers 503 with the
        // reason, so a reporter cannot mistake it for a price.
        Route::Index(asset, scale) => {
            let body = {
                let guard = tape.lock().expect("the tape is only held while copying prints out of it");
                serve::index_json(
                    asset,
                    &guard.summary(asset, now_ms(), &config.venue.stale),
                    now_ms(),
                    scale,
                )
            };
            // Answered with its own verdict in the body rather than as a status
            // code: a degraded index is still an index, the print says how much
            // it is worth, and the caller that needs a live one reads `quotable`.
            // A 503 here made one stale socket look like a transport failure, and
            // the operator dropped a whole settlement pass on it.
            respond(stream, &body, "200 OK");
        }
        Route::Feed => {
            let (_ticks, reconnects, sockets) = config.counts.snapshot();
            let body = {
                let guard = tape.lock().expect("the tape is only held while copying prints out of it");
                serve::feed_json(&guard, &config.assets, now_ms(), &config.venue.stale, sockets, reconnects)
            };
            respond(stream, &body, "200 OK");
        }
        Route::Plan(asset, expiry_ns) => {
            let spot = {
                let guard = tape.lock().expect("the tape is only held while copying prints out of it");
                guard.summary(asset, now_ms(), &config.venue.stale).median
            };
            match spot {
                Some(spot) => respond(stream, &serve::plan_json(asset, spot, expiry_ns, &config.venue), "200 OK"),
                None => respond(stream, "{\"error\":\"no index yet\"}", "503 Service Unavailable"),
            }
        }
        Route::Unknown => respond(stream, "{\"error\":\"unknown path\"}", "404 Not Found"),
    }
}

fn println_if(enabled: bool, line: &str) {
    if enabled {
        println!("{line}");
        let _ = std::io::stdout().flush();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help") {
        println!("engine [--port 8791] [--assets ETH,BTC] [--expiry-hours 4] [--vol 0.6]");
        println!("       [--margin 400] [--impact 1000] [--replay file.jsonl | --no-feed]");
        println!("       [--quiet]   GET /health /state?asset= /plan?asset=&expiry_ns=");
        return Ok(());
    }
    let config = parse(&args);
    let near_ns = (config.expiry_hours * 3_600_000_000_000.0) as u64;
    let expiries = serve::schedule(now_ns(), near_ns, config.hours);
    let tape = Arc::new(Mutex::new(Tape::default()));
    let running = Arc::new(AtomicBool::new(true));

    println_if(
        !config.quiet,
        &format!(
            "bounded engine · port {} · assets {} · {} expiries (near {} h) · vol {} · margin {} bps · impact {} bps",
            config.port,
            config
                .assets
                .iter()
                .map(|a| a.symbol())
                .collect::<Vec<_>>()
                .join(","),
            expiries.len(),
            config.expiry_hours,
            config.venue.atm_vol,
            config.venue.terms.margin_bps,
            config.venue.terms.impact_bps
        ),
    );

    if let Some(path) = config.replay.clone() {
        let tape = Arc::clone(&tape);
        let counts = Arc::clone(&config.counts);
        thread::spawn(move || read_tape(&path, &tape, &counts));
    } else if !config.no_feed {
        // Three threads, one per venue, each reconnecting on its own schedule.
        // Their handles are dropped on purpose: they run until this process ends.
        feed::spawn(Arc::clone(&tape), Arc::clone(&config.counts), Arc::clone(&running));
    }

    let listener = TcpListener::bind(("127.0.0.1", config.port))?;
    println_if(!config.quiet, &format!("listening  http://127.0.0.1:{}", config.port));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        handle(&mut stream, &tape, &config, &expiries);
    }
    running.store(false, Ordering::Relaxed);
    Ok(())
}
