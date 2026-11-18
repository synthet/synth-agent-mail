//! `agent-mail`: the CLI (ADR 0005). It never opens a network socket. `send` checks policy and
//! queues through `mail_core::send::submit`, then pokes the mailer (`agent-maild`) and waits briefly
//! to report delivered (exit 0) or queued (exit 2). Errors exit 1.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use agent_mail::install::{self, Program};
use agent_mail::ops::{self, Outcome};
use clap::{Parser, Subcommand};
use mail_core::identity::{fingerprint, validate_public, Identity, PublicIdentity};
use mail_core::migrate::import_json_store;
use mail_core::policy::Policy;
use mail_core::send::{agent_profile, local_origin, validate_endpoint, SendRequest};
use mail_core::store::{now_ms, Store};
use mail_core::{ipc, js};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "agent-mail",
    version,
    about = "Direct encrypted peer messaging through the local agent-maild mailer"
)]
struct Cli {
    /// agent-mail home (identity.json, policy.json, mail.db). Required until the TS cutover (T-10).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create local Ed25519/X25519 keys.
    Init {
        #[arg(long)]
        name: Option<String>,
    },
    /// Print this host's public identity.
    Identity,
    /// List hosts the mailer has seen recently (multicast, hosts.json, Tailscale).
    Discover {
        #[arg(long, default_value_t = 10)]
        since_minutes: u64,
    },
    /// Pin a peer: from a discovered host, or offline from its public.json.
    Trust {
        /// Discovered host name to match (optional with --file).
        name: Option<String>,
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Revoke a pinned peer.
    Untrust { fingerprint: String },
    /// List pinned peers.
    Peers,
    /// Queue a message for a pinned peer and wait briefly for delivery.
    Send {
        recipient: String,
        #[arg(long, conflicts_with = "file")]
        text: Option<String>,
        /// Read exact UTF-8 text from a file (- for stdin).
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        message_id: Option<String>,
        #[arg(long)]
        conversation: Option<String>,
        #[arg(long)]
        harness: Option<String>,
        #[arg(long)]
        agent_name: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        session: Option<String>,
        /// Project tag (agent-mail/1.2); dropped for peers that don't accept it.
        #[arg(long)]
        project: Option<String>,
        /// How long to wait for the mailer's first attempt before reporting "queued".
        #[arg(long, default_value_t = 8000)]
        wait_ms: u64,
    },
    /// Unacknowledged messages, oldest first.
    Inbox {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Mark a received message read.
    Ack {
        message_id: String,
        #[arg(long)]
        sender: String,
    },
    /// Local history of one conversation.
    Thread { conversation_id: String },
    /// Queue counts and whether the mailer is running.
    Status,
    /// Make every queued message due now and wake the mailer.
    Flush,
    /// Import the TS JSON store (contacts, inbox, acked, outbox, sent); never changes the source.
    Migrate {
        /// TS home to read; defaults to --home.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Write a deny-all policy.json, or validate the existing one.
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },
    /// Start agent-maild at logon for this home (a per-user task; no admin needed).
    Service {
        #[command(subcommand)]
        action: TaskAction,
        /// Mailer TCP port for `service install` (another than 47832 to run beside the TS listener).
        #[arg(long, default_value_t = 47832, global = true)]
        port: u16,
    },
    /// Start agent-mail-tray at logon for this home (a per-user task; no admin needed).
    Tray {
        #[command(subcommand)]
        action: TaskAction,
        /// Dashboard port for `tray install`.
        #[arg(long, default_value_t = 8788, global = true)]
        port: u16,
    },
    /// Serve the MCP tools on stdio (for mcp.json). Never opens a network socket.
    Mcp {
        /// Harness name sent as agent metadata (e.g. claude-code).
        #[arg(long)]
        harness: Option<String>,
        #[arg(long)]
        agent_name: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Accepted for TS mcp.json compatibility; the Rust server never listens.
        #[arg(long, hide = true)]
        no_listen: bool,
    },
}

#[derive(Subcommand)]
enum TaskAction {
    /// Register the logon task (replaces an existing one for this home).
    Install {
        /// Executable to run; defaults to the one next to agent-mail.
        #[arg(long)]
        exe: Option<PathBuf>,
        /// Also start it now.
        #[arg(long)]
        start: bool,
    },
    /// Remove the logon task.
    Uninstall,
    /// Whether the logon task exists.
    Status,
}

#[derive(Subcommand)]
enum PolicyAction {
    Init,
    Check,
}

type Failure = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(home) = cli.home else {
        eprintln!("agent-mail: --home is required (the Rust tools never default to the live TS home before cutover)");
        return ExitCode::from(1);
    };
    match run(&home, cli.command) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

fn print(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("output serializes")
    );
}

fn run(home: &Path, command: Command) -> Result<ExitCode, Failure> {
    match command {
        Command::Init { name } => {
            let name = name.unwrap_or_else(|| local_origin().hostname);
            let identity = Identity::generate(&name)?;
            write_new(
                &home.join("identity.json"),
                &(serde_json::to_string_pretty(&identity)?),
            )?;
            Store::open(home)?;
            print(&ops::whoami(
                &identity,
                agent_profile(None, None, None, None).as_ref(),
            )?);
        }
        Command::Identity => print(&ops::whoami(
            &Identity::load(home)?,
            agent_profile(None, None, None, None).as_ref(),
        )?),
        Command::Discover { since_minutes } => {
            let since = now_ms().saturating_sub(since_minutes * 60_000);
            print(&Store::open(home)?.hosts(since)?);
            if !ops::mailer_running(home) {
                eprintln!(
                    "agent-maild is not running for this home, so this list is not being refreshed"
                );
            }
        }
        Command::Trust {
            name,
            fingerprint: expected,
            file,
            endpoint,
        } => {
            let expected = expected.to_lowercase();
            let store = Store::open(home)?;
            let (identity, endpoint) = match file {
                Some(file) => (read_public(&file)?, endpoint),
                None => {
                    let (identity, seen) = store
                        .seen_identity(&expected)?
                        .filter(|(identity, _)| name.as_ref().is_none_or(|name| identity.name.eq_ignore_ascii_case(name)))
                        .ok_or("No discovered peer matches that name and fingerprint; use --file for offline pairing")?;
                    (identity, endpoint.or(Some(seen)))
                }
            };
            if let Some(endpoint) = &endpoint {
                validate_endpoint(endpoint)?;
            }
            store.trust(&identity, Some(&expected), endpoint.as_deref())?;
            print(&json!({ "identity": identity, "endpoint": endpoint }));
        }
        Command::Untrust { fingerprint } => {
            let fingerprint = fingerprint.to_lowercase();
            if !js::is_fingerprint(&fingerprint) {
                return Err("Use the full fingerprint".into());
            }
            Store::open(home)?.untrust(&fingerprint)?;
            print(&json!({ "revoked": fingerprint }));
        }
        Command::Peers => print(&Store::open(home)?.contacts()?),
        Command::Send {
            recipient,
            text,
            file,
            message_id,
            conversation,
            harness,
            agent_name,
            model,
            session,
            project,
            wait_ms,
        } => {
            let text = match (text, file) {
                (Some(text), None) => text,
                (None, Some(file)) => read_text(&file)?,
                _ => return Err("Provide exactly one of --text or --file".into()),
            };
            let request = SendRequest {
                recipient,
                text,
                message_id,
                conversation_id: conversation,
                agent: agent_profile(harness, agent_name, model, session),
                project,
                ..SendRequest::default()
            };
            return match ops::send_and_wait(home, request, Duration::from_millis(wait_ms))? {
                Outcome::Delivered(value) => {
                    print(&value);
                    Ok(ExitCode::SUCCESS)
                }
                Outcome::Queued(value) => {
                    print(&value);
                    Ok(ExitCode::from(2))
                }
            };
        }
        Command::Inbox { limit } => print(&Store::open(home)?.inbox(limit)?),
        Command::Ack { message_id, sender } => {
            Store::open(home)?.ack(&sender.to_lowercase(), &message_id)?;
            print(&json!({ "acknowledged": true }));
        }
        Command::Thread { conversation_id } => print(&Store::open(home)?.thread(&conversation_id)?),
        Command::Status => {
            let counts = Store::open(home)?.counts()?;
            print(
                &json!({ "pending": counts.pending, "unread": counts.unread, "sent": counts.sent, "mailer_running": ops::mailer_running(home) }),
            );
        }
        Command::Flush => {
            let queued = Store::open(home)?.retry_all()?;
            print(&json!({ "queued": queued, "mailer_running": ipc::poke(home) }));
        }
        Command::Migrate { from } => {
            let from = from.unwrap_or_else(|| home.to_path_buf());
            print(&import_json_store(&from, &Store::open(home)?)?);
        }
        Command::Service { action, port } => print(&task(Program::Mailer { port }, home, action)?),
        Command::Tray { action, port } => print(&task(Program::Tray { port }, home, action)?),
        Command::Mcp {
            harness,
            agent_name,
            model,
            no_listen: _,
        } => {
            let agent = agent_profile(harness, agent_name, model, None);
            tokio::runtime::Runtime::new()?
                .block_on(agent_mail::mcp::serve(home.to_path_buf(), agent))
                .map_err(|error| error.to_string())?;
        }
        Command::Policy {
            action: PolicyAction::Init,
        } => {
            let (created, path) = Policy::init(home)?;
            print(&json!({ "created": created, "path": path }));
        }
        Command::Policy {
            action: PolicyAction::Check,
        } => print(&json!({ "ok": true, "policy": Policy::load(home)? })),
    }
    Ok(ExitCode::SUCCESS)
}

/// A peer's public.json. The extra fields `identity` prints (`fingerprint`, `origin`, `agent`)
/// are ignored, so its output can be passed along as is; anything else must be the strict public
/// identity.
fn read_public(path: &Path) -> Result<PublicIdentity, Failure> {
    let mut value: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if let Some(object) = value.as_object_mut() {
        for key in ["fingerprint", "origin", "agent"] {
            object.remove(key);
        }
    }
    let identity: PublicIdentity = serde_json::from_value(value)?;
    validate_public(&identity)?;
    fingerprint(&identity)?;
    Ok(identity)
}

fn read_text(file: &str) -> Result<String, Failure> {
    if file == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        Ok(text)
    } else {
        Ok(std::fs::read_to_string(file)?)
    }
}

/// Creates a file that must not exist yet (private keys are never replaced).
fn write_new(path: &Path, text: &str) -> Result<(), Failure> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = options.open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::AlreadyExists => {
            "Identity already exists; refusing to replace private keys".to_owned()
        }
        _ => error.to_string(),
    })?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    Ok(())
}

fn task(program: Program, home: &Path, action: TaskAction) -> Result<Value, Failure> {
    Ok(match action {
        TaskAction::Install { exe, start } => install::install(&program, home, exe, start)?,
        TaskAction::Uninstall => install::uninstall(&program, home)?,
        TaskAction::Status => install::status(&program, home)?,
    })
}
