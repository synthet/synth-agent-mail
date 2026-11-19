//! Run mode (AC-15, AC-17): the headless agent the tray consumer starts answers through these, not
//! through `send_message`. The run id comes from `--run`, the token only from
//! `AGENT_MAIL_RUN_TOKEN`, and both are checked against the run row on every call. The recipient,
//! thread and message are the run's, from `mail.db`; nothing the agent passes can change them.

use std::path::Path;
use std::time::Duration;

use mail_core::envelope::AgentMeta;
use mail_core::runs::{run_session_id, RunMessage, RUN_ALIAS};
use mail_core::send::reply_for_run;
use mail_core::sessions::{Host, Registration};
use mail_core::store::Store;
use mail_core::Identity;

use crate::ops::{envelope_via_mailer, wait_for_delivery, OpError, Outcome};

/// Env var that carries the run token. Never put the token on a command line.
pub const RUN_TOKEN_ENV: &str = "AGENT_MAIL_RUN_TOKEN";

/// A headless run's binding: its run id and token.
#[derive(Clone)]
pub struct RunContext {
    pub run_id: String,
    token: String,
}

/// The token never appears in debug output.
impl std::fmt::Debug for RunContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunContext")
            .field("run_id", &self.run_id)
            .field("token", &format_args!("<redacted>"))
            .finish()
    }
}

impl RunContext {
    /// `--run <id>` with the token from `AGENT_MAIL_RUN_TOKEN`.
    pub fn from_env(run_id: String) -> Result<Self, OpError> {
        let token = std::env::var(RUN_TOKEN_ENV).unwrap_or_default();
        if token.trim().is_empty() {
            return Err(OpError::Refused(format!(
                "reply_refused: run mode needs {RUN_TOKEN_ENV}"
            )));
        }
        Ok(RunContext { run_id, token })
    }

    /// The session a run publishes from: one per run, so recipients can tell runs apart.
    pub fn session_id(&self) -> String {
        run_session_id(&self.run_id)
    }

    /// Checks the token and that the run is still going; returns the message it answers.
    pub fn check(&self, home: &Path) -> Result<RunMessage, OpError> {
        Store::open(home)?
            .check_run(&self.run_id, &self.token)?
            .map_err(|refusal| OpError::Refused(refusal.to_string()))
    }

    /// The run's one automatic reply, then a short wait for the mailer's first attempt.
    pub fn reply_and_wait(
        &self,
        home: &Path,
        text: String,
        agent: Option<AgentMeta>,
        wait: Duration,
    ) -> Result<Outcome, OpError> {
        let identity = Identity::load(home)?;
        let mut store = Store::open(home)?;
        let queued = reply_for_run(
            home,
            &identity,
            &mut store,
            &self.run_id,
            &self.token,
            text,
            agent,
            |fp| envelope_via_mailer(home, fp),
        )?;
        wait_for_delivery(home, &store, queued, wait)
    }

    /// Registers (or refreshes) the run's own session so it can publish to sessions on this PC. It
    /// uses the run alias, never the host's agent, so recipients see that a run wrote it from
    /// untrusted remote mail. `finish_run` ends the session.
    pub fn register(&self, home: &Path, store: &Store) -> Result<String, OpError> {
        self.check(home)?;
        let session_id = self.session_id();
        store.register_session(
            &host(&Identity::load(home)?)?,
            &Registration {
                session_id: session_id.clone(),
                alias: Some(RUN_ALIAS.into()),
                harness: Some("agent-mail-run".into()),
                ..Registration::default()
            },
        )?;
        Ok(session_id)
    }
}

/// This PC as the session tables see it.
pub fn host(identity: &Identity) -> Result<Host, OpError> {
    Ok(Host {
        fingerprint: identity.fingerprint()?,
        name: identity.name.clone(),
    })
}
