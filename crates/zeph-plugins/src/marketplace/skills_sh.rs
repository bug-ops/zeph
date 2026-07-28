// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`SkillsShClient`] — [`RegistryClient`] implementation for the public
//! [skills.sh](https://www.skills.sh) registry (`vercel-labs/skills`).
//!
//! # Verified API contract (S1, 2026-07-10)
//!
//! Per the project's live-inspection-first decision, this client is written against the
//! **real** skills.sh public API, inspected directly at implementation time
//! (<https://www.skills.sh/docs/api>, the Vercel changelog, and the `vercel-labs/skills`
//! GitHub repository) rather than against `plan.md`'s earlier, unverified guesses. What
//! follows is a record of what was confirmed live vs. what remains a documented,
//! best-effort assumption — see the **"Live-testing gate"** note at the end, which is
//! mandatory before this client is trusted in production per the architect/critic handoffs.
//!
//! ## Confirmed from `skills.sh/docs/api` (primary source)
//!
//! | Endpoint | Method | Purpose |
//! |---|---|---|
//! | `/api/v1/skills` | GET | paginated leaderboard of all skills |
//! | `/api/v1/skills/search` | GET | search by name/description; params `q` (required, min 2 chars), `limit` (1-200, default 50), `owner` |
//! | `/api/v1/skills/curated` | GET | official curated skills |
//! | `/api/v1/skills/{source}/{skill}` | GET | detail for a single skill |
//! | `/api/v1/skills/audit/{source}/{skill}` | GET | security audit results |
//!
//! Search response: `{ "data": [...], "query": str, "searchType": "fuzzy"|"semantic", "count": int, "durationMs": int }`.
//!
//! Detail response fields confirmed by name: `id` (format `"{source}/{slug}"`), `source`,
//! `slug`, `installs`, `hash`, `files`. The docs explicitly say "endpoint paths vary based on
//! source type, and you can use the `id` field from any listing or search response to
//! construct paths" — so [`SkillsShClient::fetch`] forwards `registry_id` (`=id`) verbatim as
//! the path suffix rather than re-splitting it, since a skill slug found via the GitHub
//! `find-skills` example page (`skills.sh/vercel-labs/skills/find-skills`) shows `source`
//! itself can contain an embedded `/` (an `owner/repo` pair).
//!
//! Auth: a Vercel-minted short-lived OIDC JWT sent as `Authorization: Bearer <token>` (or
//! `x-vercel-oidc-token`). From this (non-Vercel-hosted) client's point of view that token is
//! just an opaque bearer string — Zeph does not and cannot run `@vercel/oidc`'s
//! `getVercelOidcToken()` (a Vercel-deployment-only Node API); the user is expected to obtain a
//! token out-of-band (e.g. via the Vercel CLI) and store it with
//! `zeph vault set <key> <token>`, referenced by `skills.registry.auth_vault_key`.
//!
//! ## NOT independently verified — flagged, not guessed silently
//!
//! - The exact JSON shape of an individual **search result** object (`data[]` entries) beyond
//!   the detail-endpoint field names above. `SkillSummary` deserializes `name`/`description`/
//!   `tags`/`author`/`installs` as `#[serde(default)]` so a shape mismatch degrades to an empty
//!   value instead of a hard parse failure; `id`/`source`/`slug` are treated as required since
//!   the docs page named them explicitly.
//! - The exact shape of the detail endpoint's `files` field. The docs page states responses
//!   "contain file contents as strings within JSON" but does not show a worked example. This
//!   client accepts **both** a JSON array of `{"path": ..., "content": ...}` objects and a flat
//!   JSON object mapping `path -> content`, via `parse_files`, to hedge against either shape.
//! - `security_audit_status` is served by the separate `/audit/{source}/{skill}` endpoint
//!   (fields: `audits: [{provider, slug, status: "pass"|"warn"|"fail", summary, auditedAt,
//!   riskLevel}]`), not embedded in search or detail responses. Calling it per search result
//!   would add an N+1 request fan-out for a single `search()` call, which NFR-004/NFR-005 do
//!   not require — [`SkillsShClient::search`] leaves `security_audit_status: None` and does not
//!   call the audit endpoint. A future enhancement could wire it in for `zeph skill get`
//!   specifically (a single lookup, not N).
//!
//! ## Live-testing gate (non-skippable, S1)
//!
//! The above is the best-effort-verified contract achievable from public documentation
//! without an available Vercel OIDC token in this environment. Per the architect/critic
//! handoffs, a live `zeph skill search`/`zeph skill get` round-trip against real skills.sh
//! with `skills.registry.enabled = true` is a **required, non-skippable** gate before this
//! client is considered production-verified — see
//! `.local/testing/playbooks/skills-plugin-registry.md`. The wiremock tests below are
//! regression fixtures for the contract as documented here; they cannot catch a documentation
//! error, only a code regression against it.

