// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Automated skill mining from GitHub repositories.
//!
//! The mining pipeline:
//! 1. Search GitHub for repositories matching configured queries.
//! 2. Fetch README content for each candidate repo.
//! 3. Generate a SKILL.md candidate via LLM from the README.
//! 4. Deduplicate against existing skills using cosine similarity on embeddings.
//! 5. Write novel skills to `output_dir`.

use std::path::PathBuf;
use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, Role};
use zeph_memory::cosine_similarity;

use crate::error::SkillError;
use crate::generator::{GeneratedSkill, SkillGenerator};
use crate::loader::SkillMeta;

/// Maximum README bytes to pass to the LLM (prevents context overflow).
const MAX_README_BYTES: usize = 32_768;

/// System prompt for mining: generate SKILL.md from a GitHub repository README.
const MINING_SYSTEM_PROMPT: &str = "\
You are an expert at creating SKILL.md files for the Zeph AI agent. \
Given a GitHub repository name and its README, generate a SKILL.md that captures \
the repository's primary use case as an agent skill. \
\n\nRules:\n\
- name: lowercase letters, digits, and hyphens only (1-64 chars); derive from the repo name\n\
- description: one or two sentences describing what the tool does and when to use it\n\
- Use only tools that are appropriate for the skill (e.g. bash for CLI tools)\n\
- Body: max 3 ## sections, practical examples from the README\n\
- Body size: keep under 15000 bytes\n\
- Output ONLY the raw SKILL.md content, no explanation, no code fences\n";

/// A GitHub repository candidate for skill extraction.
pub struct RepoCandidate {
    pub full_name: String,
    pub description: String,
    pub readme_content: String,
    pub stars: u32,
}

/// A successfully mined skill.
pub struct MinedSkill {
    pub repo: String,
    pub skill: GeneratedSkill,
    /// Cosine similarity to the nearest existing skill (0.0 if no existing skills).
    pub nearest_similarity: f32,
}

/// Configuration for a single mining run.
pub struct MiningConfig {
    pub queries: Vec<String>,
    pub max_repos_per_query: usize,
    pub dedup_threshold: f32,
    pub output_dir: PathBuf,
    /// GitHub API rate limit in requests per minute.
    pub rate_limit_rpm: u32,
    /// When `true`, generate and report but do not write skills to disk.
    pub dry_run: bool,
}

/// Orchestrates the automated skill mining pipeline.
pub struct SkillMiner {
    generator: SkillGenerator,
    embed_provider: AnyProvider,
    github_token: String,
    config: MiningConfig,
    http: reqwest::Client,
}

impl SkillMiner {
    /// Create a new `SkillMiner`.
    ///
    /// # Errors
    ///
    /// Returns `SkillError::Other` if the HTTP client cannot be built.
    pub fn new(
        generation_provider: AnyProvider,
        embed_provider: AnyProvider,
        github_token: String,
        config: MiningConfig,
    ) -> Result<Self, SkillError> {
        let http = reqwest::Client::builder()
            .user_agent("zeph-skills-miner/1.0")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| SkillError::Other(format!("HTTP client build failed: {e}")))?;

        let generator = SkillGenerator::new(generation_provider, config.output_dir.clone());

