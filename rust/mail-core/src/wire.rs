//! The `agent-mail/1` packet and receipt (`src/peer/wire.ts` `seal`, `unseal`, `receipt`).
//!
//! A packet is `{payload, signature}`. `payload` is a JSON string naming the sender's public
//! identity and the recipient, plus the message encrypted with AES-256-GCM. The key comes from an
//! ephemeral X25519 exchange through HKDF-SHA256 (salt = recipient fingerprint, info
//! `agent-mail/1 encryption`). The sender signs the payload string with Ed25519.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

use crate::envelope::{Envelope, Message};
use crate::error::{invalid, Error, Result};
use crate::identity::{
    fingerprint, validate_public, x25519_public_from_pem, x25519_public_pem, Identity,
    PublicIdentity,
};
use crate::js::utf16_len;

pub const PROTOCOL: &str = "agent-mail/1";
pub const RECEIPT_PROTOCOL: &str = "agent-mail/1 receipt";
/// Packets more than five minutes from the receiver's clock are refused.
pub const MAX_SKEW_MS: u64 = 300_000;
const INFO: &[u8] = b"agent-mail/1 encryption";
const TAG_LEN: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Packet {
    pub payload: String,
    pub signature: String,
}

impl Packet {
    /// `packetSchema.parse`: payload up to 200,000 and signature up to 128 UTF-16 units.
    pub fn parse(value: Value) -> Result<Self> {
        let packet: Packet = serde_json::from_value(value)?;
        if utf16_len(&packet.payload) > 200_000 {
            return Err(invalid("payload", "longer than 200000 characters"));
        }
        if utf16_len(&packet.signature) > 128 {
            return Err(invalid("signature", "longer than 128 characters"));
        }
        Ok(packet)
    }
}

/// The payload as written by `seal()`; field order follows the object literal there.
#[derive(Serialize)]
struct SealedOut<'a> {
    protocol: &'static str,
    identity: PublicIdentity,
    recipient: &'a str,
    timestamp: u64,
    ephemeral: String,
    iv: String,
    tag: String,
    ciphertext: String,
}

/// `encryptedSchema` on the receiving side. `timestamp` is any JSON number there.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedIn {
    protocol: String,
    identity: PublicIdentity,
    recipient: String,
    timestamp: f64,
    ephemeral: String,
    iv: String,
    tag: String,
    ciphertext: String,
}

fn key(secret: &StaticSecret, public: &XPublicKey, recipient: &str) -> Result<[u8; 32]> {
    let shared = secret.diffie_hellman(public);
    // OpenSSL refuses an all-zero X25519 result, so a low-order key must fail here too.
    if !shared.was_contributory() {
        return Err(Error::Decrypt);
    }
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(recipient.as_bytes()), shared.as_bytes())
        .expand(INFO, &mut out)
        .expect("32 bytes is a valid HKDF length");
    Ok(out)
}

/// `seal()`. The message must already be signed by `identity`. `now_ms` becomes the packet timestamp.
pub fn seal(
    identity: &Identity,
    recipient: &PublicIdentity,
    message: &Message,
    now_ms: u64,
) -> Result<Packet> {
    if !message.verify(&identity.public()) {
        return Err(Error::BadSignature);
    }
    let recipient_fp = fingerprint(recipient)?;
    let mut ephemeral_seed = [0u8; 32];
    OsRng.fill_bytes(&mut ephemeral_seed);
    let ephemeral = StaticSecret::from(ephemeral_seed);
    let mut iv = [0u8; 12];
    OsRng.fill_bytes(&mut iv);
    let key = key(&ephemeral, &recipient.encryption_public()?, &recipient_fp)?;
    let mut sealed = Aes256Gcm::new(&key.into())
        .encrypt(Nonce::from_slice(&iv), message.json().as_bytes())
        .map_err(|_| Error::Decrypt)?;
    let tag = sealed.split_off(sealed.len() - TAG_LEN);
    let payload = serde_json::to_string(&SealedOut {
        protocol: PROTOCOL,
        identity: identity.public(),
        recipient: &recipient_fp,
        timestamp: now_ms,
        ephemeral: x25519_public_pem(&XPublicKey::from(&ephemeral)),
        iv: BASE64.encode(iv),
        tag: BASE64.encode(tag),
        ciphertext: BASE64.encode(sealed),
    })?;
    let signature = identity.sign(payload.as_bytes())?;
    Ok(Packet { payload, signature })
}