use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use tracing::Instrument as _;
use zeph_common::secret::Secret;

use super::{PackageArchive, RegistryClient, RegistryEntry, RegistryError, materialize_package};

/// Default skills.sh base URL, used when `skills.registry.backend_url` is unset.
pub const DEFAULT_BASE_URL: &str = "https://www.skills.sh";

/// Maximum accepted response body size for a single registry request (search results page or
/// one package's detail response).
///
/// JSON skill packages are source text (`SKILL.md` plus a handful of small scripts) — a few
/// MiB is ample headroom. Mirrors the spirit of [`crate::manager::MAX_ARCHIVE_BYTES`] (52 MiB
/// for binary plugin archives), scaled down for a text-only payload. Checked against
/// `Content-Length` before the body is read, matching the pre-check pattern in
/// `crate::manager::registry::download_and_extract` (review fix #2 — this path previously read
/// an unbounded body into memory).
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// [`RegistryClient`] implementation targeting the public skills.sh registry.
///
/// See the module docs for the verified API contract this client implements against.
pub struct SkillsShClient {
    base_url: String,
    token: Option<Secret>,
    client: reqwest::Client,
    /// Parent directory for [`Self::fetch`]'s extraction tempdir. `None` (the production
    /// default) uses the process temp dir via `tempfile::tempdir()`. Overridable only in
    /// tests via [`Self::with_tmp_root_for_test`], so a test can force tempdir creation to
    /// fail without mutating the process-wide `TMPDIR` env var (issue #6686).
    tmp_root: Option<std::path::PathBuf>,
}

