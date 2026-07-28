// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(all(feature = "tui", feature = "a2a"))]
use zeph_tui::{App, EventReader};

/// Resolves the client-side [`SecurityPolicy`](zeph_a2a::SecurityPolicy) for a `--connect`
/// target, applying the loopback carve-out (#5878): loopback targets (`127.0.0.1`, `::1`,
/// `localhost`) always connect over plain HTTP with SSRF checks skipped, regardless of
/// `client_cfg`; every other target is governed by `client_cfg.require_tls`/`ssrf_protection`.
///
/// `url` is matched syntactically (via [`url::Url::host_str`] + [`is_loopback_host`], no DNS
/// resolution), so this carries no SSRF risk of its own and cannot be spoofed by a malicious
/// DNS response.
///
/// [`is_loopback_host`]: zeph_common::net::is_loopback_host
#[cfg(all(feature = "tui", feature = "a2a"))]
fn resolve_client_security_policy(
    url: &str,
    client_cfg: &zeph_core::config::A2aClientConfig,
) -> zeph_a2a::SecurityPolicy {
    let is_loopback = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(zeph_common::net::is_loopback_host))
        .unwrap_or(false);
    if is_loopback {
        zeph_a2a::SecurityPolicy {
            require_tls: false,
            ssrf_protection: false,
        }
    } else {
        zeph_a2a::SecurityPolicy {
            require_tls: client_cfg.require_tls,
            ssrf_protection: client_cfg.ssrf_protection,
        }
    }
}

/// Converts `zeph_config::channels::CardTrustPolicy` (TOML-facing) to
/// `zeph_a2a::CardTrustPolicy` (protocol-facing) — the two enums are independent (no
/// cross-crate dependency between `zeph-config` and `zeph-a2a`, see both types' doc
/// comments), so every currently-known variant is mapped explicitly by name.
///
/// Both enums are `#[non_exhaustive]`, so the compiler requires a trailing wildcard arm
/// here despite every variant being named — that arm is unreachable today and is a fail
/// **closed** fallback (maps to `Require`, the strictest policy), not a silent downgrade,
/// should a future variant land on one side without a matching update here. The round-trip
/// test below enumerates every variant explicitly so drift is caught in CI rather than at
/// runtime.
#[cfg(all(feature = "tui", feature = "a2a"))]
fn convert_card_trust_policy(
    policy: zeph_core::config::CardTrustPolicy,
) -> zeph_a2a::CardTrustPolicy {
    match policy {
        zeph_core::config::CardTrustPolicy::Ignore => zeph_a2a::CardTrustPolicy::Ignore,
        zeph_core::config::CardTrustPolicy::Prefer => zeph_a2a::CardTrustPolicy::Prefer,
        zeph_core::config::CardTrustPolicy::Require => zeph_a2a::CardTrustPolicy::Require,
        _ => {
            tracing::error!(
                ?policy,
                "a2a_client.card_trust_policy: unrecognized variant, failing closed to `require`"
            );
            zeph_a2a::CardTrustPolicy::Require
        }
    }
}

/// Converts `[a2a_client].trusted_agent_keys` into `zeph_a2a::TrustedKey`s, parsing each
/// entry's `alg` string via [`zeph_a2a::SigAlg::from_jws_alg`]. An entry with an
/// unrecognized `alg` is dropped (with a `tracing::warn!`) rather than carried through as
/// a key nothing can ever match — `verify_card_signatures` only matches on `kid` + `alg`.
#[cfg(all(feature = "tui", feature = "a2a"))]
fn convert_trusted_agent_keys(
    keys: &[zeph_core::config::TrustedAgentKey],
) -> Vec<zeph_a2a::TrustedKey> {
    keys.iter()
        .filter_map(|k| {
            if let Some(alg) = zeph_a2a::SigAlg::from_jws_alg(&k.alg) {
                Some(zeph_a2a::TrustedKey {
                    kid: k.kid.clone(),
                    alg,
                    key_material: k.jwk_or_pem.clone(),
                })
            } else {
                tracing::warn!(
                    kid = %k.kid,
                    alg = %k.alg,
                    "a2a_client.trusted_agent_keys: unsupported alg, key ignored"
                );
                None
            }
        })
        .collect()
}

