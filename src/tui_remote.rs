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
    let client =
        zeph_a2a::A2aClient::new(zeph_core::http::default_client()).with_security(security);

    // Cloned before `url` is moved into the `async move` SSE pump block below.
    let remote_daemon_url = url.clone();

    // user_tx is passed to App; App sends user text through it.
    // We receive on user_rx and forward to the A2A SSE pump.
    let (user_tx, mut user_rx) = tokio::sync::mpsc::channel::<String>(32);
    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<zeph_tui::AgentEvent>(256);

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
    use super::resolve_client_security_policy;
    use zeph_core::config::A2aClientConfig;

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
}