impl SkillsShClient {
    /// Create a new client.
    ///
    /// `base_url` should have no trailing slash (e.g. `"https://www.skills.sh"`); a trailing
    /// slash is stripped defensively. `token` is the bearer credential resolved from the vault
    /// via `skills.registry.auth_vault_key`, or `None` for an anonymous request (some
    /// skills.sh endpoints may reject this with 401 — see [`RegistryError::AuthRequired`]).
    /// Held as a [`Secret`] (Debug/Display-redacted), matching the newtype used for every other
    /// vault-resolved credential in this workspace (review fix #7).
    ///
    /// The underlying `reqwest::Client` refuses any redirect that downgrades from `https` to a
    /// non-`https` scheme, mirroring `crate::manager::registry::download_and_extract`'s
    /// redirect policy (review fix #6).
    ///
    /// # Errors
    ///
    /// This constructor itself cannot fail; malformed `base_url` values surface as
    /// [`RegistryError::Request`] on the first call. A non-`https`/`http` `base_url` scheme is
    /// rejected lazily on first use via [`RegistryError::UnsafeBackendUrl`] rather than here, to
    /// keep this constructor infallible like its sibling `PluginManager::new`.
    #[must_use]
    pub fn new(base_url: impl Into<String>, token: Option<Secret>, timeout_secs: u64) -> Self {
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.url().scheme() == "https" {
                    attempt.follow()
                } else {
                    let redirect_url = attempt.url().to_string();
                    attempt.error(format!(
                        "redirect to non-HTTPS URL is not permitted: {redirect_url}"
                    ))
                }
            }))
            .build()
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "SkillsShClient: failed to build HTTP client with configured timeout/redirect \
                     policy; falling back to reqwest::Client::default() (no timeout guarantee)"
                );
                reqwest::Client::default()
            });
        Self {
            base_url,
            token,
            client,
            tmp_root: None,
        }
    }

    /// Force [`Self::fetch`] to create its extraction tempdir under `dir` instead of the
    /// process temp dir. Test-only seam (issue #6686) so a test can point at a non-directory
    /// path to make `tempdir_in` fail deterministically, without mutating the process-wide
    /// `TMPDIR` env var and racing every other test that calls `tempfile::tempdir()`.
    #[cfg(test)]
    #[must_use]
    fn with_tmp_root_for_test(mut self, dir: std::path::PathBuf) -> Self {
        self.tmp_root = Some(dir);
        self
    }

    /// Reject `base_url` schemes other than `http`/`https` (SSRF hardening — review fix #6).
    /// `backend_url` is self-configured, not attacker-supplied, but a compromised/hostile
    /// registry could still 3xx-redirect to an internal scheme if we didn't also gate the
    /// initial request; the redirect policy in [`Self::new`] handles the redirect leg.
    fn validate_base_url_scheme(&self) -> Result<(), RegistryError> {
        let parsed = reqwest::Url::parse(&self.base_url)
            .map_err(|e| RegistryError::UnsafeBackendUrl(format!("{}: {e}", self.base_url)))?;
        if matches!(parsed.scheme(), "http" | "https") {
            Ok(())
        } else {
            Err(RegistryError::UnsafeBackendUrl(format!(
                "scheme {:?} is not allowed; only http and https are permitted",
                parsed.scheme()
            )))
        }
    }

    fn request(&self, url: &str) -> reqwest::RequestBuilder {
        let builder = self.client.get(url);
        match &self.token {
            Some(token) => builder.bearer_auth(token.expose()),
            None => builder,
        }
    }

    #[tracing::instrument(name = "plugins.marketplace.get_json_text", skip(self), fields(url))]
    async fn get_json_text(&self, url: &str) -> Result<String, RegistryError> {
        let response = self
            .request(url)
            .send()
            .await
            .map_err(|e| classify_send_error(&e))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(RegistryError::AuthRequired);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(RegistryError::NotFound(url.to_owned()));
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RegistryError::Backend {
                status: status.as_u16(),
                body,
            });
        }
        reject_oversized(&response)?;
        response
            .text()
            .await
            .map_err(|e| RegistryError::Request(e.to_string()))
    }
}

/// Reject a response whose declared `Content-Length` exceeds [`MAX_RESPONSE_BYTES`] before the
/// body is read into memory (review fix #2). A missing `Content-Length` (chunked transfer) is
/// allowed through unchecked, matching the same tradeoff `download_and_extract` makes for
/// binary archives — a fully adversarial chunked-transfer cap would require switching to a
/// streaming read, which is disproportionate for this text-only JSON payload class.
fn reject_oversized(response: &reqwest::Response) -> Result<(), RegistryError> {
    if let Some(len) = response.content_length()
        && len > MAX_RESPONSE_BYTES
    {
        return Err(RegistryError::TooLarge(format!(
            "response declared {len} bytes (max {MAX_RESPONSE_BYTES})"
        )));
    }
    Ok(())
}

fn classify_send_error(e: &reqwest::Error) -> RegistryError {
    if e.is_timeout() {
        RegistryError::Timeout
    } else {
        RegistryError::Request(e.to_string())
    }
}

/// Reject a `registry_id` containing characters that could alter the request's query string or
/// fragment when interpolated verbatim into the URL path (review fix #8). Not a general
/// percent-encoding pass — skills.sh's own docs say to forward `id` verbatim as a path suffix
/// (see module docs), so encoding legitimate `/` separators would break that contract; this
/// only blocks the specific characters that change URL *structure*.
fn validate_registry_id(registry_id: &str) -> Result<(), RegistryError> {
    if registry_id.is_empty()
        || registry_id
            .chars()
            .any(|c| c == '?' || c == '#' || c.is_whitespace())
    {
        return Err(RegistryError::InvalidRegistryId(registry_id.to_owned()));
    }
    Ok(())
}

