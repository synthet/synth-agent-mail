//! Live delivery through the consumer (ADR 0006): a received message that names a checkout is
//! written, as a notice, into the one live Claude Code session working in that checkout. A stub
//! named pipe stands in for the session, and the session records live in a temp folder.
#![cfg(windows)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use agent_mail_tray::consumer::{default_runners, tick_in, RateWindow};
use agent_mail_tray::live::process_start;
use mail_core::envelope::{Envelope, Intent, Message, Origin};
use mail_core::store::{now_ms, Store};
use mail_core::Identity;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;

const TOKEN: &str = "peer-token-never-logged";

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn message(
    from: &Identity,
    to: &str,
    id: &str,
    checkout: mail_core::envelope::Checkout,
) -> Message {
    Message {
        id: id.into(),
        conversation_id: "00000000-0000-4000-8000-0000000000d1".into(),
        sender: from.fingerprint().unwrap(),
        recipient: to.into(),
        text: "BODY-THAT-MUST-NOT-REACH-THE-PIPE".into(),
        created_at: now_ms(),
        origin: Origin {
            hostname: "alpha".into(),
            platform: "win32".into(),
            arch: "x64".into(),
            addresses: vec![],
        },
        agent: None,
        to_session: None,
        hop: None,
        auto_reply: None,
        in_reply_to: None,
        project: None,
        checkout: Some(checkout),
        intent: Some(Intent::Notify),
        signature: String::new(),
    }
    .sign(from, Envelope::V1_3)
    .unwrap()
}

fn policy(home: &Path, peer: &str, roots: &Path, live: bool) {
    std::fs::write(
        home.join("policy.json"),
        json!({
            "allowed_peers": [], "max_bytes": 16384, "checkout_roots": [roots],
            "dispatch": { "enabled": true, "live": { "enabled": live },
                "limits": { "per_peer_per_minute": 10, "timeout_ms": 30000, "budget_usd": 0.1 },
                "peers": { peer: {} } }
        })
        .to_string(),
    )
    .unwrap();
}

/// A stub inbox pipe that collects whatever lines one connection sends, or nothing within `wait`.
fn listen(
    runtime: &tokio::runtime::Runtime,
    name: &str,
    wait: Duration,
) -> tokio::task::JoinHandle<Vec<String>> {
    let _context = runtime.enter();
    let server = tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .create(name)
        .unwrap();
    runtime.spawn(async move {
        if tokio::time::timeout(wait, server.connect()).await.is_err() {
            return Vec::new();
        }
        let mut lines = tokio::io::BufReader::new(server).lines();
        let mut got = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            got.push(line);
        }
        got
    })
}

#[test]
fn a_notice_is_written_to_the_checkout_session_and_the_token_stays_out_of_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("bob");
    std::fs::create_dir_all(&home).unwrap();
    let bob = Identity::generate("bob").unwrap();
    std::fs::write(
        home.join("identity.json"),
        serde_json::to_string(&bob).unwrap(),
    )
    .unwrap();
    let alice = Identity::generate("alice").unwrap();
    let alice_fp = alice.fingerprint().unwrap();
    Store::open(&home)
        .unwrap()
        .trust(&alice.public(), None, None)
        .unwrap();

    // The same repo: alice's identity of it, and bob's clone under his checkout root.
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q"]);
    git(&src, &["commit", "-q", "--allow-empty", "-m", "root"]);
    let roots = dir.path().join("roots");
    std::fs::create_dir_all(&roots).unwrap();
    let app = roots.join("app");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            &src.to_string_lossy(),
            &app.to_string_lossy(),
        ],
    );
    let checkout = mail_core::checkout::local_checkout(&src).unwrap();

    // One live session in bob's clone: this test process, with a stub pipe.
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let pid = std::process::id();
    let pipe = format!(r"\\.\pipe\agent-mail-consumer-live-{pid}");
    std::fs::write(
        sessions.join(format!("{pid}.json")),
        json!({ "pid": pid, "sessionId": "bob-session", "cwd": app.join("src"), "kind": "interactive",
                "procStart": process_start(pid).unwrap().to_string(), "messagingSocketPath": pipe, "name": "work" })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        sessions.join(format!("{pid}.abcd.key")),
        json!({ "peerToken": TOKEN }).to_string(),
    )
    .unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let runners = default_runners();
    let mut window = RateWindow::default();
    policy(&home, &alice_fp, &roots, true);
    // The first pass only records the backlog.
    assert!(tick_in(&home, &mut window, &runners, Some(&sessions))
        .unwrap()
        .is_empty());

    let received = listen(&runtime, &pipe, Duration::from_secs(10));
    let first = message(
        &alice,
        &bob.fingerprint().unwrap(),
        "00000000-0000-4000-8000-0000000000e1",
        checkout.clone(),
    );
    Store::open(&home).unwrap().receive(&first).unwrap();
    let records = tick_in(&home, &mut window, &runners, Some(&sessions)).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].step, "wake");
    assert_eq!(records[0].detail, "claude-code via checkout: written");
    assert!(!records[0].detail.contains("delivered"));

    let lines = runtime.block_on(received).unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    let auth: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(auth, json!({ "type": "auth", "token": TOKEN }));
    let user: Value = serde_json::from_str(&lines[1]).unwrap();
    let notice = user["message"]["content"].as_str().unwrap();
    assert!(
        notice.starts_with("MAIL thread=") && notice.contains("host=alpha"),
        "{notice}"
    );
    assert!(!notice.contains("BODY-THAT-MUST-NOT"), "never the body");

    let ledger = std::fs::read_to_string(home.join("ledger.jsonl")).unwrap();
    assert!(!ledger.contains(TOKEN) && !format!("{records:?}").contains(TOKEN));

    // Live delivery off: nothing reaches the pipe, and the message isn't routed to the session.
    policy(&home, &alice_fp, &roots, false);
    let untouched = listen(&runtime, &format!("{pipe}-off"), Duration::from_secs(2));
    std::fs::write(
        sessions.join(format!("{pid}.json")),
        json!({ "pid": pid, "sessionId": "bob-session", "cwd": app, "kind": "interactive",
                "procStart": process_start(pid).unwrap().to_string(), "messagingSocketPath": format!("{pipe}-off") })
        .to_string(),
    )
    .unwrap();
    let second = message(
        &alice,
        &bob.fingerprint().unwrap(),
        "00000000-0000-4000-8000-0000000000e2",
        checkout,
    );
    Store::open(&home).unwrap().receive(&second).unwrap();
    let records = tick_in(&home, &mut window, &runners, Some(&sessions)).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        (records[0].step.as_str(), records[0].detail.as_str()),
        ("skip", "no_live_owner")
    );
    assert!(
        runtime.block_on(untouched).unwrap().is_empty(),
        "nothing written with live off"
    );
}
