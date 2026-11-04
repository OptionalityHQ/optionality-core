//! One seat's key and one seat's read, in a process of its own.
//!
//! The relay fetches nothing and signs nothing for a seat: it asks "what did you
//! see about this feed", and this process fetches the source, refuses anything that
//! is not a price, signs what it read, and answers. That is what makes a seat's
//! signature mean something — a signature is a claim this process made about its
//! own read, not a favour it did for whoever could open a socket.
//!
//! The protocol is one line in, one line out:
//!
//!     pubkey         → ok <hex public key>
//!     observe <feed> → ok <price> <ns> <bps> <hex signature>
//!
//! Nothing else. There is deliberately no "sign this number for me" request: that
//! was the hole this replaced.

use bounded_core::oracle::signing_message;
use ed25519_dalek::{Signer, SigningKey};
use oracle_sources::{Tick, FEEDS};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

/// How often a seat will re-read a source, however often it is asked.
///
/// The socket is a door to somebody else's server: without this, any local process
/// could put a loop on `observe` and have the seat hammer Kraken until the seat —
/// and with it a vote — is rate limited out of the committee.
const MIN_FETCH_GAP: Duration = Duration::from_secs(1);

fn main() -> std::io::Result<()> {
    let name = arg("--name", "seat");
    let source = arg("--source", &name);
    let engine = arg("--engine", "http://127.0.0.1:8791");
    let socket = arg("--socket", "data/oracle/seat.sock");
    let key = match arg_opt("--key-file") {
        Some(path) => load_or_create_key(&path)?,
        None => SigningKey::from_bytes(&random_seed()),
    };
    // Prepare a seat's identity and stop. Deploying to a public network needs the
    // public key before any signer runs, because the seat has to be registered on
    // the oracle contract before the venue can settle anything.
    if let Some(path) = arg_opt("--registration-out") {
        let registration = serde_json::json!({
            "name": name,
            "public_key": key.verifying_key().to_bytes().to_vec(),
        });
        if let Some(parent) = Path::new(&path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, format!("{registration}\n"))?;
        println!(
            "registration {path} {}",
            hex::encode(key.verifying_key().to_bytes())
        );
        return Ok(());
    }
    if let Some(parent) = Path::new(&socket).parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A socket left by a killed run would fail to bind; nothing else lives here.
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    let cache: RefCell<HashMap<String, (Instant, Tick)>> = RefCell::new(HashMap::new());
    println!(
        "ready {name} {}",
        hex::encode(key.verifying_key().to_bytes())
    );
    std::io::stdout().flush()?;
    for stream in listener.incoming().flatten() {
        if let Err(error) = serve(stream, &name, &source, &engine, &key, &cache) {
            eprintln!("{name}: {error}");
        }
    }
    Ok(())
}

/// Answer one request. A malformed line is an error on that line, not a reason to
/// exit: a seat that dies on a typo takes a vote with it.
fn serve(
    mut stream: UnixStream,
    name: &str,
    source: &str,
    engine: &str,
    key: &SigningKey,
    cache: &RefCell<HashMap<String, (Instant, Tick)>>,
) -> std::io::Result<()> {
    let mut line = String::new();
    if BufReader::new(stream.try_clone()?).read_line(&mut line)? == 0 {
        return Ok(());
    }
    let fetch = |feed: &str| -> Result<Tick, String> {
        observe_cached(cache, feed, &|feed| {
            let known = FEEDS
                .iter()
                .find(|entry| entry.id == feed)
                .ok_or_else(|| format!("{feed} is not a feed this seat reads"))?;
            oracle_sources::fetch(source, known, engine)
        })
    };
    let reply = answer(line.trim(), name, key, &fetch);
    writeln!(stream, "{reply}")?;
    stream.flush()
}

fn answer(
    line: &str,
    name: &str,
    key: &SigningKey,
    fetch: &dyn Fn(&str) -> Result<Tick, String>,
) -> String {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("pubkey") => format!("ok {}", hex::encode(key.verifying_key().to_bytes())),
        Some("observe") => {
            let Some(feed) = parts.next() else {
                return "err observe wants <feed>".to_string();
            };
            let tick = match fetch(feed) {
                Ok(tick) => tick,
                Err(error) => return format!("err {error}"),
            };
            if tick.price_units == 0 {
                return "err refusing to sign a non-price".to_string();
            }
            if tick.confidence_bps > 10_000 {
                return "err refusing to sign a confidence that is not a fraction".to_string();
            }
            // The seat name is this process's own, not the caller's: a relay that
            // could relabel a signature could move one seat's vote to another.
            let message = signing_message(
                feed,
                name,
                tick.price_units,
                tick.publish_time_ns,
                tick.confidence_bps,
            );
            format!(
                "ok {} {} {} {}",
                tick.price_units,
                tick.publish_time_ns,
                tick.confidence_bps,
                hex::encode(key.sign(&message).to_bytes())
            )
        }
        _ => "err unknown request".to_string(),
    }
}

