//! Discovery against the TypeScript implementation (`dist/peer/discovery.js`), in both directions.
//!
//! A test-only multicast group and a free UDP port keep the live tray on 239.255.77.31:47831 out of
//! it. Homes are temp folders and listeners bind 127.0.0.1. Needs `npm run build` first.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use agent_maild::discovery::Discovery;
use agent_maild::{scan_once, start, Config};
use mail_core::store::Store;
use mail_core::Identity;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 33);

fn dist_url() -> String {
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dist");
    assert!(
        dist.join("cli.js").exists(),
        "dist/ is missing: run `npm run build` (or `npm run test:rust`) first"
    );
    format!("file:///{}", dist.to_string_lossy().replace('\\', "/"))
}

fn free_udp_port() -> u16 {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn discovery(port: u16) -> Discovery {
    Discovery {
        port,
        group: GROUP,
        interface: None,
    }
}

fn new_home(dir: &Path, name: &str) -> PathBuf {
    let home = dir.join(name);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("identity.json"),
        serde_json::to_string(&Identity::generate(name).unwrap()).unwrap(),
    )
    .unwrap();
    home
}

/// Runs a TS module snippet that prints one JSON line, then keeps running until stdin closes.
async fn node(script: &str, env: &[(&str, String)]) -> (Child, Value) {
    let mut command = Command::new("node");
    command
        .args(["--input-type=module", "-e", script])
        .env("DIST", dist_url())
        .env_remove("AGENT_MAIL_HOME");
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("node is on PATH");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = lines
        .next_line()
        .await
        .unwrap()
        .expect("node printed a line");
    (child, serde_json::from_str(&line).unwrap())
}

const TS_DISCOVER: &str = r#"
const { discover } = await import(process.env.DIST + "/peer/discovery.js");
const peers = await discover({ port: Number(process.env.DPORT), group: process.env.DGROUP, timeoutMs: 1500, tailscale: false });
console.log(JSON.stringify(peers.map((p) => ({ fingerprint: p.fingerprint, endpoint: p.endpoint }))));
"#;

const TS_ADVERTISE: &str = r#"
const { initIdentity, fingerprint } = await import(process.env.DIST + "/peer/identity.js");
const { advertise } = await import(process.env.DIST + "/peer/discovery.js");
const identity = initIdentity(process.env.TS_HOME, "ts-advertiser");
const stop = await advertise(identity, 47999, { port: Number(process.env.DPORT), group: process.env.DGROUP });
console.log(JSON.stringify({ fingerprint: fingerprint(identity) }));
process.stdin.on("end", async () => { await stop(); process.exit(0); });
process.stdin.resume();
"#;

const TS_LISTEN: &str = r#"
const { initIdentity, fingerprint } = await import(process.env.DIST + "/peer/identity.js");
const { Peer } = await import(process.env.DIST + "/peer/peer.js");
const identity = initIdentity(process.env.TS_HOME, "ts-static");
const peer = new Peer(identity, process.env.TS_HOME, { tailscale: false });
const listener = await peer.listen({ port: 0, bind: "127.0.0.1", discovery: false });
console.log(JSON.stringify({ fingerprint: fingerprint(identity), port: listener.port }));
process.stdin.on("end", async () => { await listener.close(); process.exit(0); });
process.stdin.resume();
"#;

#[tokio::test]
async fn typescript_discover_finds_the_rust_responder() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_udp_port();
    let home = new_home(dir.path(), "rust-bob");
    let mut config = Config::receive_only(home, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    config.discovery = Some(discovery(port));
    let mailer = start(config).await.unwrap();

    let (_child, peers) = node(
        TS_DISCOVER,
        &[("DPORT", port.to_string()), ("DGROUP", GROUP.to_string())],
    )
    .await;
    let found = peers
        .as_array()
        .unwrap()
        .iter()
        .find(|peer| peer["fingerprint"] == mailer.fingerprint.as_str());
    let found =
        found.unwrap_or_else(|| panic!("TS discover did not find the Rust mailer: {peers}"));
    assert!(
        found["endpoint"]
            .as_str()
            .unwrap()
            .ends_with(&format!(":{}", mailer.port)),
        "endpoint uses the advertised port: {found}"
    );
}

#[tokio::test]
async fn rust_scan_records_a_typescript_advertiser_without_trusting_it() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_udp_port();
    let ts_home = dir.path().join("ts");
    let (child, advertised) = node(
        TS_ADVERTISE,
        &[
            ("DPORT", port.to_string()),
            ("DGROUP", GROUP.to_string()),
            ("TS_HOME", ts_home.to_string_lossy().into_owned()),
        ],
    )
    .await;
    let ts_fp = advertised["fingerprint"].as_str().unwrap().to_owned();

    let home = new_home(dir.path(), "rust-bob");
    let mut config = Config::receive_only(home.clone(), SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    config.discovery = Some(discovery(port));
    let mailer = start(config.clone()).await.unwrap();
    let report = scan_once(&config, &mailer.fingerprint).await.unwrap();
    drop(child);

    assert!(report.multicast >= 1, "{report:?}");
    let store = Store::open(&home).unwrap();
    let hosts = store.hosts(0).unwrap();
    let host = hosts
        .iter()
        .find(|host| host.fingerprint == ts_fp)
        .unwrap_or_else(|| panic!("no TS host in {hosts:?}"));
    assert_eq!(host.source, "multicast");
    assert!(
        host.endpoint.ends_with(":47999"),
        "endpoint uses the advertised port: {host:?}"
    );
    assert!(!host.trusted);
    assert!(
        hosts
            .iter()
            .all(|host| host.fingerprint != mailer.fingerprint),
        "own advert is dropped"
    );
    assert!(store.contacts().unwrap().is_empty(), "discovery never pins");
}

#[tokio::test]
async fn hosts_json_entry_is_probed_and_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let ts_home = dir.path().join("ts");
    let (child, listening) = node(
        TS_LISTEN,
        &[("TS_HOME", ts_home.to_string_lossy().into_owned())],
    )
    .await;
    let ts_port = listening["port"].as_u64().unwrap();

    let home = new_home(dir.path(), "rust-bob");
    std::fs::write(
        home.join("hosts.json"),
        format!(r#"{{"hosts":["127.0.0.1:{ts_port}","no-such-host.invalid"]}}"#),
    )
    .unwrap();
    let config = Config::receive_only(home.clone(), SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let own = Identity::load(&home).unwrap().fingerprint().unwrap();
    let report = scan_once(&config, &own).await.unwrap();
    drop(child);

    assert_eq!(report.static_hosts, 1, "{report:?}");
    let store = Store::open(&home).unwrap();
    let hosts = store.hosts(0).unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(
        hosts[0].fingerprint,
        listening["fingerprint"].as_str().unwrap()
    );
    assert_eq!(hosts[0].endpoint, format!("http://127.0.0.1:{ts_port}"));
    assert_eq!(hosts[0].source, "static");
    assert!(
        !hosts[0].trusted,
        "a listed host stays untrusted until pinned"
    );
    assert!(store.contacts().unwrap().is_empty());
}
