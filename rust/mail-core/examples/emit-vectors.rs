//! Writes Rust-made identities, messages, packets and receipts for the TypeScript side to check:
//!
//!   cargo run -p mail-core --example emit-vectors -- ../test/vectors/agent-mail-v1.json target/rust-out.json
//!   node test/gen-vectors.mjs --verify rust/target/rust-out.json
//!
//! Packets carry the current time, so verify them within five minutes.

use std::time::{SystemTime, UNIX_EPOCH};

use mail_core::envelope::{Envelope, Message};
use mail_core::wire::{receipt, seal};
use mail_core::Identity;
use serde_json::{json, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (input, output) = match (args.next(), args.next()) {
        (Some(input), Some(output)) => (input, output),
        _ => return Err("usage: emit-vectors <vectors.json> <out.json>".into()),
    };
    let vectors: Value = serde_json::from_str(&std::fs::read_to_string(input)?)?;
    let from_seeds = |name: &str| -> Result<Identity, Box<dyn std::error::Error>> {
        let v = &vectors["identities"][name];
        let seed = |key: &str| -> Result<[u8; 32], Box<dyn std::error::Error>> {
            Ok(hex::decode(v[key].as_str().ok_or("missing seed")?)?
                .try_into()
                .map_err(|_| "seed is not 32 bytes")?)
        };
        Ok(Identity::from_seeds(
            name,
            seed("signing_seed")?,
            seed("encryption_seed")?,
        ))
    };
    let alice = from_seeds("alice")?;
    let bob = from_seeds("bob")?;
    let carol = Identity::generate("carol")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;

    let mut messages = Vec::new();
    for item in vectors["messages"].as_array().ok_or("missing messages")? {
        let signed = Message::parse_unsigned(item["input"].clone(), Envelope::V1_1)?
            .sign(&alice, Envelope::V1_1)?;
        messages.push(json!({
            "label": item["label"],
            "signed": signed,
            "json": signed.json(),
            "packet": seal(&alice, &bob.public(), &signed, now)?,
            "receipt": receipt(&bob, &signed)?,
        }));
    }
    let out = json!({
        "identities": { "alice": alice_json(&alice), "bob": alice_json(&bob) },
        "generated": { "name": carol.name, "signing_key": carol.signing_key, "encryption_key": carol.encryption_key,
            "signing_private": carol.signing_private, "encryption_private": carol.encryption_private,
            "fingerprint": carol.fingerprint()? },
        "messages": messages,
    });
    std::fs::write(output, serde_json::to_string_pretty(&out)? + "\n")?;
    Ok(())
}

fn alice_json(identity: &Identity) -> Value {
    json!({
        "name": identity.name,
        "signing_key": identity.signing_key,
        "encryption_key": identity.encryption_key,
        "signing_private": identity.signing_private,
        "encryption_private": identity.encryption_private,
    })
}
