//! agent-mail wire format, identity and crypto.
//!
//! Everything here must stay byte-compatible with the TypeScript peer (`src/peer/identity.ts`,
//! `src/peer/wire.ts`, `src/peer/metadata.ts`). `test/vectors/agent-mail-v1.json` pins that
//! contract; see ADR 0005.

pub mod canonical;
pub mod envelope;
pub mod error;
pub mod identity;
pub mod ipc;
pub mod js;
pub mod migrate;
pub mod outbox;
pub mod policy;
pub mod send;
pub mod store;
pub mod wire;

pub use envelope::{AgentMeta, Envelope, Message, Origin};
pub use error::{Error, Result};
pub use identity::{fingerprint, Identity, PublicIdentity};
pub use wire::{receipt, seal, unseal, verify_receipt, Packet};
