//! The tray consumer's bookkeeping in `mail.db` (schema v3): which inbox messages it has handled,
//! the one-time backlog marker, and one row per headless run.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::envelope::{Envelope, Message};
use crate::error::Result;
use crate::store::{db, now_ms, Store};

const STARTED_KEY: &str = "consumer_started";

/// One headless run, as the dashboard shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Run {
    pub run_id: String,
    pub sender: String,
    pub message_id: String,
    pub conversation_id: String,
    pub project: Option<String>,
    pub workspace: String,
    pub tier: String,
    pub runner: String,
    pub state: String,
    pub outcome: Option<String>,
    pub cost_usd: Option<f64>,
    pub reply_id: Option<String>,
    pub created_at: u64,
    pub finished_at: Option<u64>,
}

impl Store {
    /// Whether the consumer has run before for this home. Its first enabled pass only records the
    /// backlog, so turning dispatch on doesn't answer old mail.
    pub fn consumer_started(&self) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM meta WHERE key = ?1", [STARTED_KEY], |_| {
                Ok(())
            })
            .optional()
            .map_err(db)?
            .is_some())
    }

    /// Records the whole current inbox as handled and remembers that the consumer has started.
    pub fn record_backlog(&self) -> Result<usize> {
        let now = now_ms() as i64;
        let marked = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO consumer_seen (sender, id, seen_at) SELECT sender, id, ?1 FROM inbox",
                [now],
            )
            .map_err(db)?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![STARTED_KEY, now.to_string()],
            )
            .map_err(db)?;
        Ok(marked)
    }

    /// Unacknowledged inbox messages the consumer hasn't handled, oldest first.
    pub fn unhandled_inbox(&self, limit: usize) -> Result<Vec<Message>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT message_json FROM inbox i
                 WHERE NOT EXISTS (SELECT 1 FROM consumer_seen s WHERE s.sender = i.sender AND s.id = i.id)
                   AND NOT EXISTS (SELECT 1 FROM acks a WHERE a.sender = i.sender AND a.id = i.id)
                 ORDER BY created_at, received_at LIMIT ?1",
            )
            .map_err(db)?;
        let rows = statement
            .query_map([limit as i64], |row| row.get::<_, String>(0))
            .map_err(db)?;
        rows.map(|row| {
            Message::parse_signed(serde_json::from_str(&row.map_err(db)?)?, Envelope::V1_2)
        })
        .collect()
    }

    /// Marks a message handled before the consumer acts on it, so a crash can drop an action but
    /// never repeat one.
    pub fn mark_handled(&self, sender: &str, id: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO consumer_seen (sender, id, seen_at) VALUES (?1, ?2, ?3)",
                params![sender, id, now_ms() as i64],
            )
            .map_err(db)?;
        Ok(())
    }

    pub fn start_run(&self, run: &Run) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO runs (run_id, sender, message_id, conversation_id, project, workspace, tier, runner, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    run.run_id,
                    run.sender,
                    run.message_id,
                    run.conversation_id,
                    run.project,
                    run.workspace,
                    run.tier,
                    run.runner,
                    run.state,
                    run.created_at as i64
                ],
            )
            .map_err(db)?;
        Ok(())
    }

    pub fn finish_run(
        &self,
        run_id: &str,
        state: &str,
        outcome: &str,
        cost_usd: Option<f64>,
        reply_id: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET state = ?2, outcome = ?3, cost_usd = ?4, reply_id = COALESCE(?5, reply_id), finished_at = ?6
                 WHERE run_id = ?1",
                params![run_id, state, outcome, cost_usd, reply_id, now_ms() as i64],
            )
            .map_err(db)?;
        Ok(())
    }

    /// The reply a run already sent, if any (set by the consumer, or by a run-bound reply in T-7).
    pub fn run_reply(&self, run_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT reply_id FROM runs WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(db)?
            .flatten())
    }

    pub fn recent_runs(&self, limit: usize) -> Result<Vec<Run>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT run_id, sender, message_id, conversation_id, project, workspace, tier, runner, state, outcome,
                        cost_usd, reply_id, created_at, finished_at
                 FROM runs ORDER BY created_at DESC LIMIT ?1",
            )
            .map_err(db)?;
        let rows = statement
            .query_map([limit as i64], |row| {
                Ok(Run {
                    run_id: row.get(0)?,
                    sender: row.get(1)?,
                    message_id: row.get(2)?,
                    conversation_id: row.get(3)?,
                    project: row.get(4)?,
                    workspace: row.get(5)?,
                    tier: row.get(6)?,
                    runner: row.get(7)?,
                    state: row.get(8)?,
                    outcome: row.get(9)?,
                    cost_usd: row.get(10)?,
                    reply_id: row.get(11)?,
                    created_at: row.get::<_, i64>(12)? as u64,
                    finished_at: row.get::<_, Option<i64>>(13)?.map(|at| at as u64),
                })
            })
            .map_err(db)?;
        rows.map(|row| row.map_err(db)).collect()
    }
}
