//! JavaScript string semantics the TypeScript schemas rely on.

use std::net::{Ipv4Addr, Ipv6Addr};

/// `String.prototype.trim`: ECMAScript WhiteSpace and LineTerminator.
/// Rust's `char::is_whitespace` also matches U+0085, which JS keeps, and misses U+FEFF, which JS trims.
pub fn trim(value: &str) -> &str {
    let js_space = |c: char| c == '\u{FEFF}' || (c.is_whitespace() && c != '\u{0085}');
    value.trim_matches(js_space)
}

/// zod's `.min()`/`.max()` on strings count UTF-16 code units.
pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// `a < b` on JS strings compares UTF-16 code units.
pub fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `net.isIP(value) !== 0`. Node accepts an IPv6 zone id (`fe80::1%eth0`); dotted IPv4 rejects
/// leading zeros in both Node and Rust.
pub fn is_ip(value: &str) -> bool {
    if value.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    let address = match value.split_once('%') {
        Some((address, zone)) if !zone.is_empty() => address,
        Some(_) => return false,
        None => value,
    };
    address.parse::<Ipv6Addr>().is_ok()
}

/// zod v3 `uuid()`: `^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`, any case.
pub fn is_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// `^[a-f0-9]{64}$`: a lowercase hex sha256 fingerprint.
pub fn is_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_matches_javascript() {
        assert_eq!(trim("\u{FEFF}\u{00A0} a b\u{2028}\t"), "a b");
        assert_eq!(trim("\u{0085}x\u{0085}"), "\u{0085}x\u{0085}");
    }

    #[test]
    fn ip_matches_node() {
        assert!(is_ip("192.168.1.20"));
        assert!(is_ip("fe80::1%eth0"));
        assert!(is_ip("::ffff:1.2.3.4"));
        assert!(!is_ip("01.2.3.4"));
        assert!(!is_ip("999.1.1.1"));
        assert!(!is_ip("fe80::1%"));
        assert!(!is_ip("pc1"));
    }

    #[test]
    fn uuid_any_case() {
        assert!(is_uuid("6F9619FF-8B86-D011-B42D-00C04FC964FF"));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("6F9619FF8B86-D011-B42D-00C04FC964FF"));
    }
}
