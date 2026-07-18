// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Network utilities for tool crates.
//!
//! Shared SSRF and domain-policy primitives used by every outbound-network tool
//! (`scrape.rs`, `search/`). Centralizing these here means new HTTP-based tools get the
//! same URL-scheme, private-IP, and allow/deny-list enforcement without re-implementing it.

use std::net::IpAddr;

use url::Url;

use crate::domain_match::domain_matches;
use crate::executor::ToolError;

// Re-export the canonical implementation from zeph-common.
pub use zeph_common::net::is_private_ip;

/// Validate that `raw` is an HTTPS URL pointing at a non-private host.
///
/// Rejects any scheme other than `https` and any host that resolves syntactically to a
/// loopback/private/link-local address or one of the reserved `.localhost`/`.internal`/
/// `.local` domain suffixes. This is a syntactic check only — callers must still run
/// [`zeph_common::net::resolve_and_validate`] to catch DNS-level SSRF (a public hostname
/// resolving to a private IP).
///
/// # Errors
///
/// Returns [`ToolError::Blocked`] when the URL fails to parse, uses a non-HTTPS scheme,
/// or targets a private/local host.
pub fn validate_url(raw: &str) -> Result<Url, ToolError> {
    let parsed = Url::parse(raw).map_err(|_| ToolError::Blocked {
        command: format!("invalid URL: {raw}"),
    })?;

    if parsed.scheme() != "https" {
        return Err(ToolError::Blocked {
            command: format!("scheme not allowed: {}", parsed.scheme()),
        });
    }

    if let Some(host) = parsed.host()
        && is_private_host(&host)
    {
        return Err(ToolError::Blocked {
            command: format!(
                "private/local host blocked: {}",
                parsed.host_str().unwrap_or("")
            ),
        });
    }

    Ok(parsed)
}

/// Returns `true` when `host` is a loopback/private-range IP literal or a reserved
/// `.localhost`/`.internal`/`.local` domain.
#[must_use]
pub fn is_private_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(d) => {
            // Exact match or subdomain of localhost (e.g. foo.localhost)
            // and .internal/.local TLDs used in cloud/k8s environments.
            #[allow(clippy::case_sensitive_file_extension_comparisons)]
            {
                *d == "localhost"
                    || d.ends_with(".localhost")
                    || d.ends_with(".internal")
                    || d.ends_with(".local")
            }
        }
        url::Host::Ipv4(v4) => is_private_ip(IpAddr::V4(*v4)),
        url::Host::Ipv6(v6) => is_private_ip(IpAddr::V6(*v6)),
    }
}

/// Check `host` against a domain allowlist/denylist pair.
///
/// Logic:
/// 1. If `denied_domains` matches the host → block.
/// 2. If `allowed_domains` is non-empty:
///    a. IP address hosts are always rejected (no pattern can match a bare IP).
///    b. Hosts not matching any entry → block.
/// 3. Otherwise → allow.
///
/// Wildcard prefix matching: `*.example.com` matches `sub.example.com` but NOT `example.com`.
/// Multiple wildcards are not supported; patterns with more than one `*` are treated as exact.
///
/// # Errors
///
/// Returns [`ToolError::Blocked`] when `host` matches the denylist, or when the allowlist
/// is non-empty and `host` is a bare IP or does not match any allowlist entry.
pub fn check_domain_policy(
    host: &str,
    allowed_domains: &[String],
    denied_domains: &[String],
) -> Result<(), ToolError> {
    if denied_domains.iter().any(|p| domain_matches(p, host)) {
        return Err(ToolError::Blocked {
            command: format!("domain blocked by denylist: {host}"),
        });
    }
    if !allowed_domains.is_empty() {
        // Bare IP addresses cannot match any domain pattern — reject when allowlist is active.
        let is_ip =
            host.parse::<IpAddr>().is_ok() || (host.starts_with('[') && host.ends_with(']'));
        if is_ip {
            return Err(ToolError::Blocked {
                command: format!(
                    "bare IP address not allowed when domain allowlist is active: {host}"
                ),
            });
        }
        if !allowed_domains.iter().any(|p| domain_matches(p, host)) {
            return Err(ToolError::Blocked {
                command: format!("domain not in allowlist: {host}"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;
    use std::net::{Ipv4Addr, Ipv6Addr};

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

    // --- validate_url ---

    #[test]
    fn valid_https_url() {
        assert!(validate_url("https://example.com").is_ok());
    }

    #[test]
    fn http_rejected() {
        let err = validate_url("http://example.com").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn invalid_url_rejected() {
        let err = validate_url("not a url").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn localhost_blocked() {
        let err = validate_url("https://localhost/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn loopback_ip_blocked() {
        let err = validate_url("https://127.0.0.1/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn private_10_blocked() {
        let err = validate_url("https://10.0.0.1/api").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn public_ip_allowed() {
        assert!(validate_url("https://93.184.216.34/page").is_ok());
    }

    #[test]
    fn ftp_rejected() {
        let err = validate_url("ftp://files.example.com").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn file_rejected() {
        let err = validate_url("file:///etc/passwd").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn private_172_blocked() {
        let err = validate_url("https://172.16.0.1/api").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn private_192_blocked() {
        let err = validate_url("https://192.168.1.1/api").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn ipv6_loopback_blocked() {
        let err = validate_url("https://[::1]/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn url_with_port_allowed() {
        assert!(validate_url("https://example.com:8443/path").is_ok());
    }

    #[test]
    fn link_local_ip_blocked() {
        let err = validate_url("https://169.254.1.1/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn url_no_scheme_rejected() {
        let err = validate_url("example.com/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn unspecified_ipv4_blocked() {
        let err = validate_url("https://0.0.0.0/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn broadcast_ipv4_blocked() {
        let err = validate_url("https://255.255.255.255/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn ipv6_link_local_blocked() {
        let err = validate_url("https://[fe80::1]/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn ipv6_unique_local_blocked() {
        let err = validate_url("https://[fd12::1]/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn ipv4_mapped_ipv6_loopback_blocked() {
        let err = validate_url("https://[::ffff:127.0.0.1]/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn ipv4_mapped_ipv6_private_blocked() {
        let err = validate_url("https://[::ffff:10.0.0.1]/path").unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    // --- check_domain_policy ---

    #[test]
    fn denylist_blocks_regardless_of_allowlist() {
        let err = check_domain_policy("evil.com", &[], &["evil.com".to_owned()]).unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn allowlist_empty_allows_any_host() {
        assert!(check_domain_policy("example.com", &[], &[]).is_ok());
    }

    #[test]
    fn allowlist_rejects_non_matching_host() {
        let err = check_domain_policy("other.com", &["example.com".to_owned()], &[]).unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }

    #[test]
    fn allowlist_accepts_matching_host() {
        assert!(check_domain_policy("example.com", &["example.com".to_owned()], &[]).is_ok());
    }

    #[test]
    fn allowlist_rejects_bare_ip() {
        let err =
            check_domain_policy("93.184.216.34", &["example.com".to_owned()], &[]).unwrap_err();
        assert_matches!(err, ToolError::Blocked { .. });
    }
}
