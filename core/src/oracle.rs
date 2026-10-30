//! What a source signs to put a price on the chain.
//!
//! The layout lives here rather than in the oracle contract because both sides of
//! the wire need it: the contract verifies it, and the relay that runs the sources
//! builds it. Kept in one pure crate, they cannot drift — a relay that signed a
//! different message would simply be refused, which is the wrong way to discover a
//! disagreement about what a price was.

/// Observations are only meaningful to a bounded-options oracle. The prefix stops
/// a price signed for one deployment being replayed against another that happens
/// to hold the same keys.
pub const DOMAIN: &[u8] = b"near-bounded-options-oracle-v1";

/// The bytes a source signs: the domain, each string with a one-byte length
/// prefix, then fixed-width little-endian numbers. Two distinct observations
/// cannot collide on one message, because every variable-length field says how
/// long it is before it starts.
pub fn signing_message(
    feed_id: &str,
    source: &str,
    price: u128,
    publish_time_ns: u64,
    confidence_bps: u16,
) -> Vec<u8> {
    let mut message = Vec::new();
    message.extend_from_slice(DOMAIN);
    for field in [feed_id.as_bytes(), source.as_bytes()] {
        message.push(field.len() as u8);
        message.extend_from_slice(field);
    }
    message.extend_from_slice(&price.to_le_bytes());
    message.extend_from_slice(&publish_time_ns.to_le_bytes());
    message.extend_from_slice(&confidence_bps.to_le_bytes());
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_is_pinned_field_by_field() {
        let message = signing_message("BTC-USD", "kraken", 7, 8, 9);
        assert_eq!(&message[..DOMAIN.len()], DOMAIN);
        assert_eq!(message[DOMAIN.len()], 7, "the feed id announces its own length");
        assert_eq!(&message[DOMAIN.len() + 1..DOMAIN.len() + 8], b"BTC-USD");
        assert_eq!(message[DOMAIN.len() + 8], 6, "and so does the source");
        assert_eq!(&message[DOMAIN.len() + 9..DOMAIN.len() + 15], b"kraken");
        assert_eq!(&message[DOMAIN.len() + 15..DOMAIN.len() + 31], 7u128.to_le_bytes());
        assert_eq!(&message[DOMAIN.len() + 31..DOMAIN.len() + 39], 8u64.to_le_bytes());
        assert_eq!(&message[DOMAIN.len() + 39..], 9u16.to_le_bytes());
        assert_eq!(message.len(), DOMAIN.len() + 2 + 7 + 6 + 16 + 8 + 2);
    }

    #[test]
    fn the_length_prefix_makes_the_boundary_unambiguous() {
        // Without it, a feed id running into a source name would sign the same
        // bytes as a different pair of them — and one seat's signature would be
        // another seat's.
        assert_ne!(
            signing_message("BTC-US", "DKRAKEN", 7, 8, 9),
            signing_message("BTC-USD", "KRAKEN", 7, 8, 9),
            "different feeds and sources must not share a message"
        );
    }

    #[test]
    fn every_field_changes_the_message() {
        let base = signing_message("BTC-USD", "kraken", 1, 2, 3);
        for other in [
            signing_message("ETH-USD", "kraken", 1, 2, 3),
            signing_message("BTC-USD", "okx", 1, 2, 3),
            signing_message("BTC-USD", "kraken", 0, 2, 3),
            signing_message("BTC-USD", "kraken", 1, 1, 3),
            signing_message("BTC-USD", "kraken", 1, 2, 4),
        ] {
            assert_ne!(base, other);
        }
    }
}
