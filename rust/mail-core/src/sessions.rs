//! Same-PC sessions, conversation leases and local messages in `mail.db` (schema v4, T-7).
//!
//! This is the Rust port of the session half of the TS journal (`src/fabric/journal.ts`): agents
//! on this PC register a session, keep it live with heartbeats, lease the conversations they
//! answer, and publish direct messages to each other. None of it is synced or sent to a peer.
//! Leases are advisory routing state, not an access control, and not task claims (ADR 0002,
//! ADR 0003). Group, broadcast and topic audiences need `mailbox_subscribe`, which is not ported.

use std::sync::LazyLock;

use regex::Regex;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{invalid, Result};
use crate::send::new_uuid;
use crate::store::{db, now_ms, Store};

/// A session counts as live this long after its last heartbeat (ADR 0003).
pub const SESSION_TTL_MS: u64 = 10 * 60_000;
/// A conversation lease lasts this long unless renewed or released (ADR 0003).
pub const LEASE_TTL_MS: u64 = 30 * 60_000;
/// Largest local message body, in bytes (TS `MAX_BODY`).
pub const MAX_BODY: usize = 64 * 1024;
/// At most this many publishes per session in `RATE_WINDOW_MS` (TS `rateLimit`).
const RATE_LIMIT: i64 = 60;
const RATE_WINDOW_MS: u64 = 10_000;

static ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,79}$").expect("regex"));

/// TS `SessionRegistration`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Registration {
    pub session_id: String,
    pub alias: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub harness_session_id: Option<String>,
    pub display_name: Option<String>,
    pub workspace: Option<String>,
}

/// This PC, as the session tables need it: the default agent is the host itself.
#[derive(Clone, Debug)]
pub struct Host {
    pub fingerprint: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Lease {
    pub conversation_id: String,
    pub lease_until: u64,
}

/// One live session in `mailbox_list_sessions`. No message bodies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LiveSession {
    pub session_id: String,
    pub agent_id: String,
    pub alias: String,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub last_seen: u64,
    pub harness_session_id: Option<String>,
    pub display_name: Option<String>,
    pub workspace: Option<String>,
    pub leases: Vec<Lease>,
}

/// A direct local message. `agent_id` is the recipient agent from its `mailbox_register_session`.
#[derive(Clone, Debug, Default)]
pub struct Publish {
    pub session_id: String,
    pub body: String,
    pub subject: Option<String>,
    /// `direct` (the default); anything else answers `not_ported`.
    pub audience: Option<String>,
    pub agent_id: Option<String>,
    pub idempotency_key: Option<String>,
}

struct SessionRow {
    session_id: String,
    agent_id: String,
}

fn session_id(value: &str) -> Result<&str> {
    if value.is_empty() || value.chars().count() > 200 || value.contains(['\r', '\n']) {
        return Err(invalid(
            "session_id",
            "must be 1-200 characters on one line",
        ));
    }
    Ok(value)
}

fn id<'a>(value: &'a str, field: &'static str) -> Result<&'a str> {
    if !ID_RE.is_match(value) {
        return Err(invalid(
            field,
            "must match [A-Za-z0-9][A-Za-z0-9._:-]{0,79}",
        ));
    }
    Ok(value)
}

/// TS `optionalText`: trimmed, empty means absent, no control characters.
fn text(value: Option<&str>, max: usize, field: &'static str) -> Result<Option<String>> {
    let Some(value) = value.map(str::trim) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    if value.chars().count() > max || value.chars().any(|c| c.is_control()) {
        return Err(invalid(
            field,
            format!("must be at most {max} characters without control characters"),
        ));
    }
    Ok(Some(value.to_owned()))
}

