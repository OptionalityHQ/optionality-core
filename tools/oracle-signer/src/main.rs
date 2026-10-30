//! One seat's key, in a process of its own.
//!
//! The relay fetches prices from four places and delivers them to the oracle. It
//! used to hold all four signing keys as well, which meant a compromised relay
//! could sign anything for anyone and the committee was independence of *prices*
//! only. This binary removes that: each seat runs here, generates its key in its
//! own memory, and answers exactly one question over a unix socket — "sign this
//! observation for me". The key never leaves this process, is never written down,
//! and dies with it.
//!
//! The protocol is one line in, one line out:
//!
//!     pubkey                        → ok <hex public key>
//!     sign <feed> <price> <ns> <bps> → ok <hex signature>
//!
//! Nothing else. No price fetching, no transactions, no filesystem access beyond
//! the socket it is told to bind.

use bounded_core::oracle::signing_message;
use ed25519_dalek::{Signer, SigningKey};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

fn main() -> std::io::Result<()> {
    let name = arg("--name", "seat");
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
        if let Err(error) = serve(stream, &name, &key) {
            eprintln!("{name}: {error}");
        }
    }
    Ok(())
}

/// Answer one request. A malformed line is an error on that line, not a reason to
/// exit: the relay retries, and a seat that dies on a typo takes a vote with it.
fn serve(mut stream: UnixStream, name: &str, key: &SigningKey) -> std::io::Result<()> {
    let mut line = String::new();
    if BufReader::new(stream.try_clone()?).read_line(&mut line)? == 0 {
        return Ok(());
    }
    let reply = answer(line.trim(), name, key);
    writeln!(stream, "{reply}")?;
    stream.flush()
}

fn answer(line: &str, name: &str, key: &SigningKey) -> String {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("pubkey") => format!("ok {}", hex::encode(key.verifying_key().to_bytes())),
        Some("sign") => {
            let (Some(feed), Some(price), Some(time), Some(confidence)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return "err sign wants <feed> <price> <ns> <bps>".to_string();
            };
            let (Ok(price), Ok(time), Ok(confidence)) = (
                price.parse::<u128>(),
                time.parse::<u64>(),
                confidence.parse::<u16>(),
            ) else {
                return "err a sign argument is not a number".to_string();
            };
            if price == 0 || confidence > 10_000 {
                return "err refusing to sign a non-price".to_string();
            }
            // The seat name is this process's own, not the caller's: a relay that
            // could relabel a signature could move one seat's vote to another.
            let message = signing_message(feed, name, price, time, confidence);
            format!("ok {}", hex::encode(key.sign(&message).to_bytes()))
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

    #[test]
    fn pubkey_is_the_one_it_signs_with() {
        let reply = answer("pubkey", "kraken", &key());
        assert_eq!(reply, format!("ok {}", hex::encode(key().verifying_key().to_bytes())));
    }

    #[test]
    fn a_signature_covers_the_seat_it_was_asked_for() {
        let reply = answer("sign BTC-USD 1000000 1793000000000000000 9900", "kraken", &key());
        let signature: [u8; 64] = hex::decode(reply.trim_start_matches("ok "))
            .expect("hex")
            .try_into()
            .expect("64 bytes");
        let message = signing_message("BTC-USD", "kraken", 1_000_000, 1_793_000_000_000_000_000, 9_900);
        assert!(key().verifying_key().verify(&message, &ed25519_dalek::Signature::from_bytes(&signature)).is_ok());
        // The same signature against another seat's name does not verify, which is
        // what stops a relay relabelling votes it already holds.
        let elsewhere = signing_message("BTC-USD", "okx", 1_000_000, 1_793_000_000_000_000_000, 9_900);
        assert!(key().verifying_key().verify(&elsewhere, &ed25519_dalek::Signature::from_bytes(&signature)).is_err());
    }

    #[test]
    fn it_refuses_to_sign_what_is_not_a_price() {
        for bad in [
            "sign BTC-USD 0 1 9900",
            "sign BTC-USD 1000 1 10001",
            "sign BTC-USD abc 1 9900",
            "sign BTC-USD 1000",
            "sign",
            "hello",
        ] {
            assert!(answer(bad, "kraken", &key()).starts_with("err"), "{bad}");
        }
    }

    #[test]
    fn the_socket_answers_one_line_and_closes() {
        // A real round trip through a socketpair: the relay's side of the
        // protocol, without spawning the process.
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        let handle = std::thread::spawn(move || serve(theirs, "kraken", &key()).map(|_| ()));
        let mut ours = ours;
        writeln!(ours, "sign ETH-USD 2500000000 1793000000000000000 9800").expect("write");
        let mut line = String::new();
        BufReader::new(ours).read_line(&mut line).expect("read");
        assert!(line.trim_start_matches(|c: char| c.is_whitespace()).starts_with("ok "), "{line}");
        handle.join().expect("thread").expect("serve");
    }
}
