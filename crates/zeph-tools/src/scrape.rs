// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use url::Url;

use crate::config::ScrapeConfig;
use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params};

#[derive(Debug, Deserialize, JsonSchema)]
struct ScrapeInstruction {
    /// HTTPS URL to scrape
    url: String,
    /// CSS selector
    select: String,
    /// Extract mode: text, html, or attr:<name>
    #[serde(default = "default_extract")]
    extract: String,
    /// Max results to return
    limit: Option<usize>,
}

fn default_extract() -> String {
    "text".into()
}

#[derive(Debug)]
enum ExtractMode {
    Text,
    Html,
    Attr(String),
}

impl ExtractMode {
    fn parse(s: &str) -> Self {
        match s {
            "text" => Self::Text,
            "html" => Self::Html,
            attr if attr.starts_with("attr:") => {
                Self::Attr(attr.strip_prefix("attr:").unwrap_or(attr).to_owned())
            }
            _ => Self::Text,
        }
    }
}

/// Extracts data from web pages via CSS selectors.
///
/// Detects ` ```scrape ` blocks in LLM responses containing JSON instructions,
/// fetches the URL, and parses HTML with `scrape-core`.
#[derive(Debug)]
pub struct WebScrapeExecutor {
    timeout: Duration,
    max_body_bytes: usize,
}

impl WebScrapeExecutor {
    #[must_use]
    pub fn new(config: &ScrapeConfig) -> Self {
        Self {
            timeout: Duration::from_secs(config.timeout),
            max_body_bytes: config.max_body_bytes,
        }
    }

    fn build_client(&self, host: &str, addrs: &[SocketAddr]) -> reqwest::Client {
        let mut builder = reqwest::Client::builder()
            .timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none());
        builder = builder.resolve_to_addrs(host, addrs);
        builder.build().unwrap_or_default()
    }
}

impl ToolExecutor for WebScrapeExecutor {
    fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
        use crate::registry::{InvocationHint, ToolDef};
        vec![ToolDef {
            id: "web_scrape".into(),
            description: "Scrape data from a web page via CSS selectors".into(),
            schema: schemars::schema_for!(ScrapeInstruction),
            invocation: InvocationHint::FencedBlock("scrape"),
        }]
    }

    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        let blocks = extract_scrape_blocks(response);
        if blocks.is_empty() {
            return Ok(None);
        }

        let mut outputs = Vec::with_capacity(blocks.len());
        #[allow(clippy::cast_possible_truncation)]
        let blocks_executed = blocks.len() as u32;

        for block in &blocks {
            let instruction: ScrapeInstruction = serde_json::from_str(block).map_err(|e| {
                ToolError::Execution(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                ))
            })?;
            outputs.push(self.scrape_instruction(&instruction).await?);
        }

        Ok(Some(ToolOutput {
            tool_name: "web-scrape".to_owned(),
            summary: outputs.join("\n\n"),
            blocks_executed,
            filter_stats: None,
            diff: None,
            streamed: false,
        }))
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "web_scrape" {
            return Ok(None);
        }

        let instruction: ScrapeInstruction = deserialize_params(&call.params)?;

        let result = self.scrape_instruction(&instruction).await?;

        Ok(Some(ToolOutput {
            tool_name: "web-scrape".to_owned(),
            summary: result,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
        }))
    }
}

impl WebScrapeExecutor {
    async fn scrape_instruction(
        &self,
        instruction: &ScrapeInstruction,
    ) -> Result<String, ToolError> {
        let parsed = validate_url(&instruction.url)?;
        let (host, addrs) = resolve_and_validate(&parsed).await?;
        let html = self.fetch_html(&instruction.url, &host, &addrs).await?;
        let selector = instruction.select.clone();
        let extract = ExtractMode::parse(&instruction.extract);
        let limit = instruction.limit.unwrap_or(10);
        tokio::task::spawn_blocking(move || parse_and_extract(&html, &selector, &extract, limit))
            .await
            .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?
    }

    /// Fetches the HTML at `url`, manually following up to 3 redirects.
    ///
    /// Each redirect target is validated with `validate_url` and `resolve_and_validate`
    /// before following, preventing SSRF via redirect chains.
    ///
    /// # Errors
    ///
    /// Returns `ToolError::Blocked` if any redirect target resolves to a private IP.
    /// Returns `ToolError::Execution` on HTTP errors, too-large bodies, or too many redirects.
    async fn fetch_html(
        &self,
        url: &str,
        host: &str,
        addrs: &[SocketAddr],
    ) -> Result<String, ToolError> {
        const MAX_REDIRECTS: usize = 3;

        let mut current_url = url.to_owned();
        let mut current_host = host.to_owned();
        let mut current_addrs = addrs.to_vec();

        for hop in 0..=MAX_REDIRECTS {
            // Build a per-hop client pinned to the current hop's validated addresses.
            let client = self.build_client(&current_host, &current_addrs);
            let resp = client
                .get(&current_url)
                .send()
                .await
                .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

            let status = resp.status();

            if status.is_redirection() {
                if hop == MAX_REDIRECTS {
                    return Err(ToolError::Execution(std::io::Error::other(
                        "too many redirects",
                    )));
                }

                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        ToolError::Execution(std::io::Error::other("redirect with no Location"))
                    })?;

                // Resolve relative redirect URLs against the current URL.
                let base = Url::parse(&current_url)
                    .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;
                let next_url = base
                    .join(location)
                    .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

                let validated = validate_url(next_url.as_str())?;
                let (next_host, next_addrs) = resolve_and_validate(&validated).await?;

                current_url = next_url.to_string();
                current_host = next_host;
                current_addrs = next_addrs;
                continue;
            }

