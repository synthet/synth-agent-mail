//! LAN multicast discovery (`src/peer/discovery.ts` `advertise`, `discover`).
//!
//! The responder binds the shared discovery port (SO_REUSEADDR, as TS does), joins the group on
//! every external IPv4 interface, rejoins every 30 s because sleep and address changes drop
//! memberships, and answers at most 20 queries a second. Queries go out from a separate ephemeral
//! socket: replies are unicast and would otherwise land in whichever process owns the shared port.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mail_core::Identity;
use socket2::{Domain, Protocol, SockRef, Socket, Type};
use tokio::net::UdpSocket;

use crate::advert::{advertisement, parse_advert, parse_query, query, Discovered};

pub const DISCOVERY_PORT: u16 = 47831;
pub const DISCOVERY_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 31);
const REJOIN: Duration = Duration::from_secs(30);
const MAX_QUERY_BYTES: usize = 256;
const REPLIES_PER_SECOND: u32 = 20;

#[derive(Clone, Copy, Debug)]
pub struct Discovery {
    pub port: u16,
    pub group: Ipv4Addr,
    /// One IPv4 interface to use instead of every external one.
    pub interface: Option<Ipv4Addr>,
}

impl Default for Discovery {
    fn default() -> Self {
        Discovery {
            port: DISCOVERY_PORT,
            group: DISCOVERY_GROUP,
            interface: None,
        }
    }
}

/// Every external IPv4 interface. The OS default can be a virtual adapter such as WSL.
pub fn interfaces(selected: Option<Ipv4Addr>) -> Vec<Ipv4Addr> {
    if let Some(selected) = selected {
        return vec![selected];
    }
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.ip() {
            std::net::IpAddr::V4(v4) => Some(v4),
            std::net::IpAddr::V6(_) => None,
        })
        .collect()
}

/// Joins the group on each interface; a repeat join fails, so failures are only counted.
fn join(socket: &UdpSocket, group: Ipv4Addr, interfaces: &[Ipv4Addr]) -> usize {
    interfaces
        .iter()
        .filter(|interface| socket.join_multicast_v4(group, **interface).is_ok())
        .count()
}

/// Binds the responder and answers queries until the returned task is aborted.
pub fn respond(
    identity: Arc<Identity>,
    tcp_port: u16,
    discovery: Discovery,
) -> io::Result<tokio::task::JoinHandle<()>> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, discovery.port)).into())?;
    socket.set_nonblocking(true)?;
    // Windows applies multicast loopback on the receiving socket and Linux on the sending one, so
    // both sides set it, as TS does.
    socket.set_multicast_loop_v4(true)?;
    let socket = UdpSocket::from_std(socket.into())?;
    if join(&socket, discovery.group, &interfaces(discovery.interface)) == 0 {
        socket.join_multicast_v4(
            discovery.group,
            discovery.interface.unwrap_or(Ipv4Addr::UNSPECIFIED),
        )?;
    }
    Ok(tokio::spawn(async move {
        let mut rejoin = tokio::time::interval(REJOIN);
        rejoin.tick().await;
        let mut buffer = [0u8; 1024];
        let mut window = Instant::now();
        let mut replies = 0u32;
        loop {
            tokio::select! {
                _ = rejoin.tick() => { join(&socket, discovery.group, &interfaces(discovery.interface)); }
                received = socket.recv_from(&mut buffer) => {
                    let Ok((len, remote)) = received else { continue };
                    if len > MAX_QUERY_BYTES {
                        continue;
                    }
                    if window.elapsed() > Duration::from_secs(1) {
                        window = Instant::now();
                        replies = 0;
                    }
                    // A reply budget keeps multicast from becoming an amplifier.
                    if replies >= REPLIES_PER_SECOND {
                        continue;
                    }
                    let Some(nonce) = parse_query(&buffer[..len]) else { continue };
                    replies += 1;
                    if let Ok(packet) = advertisement(&identity, tcp_port, &nonce) {
                        let reply = serde_json::to_vec(&packet).expect("packet serializes");
                        let _ = socket.send_to(&reply, remote).await;
                    }
                }
            }
        }
    }))
}

/// Sends one query on each interface and collects verified adverts until `timeout`.
pub async fn query_lan(discovery: Discovery, timeout: Duration) -> io::Result<Vec<Discovered>> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
    socket.set_nonblocking(true)?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(1)?;
    let socket = UdpSocket::from_std(socket.into())?;
    let nonce = uuid::Uuid::new_v4().to_string();
    let packet = query(&nonce);
    let target = SocketAddrV4::new(discovery.group, discovery.port);
    let interfaces = interfaces(discovery.interface);
    let mut sent = false;
    let mut failure = None;
    for interface in interfaces
        .iter()
        .map(Some)
        .chain(interfaces.is_empty().then_some(None))
    {
        if let Some(interface) = interface {
            if let Err(error) = SockRef::from(&socket).set_multicast_if_v4(interface) {
                failure = Some(error);
                continue;
            }
        }
        match socket.send_to(packet.as_bytes(), target).await {
            Ok(_) => sent = true,
            Err(error) => failure = Some(error),
        }
    }
    if !sent {
        return Err(failure
            .unwrap_or_else(|| io::Error::other("no interface to send the discovery query on")));
    }
    let mut found = HashMap::new();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buffer = [0u8; 4097];
    while let Ok(Ok((len, remote))) =
        tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
    {
        if let Some(peer) = parse_advert(&buffer[..len], remote.ip(), &nonce) {
            found.insert(peer.fingerprint.clone(), peer);
        }
    }
    Ok(found.into_values().collect())
}
