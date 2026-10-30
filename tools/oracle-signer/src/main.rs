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
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

fn main() -> std::io::Result<()> {
    let name = arg("--name", "seat");
    let source = arg("--source", &name);
    let engine = arg("--engine", "http://127.0.0.1:8791");
    let socket = arg("--socket", "data/oracle/seat.sock");
    let key = SigningKey::from_bytes(&random_seed());
    if let Some(parent) = Path::new(&socket).parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A socket left by a killed run would fail to bind; nothing else lives here.
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    println!("ready {name} {}", hex::encode(key.verifying_key().to_bytes()));
    std::io::stdout().flush()?;
    for stream in listener.incoming().flatten() {
        if let Err(error) = serve(stream, &name, &source, &engine, &key) {
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
) -> std::io::Result<()> {
    let mut line = String::new();
    if BufReader::new(stream.try_clone()?).read_line(&mut line)? == 0 {
        return Ok(());
    }
    let fetch = |feed: &str| -> Result<Tick, String> {
        let known = FEEDS
            .iter()
            .find(|entry| entry.id == feed)
            .ok_or_else(|| format!("{feed} is not a feed this seat reads"))?;
        oracle_sources::fetch(source, known, engine)
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

fn arg(name: &str, fallback: &str) -> String {
    let argv: Vec<String> = std::env::args().collect();
    argv.iter()
        .position(|entry| entry == name)
        .and_then(|index| argv.get(index + 1))
        .map(|value| value.to_string())
        .unwrap_or_else(|| fallback.to_string())
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
        assert_eq!(reply, format!("ok {}", hex::encode(key().verifying_key().to_bytes())));
    }

    #[test]
    fn observe_signs_what_the_seat_read() {
        let reply = answer("observe BTC-USD", "kraken", &key(), &seen(77_000_000_000));
        let parts: Vec<&str> = reply.trim_start_matches("ok ").split_whitespace().collect();
        assert_eq!(parts.len(), 4, "{reply}");
        assert_eq!(parts[0], "77000000000");
        assert_eq!(parts[1], "1793000000000000000");
        assert_eq!(parts[2], "9900");
        let signature: [u8; 64] = hex::decode(parts[3]).expect("hex").try_into().expect("64 bytes");
        let message =
            signing_message("BTC-USD", "kraken", 77_000_000_000, 1_793_000_000_000_000_000, 9_900);
        assert!(key()
            .verifying_key()
            .verify(&message, &ed25519_dalek::Signature::from_bytes(&signature))
            .is_ok());
        // The same signature against another seat's name does not verify, which is
        // what stops a relay relabelling votes it already holds.
        let elsewhere =
            signing_message("BTC-USD", "okx", 77_000_000_000, 1_793_000_000_000_000_000, 9_900);
        assert!(key()
            .verifying_key()
            .verify(&elsewhere, &ed25519_dalek::Signature::from_bytes(&signature))
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
    fn it_refuses_to_sign_what_is_not_a_price() {
        assert!(answer("observe BTC-USD", "kraken", &key(), &seen(0)).starts_with("err refusing"));
    }
}