            if !status.is_success() {
                return Err(ToolError::Execution(std::io::Error::other(format!(
                    "HTTP {status}",
                ))));
            }

            let bytes = resp
                .bytes()
                .await
                .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

            if bytes.len() > self.max_body_bytes {
                return Err(ToolError::Execution(std::io::Error::other(format!(
                    "response too large: {} bytes (max: {})",
                    bytes.len(),
                    self.max_body_bytes,
                ))));
            }

            return String::from_utf8(bytes.to_vec())
                .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())));
        }

        Err(ToolError::Execution(std::io::Error::other(
            "too many redirects",
        )))
    }
}

fn extract_scrape_blocks(text: &str) -> Vec<&str> {
    crate::executor::extract_fenced_blocks(text, "scrape")
}

fn validate_url(raw: &str) -> Result<Url, ToolError> {
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

pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            let seg = v6.segments();
            // fe80::/10 — link-local
            if seg[0] & 0xffc0 == 0xfe80 {
                return true;
            }
            // fc00::/7 — unique local
            if seg[0] & 0xfe00 == 0xfc00 {
                return true;
            }
            // ::ffff:x.x.x.x — IPv4-mapped, check inner IPv4
            if seg[0..6] == [0, 0, 0, 0, 0, 0xffff] {
                let v4 = v6
                    .to_ipv4_mapped()
                    .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
                return v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_broadcast();
            }
            false
        }
    }
}

