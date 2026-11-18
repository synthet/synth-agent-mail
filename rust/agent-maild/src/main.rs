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
    /// Write the log to this file instead of the console. The logon task runs the mailer under
    /// `conhost --headless`, which hides the window. A file over 1 MB is moved to <file>.old first.
    #[arg(long)]
    log: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run(Args::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agent-maild: {error}");
            std::process::ExitCode::from(1)
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(path) = &args.log {
        log_to_file(path)?;
    }
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
        deliver: true,
    })
    .await?;
    eprintln!(
        "agent-maild: {} receiving on port {}",
        mailer.fingerprint, mailer.port
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// Sends stderr (every `eprintln!`) to `path`. It keeps the console: detaching would let the
/// logon task's `conhost --headless` exit, so Task Scheduler would report the task as stopped and
/// its restart-on-failure could never restart a crashed mailer.
#[cfg(windows)]
fn log_to_file(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE};
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > 1_000_000) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // Rust's stderr looks the handle up on every write, so swapping it redirects eprintln!.
    unsafe {
        SetStdHandle(STD_ERROR_HANDLE, file.into_raw_handle() as _);
    }
    Ok(())
}

#[cfg(not(windows))]
fn log_to_file(_path: &std::path::Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "--log is only supported on Windows; redirect stderr instead",
    ))
}