/// Strips a `--connect` URL down to its origin (`scheme://host[:port]`, no path) for use
/// with `AgentRegistry::discover` (#6200).
///
/// The agent card is served at the origin root (`/.well-known/agent.json`), independent of
/// whatever path the `--connect` URL's RPC endpoint is mounted at (e.g. `/a2a/stream` per
/// the CLI usage example) — see `crates/zeph-a2a/src/server/router.rs`'s route table.
/// Passing the full `--connect` URL (path included) to `discover` would fetch
/// `.../a2a/stream/.well-known/agent.json`, which does not exist.
///
/// Falls back to `url` unchanged if it fails to parse — `discover`'s own `url::Url::parse`
/// inside `check_origin` will then surface the same parse failure as a mismatch.
#[cfg(all(feature = "tui", feature = "a2a"))]
fn discovery_origin(url: &str) -> String {
    url::Url::parse(url).map_or_else(|_| url.to_owned(), |u| u.origin().ascii_serialization())
}

/// Decides whether a `discover()` failure should abort `--connect`, or be logged and
/// tolerated so the SSE session still establishes (S1 critic finding on #6200).
///
/// [`A2aError::UntrustedCard`]/[`A2aError::UrlMismatch`] come from `check_trust`, which has
/// already folded `card_trust_policy` into its verdict (`prefer` only rejects a *tampered*
/// signature; `require` rejects any unverifiable/mismatched card) — that verdict is the
/// policy's own decision and stays fatal under every policy, since silently connecting
/// anyway would defeat `prefer`'s one hard guarantee as well as `require`'s.
///
/// Every other error (network failure, non-2xx, malformed JSON, timeout) is a fetch/parse
/// failure, not a trust decision. Before #6200, `--connect` never attempted discovery at
/// all, so a peer that speaks A2A JSON-RPC/streaming but serves no card at all (an
/// older/non-compliant/non-zeph peer, or a transient outage) connected fine. Regressing
/// that path is only justified under [`CardTrustPolicy::Require`], where a card is
/// mandatory to enforce anything — under `ignore`/`prefer`, discovery is best-effort.
///
/// [`A2aError::UntrustedCard`]: zeph_a2a::A2aError::UntrustedCard
/// [`A2aError::UrlMismatch`]: zeph_a2a::A2aError::UrlMismatch
/// [`CardTrustPolicy::Require`]: zeph_a2a::CardTrustPolicy::Require
#[cfg(all(feature = "tui", feature = "a2a"))]
fn discovery_error_is_fatal(error: &zeph_a2a::A2aError, policy: zeph_a2a::CardTrustPolicy) -> bool {
    match error {
        zeph_a2a::A2aError::UntrustedCard { .. } | zeph_a2a::A2aError::UrlMismatch { .. } => true,
        _ => policy == zeph_a2a::CardTrustPolicy::Require,
    }
}

