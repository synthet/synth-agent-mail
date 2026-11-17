//! `agent-maild`, the always-on mailer (ADR 0005). It is the only process that uses the network:
//! it receives sealed messages into `mail.db` and answers and runs peer discovery. Draining the
//! outbox arrives with T-3.

pub mod advert;
pub mod http;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mail_core::store::Store;
use mail_core::Identity;
use tokio::net::TcpListener;

pub const DEFAULT_PORT: u16 = 47832;

#[derive(Clone, Debug)]
pub struct Config {
    /// The agent-mail home holding `identity.json` and `mail.db`. Always explicit: until the
    /// cutover ticket (T-10), the mailer must not take over the home the TS tools use.
    pub home: PathBuf,
    pub listen: SocketAddr,
}

/// A running mailer. Dropping it stops the listener.
pub struct Mailer {
    pub port: u16,
    pub fingerprint: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Mailer {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Binds the receiver and starts serving.
pub async fn start(config: Config) -> Result<Mailer, Box<dyn std::error::Error + Send + Sync>> {
    let identity = Identity::load(&config.home)?;
    let fingerprint = identity.fingerprint()?;
    let store = Store::open(&config.home)?;
    let listener = TcpListener::bind(config.listen).await?;
    let port = listener.local_addr()?.port();
    let receiver = Arc::new(http::Receiver {
        identity,
        store: Mutex::new(store),
        port,
    });
    let tasks = vec![tokio::spawn(http::serve(listener, receiver))];
    Ok(Mailer {
        port,
        fingerprint,
        tasks,
    })
}
