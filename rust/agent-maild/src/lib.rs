//! `agent-maild`, the always-on mailer (ADR 0005). It is the only process that uses the network:
//! it receives sealed messages into `mail.db`, answers multicast discovery, and records hosts it
//! finds by multicast, `hosts.json` and Tailscale. It also drains the outbox that the CLI, MCP and
//! tray fill through `mail_core::send::submit`.

pub mod advert;
pub mod discovery;
pub mod http;
pub mod ipc;
pub mod outbox;
pub mod probe;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mail_core::store::{HostSource, Store};
use mail_core::Identity;
use tokio::net::TcpListener;

use crate::advert::Discovered;
use crate::discovery::Discovery;

pub const DEFAULT_PORT: u16 = 47832;
const LAN_QUERY_TIMEOUT: Duration = Duration::from_millis(1200);

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Config {
    /// The agent-mail home holding `identity.json` and `mail.db`. Always explicit: until the
    /// cutover ticket (T-10), the mailer must not take over the home the TS tools use.
    pub home: PathBuf,
    pub listen: SocketAddr,
    /// Multicast responder and LAN queries; `None` turns both off.
    pub discovery: Option<Discovery>,
    /// Probe online Tailscale peers during each scan.
    pub tailscale: bool,
    /// How often to scan for hosts; `None` never scans.
    pub scan_interval: Option<Duration>,
    /// Port to probe for `hosts.json` entries without one, and for Tailscale peers.
    pub peer_port: u16,
    /// Drain the outbox and serve the local channel (`poke`, `status`).
    pub deliver: bool,
}

impl Config {
    /// Only the receiver: no discovery, no scans. For tests and embedding.
    pub fn receive_only(home: PathBuf, listen: SocketAddr) -> Self {
        Config {
            home,
            listen,
            discovery: None,
            tailscale: false,
            scan_interval: None,
            peer_port: DEFAULT_PORT,
            deliver: false,
        }
    }
}

/// A running mailer. Dropping it stops every task.
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

/// Binds the receiver, then starts discovery and the host scan as configured.
pub async fn start(config: Config) -> Result<Mailer, BoxError> {
    let identity = Identity::load(&config.home)?;
    let fingerprint = identity.fingerprint()?;
    let store = Store::open(&config.home)?;
    let listener = TcpListener::bind(config.listen).await?;
    let port = listener.local_addr()?.port();
    let receiver = Arc::new(http::Receiver {
        identity: identity.clone(),
        store: Mutex::new(store),
        port,
    });
    let mut tasks = Vec::new();
    if config.deliver {
        // Owning the local channel is what keeps a second mailer off this home, so claim it
        // before serving anything.
        let notify = Arc::new(tokio::sync::Notify::new());
        let outbox = Arc::new(outbox::Outbox::new(config.home.clone(), identity.clone()));
        let status = Arc::new(ipc::Status {
            home: config.home.clone(),
            fingerprint: fingerprint.clone(),
            port,
            outbox: outbox.clone(),
        });
        tasks.push(ipc::serve(notify.clone(), status)?);
        tasks.push(tokio::spawn(outbox.run(notify)));
    }
    tasks.push(tokio::spawn(http::serve(listener, receiver)));
    if let Some(discovery) = config.discovery {
        tasks.push(discovery::respond(Arc::new(identity), port, discovery)?);
    }
    if let Some(interval) = config.scan_interval {
        let config = config.clone();
        let own = fingerprint.clone();
        tasks.push(tokio::spawn(async move {
            let mut ticks = tokio::time::interval(interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                if let Err(error) = scan_once(&config, &own).await {
                    eprintln!("agent-maild: host scan failed: {error}");
                }
            }
        }));
    }
    Ok(Mailer {
        port,
        fingerprint,
        tasks,
    })
}

/// What one scan found, by source, after dropping this host.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub multicast: usize,
    pub static_hosts: usize,
    pub tailscale: usize,
}

/// One round: LAN multicast, `hosts.json`, then Tailscale. Every verified advert goes to the
/// `hosts` table; none of them is pinned.
pub async fn scan_once(config: &Config, own_fingerprint: &str) -> Result<ScanReport, BoxError> {
    let mut found: Vec<(Discovered, HostSource)> = Vec::new();
    if let Some(discovery) = config.discovery {
        match discovery::query_lan(discovery, LAN_QUERY_TIMEOUT).await {
            Ok(peers) => found.extend(peers.into_iter().map(|peer| (peer, HostSource::Multicast))),
            Err(error) => eprintln!("agent-maild: LAN discovery unavailable: {error}"),
        }
    }
    let client = probe::client();
    match probe::read_hosts(&config.home) {
        Ok(entries) => {
            let targets = probe::resolve_hosts(&entries, config.peer_port).await;
            found.extend(
                probe::probe_all(&client, targets)
                    .await
                    .into_iter()
                    .map(|peer| (peer, HostSource::Static)),
            );
        }
        Err(error) => eprintln!("agent-maild: {error}"),
    }
    if config.tailscale {
        if let Some(status) = probe::tailscale_status().await {
            let targets = probe::tailscale_addresses(&status)
                .into_iter()
                .map(|ip| (ip, config.peer_port))
                .collect();
            found.extend(
                probe::probe_all(&client, targets)
                    .await
                    .into_iter()
                    .map(|peer| (peer, HostSource::Tailscale)),
            );
        }
    }
    found.retain(|(peer, _)| peer.fingerprint != own_fingerprint);
    let mut report = ScanReport::default();
    for (_, source) in &found {
        match source {
            HostSource::Multicast => report.multicast += 1,
            HostSource::Static => report.static_hosts += 1,
            HostSource::Tailscale => report.tailscale += 1,
        }
    }
    let home = config.home.clone();
    tokio::task::spawn_blocking(move || record(&home, &found)).await??;
    Ok(report)
}

fn record(home: &Path, found: &[(Discovered, HostSource)]) -> Result<(), BoxError> {
    let store = Store::open(home)?;
    for (peer, source) in found {
        store.saw_host(&peer.identity, &peer.endpoint, *source)?;
    }
    Ok(())
}