/// `unseal()`. `trusted` maps a sender fingerprint to its pinned public identity; the packet is
/// checked against the pinned keys, never the keys it carries.
pub fn unseal(
    identity: &Identity,
    packet: Value,
    trusted: impl Fn(&str) -> Option<PublicIdentity>,
    accept: Envelope,
    now_ms: u64,
) -> Result<Message> {
    let packet = Packet::parse(packet)?;
    let wire: SealedIn = serde_json::from_str(&packet.payload)?;
    if wire.protocol != PROTOCOL {
        return Err(invalid("protocol", "expected agent-mail/1"));
    }
    if utf16_len(&wire.recipient) != 64
        || utf16_len(&wire.ephemeral) > 300
        || utf16_len(&wire.iv) > 32
        || utf16_len(&wire.tag) > 32
        || utf16_len(&wire.ciphertext) > 140_000
    {
        return Err(invalid("payload", "field out of range"));
    }
    validate_public(&wire.identity)?;
    let sender = fingerprint(&wire.identity)?;
    let contact = trusted(&sender).ok_or(Error::Untrusted)?;
    if !contact.verify(packet.payload.as_bytes(), &packet.signature) {
        return Err(Error::Untrusted);
    }
    // `Math.abs(now - timestamp) > 300_000` in TS. A NaN skew counts as expired here.
    let fresh = (now_ms as f64 - wire.timestamp).abs() <= MAX_SKEW_MS as f64;
    if wire.recipient != identity.fingerprint()? || !fresh {
        return Err(Error::WrongRecipientOrExpired);
    }
    let key = key(
        &identity.encryption_secret()?,
        &x25519_public_from_pem(&wire.ephemeral)?,
        &wire.recipient,
    )?;
    let iv = BASE64.decode(&wire.iv).map_err(|_| Error::Decrypt)?;
    let tag = BASE64.decode(&wire.tag).map_err(|_| Error::Decrypt)?;
    let mut sealed = BASE64
        .decode(&wire.ciphertext)
        .map_err(|_| Error::Decrypt)?;
    if iv.len() != 12 || tag.len() != TAG_LEN {
        return Err(Error::Decrypt);
    }
    sealed.extend_from_slice(&tag);
    let plain = Aes256Gcm::new(&key.into())
        .decrypt(Nonce::from_slice(&iv), sealed.as_slice())
        .map_err(|_| Error::Decrypt)?;
    let message = Message::parse_signed(serde_json::from_slice(&plain)?, accept)?;
    if message.sender != sender || message.recipient != wire.recipient {
        return Err(Error::IdentityMismatch);
    }
    if !message.verify(&contact) {
        return Err(Error::BadSignature);
    }
    Ok(message)
}

fn receipt_payload(message: &Message) -> String {
    #[derive(Serialize)]
    struct Receipt<'a> {
        protocol: &'static str,
        digest: &'a str,
    }
    serde_json::to_string(&Receipt {
        protocol: RECEIPT_PROTOCOL,
        digest: &message.digest(),
    })
    .expect("receipt serializes")
}

/// `receipt()`: the receiver's signed acknowledgement of exactly this message.
pub fn receipt(identity: &Identity, message: &Message) -> Result<Packet> {
    let payload = receipt_payload(message);
    let signature = identity.sign(payload.as_bytes())?;
    Ok(Packet { payload, signature })
}

/// `verifyReceipt()`: the sender checks the receipt names the message it sent.
pub fn verify_receipt(contact: &PublicIdentity, packet: Value, message: &Message) -> Result<()> {
    let packet = Packet::parse(packet)?;
    if !contact.verify(packet.payload.as_bytes(), &packet.signature)
        || packet.payload != receipt_payload(message)
    {
        return Err(Error::BadReceipt);
    }
    Ok(())
}
