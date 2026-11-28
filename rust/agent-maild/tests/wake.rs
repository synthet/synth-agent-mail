//! The mailer wakes a waiting consumer as soon as it stores new mail (af-2ft1h).

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_maild::{start, Config};
use mail_core::envelope::{Envelope, Message, Origin};
use mail_core::store::{now_ms, Store};
use mail_core::wire::seal;
use mail_core::Identity;

fn message(from: &Identity, to: &Identity, id: &str) -> Message {
    Message {
        id: id.into(),
        conversation_id: "00000000-0000-4000-8000-0000000000bb".into(),
        sender: from.fingerprint().unwrap(),
        recipient: to.fingerprint().unwrap(),
        text: "wake up".into(),
        created_at: now_ms(),
        origin: Origin {
            hostname: "pc1".into(),
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
        signature: String::new(),
    }
    .sign(from, Envelope::V1_2)
    .unwrap()
}

async fn post(port: u16, body: String) -> u16 {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/message"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// The client side is blocking, so it runs off the runtime thread the mailer answers on.
async fn wait(home: &Path, seen: Option<u64>, wait: Duration) -> (Option<u64>, Duration) {
    let home: PathBuf = home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        (
            mail_core::ipc::wait_received(&home, seen, wait),
            started.elapsed(),
        )
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn new_mail_wakes_a_waiter_and_a_duplicate_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("bob");
    std::fs::create_dir_all(&home).unwrap();
    let bob = Identity::generate("bob").unwrap();
    std::fs::write(
        home.join("identity.json"),
        serde_json::to_string_pretty(&bob).unwrap(),
    )
    .unwrap();
    let alice = Identity::generate("alice").unwrap();
    Store::open(&home)
        .unwrap()
        .trust(&alice.public(), None, None)
        .unwrap();
    let mailer = start(Config {
        deliver: true,
        ..Config::receive_only(home.clone(), SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
    })
    .await
    .unwrap();

    let (seen, _) = wait(&home, None, Duration::ZERO).await;
    assert_eq!(seen, Some(0), "a fresh mailer has stored nothing yet");

    let sent = message(&alice, &bob, "00000000-0000-4000-8000-000000000101");
    let packet =
        serde_json::to_string(&seal(&alice, &bob.public(), &sent, now_ms()).unwrap()).unwrap();
    let waiter = tokio::spawn({
        let home = home.clone();
        async move { wait(&home, Some(0), Duration::from_secs(10)).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let posted = Instant::now();
    assert_eq!(post(mailer.port, packet.clone()).await, 200);
    let (seen, _) = waiter.await.unwrap();
    assert_eq!(seen, Some(1), "the waiter sees the new message");
    assert!(
        posted.elapsed() < Duration::from_secs(2),
        "woken well before the 3 s poll: {:?}",
        posted.elapsed()
    );

    assert_eq!(
        post(mailer.port, packet).await,
        200,
        "a duplicate still gets a receipt"
    );
    let (seen, took) = wait(&home, Some(1), Duration::from_millis(800)).await;
    assert_eq!(seen, Some(1), "a duplicate is not new mail");
    assert!(
        took >= Duration::from_millis(700),
        "the waiter slept out its wait: {took:?}"
    );

    let (seen, took) = wait(&home, Some(0), Duration::from_secs(5)).await;
    assert_eq!(seen, Some(1), "a stale count answers at once");
    assert!(took < Duration::from_secs(2));
}

#[test]
fn without_a_mailer_the_wait_fails_fast() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    assert_eq!(
        mail_core::ipc::wait_received(dir.path(), Some(0), Duration::from_secs(3)),
        None,
        "callers fall back to polling"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}