/// Builds the `reqwest::Client` used for the one-shot `AgentRegistry::discover` call
/// performed before `--connect` establishes its SSE session (#6200), applying the same
/// `require_tls`/`ssrf_protection` posture `security` already carries for the `A2aClient`
/// itself — the discovery fetch of the very same URL must not silently bypass the posture
/// the operator configured for it. Mirrors `A2aClient`'s internal hardened-client
/// construction: redirects disabled, TLS enforced via `https_only`, and (when
/// `ssrf_protection` is set) the connection pinned to the addresses validated by
/// [`resolve_and_validate`](zeph_common::net::resolve_and_validate), closing the same
/// DNS-rebinding TOCTOU window `A2aClient` closes for its own requests.
///
/// # Errors
///
/// Returns an error if `require_tls` is set and `url` is not `https://`, if
/// `ssrf_protection` is set and `url`'s host resolves to a private/loopback/link-local
/// address, or if the underlying `reqwest::Client` fails to build.
#[cfg(all(feature = "tui", feature = "a2a"))]
async fn hardened_discovery_client(
    url: &str,
    security: zeph_a2a::SecurityPolicy,
) -> anyhow::Result<reqwest::Client> {
    if security.require_tls && !url.starts_with("https://") {
        anyhow::bail!("a2a_client.require_tls is set but --connect target uses http://: {url}");
    }

    let mut builder = reqwest::Client::builder()
        .user_agent(concat!(
            "zeph/",
            env!("CARGO_PKG_VERSION"),
            " (a2a-discovery)"
        ))
        .redirect(reqwest::redirect::Policy::none());
    if security.require_tls {
        builder = builder.https_only(true);
    }
    if security.ssrf_protection
        && let Ok(parsed) = url::Url::parse(url)
        && let Some(host) = parsed.host_str()
    {
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs = zeph_common::net::resolve_and_validate(host, port)
            .await
            .map_err(|e| anyhow::anyhow!("a2a discovery SSRF validation failed for {url}: {e}"))?;
        builder = builder.resolve_to_addrs(host, &addrs);
    }

    builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build hardened discovery client: {e}"))
}

