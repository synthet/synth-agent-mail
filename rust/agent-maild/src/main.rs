use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use agent_maild::discovery::{Discovery, DISCOVERY_GROUP, DISCOVERY_PORT};
use clap::Parser;

/// agent-mail mailer: receives sealed peer messages into mail.db and runs peer discovery.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// agent-mail home with identity.json; mail.db is created there. Required, never defaulted.
    #[arg(long)]
    home: PathBuf,
    /// TCP port for peer messages.
    #[arg(long, default_value_t = agent_maild::DEFAULT_PORT)]
    port: u16,
    /// Address to bind the receiver to.
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,
    /// Turn off the multicast responder and LAN queries.
    #[arg(long)]
    no_discovery: bool,
    /// Don't probe Tailscale peers.
    #[arg(long)]
    no_tailscale: bool,
    /// UDP port for multicast discovery.
    #[arg(long, default_value_t = DISCOVERY_PORT)]
    discovery_port: u16,
    /// Multicast group for discovery.
    #[arg(long, default_value_t = DISCOVERY_GROUP)]
    discovery_group: Ipv4Addr,
    /// Use one IPv4 interface for multicast instead of every external one.
    #[arg(long)]
    interface: Option<Ipv4Addr>,
    /// Seconds between host scans (multicast, hosts.json, Tailscale); 0 turns scanning off.
    #[arg(long, default_value_t = 60)]
    scan_interval: u64,
    /// Port probed for hosts.json entries without one, and for Tailscale peers.
    #[arg(long, default_value_t = agent_maild::DEFAULT_PORT)]
    peer_port: u16,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let discovery = (!args.no_discovery).then_some(Discovery {
        port: args.discovery_port,
        group: args.discovery_group,
        interface: args.interface,
    });
    let mailer = agent_maild::start(agent_maild::Config {
        home: args.home,
        listen: SocketAddr::new(args.bind, args.port),
        discovery,
        tailscale: !args.no_tailscale,
        scan_interval: (args.scan_interval > 0).then(|| Duration::from_secs(args.scan_interval)),
        peer_port: args.peer_port,
    })
    .await?;
    eprintln!(
        "agent-maild: {} receiving on port {}",
        mailer.fingerprint, mailer.port
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}
