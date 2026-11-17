//! One-way import of the TS JSON store (`src/peer/store.ts`) into `mail.db` (ADR 0005).
//!
//! It only reads the `contacts`, `inbox`, `acked`, `outbox` and `sent` folders and never writes,
//! renames or deletes there, so the TS tools keep working and a rollback needs nothing restored.
//! Rows already in `mail.db` are left alone, so running it again imports nothing new. Journal
//! sessions and leases follow once `mail.db` has those tables (T-7).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::envelope::{Envelope, Message};
use crate::error::Result;
use crate::identity::{fingerprint, validate_public, PublicIdentity};
use crate::store::{Contact, Store};

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
