//! TS → Rust delivery through the real TypeScript CLI (`dist/cli.js`). `npm run test:rust` builds
//! `dist/` first; running `cargo test` alone needs `npm run build` beforehand.
//!
//! Every home is a temp folder. The mailer binds 127.0.0.1 on a random port, so this never touches
//! the live `~/.agent-mail` or port 47832.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agent_maild::{start, Config};
use mail_core::store::Store;
use mail_core::{Identity, PublicIdentity};
use serde_json::Value;

fn cli() -> PathBuf {
    let cli = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dist/cli.js");
    assert!(
        cli.exists(),
        "{} is missing: run `npm run build` (or `npm run test:rust`) first",
        cli.display()
    );
    cli
}

fn ts(home: &Path, args: &[&str]) -> Output {
    let output = Command::new("node")
        .arg(cli())
        .arg("--home")
        .arg(home)
        .args(args)
        .env_remove("AGENT_MAIL_HOME")
        .output()
        .expect("node is on PATH");
    if !output.status.success() {
        eprintln!(
            "node {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

fn public(value: &Value) -> PublicIdentity {
    PublicIdentity {
        name: value["name"].as_str().unwrap().into(),
        signing_key: value["signing_key"].as_str().unwrap().into(),
        encryption_key: value["encryption_key"].as_str().unwrap().into(),
    }
}

#[tokio::test]
async fn typescript_peer_delivers_to_the_rust_mailer() {
    let dir = tempfile::tempdir().unwrap();
    let (alice_home, bob_home) = (dir.path().join("alice"), dir.path().join("bob"));
    assert!(ts(&alice_home, &["init", "--name", "alice"])
        .status
        .success());
    // TS writes bob's identity.json, so this also proves the Rust side loads a TS identity file.
    assert!(ts(&bob_home, &["init", "--name", "bob"]).status.success());
    let bob = Identity::load(&bob_home).expect("Rust loads the TS identity.json");

    let alice_json: Value = serde_json::from_slice(&ts(&alice_home, &["identity"]).stdout).unwrap();
    let alice = public(&alice_json);
    Store::open(&bob_home)
        .unwrap()
        .trust(&alice, alice_json["fingerprint"].as_str(), None)
        .unwrap();

    let mailer = start(Config {
        home: bob_home.clone(),
        listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
    })
    .await
    .unwrap();
    let bob_public = dir.path().join("bob-public.json");
    std::fs::write(&bob_public, serde_json::to_string(&bob.public()).unwrap()).unwrap();
    let bob_fp = bob.fingerprint().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", mailer.port);
    let trust = ts(
        &alice_home,
        &[
            "trust",
            "--file",
            bob_public.to_str().unwrap(),
            "--fingerprint",
            &bob_fp,
            "--endpoint",
            &endpoint,
        ],
    );
    assert!(trust.status.success());

    let text = "hello from TS: Grüße \"quoted\" 😀";
    // `send` runs in a blocking child process while the mailer answers on this runtime.
    let home = alice_home.clone();
    let fp = bob_fp.clone();
    let send = tokio::task::spawn_blocking(move || {
        ts(
            &home,
            &[
                "send",
                &fp,
                "--text",
                text,
                "--session",
                "s-1",
                "--no-tailscale",
                "--timeout",
                "50",
            ],
        )
    })
    .await
    .unwrap();
    let result: Value = serde_json::from_slice(&send.stdout).unwrap_or(Value::Null);
    // Exit 0 and "delivered" mean the TS sender verified the Rust receipt.
    assert!(
        send.status.success(),
        "send exit {:?}: {result}",
        send.status.code()
    );
    assert_eq!(result["status"], "delivered");

    let inbox = Store::open(&bob_home).unwrap().inbox(10).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].text, text);
    assert_eq!(inbox[0].sender, alice_json["fingerprint"].as_str().unwrap());
    assert_eq!(inbox[0].id, result["message_id"].as_str().unwrap());
    assert!(inbox[0].verify(&alice));
}
