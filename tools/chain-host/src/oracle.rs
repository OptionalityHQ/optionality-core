//! The relay's side of the committee: a socket per seat, and nothing else.
//!
//! The relay does not fetch the committee's prices and does not hold its keys. It
//! starts one signer per seat, asks each of them what it saw about a feed, and
//! delivers the signed answer. That is the whole of its power — and the reason a
//! seat's signature is worth something: it was produced by the process that did the
//! reading, in a process that never saw the relay's other three keys.

use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// One seat's answer, exactly as the signer signed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub price_units: u128,
    pub publish_time_ns: u64,
    pub confidence_bps: u16,
    pub signature: Vec<u8>,
}

impl Observation {
    /// The oracle's input: a signed observation, ready to submit.
    pub fn item(&self, source: &str, feed: &str) -> serde_json::Value {
        json!({
            "feed_id": feed,
            "source": source,
            "price": self.price_units,
            "publish_time_ns": self.publish_time_ns,
            "confidence_bps": self.confidence_bps,
            "signature": self.signature,
        })
    }
}

/// A seat: a name the oracle knows, and a socket to the process that holds the only
/// key able to speak for it.
#[derive(Clone)]
pub struct Seat {
    pub name: &'static str,
    pub socket: PathBuf,
    pub public_key: Vec<u8>,
}

impl Seat {
    /// The registration call for this seat, as the oracle wants it.
    pub fn registration(&self) -> serde_json::Value {
        json!({ "name": self.name, "public_key": self.public_key })
    }

    /// Ask the seat what it saw about one feed. The answer is already signed; the
    /// relay's only choice is whether to deliver it.
    pub fn observe(&self, feed: &str) -> Result<Observation, String> {
        let reply = self.ask(&format!("observe {feed}"))?;
        let parts: Vec<&str> = reply.split_whitespace().collect();
        if parts.len() != 4 {
            return Err(format!("{} answered {reply:?}, not an observation", self.name));
        }
        let parse = |value: &str, field: &str| -> Result<u128, String> {
            value
                .parse()
                .map_err(|_| format!("{} sent a {field} that is not a number: {value}", self.name))
        };
        Ok(Observation {
            price_units: parse(parts[0], "price")?,
            publish_time_ns: parse(parts[1], "timestamp")? as u64,
            confidence_bps: parse(parts[2], "confidence")? as u16,
            signature: hex::decode(parts[3])
                .map_err(|error| format!("{} sent a signature that is not hex: {error}", self.name))?,
        })
    }

    fn ask(&self, line: &str) -> Result<String, String> {
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

/// The seats, in the order the panel shows them. Ours is named for what it is — the
/// venue's own index — and it gets exactly one vote, like every other row.
pub const SEAT_NAMES: [&'static str; 4] = ["venue-index", "kraken", "okx", "pyth"];

/// Start one signer process per seat and read the public key each of them reports.
///
/// The keys are generated inside those processes and never leave them: this
/// function learns only what it needs to seat them on chain. The children are
/// returned so the caller can keep them alive for the life of the venue.
pub fn start_committee(
    root: &Path,
    engine: &str,
) -> Result<(Vec<Seat>, Vec<std::process::Child>), String> {
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
            .arg("--source")
            .arg(name)
            .arg("--engine")
            .arg(engine)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A stand-in signer: one unix socket, one line in, one line out.
    fn stub(reply: &'static str) -> (Seat, std::thread::JoinHandle<String>) {
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

    #[test]
    fn the_relay_asks_and_takes_what_it_is_given() {
        let signature = "ab".repeat(64);
        let (seat, asked) = stub(Box::leak(
            format!("ok 1000000 1793000000000000000 9900 {signature}").into_boxed_str(),
        ));
        let observation = seat.observe("BTC-USD").expect("an observation");
        assert_eq!(asked.join().expect("stub"), "observe BTC-USD", "the relay names the feed and nothing else");
        assert_eq!(observation.price_units, 1_000_000);
        assert_eq!(observation.publish_time_ns, 1_793_000_000_000_000_000);
        assert_eq!(observation.confidence_bps, 9_900);
        assert_eq!(observation.signature.len(), 64);
        let item = observation.item("kraken", "BTC-USD");
        assert_eq!(item["source"], "kraken");
        assert_eq!(item["price"], 1_000_000, "u128 goes as a number, the way the contract reads it");
    }

    #[test]
    fn a_refusal_is_an_error_not_an_empty_observation() {
        let (seat, _) = stub("err kraken says nothing about BTC-USD");
        let error = seat.observe("BTC-USD").expect_err("must refuse");
        assert!(error.contains("refused: err kraken"), "{error}");
    }

    #[test]
    fn a_malformed_reply_is_an_error() {
        let (seat, _) = stub("ok 1000000 1793000000000000000");
        assert!(seat.observe("BTC-USD").unwrap_err().contains("not an observation"));
    }

    #[test]
    fn a_dead_signer_is_reported_by_name() {
        let seat = Seat {
            name: "okx",
            socket: std::env::temp_dir().join("oracle-no-such-signer.sock"),
            public_key: vec![0u8; 32],
        };
        let error = seat.observe("BTC-USD").expect_err("no signer");
        assert!(error.contains("okx is not answering"), "{error}");
    }

    #[test]
    fn the_committee_names_four_seats() {
        assert_eq!(SEAT_NAMES, ["venue-index", "kraken", "okx", "pyth"]);
    }
}
