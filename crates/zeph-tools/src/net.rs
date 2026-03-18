// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Network utilities for tool crates.

// Re-export the canonical implementation from zeph-common.
pub use zeph_common::net::is_private_ip;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn loopback_v4() {
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn private_class_a() {
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn private_class_b() {
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
    }

    #[test]
    fn private_class_c() {
        assert!(is_private_ip("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn link_local_v4() {
        assert!(is_private_ip("169.254.1.1".parse().unwrap()));
    }

    #[test]
    fn unspecified_v4() {
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
    }

    #[test]
    fn broadcast_v4() {
        assert!(is_private_ip("255.255.255.255".parse().unwrap()));
    }

    #[test]
    fn cgnat_v4() {
        assert!(is_private_ip("100.64.0.1".parse().unwrap()));
        assert!(is_private_ip("100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn public_v4_not_blocked() {
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn loopback_v6() {
        assert!(is_private_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn unspecified_v6() {
        assert!(is_private_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
    }

    #[test]
    fn ula_v6() {
        assert!(is_private_ip("fc00::1".parse().unwrap()));
        assert!(is_private_ip("fd12:3456:789a::1".parse().unwrap()));
    }

    #[test]
    fn link_local_v6() {
        assert!(is_private_ip("fe80::1".parse().unwrap()));
    }

    #[test]
    fn ipv4_mapped_private() {
        assert!(is_private_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("::ffff:192.168.0.1".parse().unwrap()));
        assert!(is_private_ip("::ffff:100.64.0.1".parse().unwrap()));
    }

    #[test]
    fn public_v6_not_blocked() {
        assert!(!is_private_ip("2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn cgnat_boundary_not_blocked() {
        assert!(!is_private_ip("100.128.0.0".parse().unwrap()));
    }

    #[test]
    fn ipv4_mapped_unspecified() {
        assert!(is_private_ip("::ffff:0.0.0.0".parse().unwrap()));
    }
}