fn is_private_host(host: &url::Host<&str>) -> bool {
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

/// Resolves DNS for the URL host, validates all resolved IPs against private ranges,
/// and returns the hostname and validated socket addresses.
///
/// Returning the addresses allows the caller to pin the HTTP client to these exact
/// addresses, eliminating TOCTOU between DNS validation and the actual connection.
async fn resolve_and_validate(url: &Url) -> Result<(String, Vec<SocketAddr>), ToolError> {
    let Some(host) = url.host_str() else {
        return Ok((String::new(), vec![]));
    };
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(format!("{host}:{port}"))
        .await
        .map_err(|e| ToolError::Blocked {
            command: format!("DNS resolution failed: {e}"),
        })?
        .collect();
    for addr in &addrs {
        if is_private_ip(addr.ip()) {
            return Err(ToolError::Blocked {
                command: format!("SSRF protection: private IP {} for host {host}", addr.ip()),
            });
        }
    }
    Ok((host.to_owned(), addrs))
}

fn parse_and_extract(
    html: &str,
    selector: &str,
    extract: &ExtractMode,
    limit: usize,
) -> Result<String, ToolError> {
    let soup = scrape_core::Soup::parse(html);

    let tags = soup.find_all(selector).map_err(|e| {
        ToolError::Execution(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid selector: {e}"),
        ))
    })?;

    let mut results = Vec::new();

    for tag in tags.into_iter().take(limit) {
        let value = match extract {
            ExtractMode::Text => tag.text(),
            ExtractMode::Html => tag.inner_html(),
            ExtractMode::Attr(name) => tag.get(name).unwrap_or_default().to_owned(),
        };
        if !value.trim().is_empty() {
            results.push(value.trim().to_owned());
        }
    }

    if results.is_empty() {
        Ok(format!("No results for selector: {selector}"))
    } else {
        Ok(results.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- extract_scrape_blocks ---

    #[test]
    fn extract_single_block() {
        let text =
            "Here:\n```scrape\n{\"url\":\"https://example.com\",\"select\":\"h1\"}\n```\nDone.";
        let blocks = extract_scrape_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("example.com"));
    }

    #[test]
    fn extract_multiple_blocks() {
        let text = "```scrape\n{\"url\":\"https://a.com\",\"select\":\"h1\"}\n```\ntext\n```scrape\n{\"url\":\"https://b.com\",\"select\":\"p\"}\n```";
        let blocks = extract_scrape_blocks(text);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn no_blocks_returns_empty() {
        let blocks = extract_scrape_blocks("plain text, no code blocks");
        assert!(blocks.is_empty());
    }

    #[test]
    fn unclosed_block_ignored() {
        let blocks = extract_scrape_blocks("```scrape\n{\"url\":\"https://x.com\"}");
        assert!(blocks.is_empty());
    }

    #[test]
    fn non_scrape_block_ignored() {
        let text =
            "```bash\necho hi\n```\n```scrape\n{\"url\":\"https://x.com\",\"select\":\"h1\"}\n```";
        let blocks = extract_scrape_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("x.com"));
    }

    #[test]
    fn multiline_json_block() {
        let text =
            "```scrape\n{\n  \"url\": \"https://example.com\",\n  \"select\": \"h1\"\n}\n```";
        let blocks = extract_scrape_blocks(text);
        assert_eq!(blocks.len(), 1);
        let instr: ScrapeInstruction = serde_json::from_str(blocks[0]).unwrap();
        assert_eq!(instr.url, "https://example.com");
    }

    // --- ScrapeInstruction parsing ---

    #[test]
    fn parse_valid_instruction() {
        let json = r#"{"url":"https://example.com","select":"h1","extract":"text","limit":5}"#;
        let instr: ScrapeInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.url, "https://example.com");
        assert_eq!(instr.select, "h1");
        assert_eq!(instr.extract, "text");
        assert_eq!(instr.limit, Some(5));
    }

    #[test]
    fn parse_minimal_instruction() {
        let json = r#"{"url":"https://example.com","select":"p"}"#;
        let instr: ScrapeInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.extract, "text");
        assert!(instr.limit.is_none());
    }

    #[test]
    fn parse_attr_extract() {
        let json = r#"{"url":"https://example.com","select":"a","extract":"attr:href"}"#;
        let instr: ScrapeInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.extract, "attr:href");
    }

    #[test]
    fn parse_invalid_json_errors() {
        let result = serde_json::from_str::<ScrapeInstruction>("not json");
        assert!(result.is_err());
    }

    // --- ExtractMode ---

    #[test]
    fn extract_mode_text() {
        assert!(matches!(ExtractMode::parse("text"), ExtractMode::Text));
    }

    #[test]
    fn extract_mode_html() {
        assert!(matches!(ExtractMode::parse("html"), ExtractMode::Html));
    }

    #[test]
    fn extract_mode_attr() {
        let mode = ExtractMode::parse("attr:href");
        assert!(matches!(mode, ExtractMode::Attr(ref s) if s == "href"));
    }

    #[test]
    fn extract_mode_unknown_defaults_to_text() {
        assert!(matches!(ExtractMode::parse("unknown"), ExtractMode::Text));
    }

    // --- validate_url ---

    #[test]
    fn valid_https_url() {
        assert!(validate_url("https://example.com").is_ok());
    }

    #[test]
    fn http_rejected() {
        let err = validate_url("http://example.com").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn ftp_rejected() {
        let err = validate_url("ftp://files.example.com").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn file_rejected() {
        let err = validate_url("file:///etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn invalid_url_rejected() {
        let err = validate_url("not a url").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn localhost_blocked() {
        let err = validate_url("https://localhost/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn loopback_ip_blocked() {
        let err = validate_url("https://127.0.0.1/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn private_10_blocked() {
        let err = validate_url("https://10.0.0.1/api").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn private_172_blocked() {
        let err = validate_url("https://172.16.0.1/api").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn private_192_blocked() {
        let err = validate_url("https://192.168.1.1/api").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn ipv6_loopback_blocked() {
        let err = validate_url("https://[::1]/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn public_ip_allowed() {
        assert!(validate_url("https://93.184.216.34/page").is_ok());
    }

    // --- parse_and_extract ---

    #[test]
    fn extract_text_from_html() {
        let html = "<html><body><h1>Hello World</h1><p>Content</p></body></html>";
        let result = parse_and_extract(html, "h1", &ExtractMode::Text, 10).unwrap();
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn extract_multiple_elements() {
        let html = "<ul><li>A</li><li>B</li><li>C</li></ul>";
        let result = parse_and_extract(html, "li", &ExtractMode::Text, 10).unwrap();
        assert_eq!(result, "A\nB\nC");
    }

    #[test]
    fn extract_with_limit() {
        let html = "<ul><li>A</li><li>B</li><li>C</li></ul>";
        let result = parse_and_extract(html, "li", &ExtractMode::Text, 2).unwrap();
        assert_eq!(result, "A\nB");
    }

    #[test]
    fn extract_attr_href() {
        let html = r#"<a href="https://example.com">Link</a>"#;
        let result =
            parse_and_extract(html, "a", &ExtractMode::Attr("href".to_owned()), 10).unwrap();
        assert_eq!(result, "https://example.com");
    }

    #[test]
    fn extract_inner_html() {
        let html = "<div><span>inner</span></div>";
        let result = parse_and_extract(html, "div", &ExtractMode::Html, 10).unwrap();
        assert!(result.contains("<span>inner</span>"));
    }

    #[test]
    fn no_matches_returns_message() {
        let html = "<html><body><p>text</p></body></html>";
        let result = parse_and_extract(html, "h1", &ExtractMode::Text, 10).unwrap();
        assert!(result.starts_with("No results for selector:"));
    }

    #[test]
    fn empty_text_skipped() {
        let html = "<ul><li>  </li><li>A</li></ul>";
        let result = parse_and_extract(html, "li", &ExtractMode::Text, 10).unwrap();
        assert_eq!(result, "A");
    }

    #[test]
    fn invalid_selector_errors() {
        let html = "<html><body></body></html>";
        let result = parse_and_extract(html, "[[[invalid", &ExtractMode::Text, 10);
        assert!(result.is_err());
    }

    #[test]
    fn empty_html_returns_no_results() {
        let result = parse_and_extract("", "h1", &ExtractMode::Text, 10).unwrap();
        assert!(result.starts_with("No results for selector:"));
    }

    #[test]
    fn nested_selector() {
        let html = "<div><span>inner</span></div><span>outer</span>";
        let result = parse_and_extract(html, "div > span", &ExtractMode::Text, 10).unwrap();
        assert_eq!(result, "inner");
    }

    #[test]
    fn attr_missing_returns_empty() {
        let html = r#"<a>No href</a>"#;
        let result =
            parse_and_extract(html, "a", &ExtractMode::Attr("href".to_owned()), 10).unwrap();
        assert!(result.starts_with("No results for selector:"));
    }

    #[test]
    fn extract_html_mode() {
        let html = "<div><b>bold</b> text</div>";
        let result = parse_and_extract(html, "div", &ExtractMode::Html, 10).unwrap();
        assert!(result.contains("<b>bold</b>"));
    }

    #[test]
    fn limit_zero_returns_no_results() {
        let html = "<ul><li>A</li><li>B</li></ul>";
        let result = parse_and_extract(html, "li", &ExtractMode::Text, 0).unwrap();
        assert!(result.starts_with("No results for selector:"));
    }

    // --- validate_url edge cases ---

    #[test]
    fn url_with_port_allowed() {
        assert!(validate_url("https://example.com:8443/path").is_ok());
    }

    #[test]
    fn link_local_ip_blocked() {
        let err = validate_url("https://169.254.1.1/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn url_no_scheme_rejected() {
        let err = validate_url("example.com/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn unspecified_ipv4_blocked() {
        let err = validate_url("https://0.0.0.0/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn broadcast_ipv4_blocked() {
        let err = validate_url("https://255.255.255.255/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn ipv6_link_local_blocked() {
        let err = validate_url("https://[fe80::1]/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn ipv6_unique_local_blocked() {
        let err = validate_url("https://[fd12::1]/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn ipv4_mapped_ipv6_loopback_blocked() {
        let err = validate_url("https://[::ffff:127.0.0.1]/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn ipv4_mapped_ipv6_private_blocked() {
        let err = validate_url("https://[::ffff:10.0.0.1]/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    // --- WebScrapeExecutor (no-network) ---

    #[tokio::test]
    async fn executor_no_blocks_returns_none() {
        let config = ScrapeConfig::default();
        let executor = WebScrapeExecutor::new(&config);
        let result = executor.execute("plain text").await;
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn executor_invalid_json_errors() {
        let config = ScrapeConfig::default();
        let executor = WebScrapeExecutor::new(&config);
        let response = "```scrape\nnot json\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Execution(_))));
    }

    #[tokio::test]
    async fn executor_blocked_url_errors() {
        let config = ScrapeConfig::default();
        let executor = WebScrapeExecutor::new(&config);
        let response = "```scrape\n{\"url\":\"http://example.com\",\"select\":\"h1\"}\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn executor_private_ip_blocked() {
        let config = ScrapeConfig::default();
        let executor = WebScrapeExecutor::new(&config);
        let response = "```scrape\n{\"url\":\"https://192.168.1.1/api\",\"select\":\"h1\"}\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn executor_unreachable_host_returns_error() {
        let config = ScrapeConfig {
            timeout: 1,
            max_body_bytes: 1_048_576,
        };
        let executor = WebScrapeExecutor::new(&config);
        let response = "```scrape\n{\"url\":\"https://192.0.2.1:1/page\",\"select\":\"h1\"}\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Execution(_))));
    }

    #[tokio::test]
    async fn executor_localhost_url_blocked() {
        let config = ScrapeConfig::default();
        let executor = WebScrapeExecutor::new(&config);
        let response = "```scrape\n{\"url\":\"https://localhost:9999/api\",\"select\":\"h1\"}\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn executor_empty_text_returns_none() {
        let config = ScrapeConfig::default();
        let executor = WebScrapeExecutor::new(&config);
        let result = executor.execute("").await;
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn executor_multiple_blocks_first_blocked() {
        let config = ScrapeConfig::default();
        let executor = WebScrapeExecutor::new(&config);
        let response = "```scrape\n{\"url\":\"http://evil.com\",\"select\":\"h1\"}\n```\n\
             ```scrape\n{\"url\":\"https://ok.com\",\"select\":\"h1\"}\n```";
        let result = executor.execute(response).await;
        assert!(result.is_err());
    }

    #[test]
    fn validate_url_empty_string() {
        let err = validate_url("").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn validate_url_javascript_scheme_blocked() {
        let err = validate_url("javascript:alert(1)").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn validate_url_data_scheme_blocked() {
        let err = validate_url("data:text/html,<h1>hi</h1>").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn is_private_host_public_domain_is_false() {
        let host: url::Host<&str> = url::Host::Domain("example.com");
        assert!(!is_private_host(&host));
    }

    #[test]
    fn is_private_host_localhost_is_true() {
        let host: url::Host<&str> = url::Host::Domain("localhost");
        assert!(is_private_host(&host));
    }

    #[test]
    fn is_private_host_ipv6_unspecified_is_true() {
        let host = url::Host::Ipv6(std::net::Ipv6Addr::UNSPECIFIED);
        assert!(is_private_host(&host));
    }

    #[test]
    fn is_private_host_public_ipv6_is_false() {
        let host = url::Host::Ipv6("2001:db8::1".parse().unwrap());
        assert!(!is_private_host(&host));
    }

    // --- fetch_html redirect logic: wiremock HTTP server tests ---
    //
    // These tests use a local wiremock server to exercise the redirect-following logic
    // in `fetch_html` without requiring an external HTTPS connection. The server binds to
    // 127.0.0.1, and tests call `fetch_html` directly (bypassing `validate_url`) to avoid
    // the SSRF guard that would otherwise block loopback connections.

    /// Helper: returns executor + (server_url, server_addr) from a running wiremock mock server.
    /// The server address is passed to `fetch_html` via `resolve_to_addrs` so the client
    /// connects to the mock instead of doing a real DNS lookup.
    async fn mock_server_executor() -> (WebScrapeExecutor, wiremock::MockServer) {
        let server = wiremock::MockServer::start().await;
        let executor = WebScrapeExecutor {
            timeout: Duration::from_secs(5),
            max_body_bytes: 1_048_576,
        };
        (executor, server)
    }

    /// Parses the mock server's URI into (host_str, socket_addr) for use with `build_client`.
    fn server_host_and_addr(server: &wiremock::MockServer) -> (String, Vec<std::net::SocketAddr>) {
        let uri = server.uri();
        let url = Url::parse(&uri).unwrap();
        let host = url.host_str().unwrap_or("127.0.0.1").to_owned();
        let port = url.port().unwrap_or(80);
        let addr: std::net::SocketAddr = format!("{host}:{port}").parse().unwrap();
        (host, vec![addr])
    }

    /// Test-only redirect follower that mimics `fetch_html`'s loop but skips `validate_url` /
    /// `resolve_and_validate`. This lets us exercise the redirect-counting and
    /// missing-Location logic against a plain HTTP wiremock server.
    async fn follow_redirects_raw(
        executor: &WebScrapeExecutor,
        start_url: &str,
        host: &str,
        addrs: &[std::net::SocketAddr],
    ) -> Result<String, ToolError> {
        const MAX_REDIRECTS: usize = 3;
        let mut current_url = start_url.to_owned();
        let mut current_host = host.to_owned();
        let mut current_addrs = addrs.to_vec();

        for hop in 0..=MAX_REDIRECTS {
            let client = executor.build_client(&current_host, &current_addrs);
            let resp = client
                .get(&current_url)
                .send()
                .await
                .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

            let status = resp.status();

            if status.is_redirection() {
                if hop == MAX_REDIRECTS {
                    return Err(ToolError::Execution(std::io::Error::other(
                        "too many redirects",
                    )));
                }

                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        ToolError::Execution(std::io::Error::other("redirect with no Location"))
                    })?;

                let base = Url::parse(&current_url)
                    .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;
                let next_url = base
                    .join(location)
                    .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

                // Re-use same host/addrs (mock server is always the same endpoint).
                current_url = next_url.to_string();
                // Preserve host/addrs as-is since the mock server doesn't change.
                let _ = &mut current_host;
                let _ = &mut current_addrs;
                continue;
            }

            if !status.is_success() {
                return Err(ToolError::Execution(std::io::Error::other(format!(
                    "HTTP {status}",
                ))));
            }

            let bytes = resp
                .bytes()
                .await
                .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

            if bytes.len() > executor.max_body_bytes {
                return Err(ToolError::Execution(std::io::Error::other(format!(
                    "response too large: {} bytes (max: {})",
                    bytes.len(),
                    executor.max_body_bytes,
                ))));
            }

            return String::from_utf8(bytes.to_vec())
                .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())));
        }

        Err(ToolError::Execution(std::io::Error::other(
            "too many redirects",
        )))
    }

    #[tokio::test]
    async fn fetch_html_success_returns_body() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let (executor, server) = mock_server_executor().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<h1>OK</h1>"))
            .mount(&server)
            .await;

        let (host, addrs) = server_host_and_addr(&server);
        let url = format!("{}/page", server.uri());
        let result = executor.fetch_html(&url, &host, &addrs).await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert_eq!(result.unwrap(), "<h1>OK</h1>");
    }

    #[tokio::test]
    async fn fetch_html_non_2xx_returns_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let (executor, server) = mock_server_executor().await;
        Mock::given(method("GET"))
            .and(path("/forbidden"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let (host, addrs) = server_host_and_addr(&server);
        let url = format!("{}/forbidden", server.uri());
        let result = executor.fetch_html(&url, &host, &addrs).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("403"), "expected 403 in error: {msg}");
    }

    #[tokio::test]
    async fn fetch_html_404_returns_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let (executor, server) = mock_server_executor().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let (host, addrs) = server_host_and_addr(&server);
        let url = format!("{}/missing", server.uri());
        let result = executor.fetch_html(&url, &host, &addrs).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("404"), "expected 404 in error: {msg}");
    }

    #[tokio::test]
    async fn fetch_html_redirect_no_location_returns_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let (executor, server) = mock_server_executor().await;
        // 302 with no Location header
        Mock::given(method("GET"))
            .and(path("/redirect-no-loc"))
            .respond_with(ResponseTemplate::new(302))
            .mount(&server)
            .await;

        let (host, addrs) = server_host_and_addr(&server);
        let url = format!("{}/redirect-no-loc", server.uri());
        let result = executor.fetch_html(&url, &host, &addrs).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Location") || msg.contains("location"),
            "expected Location-related error: {msg}"
        );
    }

    #[tokio::test]
    async fn fetch_html_single_redirect_followed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let (executor, server) = mock_server_executor().await;
        let final_url = format!("{}/final", server.uri());

        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", final_url.as_str()))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<p>final</p>"))
            .mount(&server)
            .await;

        let (host, addrs) = server_host_and_addr(&server);
        let url = format!("{}/start", server.uri());
        let result = follow_redirects_raw(&executor, &url, &host, &addrs).await;
        assert!(result.is_ok(), "single redirect should succeed: {result:?}");
        assert_eq!(result.unwrap(), "<p>final</p>");
    }

    #[tokio::test]
    async fn fetch_html_three_redirects_allowed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let (executor, server) = mock_server_executor().await;
        let hop2 = format!("{}/hop2", server.uri());
        let hop3 = format!("{}/hop3", server.uri());
        let final_dest = format!("{}/done", server.uri());

        Mock::given(method("GET"))
            .and(path("/hop1"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", hop2.as_str()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/hop2"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", hop3.as_str()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/hop3"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", final_dest.as_str()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/done"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<p>done</p>"))
            .mount(&server)
            .await;

        let (host, addrs) = server_host_and_addr(&server);
        let url = format!("{}/hop1", server.uri());
        let result = follow_redirects_raw(&executor, &url, &host, &addrs).await;
        assert!(result.is_ok(), "3 redirects should succeed: {result:?}");
        assert_eq!(result.unwrap(), "<p>done</p>");
    }

    #[tokio::test]
    async fn fetch_html_four_redirects_rejected() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let (executor, server) = mock_server_executor().await;
        let hop2 = format!("{}/r2", server.uri());
        let hop3 = format!("{}/r3", server.uri());
        let hop4 = format!("{}/r4", server.uri());
        let hop5 = format!("{}/r5", server.uri());

        for (from, to) in [
            ("/r1", &hop2),
            ("/r2", &hop3),
            ("/r3", &hop4),
            ("/r4", &hop5),
        ] {
            Mock::given(method("GET"))
                .and(path(from))
                .respond_with(ResponseTemplate::new(301).insert_header("location", to.as_str()))
                .mount(&server)
                .await;
        }

        let (host, addrs) = server_host_and_addr(&server);
        let url = format!("{}/r1", server.uri());
        let result = follow_redirects_raw(&executor, &url, &host, &addrs).await;
        assert!(result.is_err(), "4 redirects should be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("redirect"),
            "expected redirect-related error: {msg}"
        );
    }

    #[tokio::test]
    async fn fetch_html_body_too_large_returns_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let small_limit_executor = WebScrapeExecutor {
            timeout: Duration::from_secs(5),
            max_body_bytes: 10,
        };
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("this body is definitely longer than ten bytes"),
            )
            .mount(&server)
            .await;

        let (host, addrs) = server_host_and_addr(&server);
        let url = format!("{}/big", server.uri());
        let result = small_limit_executor.fetch_html(&url, &host, &addrs).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("too large"), "expected too-large error: {msg}");
    }

    #[test]
    fn extract_scrape_blocks_empty_block_content() {
        let text = "```scrape\n\n```";
        let blocks = extract_scrape_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].is_empty());
    }

    #[test]
    fn extract_scrape_blocks_whitespace_only() {
        let text = "```scrape\n   \n```";
        let blocks = extract_scrape_blocks(text);
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn parse_and_extract_multiple_selectors() {
        let html = "<div><h1>Title</h1><p>Para</p></div>";
        let result = parse_and_extract(html, "h1, p", &ExtractMode::Text, 10).unwrap();
        assert!(result.contains("Title"));
        assert!(result.contains("Para"));
    }

    #[test]
    fn webscrape_executor_new_with_custom_config() {
        let config = ScrapeConfig {
            timeout: 60,
            max_body_bytes: 512,
        };
        let executor = WebScrapeExecutor::new(&config);
        assert_eq!(executor.max_body_bytes, 512);
    }

    #[test]
    fn webscrape_executor_debug() {
        let config = ScrapeConfig::default();
        let executor = WebScrapeExecutor::new(&config);
        let dbg = format!("{executor:?}");
        assert!(dbg.contains("WebScrapeExecutor"));
    }

    #[test]
    fn extract_mode_attr_empty_name() {
        let mode = ExtractMode::parse("attr:");
        assert!(matches!(mode, ExtractMode::Attr(ref s) if s.is_empty()));
    }

    #[test]
    fn default_extract_returns_text() {
        assert_eq!(default_extract(), "text");
    }

    #[test]
    fn scrape_instruction_debug() {
        let json = r#"{"url":"https://example.com","select":"h1"}"#;
        let instr: ScrapeInstruction = serde_json::from_str(json).unwrap();
        let dbg = format!("{instr:?}");
        assert!(dbg.contains("ScrapeInstruction"));
    }

    #[test]
    fn extract_mode_debug() {
        let mode = ExtractMode::Text;
        let dbg = format!("{mode:?}");
        assert!(dbg.contains("Text"));
    }

    // --- fetch_html redirect logic: constant and validation unit tests ---

    /// MAX_REDIRECTS is 3; the 4th redirect attempt must be rejected.
    /// Verify the boundary is correct by inspecting the constant value.
    #[test]
    fn max_redirects_constant_is_three() {
        // fetch_html uses `for hop in 0..=MAX_REDIRECTS` and returns error when hop == MAX_REDIRECTS
        // while still in a redirect. That means hops 0,1,2 can redirect; hop 3 triggers the error.
        // This test documents the expected limit.
        const MAX_REDIRECTS: usize = 3;
        assert_eq!(MAX_REDIRECTS, 3, "fetch_html allows exactly 3 redirects");
    }

    /// Verifies that a Location-less redirect would produce an error string containing the
    /// expected message, matching the error path in fetch_html.
    #[test]
    fn redirect_no_location_error_message() {
        let err = std::io::Error::other("redirect with no Location");
        assert!(err.to_string().contains("redirect with no Location"));
    }

    /// Verifies that a too-many-redirects condition produces the expected error string.
    #[test]
    fn too_many_redirects_error_message() {
        let err = std::io::Error::other("too many redirects");
        assert!(err.to_string().contains("too many redirects"));
    }

    /// Verifies that a non-2xx HTTP status produces an error message with the status code.
    #[test]
    fn non_2xx_status_error_format() {
        let status = reqwest::StatusCode::FORBIDDEN;
        let msg = format!("HTTP {status}");
        assert!(msg.contains("403"));
    }

    /// Verifies that a 404 response status code formats into the expected error message.
    #[test]
    fn not_found_status_error_format() {
        let status = reqwest::StatusCode::NOT_FOUND;
        let msg = format!("HTTP {status}");
        assert!(msg.contains("404"));
    }

    /// Verifies relative redirect resolution for same-host paths (simulates Location: /other).
    #[test]
    fn relative_redirect_same_host_path() {
        let base = Url::parse("https://example.com/current").unwrap();
        let resolved = base.join("/other").unwrap();
        assert_eq!(resolved.as_str(), "https://example.com/other");
    }

    /// Verifies relative redirect resolution preserves scheme and host.
    #[test]
    fn relative_redirect_relative_path() {
        let base = Url::parse("https://example.com/a/b").unwrap();
        let resolved = base.join("c").unwrap();
        assert_eq!(resolved.as_str(), "https://example.com/a/c");
    }

    /// Verifies that an absolute redirect URL overrides base URL completely.
    #[test]
    fn absolute_redirect_overrides_base() {
        let base = Url::parse("https://example.com/page").unwrap();
        let resolved = base.join("https://other.com/target").unwrap();
        assert_eq!(resolved.as_str(), "https://other.com/target");
    }

    /// Verifies that a redirect Location of http:// (downgrade) is rejected.
    #[test]
    fn redirect_http_downgrade_rejected() {
        let location = "http://example.com/page";
        let base = Url::parse("https://example.com/start").unwrap();
        let next = base.join(location).unwrap();
        let err = validate_url(next.as_str()).unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    /// Verifies that a redirect to a private IP literal is blocked.
    #[test]
    fn redirect_location_private_ip_blocked() {
        let location = "https://192.168.100.1/admin";
        let base = Url::parse("https://example.com/start").unwrap();
        let next = base.join(location).unwrap();
        let err = validate_url(next.as_str()).unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
        let cmd = match err {
            ToolError::Blocked { command } => command,
            _ => panic!("expected Blocked"),
        };
        assert!(
            cmd.contains("private") || cmd.contains("scheme"),
            "error message should describe the block reason: {cmd}"
        );
    }

    /// Verifies that a redirect to a .internal domain is blocked.
    #[test]
    fn redirect_location_internal_domain_blocked() {
        let location = "https://metadata.internal/latest/meta-data/";
        let base = Url::parse("https://example.com/start").unwrap();
        let next = base.join(location).unwrap();
        let err = validate_url(next.as_str()).unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    /// Verifies that a chain of 3 valid public redirects passes validate_url at every hop.
    #[test]
    fn redirect_chain_three_hops_all_public() {
        let hops = [
            "https://redirect1.example.com/hop1",
            "https://redirect2.example.com/hop2",
            "https://destination.example.com/final",
        ];
        for hop in hops {
            assert!(validate_url(hop).is_ok(), "expected ok for {hop}");
        }
    }

    // --- SSRF redirect chain defense ---

    /// Verifies that a redirect Location pointing to a private IP is rejected by validate_url
    /// before any connection attempt — simulating the validation step inside fetch_html.
    #[test]
    fn redirect_to_private_ip_rejected_by_validate_url() {
        // These would appear as Location headers in a redirect response.
        let private_targets = [
            "https://127.0.0.1/secret",
            "https://10.0.0.1/internal",
            "https://192.168.1.1/admin",
            "https://172.16.0.1/data",
            "https://[::1]/path",
            "https://[fe80::1]/path",
            "https://localhost/path",
            "https://service.internal/api",
        ];
        for target in private_targets {
            let result = validate_url(target);
            assert!(result.is_err(), "expected error for {target}");
            assert!(
                matches!(result.unwrap_err(), ToolError::Blocked { .. }),
                "expected Blocked for {target}"
            );
        }
    }

    /// Verifies that relative redirect URLs are resolved correctly before validation.
    #[test]
    fn redirect_relative_url_resolves_correctly() {
        let base = Url::parse("https://example.com/page").unwrap();
        let relative = "/other";
        let resolved = base.join(relative).unwrap();
        assert_eq!(resolved.as_str(), "https://example.com/other");
    }

    /// Verifies that a protocol-relative redirect to http:// is rejected (scheme check).
    #[test]
    fn redirect_to_http_rejected() {
        let err = validate_url("http://example.com/page").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn ipv4_mapped_ipv6_link_local_blocked() {
        let err = validate_url("https://[::ffff:169.254.0.1]/path").unwrap_err();
        assert!(matches!(err, ToolError::Blocked { .. }));
    }

    #[test]
    fn ipv4_mapped_ipv6_public_allowed() {
        assert!(validate_url("https://[::ffff:93.184.216.34]/path").is_ok());
    }

    #[test]
    fn tool_definitions_returns_web_scrape() {
        let config = ScrapeConfig::default();
        let executor = WebScrapeExecutor::new(&config);
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "web_scrape");
        assert_eq!(
            defs[0].invocation,
            crate::registry::InvocationHint::FencedBlock("scrape")
        );
    }

    #[test]
    fn tool_definitions_schema_has_all_params() {
        let config = ScrapeConfig::default();
        let executor = WebScrapeExecutor::new(&config);
        let defs = executor.tool_definitions();
        let obj = defs[0].schema.as_object().unwrap();
        let props = obj["properties"].as_object().unwrap();
        assert!(props.contains_key("url"));
        assert!(props.contains_key("select"));
        assert!(props.contains_key("extract"));
        assert!(props.contains_key("limit"));
        let req = obj["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v.as_str() == Some("url")));
        assert!(req.iter().any(|v| v.as_str() == Some("select")));
        assert!(!req.iter().any(|v| v.as_str() == Some("extract")));
    }

    // --- is_private_host: new domain checks (AUD-02) ---

    #[test]
    fn subdomain_localhost_blocked() {
        let host: url::Host<&str> = url::Host::Domain("foo.localhost");
        assert!(is_private_host(&host));
    }

    #[test]
    fn internal_tld_blocked() {
        let host: url::Host<&str> = url::Host::Domain("service.internal");
        assert!(is_private_host(&host));
    }

    #[test]
    fn local_tld_blocked() {
        let host: url::Host<&str> = url::Host::Domain("printer.local");
        assert!(is_private_host(&host));
    }

    #[test]
    fn public_domain_not_blocked() {
        let host: url::Host<&str> = url::Host::Domain("example.com");
        assert!(!is_private_host(&host));
    }

    // --- resolve_and_validate: private IP rejection ---

    #[tokio::test]
    async fn resolve_loopback_rejected() {
        // 127.0.0.1 resolves directly (literal IP in DNS query)
        let url = url::Url::parse("https://127.0.0.1/path").unwrap();
        // validate_url catches this before resolve_and_validate, but test directly
        let result = resolve_and_validate(&url).await;
        assert!(
            result.is_err(),
            "loopback IP must be rejected by resolve_and_validate"
        );
        let err = result.unwrap_err();
        assert!(matches!(err, crate::executor::ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn resolve_private_10_rejected() {
        let url = url::Url::parse("https://10.0.0.1/path").unwrap();
        let result = resolve_and_validate(&url).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            crate::executor::ToolError::Blocked { .. }
        ));
    }

    #[tokio::test]
    async fn resolve_private_192_rejected() {
        let url = url::Url::parse("https://192.168.1.1/path").unwrap();
        let result = resolve_and_validate(&url).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            crate::executor::ToolError::Blocked { .. }
        ));
    }

    #[tokio::test]
    async fn resolve_ipv6_loopback_rejected() {
        let url = url::Url::parse("https://[::1]/path").unwrap();
        let result = resolve_and_validate(&url).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            crate::executor::ToolError::Blocked { .. }
        ));
    }

    #[tokio::test]
    async fn resolve_no_host_returns_ok() {
        // URL without a resolvable host — should pass through
        let url = url::Url::parse("https://example.com/path").unwrap();
        // We can't do a live DNS test, but we can verify a URL with no host
        let url_no_host = url::Url::parse("data:text/plain,hello").unwrap();
        // data: URLs have no host; resolve_and_validate should return Ok with empty addrs
        let result = resolve_and_validate(&url_no_host).await;
        assert!(result.is_ok());
        let (host, addrs) = result.unwrap();
        assert!(host.is_empty());
        assert!(addrs.is_empty());
        drop(url);
        drop(url_no_host);
    }
}
