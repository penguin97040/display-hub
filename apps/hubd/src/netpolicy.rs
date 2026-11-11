//! Which source addresses may talk to the hub at all: the local network only.
//!
//! This is a second line of defence behind authentication and the firewall. A connection from any
//! other address is closed before TLS starts.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Private, link-local and loopback IPv4 addresses.
fn local_v4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local() || ip.is_loopback()
}

/// IPv6 loopback, link-local (`fe80::/10`) and unique local (`fc00::/7`). Global IPv6 addresses are
/// refused even on the home network, because a router's IPv6 firewall can't be relied on.
fn local_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_loopback() || (first & 0xffc0) == 0xfe80 || (first & 0xfe00) == 0xfc00
}

pub fn allowed(ip: IpAddr, allow_ipv6: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => local_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => local_v4(v4),
            None => allow_ipv6 && local_v6(v6),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str, v6: bool) -> bool {
        allowed(s.parse().unwrap(), v6)
    }

    #[test]
    fn local_ipv4_only() {
        for a in [
            "192.168.1.20",
            "10.0.0.5",
            "172.16.3.4",
            "172.31.255.1",
            "169.254.1.1",
            "127.0.0.1",
        ] {
            assert!(ok(a, false), "{a}");
        }
        for a in [
            "8.8.8.8",
            "172.32.0.1",
            "100.64.0.1",
            "203.0.113.5",
            "0.0.0.0",
            "255.255.255.255",
        ] {
            assert!(!ok(a, false), "{a}");
        }
    }

    #[test]
    fn ipv6_only_when_enabled_and_never_global() {
        assert!(ok("::ffff:192.168.1.20", false));
        assert!(!ok("::ffff:8.8.8.8", true));
        assert!(!ok("fe80::1", false));
        for a in ["fe80::1", "fd12:3456::1", "::1"] {
            assert!(ok(a, true), "{a}");
        }
        for a in ["2001:db8::1", "2404:4400::1", "ff02::1"] {
            assert!(!ok(a, true), "{a}");
        }
    }
}
