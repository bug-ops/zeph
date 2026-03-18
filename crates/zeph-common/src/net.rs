// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Network utilities shared across crates.

use std::net::IpAddr;

/// Returns `true` if `addr` is a non-routable or private IP address that
/// should be blocked for outbound connections (SSRF defense).
///
/// Covers:
/// - IPv4: loopback (`127/8`), private (`10/8`, `172.16/12`, `192.168/16`),
///   link-local (`169.254/16`), unspecified (`0.0.0.0`), broadcast (`255.255.255.255`),
///   CGNAT (`100.64.0.0/10`, RFC 6598).
/// - IPv6: loopback (`::1`), unspecified (`::`), ULA (`fc00::/7`),
///   link-local (`fe80::/10`), IPv4-mapped (`::ffff:x.x.x.x` — applies IPv4 rules).
#[must_use]
pub fn is_private_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(ip) => {
            let n = u32::from(ip);
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                // CGNAT range 100.64.0.0/10 (RFC 6598).
                || (n & 0xFFC0_0000 == 0x6440_0000)
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.to_ipv4_mapped().is_some_and(|v4| {
                    let n = u32::from(v4);
                    v4.is_loopback()
                        || v4.is_private()
                        || v4.is_link_local()
                        || v4.is_unspecified()
                        || v4.is_broadcast()
                        || (n & 0xFFC0_0000 == 0x6440_0000)
                })
                || (ip.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (ip.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn loopback_is_private() {
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_private_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn private_ranges() {
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn link_local() {
        assert!(is_private_ip("169.254.0.1".parse().unwrap()));
    }

    #[test]
    fn unspecified() {
        assert!(is_private_ip("0.0.0.0".parse().unwrap()));
        assert!(is_private_ip("::".parse().unwrap()));
    }

    #[test]
    fn broadcast() {
        assert!(is_private_ip("255.255.255.255".parse().unwrap()));
    }

    #[test]
    fn cgnat() {
        assert!(is_private_ip("100.64.0.1".parse().unwrap()));
        assert!(is_private_ip("100.127.255.255".parse().unwrap()));
        assert!(!is_private_ip("100.128.0.1".parse().unwrap()));
    }

    #[test]
    fn public_ipv4() {
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip("93.184.216.34".parse().unwrap()));
    }

    #[test]
    fn ipv6_unique_local() {
        assert!(is_private_ip("fc00::1".parse().unwrap()));
        assert!(is_private_ip("fd00::1".parse().unwrap()));
    }

    #[test]
    fn ipv6_link_local() {
        assert!(is_private_ip("fe80::1".parse().unwrap()));
    }

    #[test]
    fn ipv6_public() {
        assert!(!is_private_ip("2001:4860:4860::8888".parse().unwrap()));
    }
}