impl Store {
    /// TS `registerSession`. The alias defaults to the host name, whose agent id is the host
    /// fingerprint; any other alias gets a random agent id the first time it is used.
    pub fn register_session(&self, host: &Host, input: &Registration) -> Result<Value> {
        let sid = session_id(&input.session_id)?;
        let harness_session_id = text(
            input.harness_session_id.as_deref(),
            200,
            "harness_session_id",
        )?;
        let display_name = text(input.display_name.as_deref(), 80, "display_name")?;
        let workspace = text(input.workspace.as_deref(), 400, "workspace")?;
        let harness = text(input.harness.as_deref(), 80, "harness")?;
        let model = text(input.model.as_deref(), 120, "model")?;
        let alias: String = input
            .alias
            .as_deref()
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
            .unwrap_or(&host.name)
            .chars()
            .take(80)
            .collect();
        if alias.is_empty() {
            return Err(invalid("alias", "is empty"));
        }
        let agent_id = self.agent_for(&alias, host)?;
        let prior: Option<String> = self
            .conn
            .query_row(
                "SELECT agent_id FROM sessions WHERE session_id = ?1",
                [sid],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?;
        if prior.is_some_and(|prior| prior != agent_id) {
            return Err(invalid(
                "session_id",
                "is already registered to another agent",
            ));
        }
        self.conn
            .execute(
                "INSERT INTO sessions (session_id, agent_id, harness, model, harness_session_id, display_name, workspace, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (session_id) DO UPDATE SET last_seen = excluded.last_seen, harness = excluded.harness,
                   model = excluded.model, harness_session_id = excluded.harness_session_id,
                   display_name = excluded.display_name, workspace = excluded.workspace",
                params![sid, agent_id, harness, model, harness_session_id, display_name, workspace, now_ms() as i64],
            )
            .map_err(db)?;
        Ok(json!({ "agent_id": agent_id, "session_id": sid }))
    }

    fn agent_for(&self, alias: &str, host: &Host) -> Result<String> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT agent_id FROM agents WHERE alias = ?1",
                [alias],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?;
        if let Some(agent_id) = existing {
            return Ok(agent_id);
        }
        let agent_id = if alias == host.name {
            host.fingerprint.clone()
        } else {
            new_uuid()
        };
        self.conn
            .execute(
                "INSERT OR IGNORE INTO agents (agent_id, alias, created_at) VALUES (?1, ?2, ?3)",
                params![agent_id, alias, now_ms() as i64],
            )
            .map_err(db)?;
        // Another process may have added the alias first; its agent id wins.
        self.conn
            .query_row(
                "SELECT agent_id FROM agents WHERE alias = ?1",
                [alias],
                |row| row.get(0),
            )
            .map_err(db)
    }

