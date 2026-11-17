use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    let mailer = agent_maild::start(agent_maild::Config {
        home: args.home,
        listen: SocketAddr::new(args.bind, args.port),
    })
    .await?;
    eprintln!(
        "agent-maild: {} receiving on port {}",
        mailer.fingerprint, mailer.port
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}
