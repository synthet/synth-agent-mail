//! The signed host advert (`src/peer/discovery.ts` `advertisement`, `parseAdvert`). Multicast
//! replies and `GET /identity` both carry it. Its signature only proves the advert is
//! self-consistent; it never makes a host trusted.

use std::net::IpAddr;

use mail_core::identity::{fingerprint, validate_public, Identity, PublicIdentity};
use mail_core::js::{is_uuid, utf16_len};
use mail_core::wire::Packet;
use serde::{Deserialize, Serialize};

pub const ADVERT_PROTOCOL: &str = "agent-mail/1 advert";
pub const DISCOVER_PROTOCOL: &str = "agent-mail/1 discover";
pub const MAX_ADVERT_BYTES: usize = 4096;

/// A host whose advert verified. `endpoint` is the address it answered from plus its advertised port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    pub identity: PublicIdentity,
    pub fingerprint: String,
    pub endpoint: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Advert {
    protocol: String,
    nonce: String,
    identity: PublicIdentity,
    port: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    protocol: String,
    nonce: String,
}

/// `advertisement()`: payload fields in `advertSchema` order, signed by this host.
pub fn advertisement(identity: &Identity, port: u16, nonce: &str) -> mail_core::Result<Packet> {
    let payload = serde_json::to_string(&Advert {
        protocol: ADVERT_PROTOCOL.into(),
        nonce: nonce.into(),
        identity: identity.public(),
        port,
    })?;
    let signature = identity.sign(payload.as_bytes())?;
    Ok(Packet { payload, signature })
}

/// The nonce from a multicast query, or `None` for anything that isn't one.
pub fn parse_query(data: &[u8]) -> Option<String> {
    let query: Query = serde_json::from_slice(data).ok()?;
    (query.protocol == DISCOVER_PROTOCOL && is_uuid(&query.nonce)).then_some(query.nonce)
}

pub fn query(nonce: &str) -> String {
    serde_json::json!({ "protocol": DISCOVER_PROTOCOL, "nonce": nonce }).to_string()
}

/// `parseAdvert()`: size caps, strict shapes, the nonce we sent, and the advert's own signature.
pub fn parse_advert(data: &[u8], address: IpAddr, nonce: &str) -> Option<Discovered> {
    if data.len() > MAX_ADVERT_BYTES {
        return None;
    }
    let packet: Packet = serde_json::from_slice(data).ok()?;
    if utf16_len(&packet.payload) > 3000 || utf16_len(&packet.signature) > 128 {
        return None;
    }
    let advert: Advert = serde_json::from_str(&packet.payload).ok()?;
    if advert.protocol != ADVERT_PROTOCOL
        || advert.nonce != nonce
        || !is_uuid(&advert.nonce)
        || advert.port == 0
    {
        return None;
    }
    validate_public(&advert.identity).ok()?;
    if !advert
        .identity
        .verify(packet.payload.as_bytes(), &packet.signature)
    {
        return None;
    }
    let host = match address {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    Some(Discovered {
        fingerprint: fingerprint(&advert.identity).ok()?,
        identity: advert.identity,
        endpoint: format!("http://{host}:{}", advert.port),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advert_round_trip_needs_the_same_nonce() {
        let identity = Identity::generate("pc2").unwrap();
        let nonce = uuid::Uuid::new_v4().to_string();
        let packet = serde_json::to_vec(&advertisement(&identity, 47832, &nonce).unwrap()).unwrap();
        let found = parse_advert(&packet, "192.168.1.20".parse().unwrap(), &nonce).unwrap();
        assert_eq!(found.endpoint, "http://192.168.1.20:47832");
        assert_eq!(found.fingerprint, identity.fingerprint().unwrap());
        let other = uuid::Uuid::new_v4().to_string();
        assert!(parse_advert(&packet, "192.168.1.20".parse().unwrap(), &other).is_none());
        let v6 = parse_advert(&packet, "fe80::1".parse().unwrap(), &nonce).unwrap();
        assert_eq!(v6.endpoint, "http://[fe80::1]:47832");
    }

    #[test]
    fn query_parses_only_discover_packets() {
        let nonce = uuid::Uuid::new_v4().to_string();
        assert_eq!(parse_query(query(&nonce).as_bytes()), Some(nonce));
        assert_eq!(
            parse_query(br#"{"protocol":"agent-mail/1 discover","nonce":"x"}"#),
            None
        );
        assert_eq!(
            parse_query(br#"{"protocol":"other","nonce":"6F9619FF-8B86-D011-B42D-00C04FC964FF"}"#),
            None
        );
    }
}
