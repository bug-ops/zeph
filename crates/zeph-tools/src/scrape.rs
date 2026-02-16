use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use url::Url;

use crate::config::ScrapeConfig;
use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};

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
    client: reqwest::Client,
    max_body_bytes: usize,
}

impl WebScrapeExecutor {
    #[must_use]
    pub fn new(config: &ScrapeConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .unwrap_or_default();

        Self {
            client,
            max_body_bytes: config.max_body_bytes,
        }
    }
}

impl ToolExecutor for WebScrapeExecutor {
    fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
        use crate::registry::{InvocationHint, ToolDef};
        vec![ToolDef {
            id: "web_scrape",
            description: "Scrape data from a web page via CSS selectors",
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
        }))
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "web_scrape" {
            return Ok(None);
        }

        let instruction: ScrapeInstruction = serde_json::from_value(serde_json::Value::Object(
            call.params
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ))
        .map_err(|e| {
            ToolError::Execution(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            ))
        })?;

        let result = self.scrape_instruction(&instruction).await?;

        Ok(Some(ToolOutput {
            tool_name: "web-scrape".to_owned(),
            summary: result,
            blocks_executed: 1,
        }))
    }
}

impl WebScrapeExecutor {
    async fn scrape_instruction(
        &self,
        instruction: &ScrapeInstruction,
    ) -> Result<String, ToolError> {
        validate_url(&instruction.url)?;
        let html = self.fetch_html(&instruction.url).await?;
        let selector = instruction.select.clone();
        let extract = ExtractMode::parse(&instruction.extract);
        let limit = instruction.limit.unwrap_or(10);
        tokio::task::spawn_blocking(move || parse_and_extract(&html, &selector, &extract, limit))
            .await
            .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?
    }

    async fn fetch_html(&self, url: &str) -> Result<String, ToolError> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

        if !resp.status().is_success() {
            return Err(ToolError::Execution(std::io::Error::other(format!(
                "HTTP {}",
                resp.status(),
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

        String::from_utf8(bytes.to_vec())
            .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))
    }
}

fn extract_scrape_blocks(text: &str) -> Vec<&str> {
    crate::executor::extract_fenced_blocks(text, "scrape")
}

fn validate_url(raw: &str) -> Result<(), ToolError> {
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

    Ok(())
}

fn is_private_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(d) => *d == "localhost",
        url::Host::Ipv4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        url::Host::Ipv6(v6) => {
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
}
