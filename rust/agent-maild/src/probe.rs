//! Identity probes over HTTP: the static `hosts.json` list and online Tailscale peers
//! (`src/peer/discovery.ts` `probeIdentity`, `discoverTailscale`). Only addresses the owner listed
//! or the local Tailscale CLI reported are probed; there is no subnet sweep.

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::advert::{parse_advert, Discovered, MAX_ADVERT_BYTES};

pub const HOSTS_FILE: &str = "hosts.json";
const PROBE_TIMEOUT: Duration = Duration::from_millis(1200);
const ROUND_DEADLINE: Duration = Duration::from_secs(4);
const CONCURRENCY: usize = 16;
const MAX_ADDRESSES: usize = 256;

/// No proxy, no redirects: a probe must reach exactly the address it names.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(PROBE_TIMEOUT)
        .build()
        .expect("static client configuration")
}

/// `GET /identity?nonce=…`, capped at 4096 bytes, verified against the nonce sent.
pub async fn probe_identity(
    client: &reqwest::Client,
    address: IpAddr,
    port: u16,
) -> Option<Discovered> {
    let nonce = uuid::Uuid::new_v4().to_string();
    let host = match address {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let mut response = client
        .get(format!("http://{host}:{port}/identity?nonce={nonce}"))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_ADVERT_BYTES {
            return None;
        }
    }
    parse_advert(&body, address, &nonce)
}