/// One fetch a second per feed, and the same answer for everyone in between. The
/// relay asks on its own cadence; a caller in a loop gets the cached observation,
/// which the oracle would refuse as a replay anyway.
fn observe_cached(
    cache: &RefCell<HashMap<String, (Instant, Tick)>>,
    feed: &str,
    fetch: &dyn Fn(&str) -> Result<Tick, String>,
) -> Result<Tick, String> {
    if let Some((at, tick)) = cache.borrow().get(feed) {
        if at.elapsed() < MIN_FETCH_GAP {
            return Ok(*tick);
        }
    }
    let tick = fetch(feed)?;
    cache
        .borrow_mut()
        .insert(feed.to_string(), (Instant::now(), tick));
    Ok(tick)
}

fn arg_opt(name: &str) -> Option<String> {
    let argv: Vec<String> = std::env::args().collect();
    argv.iter()
        .position(|entry| entry == name)
        .and_then(|index| argv.get(index + 1))
        .cloned()
}

fn arg(name: &str, fallback: &str) -> String {
    let argv: Vec<String> = std::env::args().collect();
    argv.iter()
        .position(|entry| entry == name)
        .and_then(|index| argv.get(index + 1))
        .map(|value| value.to_string())
        .unwrap_or_else(|| fallback.to_string())
}

/// A seat's key, from a file, created the first time it is asked for.
///
/// The default stays ephemeral: a sandbox seat is correct to forget its key,
/// because the chain it signs for is deleted along with the process. A public
/// network is the opposite — a seat whose key changes on restart is a registered
/// source that can never sign again, which means the venue can never settle a
/// ticket, which means the product is a set of contracts nobody can use.
///
/// This trades one property for another, and the trade is real: a key on disk can
/// be read by anything running as the same user, including the relay that owns the
/// socket. The protection that matters — that a signature is this process's claim
/// about its own read, not a number it was handed — survives only as long as the
/// file is not readable by another party. So it is written `0600`, and an operator
/// running a real committee should keep these somewhere the relay cannot reach.
fn load_or_create_key(path: &str) -> std::io::Result<SigningKey> {
    match std::fs::read_to_string(path) {
        Ok(existing) => {
            let trimmed = existing.trim();
            let bytes = hex::decode(trimmed).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{path} is not a hex key: {error}"),
                )
            })?;
            let seed: [u8; 32] = bytes.try_into().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{path} must hold exactly 32 bytes, not {}",
                        trimmed.len() / 2
                    ),
                )
            })?;
            return Ok(SigningKey::from_bytes(&seed));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let seed = random_seed();
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Write the file before trusting it, and make it private in the same step:
    // a key that exists for a moment with default permissions is a key that has
    // already been copied.
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(hex::encode(seed).as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    Ok(SigningKey::from_bytes(&seed))
}

