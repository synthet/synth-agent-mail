//! One-way import of the TS JSON store (`src/peer/store.ts`) into `mail.db` (ADR 0005).
//!
//! It only reads the `contacts`, `inbox`, `acked`, `outbox` and `sent` folders and never writes,
//! renames or deletes there, so the TS tools keep working and a rollback needs nothing restored.
//! Rows already in `mail.db` are left alone, so running it again imports nothing new.
//! `import_journal` also brings over the journal's sessions, their agents and conversation leases
//! (T-7), reading a copy of `journal.db` so the original is never opened or changed.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::envelope::{Envelope, Message};
use crate::error::Result;
use crate::identity::{fingerprint, validate_public, PublicIdentity};
use crate::sessions::Host;
use crate::store::{db, now_ms, Contact, Store};
use rusqlite::OptionalExtension;

/// Per area: rows inserted, rows already present, files that didn't parse.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AreaReport {
    pub imported: usize,
    pub already_present: usize,
    pub invalid: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MigrationReport {
    pub contacts: AreaReport,
    pub inbox: AreaReport,
    pub acked: AreaReport,
    pub outbox: AreaReport,
    pub sent: AreaReport,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonContact {
    identity: PublicIdentity,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonAck {
    sender: String,
    id: String,
}

/// Imports `<from>/{contacts,inbox,acked,outbox,sent}/*.json`. A missing folder is empty.
pub fn import_json_store(from: &Path, store: &Store) -> Result<MigrationReport> {
    let mut report = MigrationReport::default();
    each(from, "contacts", &mut report.contacts, |text| {
        let contact: JsonContact = serde_json::from_str(text)?;
        validate_public(&contact.identity)?;
        let fp = fingerprint(&contact.identity)?;
        store.import_contact(&Contact {
            fingerprint: fp,
            identity: contact.identity,
            endpoint: contact.endpoint,
        })
    })?;
    each(from, "inbox", &mut report.inbox, |text| {
        store.import_inbox(&signed(text)?)
    })?;
    each(from, "acked", &mut report.acked, |text| {
        let ack: JsonAck = serde_json::from_str(text)?;
        store.import_ack(&ack.sender, &ack.id)
    })?;
    each(from, "sent", &mut report.sent, |text| {
        store.import_sent(&signed(text)?)
    })?;
    // After `sent`, so a message that was both queued and delivered counts as delivered.
    each(from, "outbox", &mut report.outbox, |text| {
        store.import_outbox(&signed(text)?)
    })?;
    Ok(report)
}

/// TS stores messages already signed, under the 1.1 schema.
fn signed(text: &str) -> Result<Message> {
    Message::parse_signed(serde_json::from_str(text)?, Envelope::V1_1)
}

fn each(
    from: &Path,
    area: &str,
    report: &mut AreaReport,
    mut import: impl FnMut(&str) -> Result<bool>,
) -> Result<()> {
    let dir = from.join(area);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut paths: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => {
                report.invalid.push(name);
                continue;
            }
        };
        match import(&text) {
            Ok(true) => report.imported += 1,
            Ok(false) => report.already_present += 1,
            Err(crate::Error::Db(error)) => return Err(crate::Error::Db(error)),
            Err(_) => report.invalid.push(name),
        }
    }
    Ok(())
}

/// What `import_journal` brought over from `journal.db`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct JournalReport {
    pub sessions: AreaReport,
    pub leases: AreaReport,
}

