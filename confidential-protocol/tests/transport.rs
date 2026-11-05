use confidential_protocol::*;
use ed25519_dalek::{Signer, SigningKey};
const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn domain() -> Domain<'static> {
    Domain {
        network: "testnet",
        vault: "vault.example.testnet",
        wasm_hash: HASH,
        direction: Direction::Command,
    }
}
#[test]
fn only_the_intended_recipient_can_open_and_sizes_are_padded() {
    let key = [7; 32];
    let short = seal(b"buy", public_key(key), [8; 32], [1; 12], &domain()).unwrap();
    let long = seal(&vec![9; 3000], public_key(key), [9; 32], [2; 12], &domain()).unwrap();
    assert_eq!(short.ciphertext.len(), long.ciphertext.len());
    assert_eq!(open(&short, key, &domain()).unwrap(), b"buy");
    assert!(open(&short, [6; 32], &domain()).is_err());
}
#[test]
fn network_vault_code_and_direction_cannot_be_substituted() {
    let key = [7; 32];
    let envelope = seal(b"read", public_key(key), [8; 32], [1; 12], &domain()).unwrap();
    for changed in [
        Domain {
            network: "mainnet",
            ..domain()
        },
        Domain {
            vault: "evil.testnet",
            ..domain()
        },
        Domain {
            wasm_hash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ..domain()
        },
        Domain {
            direction: Direction::Reply,
            ..domain()
        },
    ] {
        assert!(open(&envelope, key, &changed).is_err());
    }
}
#[test]
fn tampered_metadata_and_oversized_payloads_are_refused() {
    let key = [7; 32];
    let envelope = seal(b"subscribe", public_key(key), [8; 32], [1; 12], &domain()).unwrap();
    let mut changed = envelope.clone();
    changed.ciphertext.replace_range(0..2, "00");
    assert!(open(&changed, key, &domain()).is_err());
    let mut changed = envelope.clone();
    changed.nonce = "00".repeat(12);
    assert!(open(&changed, key, &domain()).is_err());
    let mut changed = envelope;
    changed.ephemeral_key = "00".repeat(32);
    assert!(open(&changed, key, &domain()).is_err());
    assert!(seal(
        &vec![1; PADDED_BYTES],
        public_key(key),
        [8; 32],
        [1; 12],
        &domain()
    )
    .is_err());
    assert!(seal(b"x", [0; 32], [8; 32], [1; 12], &domain()).is_err());
}

#[test]
fn replies_require_the_attested_signer_exact_request_and_finalized_checkpoint() {
    let signer = SigningKey::from_bytes(&[55; 32]); // test fixture, never a worker key
    let recipient = [7; 32];
    let request = seal(b"read", public_key([4; 32]), [9; 32], [3; 12], &domain()).unwrap();
    let request_hash = request_hash(&request).unwrap();
    let reply_domain = Domain {
        direction: Direction::Reply,
        ..domain()
    };
    let expected = ExpectedReply {
        domain: reply_domain.clone(),
        request_hash: &request_hash,
        sequence: 42,
        root: HASH,
        attested_signing_key: signer.verifying_key().to_bytes(),
    };
    let body = ReplyBody {
        network: "testnet".into(),
        vault: domain().vault.into(),
        wasm_hash: HASH.into(),
        request_hash: request_hash.clone(),
        sequence: 42,
        root: HASH.into(),
        envelope: seal(
            b"private result",
            public_key(recipient),
            [8; 32],
            [1; 12],
            &reply_domain,
        )
        .unwrap(),
    };
    let reply = AuthenticatedReply {
        signature: hex::encode(signer.sign(&body.signing_bytes().unwrap()).to_bytes()),
        body,
    };
    assert_eq!(
        open_verified_reply(&reply, recipient, &expected).unwrap(),
        b"private result"
    );
    // A relay can encrypt to the client, but cannot authenticate that response.
    let mut forged = reply.clone();
    forged.body.envelope = seal(
        b"fake balance",
        public_key(recipient),
        [11; 32],
        [2; 12],
        &reply_domain,
    )
    .unwrap();
    assert!(open_verified_reply(&forged, recipient, &expected).is_err());
    for changed in [
        ExpectedReply {
            request_hash: HASH,
            ..expected.clone()
        },
        ExpectedReply {
            sequence: 43,
            ..expected.clone()
        },
        ExpectedReply {
            root: &"b".repeat(64),
            ..expected.clone()
        },
        ExpectedReply {
            attested_signing_key: SigningKey::from_bytes(&[56; 32]).verifying_key().to_bytes(),
            ..expected.clone()
        },
    ] {
        assert!(open_verified_reply(&reply, recipient, &changed).is_err());
    }
}