fn random_seed() -> [u8; 32] {
    use std::io::Read;
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .expect("no randomness to sign settlement prices with")
        .read_exact(&mut seed)
        .expect("could not read randomness");
    seed
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[11u8; 32])
    }

    fn seen(price: u128) -> impl Fn(&str) -> Result<Tick, String> {
        move |_feed| {
            Ok(Tick {
                price_units: price,
                publish_time_ns: 1_793_000_000_000_000_000,
                confidence_bps: 9_900,
            })
        }
    }

    #[test]
    fn pubkey_is_the_one_it_signs_with() {
        let reply = answer("pubkey", "kraken", &key(), &seen(1));
        assert_eq!(
            reply,
            format!("ok {}", hex::encode(key().verifying_key().to_bytes()))
        );
    }

    #[test]
    fn observe_signs_what_the_seat_read() {
        let reply = answer("observe BTC-USD", "kraken", &key(), &seen(77_000_000_000));
        let parts: Vec<&str> = reply.trim_start_matches("ok ").split_whitespace().collect();
        assert_eq!(parts.len(), 4, "{reply}");
        assert_eq!(parts[0], "77000000000");
        assert_eq!(parts[1], "1793000000000000000");
        assert_eq!(parts[2], "9900");
        let signature: [u8; 64] = hex::decode(parts[3])
            .expect("hex")
            .try_into()
            .expect("64 bytes");
        let message = signing_message(
            "BTC-USD",
            "kraken",
            77_000_000_000,
            1_793_000_000_000_000_000,
            9_900,
        );
        assert!(key()
            .verifying_key()
            .verify(&message, &ed25519_dalek::Signature::from_bytes(&signature))
            .is_ok());
        // The same signature against another seat's name does not verify, which is
        // what stops a relay relabelling votes it already holds.
        let elsewhere = signing_message(
            "BTC-USD",
            "okx",
            77_000_000_000,
            1_793_000_000_000_000_000,
            9_900,
        );
        assert!(key()
            .verifying_key()
            .verify(
                &elsewhere,
                &ed25519_dalek::Signature::from_bytes(&signature)
            )
            .is_err());
    }

    #[test]
    fn there_is_no_way_to_ask_it_to_sign_a_number_of_your_choosing() {
        // The old protocol took a price from the caller. It is gone: that is the
        // hole that moving the fetch into the seat was for.
        let reply = answer(
            "sign BTC-USD 999999999999 1793000000000000000 9900",
            "kraken",
            &key(),
            &seen(1),
        );
        assert_eq!(reply, "err unknown request");
    }

    #[test]
    fn a_feed_it_cannot_read_is_an_error_not_a_blank() {
        let failing = |_feed: &str| Err::<Tick, String>("kraken says nothing about X".to_string());
        let reply = answer("observe BTC-USD", "kraken", &key(), &failing);
        assert!(reply.starts_with("err kraken says nothing"), "{reply}");
        assert!(answer("observe", "kraken", &key(), &seen(1)).starts_with("err observe wants"));
    }

    #[test]
    fn a_loop_on_the_socket_does_not_become_a_loop_on_the_source() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let cache: RefCell<HashMap<String, (Instant, Tick)>> = RefCell::new(HashMap::new());
        let fetch = |_feed: &str| -> Result<Tick, String> {
            calls.set(calls.get() + 1);
            Ok(Tick {
                price_units: 7,
                publish_time_ns: 1,
                confidence_bps: 9_900,
            })
        };
        for _ in 0..25 {
            observe_cached(&cache, "BTC-USD", &fetch).expect("an observation");
        }
        assert_eq!(calls.get(), 1, "twenty-five asks, one read of the source");
        observe_cached(&cache, "ETH-USD", &fetch).expect("its own cache line");
        assert_eq!(calls.get(), 2, "and each feed has its own");
    }

    /// A seat on a public network must come back as itself. If the key were
    /// regenerated on restart, the seat registered on the oracle contract would be
    /// one nobody holds the private half of, and the venue could never settle.
    #[test]
    fn a_durable_seat_keeps_its_key_across_restarts() {
        let dir = std::env::temp_dir().join(format!("seat-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("kraken.key");
        let path = path.to_str().expect("utf-8 path");

        let first = load_or_create_key(path).expect("first key");
        let second = load_or_create_key(path).expect("reloaded key");
        assert_eq!(first.verifying_key(), second.verifying_key());

        // And it is the key on disk, not one that merely round-tripped in memory.
        let on_disk = std::fs::read_to_string(path).expect("read");
        let seed: [u8; 32] = hex::decode(on_disk.trim())
            .expect("hex")
            .try_into()
            .expect("32 bytes");
        assert_eq!(
            SigningKey::from_bytes(&seed).verifying_key(),
            first.verifying_key()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The dangerous failure is the quiet one: a corrupt file that we "helpfully"
    /// replace with a fresh key looks like success and silently unseats the
    /// committee. Refusing is the only safe answer.
    #[test]
    fn a_damaged_key_file_is_refused_not_regenerated() {
        let dir = std::env::temp_dir().join(format!("seat-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let not_hex = dir.join("not-hex.key");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(&not_hex, "this is not a key\n").expect("write");
        let error = load_or_create_key(not_hex.to_str().expect("utf-8")).expect_err("refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let short = dir.join("short.key");
        std::fs::write(&short, "aabbcc\n").expect("write");
        let error = load_or_create_key(short.to_str().expect("utf-8")).expect_err("refused");
        assert!(
            error.to_string().contains("32 bytes"),
            "the message must say what is wrong: {error}"
        );
        // The bad file is still there, untouched: we refused, we did not overwrite.
        assert_eq!(std::fs::read_to_string(&short).expect("read"), "aabbcc\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn it_refuses_to_sign_what_is_not_a_price() {
        assert!(answer("observe BTC-USD", "kraken", &key(), &seen(0)).starts_with("err refusing"));
    }
}
