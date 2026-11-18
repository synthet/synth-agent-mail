//! `agent-mail mcp` driven by the official TypeScript MCP SDK client (`@modelcontextprotocol/sdk`,
//! already a dependency of this repo) over stdio, the way `mcp.json` launches it. Needs
//! `npm ci` and `npm run build` (for the TS peer).

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_maild::{start, Config, Mailer};
use serde_json::{json, Value};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Runs `calls` ([{name, arguments}]) in one MCP session and returns
/// {tools: [names], results: [{isError, text}]}.
const DRIVER: &str = r#"
const { Client } = await import("@modelcontextprotocol/sdk/client/index.js");
const { StdioClientTransport } = await import("@modelcontextprotocol/sdk/client/stdio.js");
const transport = new StdioClientTransport({ command: process.env.BIN, args: JSON.parse(process.env.ARGS), stderr: "pipe" });
const client = new Client({ name: "rust-mcp-test", version: "1.0.0" });
await client.connect(transport);
const tools = (await client.listTools()).tools;
const results = [];
for (const call of JSON.parse(process.env.CALLS)) {
  try {
    const result = await client.callTool(call);
    results.push({ isError: !!result.isError, text: result.content.map((c) => c.text).join("") });
  } catch (error) {
    results.push({ isError: true, protocol: true, text: String(error.message ?? error) });
  }
}
await client.close();
console.log(JSON.stringify({ server: client.getServerVersion(), tools: tools.map((t) => ({ name: t.name, input: t.inputSchema })), results }));
"#;

fn mcp(home: &Path, calls: Value) -> Value {
    let args = json!([
        "--home",
        home,
        "mcp",
        "--harness",
        "claude-code",
        "--agent-name",
        "claude",
        "--no-listen"
    ]);
    let output = Command::new("node")
        .args(["--input-type=module", "-e", DRIVER])
        .current_dir(repo())
        .env("BIN", env!("CARGO_BIN_EXE_agent-mail"))
        .env("ARGS", args.to_string())
        .env("CALLS", calls.to_string())
        .env_remove("AGENT_MAIL_SESSION")
        .output()
        .expect("node is on PATH");
    assert!(
        output.status.success(),
        "driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("driver JSON")
}

fn cli(home: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-mail"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "agent-mail {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
}

fn text(result: &Value) -> Value {
    serde_json::from_str(result["text"].as_str().unwrap())
        .unwrap_or_else(|_| result["text"].clone())
}

async fn mailer(home: &Path) -> Mailer {
    let mut config = Config::receive_only(
        home.to_path_buf(),
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
    );
    config.deliver = true;
    start(config).await.unwrap()
}

fn public_file(dir: &Path, identity: &Value) -> PathBuf {
    let path = dir.join(format!(
        "{}-public.json",
        identity["name"].as_str().unwrap()
    ));
    std::fs::write(&path, identity.to_string()).unwrap();
    path
}

fn allow(home: &Path, peers: &[&str]) {
    cli(home, &["policy", "init"]);
    let mut policy: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("policy.json")).unwrap()).unwrap();
    policy["allowed_peers"] = json!(peers);
    std::fs::write(home.join("policy.json"), policy.to_string()).unwrap();
}

const PEER_TOOLS: [&str; 10] = [
    "whoami",
    "discover_peers",
    "list_agents",
    "send_message",
    "reply_to_message",
    "fetch_inbox",
    "acknowledge_message",
    "get_thread",
    "queue_status",
    "retry_outbox",
];