impl RegistryClient for SkillsShClient {
    fn search(
        &self,
        query: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<RegistryEntry>, RegistryError>> + Send + '_>> {
        let query = query.to_owned();
        let span = tracing::info_span!("plugins.marketplace.search", query = %query);
        Box::pin(
            async move {
                self.validate_base_url_scheme()?;
                let url = format!("{}/api/v1/skills/search", self.base_url);
                let response = self
                    .request(&url)
                    .query(&[("q", query.as_str()), ("limit", "50")])
                    .send()
                    .await
                    .map_err(|e| classify_send_error(&e))?;

                let status = response.status();
                if status == reqwest::StatusCode::UNAUTHORIZED
                    || status == reqwest::StatusCode::FORBIDDEN
                {
                    return Err(RegistryError::AuthRequired);
                }
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    return Err(RegistryError::Backend {
                        status: status.as_u16(),
                        body,
                    });
                }
                reject_oversized(&response)?;
                let text = response
                    .text()
                    .await
                    .map_err(|e| RegistryError::Request(e.to_string()))?;
                let parsed: SearchResponse = serde_json::from_str(&text)
                    .map_err(|e| RegistryError::InvalidResponse(e.to_string()))?;

                Ok(parsed
                    .data
                    .into_iter()
                    .map(SkillSummary::into_entry)
                    .collect())
            }
            .instrument(span),
        )
    }

    fn fetch(
        &self,
        registry_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<PackageArchive, RegistryError>> + Send + '_>> {
        let registry_id = registry_id.to_owned();
        let span = tracing::info_span!("plugins.marketplace.fetch", registry_id = %registry_id);
        Box::pin(
            async move {
                self.validate_base_url_scheme()?;
                validate_registry_id(&registry_id)?;
                let suffix = registry_id.trim_start_matches('/');
                let url = format!("{}/api/v1/skills/{suffix}", self.base_url);
                let text = self.get_json_text(&url).await.map_err(|e| {
                    if matches!(e, RegistryError::NotFound(_)) {
                        RegistryError::NotFound(registry_id.clone())
                    } else {
                        e
                    }
                })?;
                let detail: SkillDetail = serde_json::from_str(&text)
                    .map_err(|e| RegistryError::InvalidResponse(e.to_string()))?;

                let files = parse_files(&detail.files)?;
                if files.is_empty() {
                    return Err(RegistryError::InvalidResponse(
                        "package response contained no files".to_owned(),
                    ));
                }

                let tmp = match &self.tmp_root {
                    Some(root) => tempfile::Builder::new().tempdir_in(root)?,
                    None => tempfile::tempdir()?,
                };
                let (has_plugin_manifest, install_dir) = materialize_package(tmp.path(), &files)?;

                Ok(PackageArchive {
                    registry_id,
                    has_plugin_manifest,
                    extracted_dir: tmp,
                    install_dir,
                })
            }
            .instrument(span),
        )
    }
}

/// `{ "data": [...], "query": ..., "searchType": ..., "count": ..., "durationMs": ... }`
#[derive(Debug, Deserialize)]
struct SearchResponse {
    data: Vec<SkillSummary>,
}

/// One entry of `SearchResponse.data`. See module docs — fields beyond `id`/`source`/`slug`
/// are `#[serde(default)]` because the exact search-result shape was not independently
/// verified beyond the primary docs page's field list for the *detail* endpoint.
#[derive(Debug, Deserialize)]
struct SkillSummary {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    author: Option<String>,
}

impl SkillSummary {
    fn into_entry(self) -> RegistryEntry {
        let fallback_name = self
            .id
            .rsplit('/')
            .next()
            .unwrap_or(self.id.as_str())
            .to_owned();
        RegistryEntry {
            registry_id: self.id,
            name: self.name.unwrap_or(fallback_name),
            description: self.description.unwrap_or_default(),
            tags: self.tags,
            author: self.author,
            // Not fetched here — see module docs on the separate `/audit/*` endpoint.
            security_audit_status: None,
        }
    }
}

/// Detail response for `/api/v1/skills/{source}/{skill}`.
#[derive(Debug, Deserialize)]
struct SkillDetail {
    #[serde(default)]
    files: serde_json::Value,
}

