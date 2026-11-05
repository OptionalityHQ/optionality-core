//! End-to-end transport primitive. A browser/worker must verify the pinned
//! attestation and expected transport public key before using it. Ciphertext
//! alone says nothing about custody, funding privacy or code authenticity.
use aes_gcm::{
    aead::{Aead, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use ed25519_dalek::{Signature, VerifyingKey};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

type Result<T> = std::result::Result<T, String>;
pub const PADDED_BYTES: usize = 4096;
const HEADER: usize = 4;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub ephemeral_key: String,
    pub nonce: String,
    pub ciphertext: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Domain<'a> {
    pub network: &'a str,
    pub vault: &'a str,
    pub wasm_hash: &'a str,
    pub direction: Direction,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Command,
    Reply,
}
impl Domain<'_> {
    fn bytes(&self) -> Result<Vec<u8>> {
        if self.network.is_empty()
            || self.network.len() > 32
            || self.vault.is_empty()
            || self.vault.len() > 64
            || self.wasm_hash.len() != 64
            || !self
                .wasm_hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid transport domain".into());
        }
        let mut bytes = b"optionality-confidential-transport-v1\0".to_vec();
        for field in [self.network, self.vault, self.wasm_hash] {
            bytes.extend((field.len() as u32).to_le_bytes());
            bytes.extend(field.as_bytes());
        }
        bytes.push(match self.direction {
            Direction::Command => 0,
            Direction::Reply => 1,
        });
        Ok(bytes)
    }
}
pub fn public_key(secret: [u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(secret)).to_bytes()
}
/// Check before accepting a signed reply key or consuming its request nonce.
pub fn validate_recipient(recipient: [u8; 32]) -> Result<()> {
    let shared = StaticSecret::from([1u8; 32]).diffie_hellman(&PublicKey::from(recipient));
    if shared.as_bytes() == &[0; 32] {
        return Err("non-contributory transport key".into());
    }
    Ok(())
}

/// Canonical digest of the encrypted request, never its private plaintext.
pub fn request_hash(envelope: &Envelope) -> Result<String> {
    let bytes = serde_json::to_vec(envelope).map_err(|_| "request encoding failed")?;
    let mut hash = Sha256::new();
    hash.update(b"optionality-private-request-v1\0");
    hash.update(bytes);
    Ok(hex::encode(hash.finalize()))
}