#[cfg(all(feature = "tui", feature = "a2a"))]
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_tui_remote(
    url: String,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use futures::StreamExt;
    use std::time::Duration;

    let config_file = crate::bootstrap::resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file)
        .unwrap_or_else(|_| zeph_core::config::Config::default());
    config.validate()?;
    let auth_token = config.a2a.auth_token.clone();

    // `[a2a_client]` is a dedicated client-side policy for this `--connect` path — distinct
    // from `[a2a]` (`A2aServerConfig`), which only governs this process's own A2A server (#5878).
    let security = resolve_client_security_policy(&url, &config.a2a_client);

    // Verify the peer's AgentCard (signature + URL-origin trust policy, A2A 1.0.0 §8.4)
    // before establishing the SSE session — this is the `AgentRegistry` construction site
    // that makes `[a2a_client].card_trust_policy` actually enforce (#6200); previously
    // nothing in the codebase ever called `AgentRegistry::discover`.
    let discovery_base_url = discovery_origin(&url);
    let discovery_client = hardened_discovery_client(&discovery_base_url, security).await?;
    let trust_policy = convert_card_trust_policy(config.a2a_client.card_trust_policy);
    if trust_policy == zeph_a2a::CardTrustPolicy::Ignore {
        // Default policy (issue #6553): peer AgentCards are accepted without signature or
        // URL-origin verification. Bounded risk — the A2A session below connects to the
        // CLI/config-supplied `url`, never to the discovered card's self-declared `url` — but
        // still worth a visible signal since it means capability metadata is fully trusted.
        tracing::warn!(
            "a2a discovery: card_trust_policy=ignore — peer AgentCard accepted without \
             signature/origin verification; set a2a_client.card_trust_policy to \"prefer\" or \
             \"require\" to verify peer identity"
        );
    }
    let registry = zeph_a2a::AgentRegistry::new(discovery_client, Duration::from_mins(5))
        .with_trust(
            trust_policy,
            convert_trusted_agent_keys(&config.a2a_client.trusted_agent_keys),
        );
    match registry.discover(&discovery_base_url).await {
        Ok(peer_card) => {
            tracing::info!(
                peer = %peer_card.name,
                policy = ?config.a2a_client.card_trust_policy,
                "a2a discovery: peer card verified per card_trust_policy"
            );
        }
        Err(e) if discovery_error_is_fatal(&e, trust_policy) => {
            anyhow::bail!("A2A peer discovery/trust check failed for {discovery_base_url}: {e}");
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                policy = ?config.a2a_client.card_trust_policy,
                "a2a discovery: could not fetch peer card, proceeding without trust verification"
            );
        }
    }

    let client = zeph_a2a::A2aClient::new(zeph_core::http::default_client(), security);

    // Cloned before `url` is moved into the `async move` SSE pump block below.
    let remote_daemon_url = url.clone();

    // user_tx is passed to App; App sends user text through it.
    // We receive on user_rx and forward to the A2A SSE pump.
    let (user_tx, mut user_rx) = tokio::sync::mpsc::channel::<String>(32);
    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<zeph_tui::AgentEvent>(256);

    if trust_policy == zeph_a2a::CardTrustPolicy::Ignore {
        // Mirrors the `tracing::warn!` above, but the `tracing::warn!` alone is not a
        // reliable signal once the TUI screen is up: `init_tracing` only keeps the stderr
        // layer when `--tui` was NOT passed (`tracing_init.rs`), yet this function renders a
        // ratatui screen unconditionally — so `zeph --tui --connect <URL>` would otherwise
        // suppress the warning to the file-only log layer with no on-screen trace at all
        // (CLAUDE.md TUI Rules: implicit/security-relevant state needs a visible indicator).
        // Queued on `agent_tx` before `App::new` even takes `agent_rx`, so it is guaranteed to
        // be the first system message the chat pane renders once the TUI starts.
        let _ = agent_tx
            .send(zeph_tui::AgentEvent::FullMessage(
                "Warning: card_trust_policy=ignore — peer AgentCard accepted without \
                 signature/origin verification. Set a2a_client.card_trust_policy to \
                 \"prefer\" or \"require\" to verify peer identity."
                    .into(),
            ))
            .await;
    }

    let tui_cancel = tokio_util::sync::CancellationToken::new();
    let tui_supervisor = zeph_common::TaskSupervisor::new(tui_cancel.clone());

    let agent_tx_pump = agent_tx.clone();
    let sse_fut = async move {
        while let Some(text) = user_rx.recv().await {
            let message = zeph_a2a::Message::user_text(&text);
            let params = zeph_a2a::SendMessageParams {
                message,
                configuration: None,
            };

            let _ = agent_tx_pump.send(zeph_tui::AgentEvent::Typing).await;

            let stream_result = client
                .stream_message(&url, params, auth_token.as_deref())
                .await;

            match stream_result {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        match event {
                            Ok(zeph_a2a::TaskEvent::ArtifactUpdate(artifact_evt)) => {
                                let text: String = artifact_evt
                                    .artifact
                                    .parts
                                    .iter()
                                    .filter_map(|p| {
                                        if let zeph_a2a::Part::Text { text, .. } = p {
                                            Some(text.as_str())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                                let is_final = artifact_evt.is_final;
                                if !text.is_empty() {
                                    let _ =
                                        agent_tx_pump.send(zeph_tui::AgentEvent::Chunk(text)).await;
                                }
                                if is_final {
                                    let _ = agent_tx_pump.send(zeph_tui::AgentEvent::Flush).await;
                                }
                            }
                            Ok(zeph_a2a::TaskEvent::StatusUpdate(status_evt)) => {
                                match status_evt.status.state {
                                    zeph_a2a::TaskState::Completed => {
                                        let _ =
                                            agent_tx_pump.send(zeph_tui::AgentEvent::Flush).await;
                                    }
                                    zeph_a2a::TaskState::Failed => {
                                        let _ = agent_tx_pump
                                            .send(zeph_tui::AgentEvent::FullMessage(
                                                "Error: task failed".into(),
                                            ))
                                            .await;
                                    }
                                    _ => {}
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                let _ = agent_tx_pump
                                    .send(zeph_tui::AgentEvent::FullMessage(format!(
                                        "Connection error: {e}"
                                    )))
                                    .await;
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = agent_tx_pump
                        .send(zeph_tui::AgentEvent::FullMessage(format!(
                            "Connection error: {e}"
                        )))
                        .await;
                }
            }
        }
    };

    let sse_cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(sse_fut)));
    tui_supervisor.spawn(zeph_common::TaskDescriptor {
        name: "a2a_sse_pump",
        restart: zeph_common::RestartPolicy::RunOnce,
        factory: move || {
            let f = sse_cell.lock().take();
            async move {
                if let Some(f) = f {
                    f.await;
                }
            }
        },
    });

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
    let reader = EventReader::new(event_tx, Duration::from_millis(100));
    std::thread::spawn(move || reader.run());

    let (tui_theme, tui_theme_name, tui_color_mode) = {
        use zeph_tui::theme::{EffectiveColorMode, Theme, resolve_color_mode, resolve_palette};
        let theme_cfg = &config.tui.theme;
        let mode = resolve_color_mode(theme_cfg.color_mode);
        // resolve_palette may do std::fs I/O for user-defined themes — run off the async thread.
        let theme_name = theme_cfg.name.clone();
        let palette_result =
            tokio::task::spawn_blocking(move || resolve_palette(&theme_name)).await;
        match palette_result {
            Ok(Ok(p)) => (
                Theme::from_palette_with_mode(&p, mode),
                theme_cfg.name.clone(),
                mode,
            ),
            Ok(Err(e)) => {
                tracing::warn!("TUI theme '{}' could not be loaded: {e}", theme_cfg.name);
                (
                    Theme::default(),
                    "zephyr".to_owned(),
                    EffectiveColorMode::Truecolor,
                )
            }
            Err(e) => {
                tracing::warn!("TUI theme '{}' resolution panicked: {e}", theme_cfg.name);
                (
                    Theme::default(),
                    "zephyr".to_owned(),
                    EffectiveColorMode::Truecolor,
                )
            }
        }
    };
    let mut tui_app = App::new(user_tx, agent_rx)
        .with_tool_density(config.tui.tool_density)
        .with_panel_sizing(config.tui.panel_sizing)
        .with_theme(tui_theme)
        .with_theme_name(tui_theme_name)
        .with_effective_color_mode(tui_color_mode)
        .with_remote_daemon_url(remote_daemon_url);
    tui_app.set_show_source_labels(config.tui.show_source_labels);

    zeph_tui::run_tui(tui_app, event_rx).await?;
    tui_cancel.cancel();
    tui_supervisor
        .shutdown_all(std::time::Duration::from_secs(5))
        .await;
    Ok(())
}

#[cfg(all(test, feature = "tui", feature = "a2a"))]
mod tests {
    use super::{
        convert_card_trust_policy, convert_trusted_agent_keys, discovery_error_is_fatal,
        discovery_origin, hardened_discovery_client, resolve_client_security_policy,
    };
    use zeph_core::config::{A2aClientConfig, CardTrustPolicy, TrustedAgentKey};

    fn hardened_client_cfg() -> A2aClientConfig {
        A2aClientConfig::default()
    }

    #[test]
    fn loopback_ipv4_bypasses_tls_and_ssrf_even_when_hardened() {
        let policy = resolve_client_security_policy(
            "http://127.0.0.1:8080/a2a/stream",
            &hardened_client_cfg(),
        );
        assert!(!policy.require_tls);
        assert!(!policy.ssrf_protection);
    }

    #[test]
    fn loopback_ipv6_bypasses_tls_and_ssrf() {
        let policy =
            resolve_client_security_policy("http://[::1]:8080/a2a/stream", &hardened_client_cfg());
        assert!(!policy.require_tls);
        assert!(!policy.ssrf_protection);
    }

    #[test]
    fn loopback_hostname_bypasses_tls_and_ssrf() {
        let policy = resolve_client_security_policy(
            "http://localhost:8080/a2a/stream",
            &hardened_client_cfg(),
        );
        assert!(!policy.require_tls);
        assert!(!policy.ssrf_protection);
    }

    #[test]
    fn non_loopback_http_uses_configured_client_policy() {
        let policy = resolve_client_security_policy(
            "http://agent.example.com/a2a/stream",
            &hardened_client_cfg(),
        );
        assert!(policy.require_tls);
        assert!(policy.ssrf_protection);
    }

    #[test]
    fn non_loopback_respects_permissive_client_config() {
        let permissive = A2aClientConfig {
            require_tls: false,
            ssrf_protection: false,
            ..A2aClientConfig::default()
        };
        let policy =
            resolve_client_security_policy("http://agent.example.com/a2a/stream", &permissive);
        assert!(!policy.require_tls);
        assert!(!policy.ssrf_protection);
    }

    #[test]
    fn unparseable_url_falls_back_to_configured_client_policy() {
        let policy = resolve_client_security_policy("not a url", &hardened_client_cfg());
        assert!(policy.require_tls);
        assert!(policy.ssrf_protection);
    }

    #[test]
    fn uppercase_scheme_still_matches_loopback() {
        // `url::Url::parse` normalizes the scheme to lowercase, but host extraction is
        // unaffected either way — `HTTP://` must resolve identically to `http://`.
        let policy = resolve_client_security_policy(
            "HTTP://127.0.0.1:8080/a2a/stream",
            &hardened_client_cfg(),
        );
        assert!(!policy.require_tls);
        assert!(!policy.ssrf_protection);
    }

    #[test]
    fn userinfo_and_port_url_still_matches_loopback() {
        // `Url::host_str()` must return only the host, ignoring userinfo and port.
        let policy = resolve_client_security_policy(
            "http://user:pass@127.0.0.1:8080/a2a/stream",
            &hardened_client_cfg(),
        );
        assert!(!policy.require_tls);
        assert!(!policy.ssrf_protection);
    }

    #[test]
    fn unspecified_address_is_not_treated_as_loopback() {
        // `0.0.0.0` is unspecified, not loopback — must stay TLS/SSRF-hardened.
        let policy = resolve_client_security_policy(
            "http://0.0.0.0:8080/a2a/stream",
            &hardened_client_cfg(),
        );
        assert!(policy.require_tls);
        assert!(policy.ssrf_protection);
    }

    #[test]
    fn top_of_loopback_range_bypasses_tls_and_ssrf() {
        // 127.0.0.0/8 is entirely loopback, not just 127.0.0.1 — verify the top of the range.
        let policy = resolve_client_security_policy(
            "http://127.255.255.255:8080/a2a/stream",
            &hardened_client_cfg(),
        );
        assert!(!policy.require_tls);
        assert!(!policy.ssrf_protection);
    }

    #[test]
    fn private_non_loopback_ips_do_not_bypass_ssrf_protection() {
        // SSRF protection must still apply to private-but-non-loopback ranges — the
        // carve-out is loopback-only, not "any local network address".
        for url in [
            "http://10.0.0.1:8080/a2a/stream",
            "http://192.168.1.1:8080/a2a/stream",
        ] {
            let policy = resolve_client_security_policy(url, &hardened_client_cfg());
            assert!(policy.require_tls, "expected require_tls for {url}");
            assert!(policy.ssrf_protection, "expected ssrf_protection for {url}");
        }
    }

    #[test]
    fn hostname_containing_localhost_as_substring_does_not_bypass() {
        // `is_loopback_host` compares the whole host against "localhost", not a substring
        // match — a lookalike hostname must not get the carve-out.
        let policy = resolve_client_security_policy(
            "http://notlocalhost.example.com/a2a/stream",
            &hardened_client_cfg(),
        );
        assert!(policy.require_tls);
        assert!(policy.ssrf_protection);
    }

    #[test]
    fn ipv4_mapped_ipv6_loopback_fails_closed_not_open() {
        // `::ffff:127.0.0.1` is not recognized as loopback by `is_loopback_host` (see
        // zeph_common::net tests) since `Ipv6Addr::is_loopback` doesn't unwrap IPv4-mapped
        // addresses. Confirm the resulting behavior is safe: it falls through to the
        // configured (hardened) client policy rather than silently bypassing security.
        let policy = resolve_client_security_policy(
            "http://[::ffff:127.0.0.1]:8080/a2a/stream",
            &hardened_client_cfg(),
        );
        assert!(policy.require_tls);
        assert!(policy.ssrf_protection);
    }

    // --- convert_card_trust_policy / convert_trusted_agent_keys (#6200) ---

    #[test]
    fn card_trust_policy_round_trip_covers_all_known_variants() {
        assert_eq!(
            convert_card_trust_policy(CardTrustPolicy::Ignore),
            zeph_a2a::CardTrustPolicy::Ignore
        );
        assert_eq!(
            convert_card_trust_policy(CardTrustPolicy::Prefer),
            zeph_a2a::CardTrustPolicy::Prefer
        );
        assert_eq!(
            convert_card_trust_policy(CardTrustPolicy::Require),
            zeph_a2a::CardTrustPolicy::Require
        );
    }

    #[test]
    fn trusted_agent_keys_recognized_alg_converts() {
        let keys = vec![TrustedAgentKey {
            kid: "key-1".into(),
            alg: "ES256".into(),
            jwk_or_pem: "pem-data".into(),
        }];
        let converted = convert_trusted_agent_keys(&keys);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].kid, "key-1");
        assert_eq!(converted[0].alg, zeph_a2a::SigAlg::Es256);
        assert_eq!(converted[0].key_material, "pem-data");
    }

    #[test]
    fn trusted_agent_keys_unsupported_alg_is_dropped() {
        let keys = vec![
            TrustedAgentKey {
                kid: "key-1".into(),
                alg: "ES256".into(),
                jwk_or_pem: "pem-data".into(),
            },
            TrustedAgentKey {
                kid: "key-2".into(),
                alg: "EdDSA".into(),
                jwk_or_pem: "pem-data-2".into(),
            },
        ];
        let converted = convert_trusted_agent_keys(&keys);
        assert_eq!(
            converted.len(),
            1,
            "the unsupported-alg key must be dropped, not carried through unmatchable"
        );
        assert_eq!(converted[0].kid, "key-1");
    }

    #[test]
    fn trusted_agent_keys_empty_input_converts_to_empty() {
        assert!(convert_trusted_agent_keys(&[]).is_empty());
    }

    // --- discovery_origin (#6200) ---

    #[test]
    fn discovery_origin_strips_rpc_path() {
        // Regression test: the well-known agent card is served at the origin root, not
        // relative to the `--connect` URL's RPC path — passing the full URL through
        // unchanged would make `discover` fetch `.../a2a/stream/.well-known/agent.json`,
        // which does not exist (see `crates/zeph-a2a/src/server/router.rs`'s route table).
        assert_eq!(
            discovery_origin("http://127.0.0.1:8080/a2a/stream"),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn discovery_origin_strips_path_and_query() {
        assert_eq!(
            discovery_origin("https://agent.example.com/a2a/stream?foo=bar"),
            "https://agent.example.com"
        );
    }

    #[test]
    fn discovery_origin_omits_default_port() {
        assert_eq!(
            discovery_origin("https://agent.example.com:443/a2a/stream"),
            "https://agent.example.com"
        );
    }

    #[test]
    fn discovery_origin_preserves_non_default_port() {
        assert_eq!(
            discovery_origin("https://agent.example.com:9443/a2a/stream"),
            "https://agent.example.com:9443"
        );
    }

    #[test]
    fn discovery_origin_falls_back_to_input_on_parse_failure() {
        assert_eq!(discovery_origin("not a url"), "not a url");
    }

    // --- discovery_error_is_fatal (S1, #6200) ---

    #[test]
    fn discovery_error_untrusted_card_is_always_fatal() {
        let err = zeph_a2a::A2aError::UntrustedCard {
            reason: "bad signature".into(),
        };
        for policy in [
            zeph_a2a::CardTrustPolicy::Ignore,
            zeph_a2a::CardTrustPolicy::Prefer,
            zeph_a2a::CardTrustPolicy::Require,
        ] {
            assert!(
                discovery_error_is_fatal(&err, policy),
                "UntrustedCard must stay fatal under {policy:?} — it's the policy's own verdict"
            );
        }
    }

    #[test]
    fn discovery_error_url_mismatch_is_always_fatal() {
        let err = zeph_a2a::A2aError::UrlMismatch {
            queried: "http://a".into(),
            advertised: "http://b".into(),
        };
        for policy in [
            zeph_a2a::CardTrustPolicy::Ignore,
            zeph_a2a::CardTrustPolicy::Prefer,
            zeph_a2a::CardTrustPolicy::Require,
        ] {
            assert!(discovery_error_is_fatal(&err, policy));
        }
    }

    #[test]
    fn discovery_error_fetch_failure_not_fatal_under_ignore_or_prefer() {
        let discovery_err = zeph_a2a::A2aError::Discovery {
            url: "http://peer.example.com/.well-known/agent.json".into(),
            reason: "HTTP 404".into(),
        };
        let timeout_err = zeph_a2a::A2aError::Timeout(std::time::Duration::from_secs(10));
        for err in [&discovery_err, &timeout_err] {
            assert!(
                !discovery_error_is_fatal(err, zeph_a2a::CardTrustPolicy::Ignore),
                "a card-less/unreachable peer must still connect under `ignore` (pre-#6200 \
                 behavior) — {err}"
            );
            assert!(
                !discovery_error_is_fatal(err, zeph_a2a::CardTrustPolicy::Prefer),
                "discovery is best-effort under `prefer` too — {err}"
            );
        }
    }

    #[test]
    fn discovery_error_fetch_failure_fatal_under_require() {
        let err = zeph_a2a::A2aError::Discovery {
            url: "http://peer.example.com/.well-known/agent.json".into(),
            reason: "HTTP 404".into(),
        };
        assert!(
            discovery_error_is_fatal(&err, zeph_a2a::CardTrustPolicy::Require),
            "require` cannot enforce a trust policy without a card, so a fetch failure must \
             still abort --connect"
        );
    }

    // --- hardened_discovery_client (#6200) ---

    #[tokio::test]
    async fn discovery_client_rejects_http_when_require_tls_set() {
        let security = zeph_a2a::SecurityPolicy {
            require_tls: true,
            ssrf_protection: false,
        };
        let result = hardened_discovery_client("http://agent.example.com/", security).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("require_tls"));
    }

    #[tokio::test]
    async fn discovery_client_permissive_loopback_builds_without_network() {
        // Mirrors the loopback carve-out `resolve_client_security_policy` already applies:
        // no TLS/SSRF check should require any network access for a permissive policy.
        let security = zeph_a2a::SecurityPolicy {
            require_tls: false,
            ssrf_protection: false,
        };
        let result = hardened_discovery_client("http://127.0.0.1:8080/a2a/stream", security).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn discovery_client_accepts_https_when_require_tls_set() {
        let security = zeph_a2a::SecurityPolicy {
            require_tls: true,
            ssrf_protection: false,
        };
        let result = hardened_discovery_client("https://agent.example.com/", security).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn discovery_client_ssrf_protection_rejects_loopback_target() {
        let security = zeph_a2a::SecurityPolicy {
            require_tls: false,
            ssrf_protection: true,
        };
        // Unlike the loopback carve-out in `resolve_client_security_policy` (which only
        // applies when the *policy* is computed for a loopback target), this directly
        // exercises `resolve_and_validate`'s own private-address rejection when SSRF
        // protection is explicitly requested against a loopback address.
        let result = hardened_discovery_client("http://127.0.0.1:8080/a2a/stream", security).await;
        assert!(result.is_err());
    }
}