/// Probes addresses 16 at a time, stopping new probes after four seconds.
pub async fn probe_all(client: &reqwest::Client, targets: Vec<(IpAddr, u16)>) -> Vec<Discovered> {
    let queue = Arc::new(Mutex::new(
        targets.into_iter().take(MAX_ADDRESSES).collect::<Vec<_>>(),
    ));
    let found = Arc::new(Mutex::new(Vec::new()));
    let deadline = tokio::time::Instant::now() + ROUND_DEADLINE;
    let workers: Vec<_> = (0..CONCURRENCY)
        .map(|_| {
            let (queue, found, client) = (queue.clone(), found.clone(), client.clone());
            tokio::spawn(async move {
                while tokio::time::Instant::now() < deadline {
                    let Some((address, port)) = queue.lock().await.pop() else {
                        break;
                    };
                    if let Some(peer) = probe_identity(&client, address, port).await {
                        found.lock().await.push(peer);
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.await;
    }
    Arc::try_unwrap(found)
        .map(Mutex::into_inner)
        .unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostsFile {
    hosts: Vec<String>,
}

/// The owner's static host list: `{"hosts": ["pc2", "192.168.1.20", "100.64.0.3:47832", "[fe80::1]:47832"]}`.
/// A missing file is an empty list. Listing a host never trusts it.
pub fn read_hosts(home: &Path) -> Result<Vec<String>, String> {
    match std::fs::read_to_string(home.join(HOSTS_FILE)) {
        Ok(text) => serde_json::from_str::<HostsFile>(&text)
            .map(|file| file.hosts.into_iter().take(MAX_ADDRESSES).collect())
            .map_err(|error| format!("{HOSTS_FILE}: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("{HOSTS_FILE}: {error}")),
    }
}

/// Splits `host`, `host:port`, `ip`, `[v6]:port` or a bare IPv6 address.
pub fn parse_host_entry(entry: &str, default_port: u16) -> Option<(String, u16)> {
    let entry = entry.trim();
    if entry.is_empty() || entry.len() > 255 || entry.contains(['/', '@', ' ']) {
        return None;
    }
    if let Some(rest) = entry.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail {
            "" => default_port,
            tail => tail.strip_prefix(':')?.parse().ok()?,
        };
        return host
            .parse::<std::net::Ipv6Addr>()
            .ok()
            .map(|_| (host.to_owned(), port));
    }
    if entry.parse::<IpAddr>().is_ok() {
        return Some((entry.to_owned(), default_port));
    }
    match entry.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(':') => {
            Some((host.to_owned(), port.parse().ok().filter(|p| *p != 0)?))
        }
        Some(_) => None,
        None => Some((entry.to_owned(), default_port)),
    }
}

/// Resolves the static entries to addresses (at most eight per name).
pub async fn resolve_hosts(entries: &[String], default_port: u16) -> Vec<(IpAddr, u16)> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        let Some((host, port)) = parse_host_entry(entry, default_port) else {
            crate::log!("agent-maild: ignoring hosts.json entry {entry:?}");
            continue;
        };
        let Ok(addresses) = tokio::net::lookup_host((host.as_str(), port)).await else {
            continue;
        };
        for address in addresses.take(8) {
            if seen.insert((address.ip(), port)) {
                targets.push((address.ip(), port));
            }
        }
    }
    targets
}

/// Online Tailscale peer IPs from `tailscale status --json`.
pub fn tailscale_addresses(status: &Value) -> Vec<IpAddr> {
    let mut seen = HashSet::new();
    let Some(peers) = status.get("Peer").and_then(Value::as_object) else {
        return Vec::new();
    };
    peers
        .values()
        .filter(|peer| peer.get("Online").and_then(Value::as_bool) != Some(false))
        .filter_map(|peer| peer.get("TailscaleIPs").and_then(Value::as_array))
        .flatten()
        .filter_map(|ip| ip.as_str()?.parse::<IpAddr>().ok())
        .filter(|ip| seen.insert(*ip))
        .take(MAX_ADDRESSES)
        .collect()
}

/// Runs the local Tailscale CLI by absolute path on Windows, with a 2 s limit and no console window.
/// `None` when Tailscale isn't installed or isn't running.
pub async fn tailscale_status() -> Option<Value> {
    #[cfg(windows)]
    let executable = std::path::PathBuf::from(
        std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into()),
    )
    .join("Tailscale")
    .join("tailscale.exe");
    #[cfg(not(windows))]
    let executable = std::path::PathBuf::from("tailscale");
    let mut command = tokio::process::Command::new(executable);
    command
        .args(["status", "--json"])
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let output = tokio::time::timeout(Duration::from_secs(2), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() || output.stdout.len() > 2 * 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn host_entries() {
        assert_eq!(parse_host_entry("pc2", 47832), Some(("pc2".into(), 47832)));
        assert_eq!(
            parse_host_entry("100.64.0.3:47900", 47832),
            Some(("100.64.0.3".into(), 47900))
        );
        assert_eq!(
            parse_host_entry("fe80::1", 47832),
            Some(("fe80::1".into(), 47832))
        );
        assert_eq!(
            parse_host_entry("[fe80::1]:47900", 47832),
            Some(("fe80::1".into(), 47900))
        );
        assert_eq!(parse_host_entry("http://pc2", 47832), None);
        assert_eq!(parse_host_entry("pc2:0", 47832), None);
        assert_eq!(parse_host_entry("", 47832), None);
    }

    #[test]
    fn tailscale_online_peers_only() {
        let status = json!({ "Peer": {
            "a": { "Online": true, "TailscaleIPs": ["100.64.0.3", "fd7a:115c:a1e0::3"] },
            "b": { "Online": false, "TailscaleIPs": ["100.64.0.4"] },
            "c": { "TailscaleIPs": ["100.64.0.5", "not-an-ip", "100.64.0.3"] }
        }});
        let mut found = tailscale_addresses(&status);
        found.sort();
        let expected: Vec<IpAddr> = ["100.64.0.3", "100.64.0.5", "fd7a:115c:a1e0::3"]
            .iter()
            .map(|ip| ip.parse().unwrap())
            .collect();
        assert_eq!(found, expected);
        assert!(tailscale_addresses(&json!({})).is_empty());
    }

    #[test]
    fn missing_hosts_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_hosts(dir.path()), Ok(vec![]));
        std::fs::write(
            dir.path().join(HOSTS_FILE),
            r#"{"hosts":["pc2"],"extra":1}"#,
        )
        .unwrap();
        assert!(
            read_hosts(dir.path()).is_err(),
            "unknown fields are refused"
        );
    }
}
