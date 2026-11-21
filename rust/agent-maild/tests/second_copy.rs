//! A second mailer started for a home and port that a mailer already serves must fail without
//! taking the first one down (af-4ha87).

use std::net::{Ipv4Addr, SocketAddr};

use agent_maild::{start, Config};
use mail_core::Identity;

#[tokio::test]
async fn second_copy_on_the_same_port_fails_and_the_first_keeps_serving() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let identity = Identity::generate("solo").unwrap();
    std::fs::write(
        home.join("identity.json"),
        serde_json::to_string_pretty(&identity).unwrap(),
    )
    .unwrap();
    let config = |port: u16| Config {
        deliver: true,
        ..Config::receive_only(home.clone(), SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
    };

    let first = start(config(0)).await.expect("first mailer starts");
    let port = first.port;
    assert!(
        status_answers(&home).await,
        "first mailer owns the local channel"
    );

    let second = start(config(port)).await;
    assert!(
        second.is_err(),
        "a second mailer on port {port} must not start"
    );

    let url = format!("http://127.0.0.1:{port}/capabilities");
    let response = reqwest::get(&url)
        .await
        .expect("first mailer still answers");
    assert_eq!(response.status(), 200);
    assert!(
        status_answers(&home).await,
        "first mailer still owns the local channel"
    );
}

/// The client side is blocking, so it runs off the runtime thread the mailer answers on.
async fn status_answers(home: &std::path::Path) -> bool {
    let home = home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        mail_core::ipc::request(&home, "status", std::time::Duration::from_secs(2))
            .is_ok_and(|reply| reply.starts_with('{'))
    })
    .await
    .unwrap()
}