/// Parse the detail endpoint's `files` field into `(path, content)` pairs.
///
/// Accepts either a JSON array of `{"path": ..., "content": ...}` objects, or a flat JSON
/// object mapping `path -> content` string — see module docs for why both are supported.
///
/// # Errors
///
/// Returns [`RegistryError::InvalidResponse`] when `files` is neither shape.
fn parse_files(files: &serde_json::Value) -> Result<Vec<(String, String)>, RegistryError> {
    match files {
        serde_json::Value::Array(entries) => entries
            .iter()
            .map(|entry| {
                let path = entry
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        RegistryError::InvalidResponse(
                            "files[] entry missing string \"path\"".to_owned(),
                        )
                    })?;
                let content = entry
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        RegistryError::InvalidResponse(
                            "files[] entry missing string \"content\"".to_owned(),
                        )
                    })?;
                Ok((path.to_owned(), content.to_owned()))
            })
            .collect(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(path, content)| {
                let content = content.as_str().ok_or_else(|| {
                    RegistryError::InvalidResponse(format!("files[{path:?}] value is not a string"))
                })?;
                Ok((path.clone(), content.to_owned()))
            })
            .collect(),
        other => Err(RegistryError::InvalidResponse(format!(
            "unexpected \"files\" shape: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer, token: Option<&str>) -> SkillsShClient {
        SkillsShClient::new(server.uri(), token.map(Secret::new), 5)
    }

    #[tokio::test]
    async fn search_parses_confirmed_response_shape() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/skills/search"))
            .and(query_param("q", "pdf"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": "acme/pdf-tools", "source": "acme", "slug": "pdf-tools",
                     "name": "PDF Tools", "description": "Manipulate PDFs", "tags": ["pdf"]}
                ],
                "query": "pdf",
                "searchType": "fuzzy",
                "count": 1,
                "durationMs": 12
            })))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let results = client.search("pdf").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].registry_id, "acme/pdf-tools");
        assert_eq!(results[0].name, "PDF Tools");
    }

    #[tokio::test]
    async fn search_degrades_gracefully_on_missing_optional_fields() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/skills/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "acme/bare", "source": "acme", "slug": "bare"}],
                "query": "bare", "searchType": "fuzzy", "count": 1, "durationMs": 1
            })))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let results = client.search("bare").await.unwrap();
        assert_eq!(results[0].name, "bare");
        assert_eq!(results[0].description, "");
    }

    #[tokio::test]
    async fn search_sends_bearer_token_when_configured() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/skills/search"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [], "query": "x", "searchType": "fuzzy", "count": 0, "durationMs": 1
            })))
            .mount(&server)
            .await;

        let client = client_for(&server, Some("test-token"));
        let results = client.search("x").await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_maps_401_to_auth_required() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/skills/search"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let err = client.search("x").await.unwrap_err();
        assert!(matches!(err, RegistryError::AuthRequired));
    }

    #[tokio::test]
    async fn search_rejects_oversized_response_before_reading_body() {
        let server = MockServer::start().await;
        // A real oversized body (hyper enforces Content-Length/body-length coherence server
        // side, so a lying header alone panics the mock server task instead of exercising the
        // client's pre-check) — rejection must fire from the Content-Length header alone,
        // before `.text()` reads the (large) body, which this test cannot directly observe but
        // is exercised by construction: reject_oversized runs before the `.text()` call.
        let oversized = vec![b' '; usize::try_from(MAX_RESPONSE_BYTES + 1).unwrap()];
        Mock::given(method("GET"))
            .and(path("/api/v1/skills/search"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(oversized))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let err = client.search("x").await.unwrap_err();
        assert!(matches!(err, RegistryError::TooLarge(_)));
    }

    #[tokio::test]
    async fn search_maps_connect_failure_to_request_error() {
        // Port 0 never accepts connections — deterministic connect failure, no live network.
        let client = SkillsShClient::new("http://127.0.0.1:0", None, 1);
        let err = client.search("x").await.unwrap_err();
        assert!(matches!(err, RegistryError::Request(_)));
    }

    #[tokio::test]
    async fn search_rejects_non_http_backend_url() {
        let client = SkillsShClient::new("file:///etc/passwd", None, 1);
        let err = client.search("x").await.unwrap_err();
        assert!(matches!(err, RegistryError::UnsafeBackendUrl(_)));
    }

    #[tokio::test]
    async fn fetch_rejects_registry_id_with_query_separator() {
        let server = MockServer::start().await;
        let client = client_for(&server, None);
        let err = client.fetch("acme/x?evil=1").await.unwrap_err();
        assert!(matches!(err, RegistryError::InvalidRegistryId(_)));
    }

    #[tokio::test]
    async fn fetch_rejects_registry_id_with_fragment() {
        let server = MockServer::start().await;
        let client = client_for(&server, None);
        let err = client.fetch("acme/x#frag").await.unwrap_err();
        assert!(matches!(err, RegistryError::InvalidRegistryId(_)));
    }

    #[tokio::test]
    async fn fetch_materializes_array_shaped_files() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/skills/acme/pdf-tools"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "acme/pdf-tools", "source": "acme", "slug": "pdf-tools",
                "installs": 10, "hash": "abc123",
                "files": [
                    {"path": "SKILL.md", "content": "---\nname: pdf-tools\ndescription: x\n---\nbody"}
                ]
            })))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let archive = client.fetch("acme/pdf-tools").await.unwrap();
        assert!(!archive.has_plugin_manifest);
        // install_dir, not extracted_dir directly — must be named after the skill (see
        // PackageArchive::install_dir docs) for SkillManager::install_from_path to accept it.
        assert_eq!(
            archive.install_dir,
            archive.extracted_dir.path().join("pdf-tools")
        );
        assert!(archive.install_dir.join("SKILL.md").is_file());
    }

    #[tokio::test]
    async fn fetch_materializes_map_shaped_files_and_detects_plugin_manifest() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/skills/acme/full-plugin"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "acme/full-plugin", "source": "acme", "slug": "full-plugin",
                "files": {
                    "plugin.toml": "[plugin]\nname=\"full-plugin\"\nversion=\"0.1.0\"",
                    "skills/x/SKILL.md": "---\nname: x\ndescription: y\n---\nbody"
                }
            })))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let archive = client.fetch("acme/full-plugin").await.unwrap();
        assert!(archive.has_plugin_manifest);
        assert!(
            archive
                .extracted_dir
                .path()
                .join("skills/x/SKILL.md")
                .is_file()
        );
    }

    #[tokio::test]
    async fn fetch_maps_404_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/skills/nope/nope"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let err = client.fetch("nope/nope").await.unwrap_err();
        assert!(matches!(err, RegistryError::NotFound(id) if id == "nope/nope"));
    }

    #[tokio::test]
    async fn fetch_rejects_oversized_detail_response() {
        let server = MockServer::start().await;
        let oversized = vec![b' '; usize::try_from(MAX_RESPONSE_BYTES + 1).unwrap()];
        Mock::given(method("GET"))
            .and(path("/api/v1/skills/acme/huge"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(oversized))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let err = client.fetch("acme/huge").await.unwrap_err();
        assert!(matches!(err, RegistryError::TooLarge(_)));
    }

    #[tokio::test]
    async fn fetch_surfaces_io_error_when_write_target_unwritable() {
        // write_files_safe's own Io-mapping is exercised via a path component that cannot be
        // created: a regular file used as a parent "directory" fails create_dir_all with an
        // io::Error, which RegistryError::Io(#[from]) surfaces without downcasting.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/skills/acme/x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "acme/x", "source": "acme", "slug": "x",
                "files": [{"path": "SKILL.md", "content": "body"}]
            })))
            .mount(&server)
            .await;

        // Force tempdir_in() itself to fail by pointing the client's tempdir parent at a
        // regular file rather than a directory — injected directly into this client instance
        // (issue #6686), not via the process-wide TMPDIR env var, so no other concurrently
        // running test racing on `tempfile::tempdir()` is affected.
        let bogus_parent = tempfile::NamedTempFile::new().unwrap();
        let client =
            client_for(&server, None).with_tmp_root_for_test(bogus_parent.path().to_path_buf());
        let result = client.fetch("acme/x").await;

        let err = result.unwrap_err();
        assert!(matches!(err, RegistryError::Io(_)));
    }

    #[test]
    fn parse_files_rejects_unexpected_shape() {
        let value = serde_json::json!("not files");
        let err = parse_files(&value).unwrap_err();
        assert!(matches!(err, RegistryError::InvalidResponse(_)));
    }
}