    fn must_session(&self, sid: &str) -> Result<SessionRow> {
        let sid = session_id(sid)?;
        self.conn
            .query_row(
                "SELECT session_id, agent_id FROM sessions WHERE session_id = ?1",
                [sid],
                |row| {
                    Ok(SessionRow {
                        session_id: row.get(0)?,
                        agent_id: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(db)?
            .ok_or_else(|| invalid("session_id", "Unknown session; register it first"))
    }

    /// TS `heartbeat`: marks the session live and renews its unexpired leases.
    pub fn heartbeat(&mut self, sid: &str) -> Result<Value> {
        let session = self.must_session(sid)?;
        let now = now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        tx.execute(
            "UPDATE sessions SET last_seen = ?1 WHERE session_id = ?2",
            params![now as i64, session.session_id],
        )
        .map_err(db)?;
        let renewed = tx
            .execute(
                "UPDATE conversation_leases SET lease_until = ?1 WHERE session_id = ?2 AND lease_until > ?3",
                params![(now + LEASE_TTL_MS) as i64, session.session_id, now as i64],
            )
            .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(json!({ "session_id": session.session_id, "last_seen": now, "leases_renewed": renewed }))
    }

    /// TS `endSession`: the session stops counting as live and its leases are released. The row
    /// stays, so its acknowledgements and agent binding survive a later heartbeat.
    pub fn end_session(&mut self, sid: &str) -> Result<Value> {
        let session = self.must_session(sid)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        tx.execute(
            "UPDATE sessions SET last_seen = 0 WHERE session_id = ?1",
            [&session.session_id],
        )
        .map_err(db)?;
        let released = tx
            .execute(
                "DELETE FROM conversation_leases WHERE session_id = ?1",
                [&session.session_id],
            )
            .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(json!({ "session_id": session.session_id, "released": released }))
    }

    /// TS `sessionDirectory`: live sessions, newest heartbeat first, with alias and leases.
    pub fn live_sessions(&self) -> Result<Vec<LiveSession>> {
        let now = now_ms();
        let since = now.saturating_sub(SESSION_TTL_MS);
        let mut statement = self
            .conn
            .prepare(
                "SELECT s.session_id, s.agent_id, COALESCE(a.alias, ''), s.harness, s.model, s.last_seen,
                        s.harness_session_id, s.display_name, s.workspace
                 FROM sessions s LEFT JOIN agents a ON a.agent_id = s.agent_id
                 WHERE s.last_seen > 0 AND s.last_seen >= ?1 ORDER BY s.last_seen DESC, s.session_id",
            )
            .map_err(db)?;
        let rows = statement
            .query_map([since as i64], |row| {
                Ok(LiveSession {
                    session_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    alias: row.get(2)?,
                    harness: row.get(3)?,
                    model: row.get(4)?,
                    last_seen: row.get::<_, i64>(5)? as u64,
                    harness_session_id: row.get(6)?,
                    display_name: row.get(7)?,
                    workspace: row.get(8)?,
                    leases: Vec::new(),
                })
            })
            .map_err(db)?;
        let mut sessions = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db)?;
        let mut leases = self
            .conn
            .prepare(
                "SELECT conversation_id, lease_until FROM conversation_leases
                 WHERE session_id = ?1 AND lease_until > ?2 ORDER BY conversation_id",
            )
            .map_err(db)?;
        for session in &mut sessions {
            session.leases = leases
                .query_map(params![session.session_id, now as i64], |row| {
                    Ok(Lease {
                        conversation_id: row.get(0)?,
                        lease_until: row.get::<_, i64>(1)? as u64,
                    })
                })
                .map_err(db)?
                .collect::<rusqlite::Result<_>>()
                .map_err(db)?;
        }
        Ok(sessions)
    }

    /// TS `claimConversation`: takes or renews the lease. Another session's unexpired lease wins
    /// and is reported as `lease_conflict`. Claiming also counts as a heartbeat.
    pub fn lease_conversation(&mut self, sid: &str, conversation_id: &str) -> Result<Value> {
        let session = self.must_session(sid)?;
        let conversation = id(conversation_id, "conversation_id")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        let now = now_ms();
        let held: Option<(String, i64)> = tx
            .query_row(
                "SELECT session_id, lease_until FROM conversation_leases WHERE conversation_id = ?1",
                [conversation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(db)?;
        if let Some((holder, until)) = &held {
            if *holder != session.session_id && *until as u64 > now {
                return Ok(
                    json!({ "status": "lease_conflict", "conversation_id": conversation, "holder": holder, "lease_until": until }),
                );
            }
        }
        let lease_until = now + LEASE_TTL_MS;
        tx.execute(
            "INSERT INTO conversation_leases (conversation_id, session_id, lease_until) VALUES (?1, ?2, ?3)
             ON CONFLICT (conversation_id) DO UPDATE SET session_id = excluded.session_id, lease_until = excluded.lease_until",
            params![conversation, session.session_id, lease_until as i64],
        )
        .map_err(db)?;
        tx.execute(
            "UPDATE sessions SET last_seen = ?1 WHERE session_id = ?2",
            params![now as i64, session.session_id],
        )
        .map_err(db)?;
        tx.commit().map_err(db)?;
        let renewed =
            held.is_some_and(|(holder, until)| holder == session.session_id && until as u64 > now);
        Ok(json!({
            "status": if renewed { "renewed" } else { "leased" },
            "conversation_id": conversation, "session_id": session.session_id, "lease_until": lease_until,
        }))
    }

    /// TS `releaseConversation`. Releasing a lease the session doesn't hold changes nothing.
    pub fn release_conversation(&self, sid: &str, conversation_id: &str) -> Result<Value> {
        let session = self.must_session(sid)?;
        let conversation = id(conversation_id, "conversation_id")?;
        let changes = self
            .conn
            .execute(
                "DELETE FROM conversation_leases WHERE conversation_id = ?1 AND session_id = ?2",
                params![conversation, session.session_id],
            )
            .map_err(db)?;
        Ok(json!({ "conversation_id": conversation, "released": changes > 0 }))
    }

    /// The session holding an unexpired lease on a conversation, if any.
    pub fn conversation_holder(&self, conversation_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT session_id FROM conversation_leases WHERE conversation_id = ?1 AND lease_until > ?2",
                params![conversation_id, now_ms() as i64],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)
    }

    /// TS `publish` for the direct audience: stores the message for the sender's agent and the
    /// recipient agent. The recipient sees it on its next list, poll or hook; nothing is resumed.
    pub fn publish_local(&mut self, input: &Publish) -> Result<Value> {
        let session = self.must_session(&input.session_id)?;
        match input.audience.as_deref().unwrap_or("direct") {
            "direct" => {}
            "group" | "broadcast" | "topic" => {
                return Err(invalid(
                    "audience",
                    "not_ported: group, broadcast and topic need mailbox_subscribe, which the Rust agent-mail does not carry; use direct",
                ))
            }
            _ => return Err(invalid("audience", "must be direct, group, broadcast or topic")),
        }
        if input.body.is_empty() || input.body.len() > MAX_BODY {
            return Err(invalid("body", "must be 1-65536 bytes"));
        }
        let target = id(input.agent_id.as_deref().unwrap_or(""), "agent_id")?.to_owned();
        let subject: String = input
            .subject
            .as_deref()
            .unwrap_or("inform")
            .chars()
            .take(200)
            .collect();
        let key = input
            .idempotency_key
            .as_deref()
            .map(|key| key.chars().take(200).collect::<String>());
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        if let Some(key) = &key {
            let prior: Option<(String, String)> = tx
                .query_row(
                    "SELECT message_id, thread_id FROM local_messages WHERE idempotency_key = ?1",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(db)?;
            if let Some((message_id, thread_id)) = prior {
                return Ok(
                    json!({ "duplicate": true, "message_id": message_id, "thread_id": thread_id }),
                );
            }
        }
        let known: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM agents WHERE agent_id = ?1)",
                [&target],
                |row| row.get(0),
            )
            .map_err(db)?;
        if !known {
            return Err(invalid("agent_id", "Unknown agent"));
        }
        let now = now_ms();
        let recent: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM local_messages WHERE sender_session = ?1 AND created_at > ?2",
                params![
                    session.session_id,
                    now.saturating_sub(RATE_WINDOW_MS) as i64
                ],
                |row| row.get(0),
            )
            .map_err(db)?;
        if recent >= RATE_LIMIT {
            return Err(invalid("session_id", "Publish rate exceeded"));
        }
        let message_id = new_uuid();
        tx.execute(
            "INSERT INTO local_messages (message_id, thread_id, sender_agent, sender_session, subject, body, idempotency_key, created_at)
             VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![message_id, session.agent_id, session.session_id, subject, input.body, key, now as i64],
        )
        .map_err(db)?;
        for agent in [&session.agent_id, &target] {
            tx.execute(
                "INSERT OR IGNORE INTO local_audience (message_id, agent_id) VALUES (?1, ?2)",
                params![message_id, agent],
            )
            .map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(json!({ "duplicate": false, "message_id": message_id, "thread_id": message_id }))
    }

    fn visible(&self, session: &SessionRow, limit: usize, with_body: bool) -> Result<Vec<Value>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT m.message_id, m.thread_id, m.subject, m.body, m.sender_agent, m.sender_session, m.created_at
                 FROM local_messages m JOIN local_audience a ON a.message_id = m.message_id
                 WHERE a.agent_id = ?1
                   AND NOT EXISTS (SELECT 1 FROM local_acks k WHERE k.session_id = ?2 AND k.message_id = m.message_id)
                 ORDER BY m.created_at, m.message_id LIMIT ?3",
            )
            .map_err(db)?;
        let rows = statement
            .query_map(
                params![session.agent_id, session.session_id, limit as i64],
                |row| {
                    let mut value = json!({
                        "message_id": row.get::<_, String>(0)?,
                        "thread_id": row.get::<_, String>(1)?,
                        "subject": row.get::<_, String>(2)?,
                        "semantic": "inform",
                        "sender_agent": row.get::<_, String>(4)?,
                        "sender_session": row.get::<_, String>(5)?,
                        "created_at": row.get::<_, i64>(6)?,
                        "untrusted": true,
                    });
                    if with_body {
                        value["body"] = json!(row.get::<_, String>(3)?);
                    }
                    Ok(value)
                },
            )
            .map_err(db)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db)
    }