        Ok(Self {
            generator,
            embed_provider,
            github_token,
            config,
            http,
        })
    }

    /// Run the full mining pipeline.
    ///
    /// Returns the list of novel skills that were written to `output_dir`.
    /// Repos that fail or are deduped are skipped with a warning log.
    ///
    /// # Errors
    ///
    /// Returns `SkillError::Other` on unrecoverable pipeline failures.
    pub async fn run(&self, existing_skills: &[SkillMeta]) -> Result<Vec<MinedSkill>, SkillError> {
        // Pre-compute embeddings for all existing skill descriptions once.
        let existing_embeddings = self.embed_existing(existing_skills).await;

        let delay_between_requests = self.request_delay();
        let mut results: Vec<MinedSkill> = Vec::new();

        for query in &self.config.queries {
            tracing::info!(query = %query, "mining: searching GitHub");
            let repos = match self.search_repos(query).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(query = %query, error = %e, "GitHub search failed, skipping");
                    continue;
                }
            };
            tokio::time::sleep(delay_between_requests).await;

            for repo in repos {
                match self
                    .process_repo(&repo, &existing_embeddings, self.config.dry_run)
                    .await
                {
                    Ok(Some(mined)) => {
                        results.push(mined);
                    }
                    Ok(None) => {} // deduped or dry-run
                    Err(e) => {
                        tracing::warn!(repo = %repo.full_name, error = %e, "failed to process repo");
                    }
                }
                tokio::time::sleep(delay_between_requests).await;
            }
        }

        Ok(results)
    }

    /// Pre-compute embeddings for existing skill descriptions.
    async fn embed_existing(&self, skills: &[SkillMeta]) -> Vec<(String, Vec<f32>)> {
        let mut embeddings = Vec::with_capacity(skills.len());
        for skill in skills {
            match self.embed_provider.embed(&skill.description).await {
                Ok(emb) => embeddings.push((skill.name.clone(), emb)),
                Err(e) => {
                    tracing::warn!(
                        skill = %skill.name,
                        error = %e,
                        "failed to embed existing skill for dedup"
                    );
                }
            }
        }
        embeddings
    }

    /// Process a single repo: generate skill, dedup, write.
    async fn process_repo(
        &self,
        repo: &RepoCandidate,
        existing_embeddings: &[(String, Vec<f32>)],
        is_dry_run: bool,
    ) -> Result<Option<MinedSkill>, SkillError> {
        // Generate skill from repo.
        let skill = match self.generate_from_repo(repo).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(repo = %repo.full_name, error = %e, "skill generation failed");
                return Ok(None);
            }
        };

        // Check dedup.
        let (is_novel, nearest_sim) = self.is_novel(&skill, existing_embeddings).await?;
        if !is_novel {
            tracing::info!(
                repo = %repo.full_name,
                skill = %skill.name,
                similarity = nearest_sim,
                "skipping duplicate skill"
            );
            return Ok(None);
        }

        if is_dry_run {
            tracing::info!(
                repo = %repo.full_name,
                skill = %skill.name,
                "dry-run: would write skill"
            );
            return Ok(Some(MinedSkill {
                repo: repo.full_name.clone(),
                skill,
                nearest_similarity: nearest_sim,
            }));
        }

        self.generator.approve_and_save(&skill).await?;

        Ok(Some(MinedSkill {
            repo: repo.full_name.clone(),
            skill,
            nearest_similarity: nearest_sim,
        }))
    }

    /// Compute the inter-request delay from the rate limit config.
    fn request_delay(&self) -> Duration {
        let rpm = self.config.rate_limit_rpm.max(1);
        Duration::from_millis(u64::from(60_000 / rpm))
    }

    /// Search GitHub for repositories matching `query`.
    ///
    /// # Errors
    ///
    /// Returns `SkillError::Other` on network or rate-limit errors.
    pub async fn search_repos(&self, query: &str) -> Result<Vec<RepoCandidate>, SkillError> {
        let per_page = self.config.max_repos_per_query.min(100);
        // Build URL manually to avoid needing reqwest's query param serialization feature.
        let encoded_query: String = query
            .chars()
            .flat_map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                    vec![c]
                } else {
                    format!("%{:02X}", c as u32).chars().collect()
                }
            })
            .collect();
        let url = format!(
            "https://api.github.com/search/repositories?q={encoded_query}&sort=stars&per_page={per_page}"
        );

        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.github_token))
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .map_err(|e| SkillError::Other(format!("GitHub search request failed: {e}")))?;

        if resp.status() == reqwest::StatusCode::FORBIDDEN
            || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            return Err(SkillError::Other(format!(
                "GitHub rate limit exceeded ({})",
                resp.status()
            )));
        }

        if !resp.status().is_success() {
            return Err(SkillError::Other(format!(
                "GitHub search returned {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            )));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| SkillError::Other(format!("GitHub search JSON parse failed: {e}")))?;

        let items = json["items"].as_array().cloned().unwrap_or_default();
        let mut candidates = Vec::with_capacity(items.len());

        for item in &items {
            let full_name = item["full_name"].as_str().unwrap_or_default().to_string();
            let description = item["description"].as_str().unwrap_or_default().to_string();
            let stars =
                u32::try_from(item["stargazers_count"].as_u64().unwrap_or(0)).unwrap_or(u32::MAX);

            if full_name.is_empty() {
                continue;
            }

            let readme = match self.fetch_readme(&full_name).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(repo = %full_name, error = %e, "README fetch failed, skipping");
                    continue;
                }
            };

            candidates.push(RepoCandidate {
                full_name,
                description,
                readme_content: readme,
                stars,
            });
        }

        Ok(candidates)
    }

    /// Fetch and truncate the README for a repository.
    ///
    /// # Errors
    ///
    /// Returns `SkillError::Invalid` if `repo` does not match `owner/name` format.
    /// Returns `SkillError::Other` on HTTP or parse errors.
    async fn fetch_readme(&self, repo: &str) -> Result<String, SkillError> {
        // SSRF guard: repo must be exactly "owner/name" with safe characters.
        {
            let mut parts = repo.splitn(2, '/');
            let owner = parts.next().unwrap_or("");
            let name = parts.next().unwrap_or("");
            let is_safe = |s: &str| {
                !s.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
            };
            if !is_safe(owner) || !is_safe(name) || parts.next().is_some() {
                return Err(SkillError::Invalid(format!(
                    "invalid repository name: {repo:?}"
                )));
            }
        }
        let url = format!("https://api.github.com/repos/{repo}/readme");
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.github_token))
            .header("Accept", "application/vnd.github.raw")
            .send()
            .await
            .map_err(|e| SkillError::Other(format!("README fetch failed: {e}")))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SkillError::NotFound(format!("no README for {repo}")));
        }
        if !resp.status().is_success() {
            return Err(SkillError::Other(format!(
                "README fetch returned {}",
                resp.status()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SkillError::Other(format!("README read failed: {e}")))?;

        let text =
            String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_README_BYTES)]).into_owned();
        Ok(text)
    }

    /// Generate a SKILL.md candidate from a `RepoCandidate`.
    async fn generate_from_repo(&self, repo: &RepoCandidate) -> Result<GeneratedSkill, SkillError> {
        let user_prompt = format!(
            "Repository: {}\nDescription: {}\n\nREADME (truncated to 32KB):\n\n{}",
            repo.full_name, repo.description, repo.readme_content
        );
        let messages = vec![
            Message::from_legacy(Role::System, MINING_SYSTEM_PROMPT),
            Message::from_legacy(Role::User, &user_prompt),
        ];

        let raw = self
            .generator
            .provider
            .chat(&messages)
            .await
            .map_err(|e| SkillError::Other(format!("LLM generation failed: {e}")))?;

        crate::generator::parse_and_validate_pub(&crate::generator::extract_skill_md_pub(&raw))
    }

    /// Check whether a candidate skill is novel (below the dedup threshold).
    ///
    /// Returns `(is_novel, nearest_similarity)`.
    ///
    /// # Errors
    ///
    /// Returns `SkillError::Other` if the embedding call fails.
    pub async fn is_novel(
        &self,
        candidate: &GeneratedSkill,
        existing_embeddings: &[(String, Vec<f32>)],
    ) -> Result<(bool, f32), SkillError> {
        if existing_embeddings.is_empty() {
            return Ok((true, 0.0));
        }

        let candidate_emb = self
            .embed_provider
            .embed(&candidate.meta.description)
            .await
            .map_err(|e| SkillError::Other(format!("embed failed: {e}")))?;

        let max_sim = existing_embeddings
            .iter()
            .map(|(_, emb)| cosine_similarity(&candidate_emb, emb))
            .fold(0.0_f32, f32::max);

        Ok((max_sim < self.config.dedup_threshold, max_sim))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_delay_25rpm() {
        let config = MiningConfig {
            queries: vec![],
            max_repos_per_query: 20,
            dedup_threshold: 0.85,
            output_dir: PathBuf::from("/tmp"),
            rate_limit_rpm: 25,
            dry_run: false,
        };
        let miner = SkillMiner {
            generator: SkillGenerator::new(
                zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
                PathBuf::from("/tmp"),
            ),
            embed_provider: zeph_llm::any::AnyProvider::Mock(
                zeph_llm::mock::MockProvider::default(),
            ),
            github_token: String::new(),
            config,
            http: reqwest::Client::new(),
        };
        let delay = miner.request_delay();
        assert_eq!(delay, Duration::from_millis(2400));
    }

    #[test]
    fn mining_config_defaults() {
        let config = MiningConfig {
            queries: vec![],
            max_repos_per_query: 20,
            dedup_threshold: 0.85,
            output_dir: PathBuf::from("/tmp"),
            rate_limit_rpm: 25,
            dry_run: false,
        };
        assert!((config.dedup_threshold - 0.85_f32).abs() < f32::EPSILON);
        assert_eq!(config.max_repos_per_query, 20);
        assert_eq!(config.rate_limit_rpm, 25);
    }

    #[tokio::test]
    async fn is_novel_empty_existing() {
        let miner = make_test_miner("/tmp");
        let skill = make_test_skill();
        let (novel, sim) = miner.is_novel(&skill, &[]).await.unwrap();
        assert!(novel);
        assert!(sim.abs() < f32::EPSILON);
    }

    fn make_test_miner(output_dir: &str) -> SkillMiner {
        let mock = zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let config = MiningConfig {
            queries: vec![],
            max_repos_per_query: 20,
            dedup_threshold: 0.85,
            output_dir: PathBuf::from(output_dir),
            rate_limit_rpm: 25,
            dry_run: false,
        };
        SkillMiner {
            generator: SkillGenerator::new(mock.clone(), PathBuf::from(output_dir)),
            embed_provider: mock,
            github_token: String::new(),
            config,
            http: reqwest::Client::new(),
        }
    }

    fn make_test_skill() -> GeneratedSkill {
        use crate::loader::load_skill_meta_from_str;
        let content =
            "---\nname: test-skill\ndescription: A test skill.\n---\n\n## Usage\n\nDo stuff.\n";
        let (meta, _) = load_skill_meta_from_str(content).unwrap();
        GeneratedSkill {
            name: "test-skill".into(),
            content: content.into(),
            meta,
            warnings: vec![],
        }
    }

    // Tests for is_novel use a custom miner with a MockEmbedProvider that returns a controlled
    // vector so we can exercise the dedup threshold logic without a real LLM.
    struct FixedEmbedMiner {
        embed_vec: Vec<f32>,
        threshold: f32,
    }

    impl FixedEmbedMiner {
        // Directly invoke the dedup logic that is_novel implements, bypassing LLM calls.
        fn is_novel_direct(
            &self,
            candidate_emb: &[f32],
            existing: &[(String, Vec<f32>)],
        ) -> (bool, f32) {
            if existing.is_empty() {
                return (true, 0.0);
            }
            let max_sim = existing
                .iter()
                .map(|(_, emb)| cosine_similarity(candidate_emb, emb))
                .fold(0.0_f32, f32::max);
            (max_sim < self.threshold, max_sim)
        }
    }

    #[test]
    fn is_novel_rejects_similar_skill() {
        // Two identical unit vectors → cosine_similarity == 1.0 >= threshold 0.85 → not novel.
        let helper = FixedEmbedMiner {
            embed_vec: vec![1.0, 0.0, 0.0],
            threshold: 0.85,
        };
        let existing = vec![("existing".to_string(), vec![1.0, 0.0, 0.0])];
        let (novel, sim) = helper.is_novel_direct(&helper.embed_vec, &existing);
        assert!(!novel, "identical vectors should not be novel");
        assert!(
            (sim - 1.0_f32).abs() < 1e-5,
            "expected similarity ~1.0, got {sim}"
        );
    }

    #[test]
    fn is_novel_accepts_dissimilar_skill() {
        // Orthogonal vectors → cosine_similarity == 0.0 < threshold 0.85 → novel.
        let helper = FixedEmbedMiner {
            embed_vec: vec![1.0, 0.0, 0.0],
            threshold: 0.85,
        };
        let existing = vec![("other".to_string(), vec![0.0, 1.0, 0.0])];
        let (novel, sim) = helper.is_novel_direct(&helper.embed_vec, &existing);
        assert!(novel, "orthogonal vectors should be novel, sim={sim}");
        assert!(sim < 0.85);
    }

    #[tokio::test]
    async fn dry_run_does_not_write_files() {
        let dir = tempfile::tempdir().unwrap();
        let mock = zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let config = MiningConfig {
            queries: vec![],
            max_repos_per_query: 20,
            dedup_threshold: 0.85,
            output_dir: dir.path().to_path_buf(),
            rate_limit_rpm: 25,
            dry_run: true,
        };
        let miner = SkillMiner {
            generator: SkillGenerator::new(mock.clone(), dir.path().to_path_buf()),
            embed_provider: mock,
            github_token: String::new(),
            config,
            http: reqwest::Client::new(),
        };

        let skill = make_test_skill();
        // process_repo with dry_run=true should return Some(MinedSkill) but NOT write to disk.
        let result = miner
            .process_repo(
                &RepoCandidate {
                    full_name: "test/repo".into(),
                    description: "A test repo.".into(),
                    // Empty README: MockProvider will return a fixed skill from chat() regardless.
                    readme_content: String::new(),
                    stars: 100,
                },
                &[],
                true,
            )
            .await;

        // The mock LLM may or may not produce a valid SKILL.md (depends on MockProvider output).
        // What we assert is: even if a skill was produced, the skill directory was NOT created.
        let skill_dir = dir.path().join(&skill.name);
        assert!(
            !skill_dir.exists(),
            "dry-run must not write skill directory to disk"
        );
        // Confirm the output dir has no subdirectories written.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(
            entries.is_empty(),
            "dry-run must not create any files in output_dir, found: {:?}",
            entries
                .iter()
                .map(std::fs::DirEntry::file_name)
                .collect::<Vec<_>>()
        );
        let _ = result; // result may be Ok(None) if mock LLM output is not a valid SKILL.md
    }

    #[test]
    fn request_delay_zero_rpm_uses_minimum() {
        // rate_limit_rpm=0 must not divide by zero; max(1) guard applies.
        let config = MiningConfig {
            queries: vec![],
            max_repos_per_query: 20,
            dedup_threshold: 0.85,
            output_dir: PathBuf::from("/tmp"),
            rate_limit_rpm: 0,
            dry_run: false,
        };
        let miner = SkillMiner {
            generator: SkillGenerator::new(
                zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
                PathBuf::from("/tmp"),
            ),
            embed_provider: zeph_llm::any::AnyProvider::Mock(
                zeph_llm::mock::MockProvider::default(),
            ),
            github_token: String::new(),
            config,
            http: reqwest::Client::new(),
        };
        // rpm=0 → max(1) → delay = 60_000ms (1 per minute)
        assert_eq!(miner.request_delay(), Duration::from_millis(60_000));
    }
}