/// Imports `<from>/journal.db` sessions (with their agent aliases) and conversation leases.
/// `None` when there is no journal. The TS default agent (the TS host itself) becomes this host,
/// and an alias already in `mail.db` keeps its agent id. Existing sessions and leases win.
pub fn import_journal(from: &Path, store: &Store, host: &Host) -> Result<Option<JournalReport>> {
    let source = from.join("journal.db");
    if !source.is_file() {
        return Ok(None);
    }
    // Read a private copy: opening the live file could create -wal/-shm or checkpoint it.
    let scratch =
        std::env::temp_dir().join(format!("agent-mail-journal-{}", crate::send::new_uuid()));
    std::fs::create_dir_all(&scratch)?;
    let result = (|| {
        let journal = snapshot(from, &scratch)?;
        let report = copy_sessions(&journal, store, host);
        drop(journal);
        report
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result.map(Some)
}

/// Copies `journal.db` and its WAL, then checks the copy. A TS process writing during the copy
/// can tear it, so a copy that fails `quick_check` is taken again, up to three times.
fn snapshot(from: &Path, scratch: &Path) -> Result<rusqlite::Connection> {
    for attempt in 0..3 {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(scratch.join(format!("journal.db{suffix}")));
        }
        for suffix in ["", "-wal"] {
            let file = from.join(format!("journal.db{suffix}"));
            if file.is_file() {
                std::fs::copy(&file, scratch.join(format!("journal.db{suffix}")))?;
            }
        }
        let journal = rusqlite::Connection::open(scratch.join("journal.db")).map_err(db)?;
        let check: rusqlite::Result<String> =
            journal.query_row("PRAGMA quick_check", [], |row| row.get(0));
        if check.as_deref() == Ok("ok") {
            return Ok(journal);
        }
        drop(journal);
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    Err(crate::Error::Db(
        "journal.db changed while it was copied; stop the TS tray and MCP servers, then run migrate again"
            .into(),
    ))
}

fn has_table(journal: &rusqlite::Connection, table: &str) -> Result<bool> {
    journal
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(db)
}

type JournalSession = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn copy_sessions(
    journal: &rusqlite::Connection,
    store: &Store,
    host: &Host,
) -> Result<JournalReport> {
    let mut report = JournalReport::default();
    if !has_table(journal, "sessions")? {
        return Ok(report);
    }
    let columns: Vec<String> = journal
        .prepare("SELECT name FROM pragma_table_info('sessions')")
        .map_err(db)?
        .query_map([], |row| row.get(0))
        .map_err(db)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db)?;
    let optional = |name: &str| {
        if columns.iter().any(|c| c == name) {
            format!("s.{name}")
        } else {
            "NULL".to_owned()
        }
    };
    let has_agents = has_table(journal, "agents")?;
    // The TS host's own agent id is its fingerprint (`ensureAgent(hostId, name)`).
    let ts_host: Option<String> = if has_agents {
        journal
            .query_row(
                "SELECT host_id FROM agents WHERE agent_id = host_id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?
    } else {
        None
    };
    let alias_sql = if has_agents {
        "(SELECT alias FROM agents a WHERE a.agent_id = s.agent_id)"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT s.session_id, s.agent_id, {alias_sql}, s.harness, s.model, s.last_seen, {}, {}, {}
         FROM sessions s ORDER BY s.session_id",
        optional("harness_session_id"),
        optional("display_name"),
        optional("workspace"),
    );
    let rows: Vec<JournalSession> = journal
        .prepare(&sql)
        .map_err(db)?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        })
        .map_err(db)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db)?;
    for (session_id, agent_id, alias, harness, model, last_seen, hsid, display, workspace) in rows {
        let is_ts_host = ts_host.as_deref() == Some(agent_id.as_str());
        let alias = match alias {
            _ if is_ts_host => host.name.clone(),
            Some(alias) if !alias.trim().is_empty() => alias,
            _ => {
                report.sessions.invalid.push(session_id);
                continue;
            }
        };
        let existing: Option<String> = store
            .conn
            .query_row(
                "SELECT agent_id FROM agents WHERE alias = ?1",
                [&alias],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?;
        let agent_id = match existing {
            Some(agent_id) => agent_id,
            None => {
                let agent_id = if is_ts_host || alias == host.name {
                    host.fingerprint.clone()
                } else {
                    agent_id
                };
                store
                    .conn
                    .execute(
                        "INSERT OR IGNORE INTO agents (agent_id, alias, created_at) VALUES (?1, ?2, ?3)",
                        rusqlite::params![agent_id, alias, now_ms() as i64],
                    )
                    .map_err(db)?;
                agent_id
            }
        };
        let inserted = store
            .conn
            .execute(
                "INSERT OR IGNORE INTO sessions (session_id, agent_id, harness, model, harness_session_id,
                   display_name, workspace, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    session_id, agent_id, harness, model, hsid, display, workspace, last_seen
                ],
            )
            .map_err(db)?;
        if inserted == 1 {
            report.sessions.imported += 1;
        } else {
            report.sessions.already_present += 1;
        }
    }
    if has_table(journal, "conversation_leases")? {
        let leases: Vec<(String, String, i64)> = journal
            .prepare(
                "SELECT conversation_id, session_id, lease_until FROM conversation_leases
                 ORDER BY conversation_id",
            )
            .map_err(db)?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?;
        for (conversation_id, session_id, lease_until) in leases {
            let known: bool = store
                .conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM sessions WHERE session_id = ?1)",
                    [&session_id],
                    |row| row.get(0),
                )
                .map_err(db)?;
            if !known {
                report.leases.invalid.push(conversation_id);
                continue;
            }
            let inserted = store
                .conn
                .execute(
                    "INSERT OR IGNORE INTO conversation_leases (conversation_id, session_id, lease_until)
                     SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM sessions WHERE session_id = ?2)",
                    rusqlite::params![conversation_id, session_id, lease_until],
                )
                .map_err(db)?;
            if inserted == 1 {
                report.leases.imported += 1;
            } else {
                report.leases.already_present += 1;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(dir: &Path, ts_host: &str) {
        let journal = rusqlite::Connection::open(dir.join("journal.db")).unwrap();
        journal
            .execute_batch(&format!(
                "CREATE TABLE agents (agent_id TEXT PRIMARY KEY, alias TEXT NOT NULL, host_id TEXT NOT NULL,
                   created_at INTEGER NOT NULL);
                 CREATE TABLE sessions (session_id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, harness TEXT,
                   model TEXT, last_seen INTEGER NOT NULL, harness_session_id TEXT, display_name TEXT,
                   workspace TEXT);
                 CREATE TABLE conversation_leases (conversation_id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                   lease_until INTEGER NOT NULL);
                 INSERT INTO agents VALUES ('{ts_host}', 'OLDNAME', '{ts_host}', 1);
                 INSERT INTO agents VALUES ('agent-r', 'reviewer', '{ts_host}', 1);
                 INSERT INTO sessions VALUES ('s-main', '{ts_host}', 'claude-code', NULL, 5, 'h1', 'main', 'D:/w');
                 INSERT INTO sessions VALUES ('s-rev', 'agent-r', 'codex', NULL, 6, NULL, NULL, NULL);
                 INSERT INTO conversation_leases VALUES ('conv-1', 's-main', 99);
                 INSERT INTO conversation_leases VALUES ('conv-2', 'gone', 99);"
            ))
            .unwrap();
    }

    #[test]
    fn journal_sessions_and_leases_import_once_and_the_journal_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (from, home) = (dir.path().join("ts"), dir.path().join("rs"));
        std::fs::create_dir_all(&from).unwrap();
        journal(&from, &"b".repeat(64));
        let before = std::fs::read(from.join("journal.db")).unwrap();
        let store = Store::open(&home).unwrap();
        let host = Host {
            fingerprint: "a".repeat(64),
            name: "ZEPHYR".into(),
        };

        let first = import_journal(&from, &store, &host).unwrap().unwrap();
        assert_eq!(first.sessions.imported, 2);
        assert_eq!(first.leases.imported, 1);
        assert_eq!(
            first.leases.invalid,
            vec!["conv-2"],
            "a lease for an unknown session is skipped"
        );
        let again = import_journal(&from, &store, &host).unwrap().unwrap();
        assert_eq!(again.sessions.imported, 0);
        assert_eq!(again.sessions.already_present, 2);
        assert_eq!(again.leases.imported, 0);

        let agent: String = store
            .conn
            .query_row(
                "SELECT agent_id FROM sessions WHERE session_id = 's-main'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            agent,
            "a".repeat(64),
            "the TS host's agent becomes this host"
        );
        let alias: String = store
            .conn
            .query_row(
                "SELECT alias FROM agents WHERE agent_id = 'agent-r'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(alias, "reviewer");
        assert_eq!(std::fs::read(from.join("journal.db")).unwrap(), before);
        assert!(!from.join("journal.db-shm").exists() && !from.join("journal.db-wal").exists());
        assert!(
            import_journal(&home, &store, &host).unwrap().is_none(),
            "no journal, nothing to do"
        );
    }
}