    /// TS `inbox`: unread local messages for one session. Bodies are untrusted.
    pub fn local_inbox(&self, sid: &str, limit: usize) -> Result<Vec<Value>> {
        let session = self.must_session(sid)?;
        self.visible(&session, limit, true)
    }

    /// TS `acknowledge`, for this session only.
    pub fn local_ack(&self, sid: &str, message_id: &str) -> Result<Value> {
        let session = self.must_session(sid)?;
        let visible: bool = self
            .conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM local_audience WHERE message_id = ?1 AND agent_id = ?2)",
                params![message_id, session.agent_id],
                |row| row.get(0),
            )
            .map_err(db)?;
        if !visible {
            return Err(invalid(
                "message_id",
                "Unknown message or outside this session's audience",
            ));
        }
        let inserted = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO local_acks (session_id, message_id, acked_at) VALUES (?1, ?2, ?3)",
                params![session.session_id, message_id, now_ms() as i64],
            )
            .map_err(db)?;
        Ok(json!({ "acknowledged": true, "duplicate": inserted == 0 }))
    }

    /// TS `search`: subjects and bodies visible to this session. Results omit the body.
    pub fn local_search(&self, sid: &str, query: &str) -> Result<Vec<Value>> {
        let session = self.must_session(sid)?;
        let needle: String = query.trim().chars().take(80).collect();
        if needle.is_empty() {
            return Err(invalid("query", "Search query is empty"));
        }
        let like = format!(
            "%{}%",
            needle
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut statement = self
            .conn
            .prepare(
                "SELECT m.message_id, m.subject, m.sender_agent, m.created_at
                 FROM local_messages m JOIN local_audience a ON a.message_id = m.message_id
                 WHERE a.agent_id = ?1 AND (m.subject LIKE ?2 ESCAPE '\\' OR m.body LIKE ?2 ESCAPE '\\')
                 ORDER BY m.created_at LIMIT 20",
            )
            .map_err(db)?;
        let rows = statement
            .query_map(params![session.agent_id, like], |row| {
                Ok(json!({
                    "message_id": row.get::<_, String>(0)?,
                    "subject": row.get::<_, String>(1)?,
                    "semantic": "inform",
                    "sender_agent": row.get::<_, String>(2)?,
                    "created_at": row.get::<_, i64>(3)?,
                    "untrusted": true,
                }))
            })
            .map_err(db)?;
        rows.collect::<rusqlite::Result<_>>().map_err(db)
    }

    /// TS `digest` (`mailbox_poll`): a bounded metadata notice. The body is never included, and an
    /// unknown session gets an empty digest rather than an error.
    pub fn local_digest(&self, sid: &str, limit: usize) -> Result<Value> {
        let messages = match self.must_session(sid) {
            Ok(session) => self.visible(&session, limit, false)?,
            Err(_) => Vec::new(),
        };
        let messages: Vec<Value> = messages
            .into_iter()
            .map(|row| {
                let subject: String = row["subject"]
                    .as_str()
                    .unwrap_or("")
                    .replace(['\r', '\n'], " ")
                    .chars()
                    .take(120)
                    .collect();
                json!({ "message_id": row["message_id"], "subject": subject, "sender_agent": row["sender_agent"] })
            })
            .collect();
        Ok(json!({ "ok": true, "untrusted": true, "messages": messages }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host {
            fingerprint: "a".repeat(64),
            name: "ZEPHYR".into(),
        }
    }

    fn register(store: &Store, sid: &str, alias: Option<&str>) -> String {
        let registration = Registration {
            session_id: sid.into(),
            alias: alias.map(str::to_owned),
            harness: Some("claude-code".into()),
            ..Registration::default()
        };
        store.register_session(&host(), &registration).unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn default_alias_is_the_host_and_sessions_are_listed_with_leases() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(register(&store, "s1", None), "a".repeat(64));
        let other = register(&store, "s2", Some("reviewer"));
        assert_ne!(other, "a".repeat(64));
        assert_eq!(
            register(&store, "s3", Some("reviewer")),
            other,
            "same alias, same agent"
        );
        assert_eq!(
            store.lease_conversation("s1", "conv-1").unwrap()["status"],
            "leased"
        );
        assert_eq!(
            store.lease_conversation("s1", "conv-1").unwrap()["status"],
            "renewed"
        );
        let conflict = store.lease_conversation("s2", "conv-1").unwrap();
        assert_eq!(conflict["status"], "lease_conflict");
        assert_eq!(conflict["holder"], "s1");
        assert_eq!(
            store.conversation_holder("conv-1").unwrap().as_deref(),
            Some("s1")
        );
        let live = store.live_sessions().unwrap();
        assert_eq!(live.len(), 3);
        let s1 = live.iter().find(|s| s.session_id == "s1").unwrap();
        assert_eq!(s1.alias, "ZEPHYR");
        assert_eq!(s1.leases[0].conversation_id, "conv-1");
        assert_eq!(store.heartbeat("s1").unwrap()["leases_renewed"], 1);
        assert_eq!(store.end_session("s1").unwrap()["released"], 1);
        assert!(store
            .live_sessions()
            .unwrap()
            .iter()
            .all(|s| s.session_id != "s1"));
        assert_eq!(
            store.release_conversation("s2", "conv-1").unwrap()["released"],
            false
        );
        assert!(store.heartbeat("nobody").is_err());
        assert!(store.lease_conversation("s2", "bad id").is_err());
    }

    #[test]
    fn a_session_cannot_move_to_another_agent() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        register(&store, "s1", None);
        let moved = Registration {
            session_id: "s1".into(),
            alias: Some("other".into()),
            ..Registration::default()
        };
        assert!(store.register_session(&host(), &moved).is_err());
    }

    #[test]
    fn direct_publish_reaches_the_recipient_once_and_poll_has_no_body() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        register(&store, "sender", None);
        let reviewer = register(&store, "reader", Some("reviewer"));
        let publish = Publish {
            session_id: "sender".into(),
            body: "the body".into(),
            subject: Some("hello".into()),
            agent_id: Some(reviewer.clone()),
            idempotency_key: Some("k1".into()),
            ..Publish::default()
        };
        let first = store.publish_local(&publish).unwrap();
        assert_eq!(first["duplicate"], false);
        let again = store.publish_local(&publish).unwrap();
        assert_eq!(again["duplicate"], true);
        assert_eq!(again["message_id"], first["message_id"]);

        let inbox = store.local_inbox("reader", 20).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0]["body"], "the body");
        assert_eq!(inbox[0]["untrusted"], true);
        let digest = store.local_digest("reader", 5).unwrap();
        assert_eq!(digest["messages"][0]["subject"], "hello");
        assert!(digest["messages"][0].get("body").is_none());
        assert_eq!(store.local_search("reader", "body").unwrap().len(), 1);
        assert!(store.local_search("reader", "body").unwrap()[0]
            .get("body")
            .is_none());

        let id = first["message_id"].as_str().unwrap();
        assert_eq!(store.local_ack("reader", id).unwrap()["duplicate"], false);
        assert_eq!(store.local_ack("reader", id).unwrap()["duplicate"], true);
        assert!(store.local_inbox("reader", 20).unwrap().is_empty());
        assert_eq!(
            store.local_digest("nobody", 5).unwrap()["messages"],
            json!([])
        );
    }

    #[test]
    fn publish_refuses_unknown_agents_other_audiences_and_oversize_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        register(&store, "sender", None);
        let base = Publish {
            session_id: "sender".into(),
            body: "x".into(),
            agent_id: Some("nope".into()),
            ..Publish::default()
        };
        assert!(store
            .publish_local(&base)
            .unwrap_err()
            .to_string()
            .contains("Unknown agent"));
        let group = Publish {
            audience: Some("group".into()),
            ..base.clone()
        };
        assert!(store
            .publish_local(&group)
            .unwrap_err()
            .to_string()
            .contains("not_ported"));
        let big = Publish {
            body: "x".repeat(MAX_BODY + 1),
            agent_id: Some("a".repeat(64)),
            ..base
        };
        assert!(store.publish_local(&big).is_err());
    }
}
