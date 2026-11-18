//! The one way to send (ADR 0005 decision 9). The CLI, MCP and the tray all call `submit`, which
//! loads `policy.json`, runs `check_outbound`, builds the message and queues it. Nothing here
//! touches the network; the mailer delivers.

use std::path::Path;

use crate::envelope::{AgentMeta, Envelope, Message, Origin};
use crate::error::{invalid, Result};
use crate::identity::Identity;
use crate::outbox::Delivery;
use crate::policy::{check_outbound, Policy};
use crate::store::{now_ms, Store};

/// What a caller asks to send. Mirrors TS `peerSendShape`, plus the 1.2 `project` tag.
#[derive(Clone, Debug, Default)]
pub struct SendRequest {
    pub recipient: String,
    pub text: String,
    pub message_id: Option<String>,
    pub conversation_id: Option<String>,
    pub agent: Option<AgentMeta>,
    pub to_session: Option<String>,
    pub hop: Option<u8>,
    pub auto_reply: Option<bool>,
    pub in_reply_to: Option<String>,
    pub project: Option<String>,
}

/// Why a send didn't queue.
#[derive(Debug)]
pub enum SendError {
    /// `check_outbound` or a missing/invalid policy refused it.
    Refused(String),
    Other(crate::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Refused(reason) => write!(f, "send refused: {reason}"),
            SendError::Other(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SendError {}

impl From<crate::Error> for SendError {
    fn from(error: crate::Error) -> Self {
        SendError::Other(error)
    }
}

/// Checks policy, then queues the message unsigned. The recipient must be a pinned contact.
pub fn submit(
    home: &Path,
    identity: &Identity,
    store: &mut Store,
    request: SendRequest,
) -> std::result::Result<Delivery, SendError> {
    let policy = Policy::load(home).map_err(SendError::Refused)?;
    check_outbound(&request.text, Some(&request.recipient), &policy, store)
        .map_err(SendError::Refused)?;
    let contact = store.resolve_contact(&request.recipient)?;
    let explicit_conversation = request.conversation_id.is_some();
    let message = Message {
        id: request.message_id.unwrap_or_else(new_uuid),
        conversation_id: request.conversation_id.unwrap_or_else(new_uuid),
        sender: identity.fingerprint()?,
        recipient: contact.fingerprint,
        text: request.text,
        created_at: now_ms(),
        origin: local_origin(),
        agent: request.agent.filter(|agent| *agent != AgentMeta::default()),
        to_session: request.to_session,
        hop: request.hop,
        auto_reply: request.auto_reply,
        in_reply_to: request.in_reply_to,
        project: request.project,
        signature: String::new(),
    }
    .normalized(Envelope::V1_2)?;
    Ok(store.queue(&message, explicit_conversation)?)
}

/// ADR 0003 loop guard: an automatic reply is refused once the exchange reaches this depth.
pub const MAX_AUTO_HOP: u8 = 6;

/// TS `peerReplyShape`: answer a stored message.
#[derive(Clone, Debug, Default)]
pub struct ReplyRequest {
    pub sender: String,
    pub message_id: String,
    pub text: String,
    /// No human wrote or approved this reply: sent with `auto_reply`, and guarded.
    pub automatic: bool,
    pub reply_id: Option<String>,
    /// Agent metadata for the reply (the sender's own harness and session).
    pub agent: Option<AgentMeta>,
}

/// TS `Peer.reply`. The recipient, conversation, `in_reply_to`, hop and `to_session` come from the
/// stored message, never from the caller. `accepts` reports which envelope the peer takes (through
/// the mailer); `None` means it couldn't be checked, which refuses an automatic reply.
pub fn reply(
    home: &Path,
    identity: &Identity,
    store: &mut Store,
    request: ReplyRequest,
    accepts: impl FnOnce(&str) -> Option<Envelope>,
) -> std::result::Result<Delivery, SendError> {
    let sender = request.sender.to_lowercase();
    let original = store
        .inbox_message(&sender, &request.message_id)?
        .ok_or_else(|| SendError::Other(invalid("message_id", "Unknown inbox message")))?;
    let depth = original.hop.unwrap_or(0);
    if request.automatic {
        if original.auto_reply == Some(true) {
            return Err(SendError::Refused(
                "reply_refused: the message is itself an automatic reply".into(),
            ));
        }
        if depth >= MAX_AUTO_HOP {
            return Err(SendError::Refused(format!(
                "reply_refused: hop limit {MAX_AUTO_HOP} reached"
            )));
        }
        if store.has_auto_reply(&original.id, request.reply_id.as_deref())? {
            return Err(SendError::Refused(
                "reply_refused: this message already has an automatic reply".into(),
            ));
        }
        match accepts(&original.sender) {
            Some(envelope) if envelope >= Envelope::V1_1 => {}
            Some(_) => {
                return Err(SendError::Refused(
                    "reply_refused: the peer does not accept agent-mail/1.1, so it would not see auto_reply".into(),
                ))
            }
            None => {
                return Err(SendError::Refused(
                    "reply_refused: couldn't confirm the peer accepts agent-mail/1.1 (is agent-maild running?)".into(),
                ))
            }
        }
    }
    submit(
        home,
        identity,
        store,
        SendRequest {
            recipient: original.sender.clone(),
            text: request.text,
            message_id: request.reply_id,
            conversation_id: Some(original.conversation_id.clone()),
            agent: request.agent,
            to_session: original
                .agent
                .as_ref()
                .and_then(|agent| agent.session_id.clone()),
            hop: Some((depth + 1).min(crate::envelope::MAX_HOP)),
            auto_reply: request.automatic.then_some(true),
            in_reply_to: Some(original.id.clone()),
            // A reply stays in the project the question was about.
            project: original.project.clone(),
        },
    )
}

/// A random (v4) UUID.
pub fn new_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `localOrigin()`: Node's platform and arch names, and non-loopback addresses, sorted, at most 16.
pub fn local_origin() -> Origin {
    let platform = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    let mut addresses: Vec<String> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .map(|interface| interface.ip().to_string())
        .collect();
    addresses.sort();
    addresses.dedup();
    addresses.truncate(16);
    let hostname = gethostname::gethostname().to_string_lossy().into_owned();
    Origin {
        hostname: if hostname.trim().is_empty() {
            "unknown".into()
        } else {
            hostname
        },
        platform: platform.into(),
        arch: arch.into(),
        addresses,
    }
}

/// Agent metadata from flags, falling back to `AGENT_MAIL_*` (`agentProfileFromEnv`).
pub fn agent_profile(
    harness: Option<String>,
    name: Option<String>,
    model: Option<String>,
    session_id: Option<String>,
) -> Option<AgentMeta> {
    let pick = |value: Option<String>, env: &str| {
        value
            .or_else(|| std::env::var(env).ok())
            .filter(|value| !value.trim().is_empty())
    };
    let agent = AgentMeta {
        harness: pick(harness, "AGENT_MAIL_HARNESS"),
        name: pick(name, "AGENT_MAIL_AGENT_NAME"),
        model: pick(model, "AGENT_MAIL_MODEL"),
        session_id: pick(session_id, "AGENT_MAIL_SESSION"),
    };
    (agent != AgentMeta::default()).then_some(agent)
}

/// Validates an endpoint as TS `validateEndpoint` does: `http://host:port` and nothing else.
pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| invalid("endpoint", "must start with http://"))?;
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty() || authority.contains(['/', '?', '#', '@', ' ']) {
        return Err(invalid(
            "endpoint",
            "must be http://host:port without credentials, path, query, or fragment",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_v4() {
        let id = new_uuid();
        assert!(crate::js::is_uuid(&id));
        assert_eq!(&id[14..15], "4");
    }

    #[test]
    fn endpoints() {
        assert!(validate_endpoint("http://192.168.1.20:47832").is_ok());
        assert!(validate_endpoint("http://[fe80::1]:47832/").is_ok());
        assert!(validate_endpoint("https://pc2:47832").is_err());
        assert!(validate_endpoint("http://user@pc2:47832").is_err());
        assert!(validate_endpoint("http://pc2:47832/path").is_err());
    }

    #[test]
    fn origin_uses_node_names() {
        let origin = local_origin();
        assert!(!origin.hostname.is_empty());
        assert!(origin.addresses.len() <= 16);
        if cfg!(windows) {
            assert_eq!(origin.platform, "win32");
        }
    }
}