#[tokio::test(flavor = "multi_thread")]
async fn official_sdk_client_lists_and_calls_the_tools() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("solo");
    let identity = cli(&home, &["init", "--name", "solo"]);
    let out = tokio::task::spawn_blocking(move || {
        mcp(
            &home,
            json!([
                { "name": "whoami", "arguments": {} },
                { "name": "queue_status", "arguments": {} },
                { "name": "mailbox_claim_task", "arguments": { "session_id": "s", "task_id": "t" } },
                { "name": "fetch_inbox", "arguments": { "limit": 0 } },
                { "name": "send_message", "arguments": { "recipient": "nobody", "text": "hi", "bogus": 1 } },
            ]),
        )
    })
    .await
    .unwrap();

    let names: Vec<&str> = out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for tool in PEER_TOOLS.iter().chain(&[
        "mailbox_subscribe",
        "mailbox_offer_task",
        "mailbox_claim_task",
        "mailbox_sync_status",
    ]) {
        assert!(names.contains(tool), "{tool} missing from {names:?}");
    }
    let send = out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "send_message")
        .unwrap();
    let properties = send["input"]["properties"].as_object().unwrap();
    for field in [
        "recipient",
        "text",
        "message_id",
        "conversation_id",
        "to_session",
        "hop",
        "auto_reply",
        "in_reply_to",
        "project",
    ] {
        assert!(properties.contains_key(field), "send_message lacks {field}");
    }
    assert_eq!(out["server"]["name"], "agent-mail");

    let results = out["results"].as_array().unwrap();
    let whoami = text(&results[0]);
    assert_eq!(whoami["fingerprint"], identity["fingerprint"]);
    assert_eq!(whoami["agent"]["harness"], "claude-code");
    assert_eq!(text(&results[1])["pending"], 0);
    assert!(results[2]["isError"].as_bool().unwrap());
    assert!(results[2]["text"]
        .as_str()
        .unwrap()
        .starts_with("not_ported"));
    assert!(
        results[3]["isError"].as_bool().unwrap(),
        "limit 0 is refused"
    );
    assert!(
        results[4]["isError"].as_bool().unwrap(),
        "unknown arguments are refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn send_and_automatic_reply_over_mcp_keep_the_project_and_the_guards() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (dir.path().join("alice"), dir.path().join("bob"));
    let alice_id = cli(&alice, &["init", "--name", "alice"]);
    let bob_id = cli(&bob, &["init", "--name", "bob"]);
    let (alice_mailer, bob_mailer) = (mailer(&alice).await, mailer(&bob).await);
    let (alice_fp, bob_fp) = (
        alice_id["fingerprint"].as_str().unwrap(),
        bob_id["fingerprint"].as_str().unwrap(),
    );
    let to_bob = format!("http://127.0.0.1:{}", bob_mailer.port);
    let to_alice = format!("http://127.0.0.1:{}", alice_mailer.port);
    cli(
        &alice,
        &[
            "trust",
            "--file",
            public_file(dir.path(), &bob_id).to_str().unwrap(),
            "--fingerprint",
            bob_fp,
            "--endpoint",
            &to_bob,
        ],
    );
    cli(
        &bob,
        &[
            "trust",
            "--file",
            public_file(dir.path(), &alice_id).to_str().unwrap(),
            "--fingerprint",
            alice_fp,
            "--endpoint",
            &to_alice,
        ],
    );
    allow(&alice, &["bob"]);
    allow(&bob, &["alice"]);

    let home = alice.clone();
    let sent = tokio::task::spawn_blocking(move || {
        mcp(
            &home,
            json!([
                { "name": "send_message", "arguments": { "recipient": "bob", "text": "what is the build status?", "project": "synthet-agent-mailbox", "session_id": "alice-s1" } },
                { "name": "send_message", "arguments": { "recipient": "bob", "text": "token = abcdefghijkl" } },
            ]),
        )
    })
    .await
    .unwrap();
    let results = sent["results"].as_array().unwrap();
    let delivered = text(&results[0]);
    assert_eq!(delivered["status"], "delivered", "{delivered}");
    assert!(
        delivered.get("envelope_downgraded").is_none(),
        "Rust to Rust keeps 1.2"
    );
    assert!(results[1]["isError"].as_bool().unwrap());
    assert!(
        results[1]["text"]
            .as_str()
            .unwrap()
            .contains("send refused"),
        "policy is checked on MCP sends too"
    );

    let question = &cli(&bob, &["inbox"])[0];
    assert_eq!(question["project"], "synthet-agent-mailbox");
    let (sender, id) = (
        question["sender"].as_str().unwrap().to_owned(),
        question["id"].as_str().unwrap().to_owned(),
    );
    let home = bob.clone();
    let replied = tokio::task::spawn_blocking(move || {
        let reply = |text: &str| json!({ "name": "reply_to_message", "arguments": { "sender": sender, "message_id": id, "text": text, "automatic": true } });
        mcp(&home, json!([reply("green on main"), reply("a second automatic reply")]))
    })
    .await
    .unwrap();
    let results = replied["results"].as_array().unwrap();
    assert_eq!(text(&results[0])["status"], "delivered", "{}", results[0]);
    assert!(results[1]["isError"].as_bool().unwrap());
    assert!(results[1]["text"]
        .as_str()
        .unwrap()
        .contains("already has an automatic reply"));

    let answer = &cli(&alice, &["inbox"])[0];
    assert_eq!(answer["text"], "green on main");
    assert_eq!(answer["auto_reply"], true);
    assert_eq!(answer["hop"], 1);
    assert_eq!(answer["in_reply_to"], question["id"]);
    assert_eq!(answer["conversation_id"], question["conversation_id"]);
    assert_eq!(
        answer["to_session"], "alice-s1",
        "the reply goes back to the asking session"
    );
    assert_eq!(
        answer["project"], "synthet-agent-mailbox",
        "a reply stays in the question's project"
    );

    // An automatic reply to an automatic message is refused before anything is queued.
    let (sender, id) = (
        answer["sender"].as_str().unwrap().to_owned(),
        answer["id"].as_str().unwrap().to_owned(),
    );
    let home = alice.clone();
    let loop_guard = tokio::task::spawn_blocking(move || {
        mcp(&home, json!([{ "name": "reply_to_message", "arguments": { "sender": sender, "message_id": id, "text": "thanks", "automatic": true } }]))
    })
    .await
    .unwrap();
    assert!(loop_guard["results"][0]["text"]
        .as_str()
        .unwrap()
        .contains("itself an automatic reply"));
}
