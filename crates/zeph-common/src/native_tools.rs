// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Authoritative ids of the native web-egress tools.
//!
//! The executors in `zeph-tools` register these ids, and every crate that classifies, guards or
//! displays them derives from [`WebEgressTool`], so a tool added here cannot be missed by the
//! trust classifier, the sub-agent network deny list or the guarded-tools default.

/// A native tool that reaches the network on the model's behalf.
///
/// # Examples
///
/// ```
/// use zeph_common::native_tools::WebEgressTool;
///
/// assert_eq!(WebEgressTool::Fetch.id(), "fetch");
/// assert_eq!(WebEgressTool::from_id("web_search"), Some(WebEgressTool::WebSearch));
/// assert_eq!(WebEgressTool::from_id("bash"), None);
/// assert!(WebEgressTool::ALL.iter().all(|t| WebEgressTool::from_id(t.id()) == Some(*t)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WebEgressTool {
    /// CSS-selector page extraction (`web_scrape`).
    WebScrape,
    /// Plain URL fetch (`fetch`).
    Fetch,
    /// Web search (`web_search`).
    WebSearch,
}

impl WebEgressTool {
    /// Every web-egress tool.
    pub const ALL: [Self; 3] = [Self::WebScrape, Self::Fetch, Self::WebSearch];

    /// The tool id the executor registers and the model calls.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::WebScrape => "web_scrape",
            Self::Fetch => "fetch",
            Self::WebSearch => "web_search",
        }
    }

    /// The web-egress tool registered under `id`, if any.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.id() == id)
    }
}

/// Id of the `web_scrape` tool.
pub const WEB_SCRAPE_ID: &str = WebEgressTool::WebScrape.id();
/// Id of the `fetch` tool.
pub const FETCH_ID: &str = WebEgressTool::Fetch.id();
/// Id of the `web_search` tool.
pub const WEB_SEARCH_ID: &str = WebEgressTool::WebSearch.id();

/// `ToolOutput::tool_name` the fenced-block `web_scrape` path reports instead of its tool id.
pub const WEB_SCRAPE_OUTPUT_NAME: &str = "web-scrape";

/// Returns `true` when `name`, a tool id or a result's tool name, belongs to a web-egress tool.
///
/// Unlike [`is_web_egress_tool`] it also accepts [`WEB_SCRAPE_OUTPUT_NAME`], so results of the
/// fenced `web_scrape` path classify like the structured call.
///
/// # Examples
///
/// ```
/// use zeph_common::native_tools::is_web_egress_output;
///
/// assert!(is_web_egress_output("web-scrape"));
/// assert!(is_web_egress_output("web_search"));
/// assert!(!is_web_egress_output("shell"));
/// ```
#[must_use]
pub fn is_web_egress_output(name: &str) -> bool {
    name == WEB_SCRAPE_OUTPUT_NAME || is_web_egress_tool(name)
}

/// Returns `true` when `id` is a native web-egress tool id.
///
/// # Examples
///
/// ```
/// use zeph_common::native_tools::is_web_egress_tool;
///
/// assert!(is_web_egress_tool("fetch"));
/// assert!(!is_web_egress_tool("shell"));
/// ```
#[must_use]
pub fn is_web_egress_tool(id: &str) -> bool {
    WebEgressTool::from_id(id).is_some()
}