/// The worker signs this only after its checkpoint has finalized. A signature
/// from a key not pinned by independently verified attestation is insufficient.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyBody {
    pub network: String,
    pub vault: String,
    pub wasm_hash: String,
    pub request_hash: String,
    pub sequence: u64,
    pub root: String,
    pub envelope: Envelope,
}
impl ReplyBody {
    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = b"optionality-private-reply-v1\0".to_vec();
        bytes.extend(serde_json::to_vec(self).map_err(|_| "reply encoding failed")?);
        Ok(bytes)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedReply {
    pub body: ReplyBody,
    pub signature: String,
}
/// All expected values come from the client's sent request and independently
/// finalized anchor, never from the received reply itself.
#[derive(Clone)]
pub struct ExpectedReply<'a> {
    pub domain: Domain<'a>,
    pub request_hash: &'a str,
    pub sequence: u64,
    pub root: &'a str,
    pub attested_signing_key: [u8; 32],
}
pub fn open_verified_reply(
    reply: &AuthenticatedReply,
    recipient_secret: [u8; 32],
    expected: &ExpectedReply<'_>,
) -> Result<Vec<u8>> {
    let refuse = || "private reply authentication failed".to_string();
    let b = &reply.body;
    if expected.domain.direction != Direction::Reply
        || b.network != expected.domain.network
        || b.vault != expected.domain.vault
        || b.wasm_hash != expected.domain.wasm_hash
        || b.request_hash != expected.request_hash
        || b.sequence != expected.sequence
        || b.root != expected.root
    {
        return Err(refuse());
    }
    let key = VerifyingKey::from_bytes(&expected.attested_signing_key).map_err(|_| refuse())?;
    let signature = hex::decode(&reply.signature).map_err(|_| refuse())?;
    let signature = Signature::from_slice(&signature).map_err(|_| refuse())?;
    key.verify_strict(&b.signing_bytes()?, &signature)
        .map_err(|_| refuse())?;
    open(&b.envelope, recipient_secret, &expected.domain).map_err(|_| refuse())
}
fn key(
    secret: [u8; 32],
    peer: [u8; 32],
    ephemeral: [u8; 32],
    recipient: [u8; 32],
    aad: &[u8],
) -> Result<[u8; 32]> {
    let shared = StaticSecret::from(secret).diffie_hellman(&PublicKey::from(peer));
    if shared.as_bytes() == &[0; 32] {
        return Err("non-contributory transport key".into());
    }
    let mut salt = Sha256::new();
    salt.update(aad);
    salt.update(ephemeral);
    salt.update(recipient);
    let hkdf = Hkdf::<Sha256>::new(Some(&salt.finalize()), shared.as_bytes());
    let mut out = [0; 32];
    hkdf.expand(b"aes-256-gcm", &mut out)
        .map_err(|_| "key derivation failed")?;
    Ok(out)
}
/// `ephemeral_secret` MUST be fresh unpredictable random bytes for each
/// message. `nonce` MUST also come from a CSPRNG. Neither is accepted from
/// untrusted application input by a production sender.
pub fn seal(
    plaintext: &[u8],
    recipient: [u8; 32],
    ephemeral_secret: [u8; 32],
    nonce: [u8; 12],
    domain: &Domain<'_>,
) -> Result<Envelope> {
    if plaintext.len() > PADDED_BYTES - HEADER {
        return Err("private payload too large".into());
    }
    if ephemeral_secret == [0; 32] {
        return Err("random ephemeral secret required".into());
    }
    let aad = domain.bytes()?;
    let ephemeral = public_key(ephemeral_secret);
    let mut derived = key(ephemeral_secret, recipient, ephemeral, recipient, &aad)?;
    let cipher = Aes256Gcm::new_from_slice(&derived).map_err(|_| "cipher initialization failed")?;
    let mut padded = vec![0; PADDED_BYTES];
    padded[..HEADER].copy_from_slice(&(plaintext.len() as u32).to_le_bytes());
    padded[HEADER..HEADER + plaintext.len()].copy_from_slice(plaintext);
    let result = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &padded,
                aad: &aad,
            },
        )
        .map_err(|_| "encryption failed");
    padded.fill(0);
    derived.fill(0);
    Ok(Envelope {
        ephemeral_key: hex::encode(ephemeral),
        nonce: hex::encode(nonce),
        ciphertext: hex::encode(result?),
    })
}
/// Only the enclave should open commands; only the subscriber/browser that
/// owns the reply private key should open replies. Never log plaintext errors.
pub fn open(
    envelope: &Envelope,
    recipient_secret: [u8; 32],
    domain: &Domain<'_>,
) -> Result<Vec<u8>> {
    let refuse = || "private envelope authentication failed".to_string();
    if envelope.ephemeral_key.len() != 64
        || envelope.nonce.len() != 24
        || envelope.ciphertext.len() != (PADDED_BYTES + 16) * 2
    {
        return Err(refuse());
    }
    let ephemeral: [u8; 32] = hex::decode(&envelope.ephemeral_key)
        .map_err(|_| refuse())?
        .try_into()
        .map_err(|_| refuse())?;
    let nonce: [u8; 12] = hex::decode(&envelope.nonce)
        .map_err(|_| refuse())?
        .try_into()
        .map_err(|_| refuse())?;
    let ciphertext = hex::decode(&envelope.ciphertext).map_err(|_| refuse())?;
    let aad = domain.bytes().map_err(|_| refuse())?;
    let recipient = public_key(recipient_secret);
    let mut derived =
        key(recipient_secret, ephemeral, ephemeral, recipient, &aad).map_err(|_| refuse())?;
    let cipher = Aes256Gcm::new_from_slice(&derived).map_err(|_| refuse())?;
    let decrypted = cipher
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| refuse());
    derived.fill(0);
    let mut padded = decrypted?;
    if padded.len() != PADDED_BYTES {
        return Err(refuse());
    }
    let size = u32::from_le_bytes(padded[..HEADER].try_into().map_err(|_| refuse())?) as usize;
    if size > PADDED_BYTES - HEADER || padded[HEADER + size..].iter().any(|b| *b != 0) {
        padded.fill(0);
        return Err(refuse());
    }
    let plaintext = padded[HEADER..HEADER + size].to_vec();
    padded.fill(0);
    Ok(plaintext)
}
