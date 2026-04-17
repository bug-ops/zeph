// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `AdaptOrch` — bandit-driven topology advisor for the LLM planner.
//!
//! [`TopologyAdvisor`] runs before [`crate::planner::LlmPlanner`] and injects a
//! soft topology hint into the planner system prompt. A 16-arm Thompson Beta-bandit
//! (4 task classes × 4 topology hints) learns which hint works best for each class.
//!
//! State is persisted on shutdown alongside the Thompson router state; `record_outcome`
//! is synchronous and never spawns a task.

use std::collections::HashMap;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use rand::SeedableRng as _;
use rand_distr::{Beta, Distribution};
use serde::{Deserialize, Serialize};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, Role};

/// Task decomposition shape inferred from the user goal text.
///
/// `Unknown` absorbs all unclassified cases and defaults the hint to [`TopologyHint::Hybrid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskClass {
    /// Fan-out work with no cross-dependencies (research, comparisons, multi-source queries).
    IndependentBatch,
    /// Strict ordering: build → test → deploy, ETL pipelines.
    SequentialPipeline,
    /// Tree decomposition: subgoal expansion, recursive analysis.
    HierarchicalDecomp,
    /// Unknown / fallback; defaults hint to `Hybrid`.
    Unknown,
}

/// Soft topology hint injected into the planner system prompt.
///
/// Advisory only — `TopologyClassifier::analyze` still runs on the produced graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyHint {
    /// Maximize independent tasks; avoid unnecessary `depends_on` chains.
    Parallel,
    /// Prefer a strict linear chain unless impossible.
    Sequential,
    /// Decompose into subgoals; expect 2–3 levels of depth.
    Hierarchical,
    /// No constraint (free planning). Default for `Unknown` class.
    Hybrid,
}

impl TopologyHint {
    /// One-sentence injection appended to the planner system prompt.
    /// Returns `None` for `Hybrid` (no injection).
    #[must_use]
    pub fn prompt_sentence(self) -> Option<&'static str> {
        match self {
            Self::Parallel => {
                Some("Prefer maximizing parallel tasks; avoid unnecessary `depends_on` chains.")
            }
            Self::Sequential => Some(
                "This goal is naturally a pipeline; produce a strict linear chain unless \
                 impossible.",
            ),
            Self::Hierarchical => {
                Some("Decompose this goal into subgoals; expect 2–3 levels of depth.")
            }
            Self::Hybrid => None,
        }
    }
}

/// Result of a `TopologyAdvisor::recommend` call.
#[derive(Debug, Clone)]
pub struct AdvisorVerdict {
    /// Inferred task class for the goal.
    pub class: TaskClass,
    /// Sampled topology hint.
    pub hint: TopologyHint,
    /// `true` if Thompson exploited the best-known arm (vs. explored).
    pub exploit: bool,
    /// `true` if classification failed and `Hybrid` was used as the default.
    pub fallback: bool,
}

/// Per-(class, hint) arm for the Beta-Thompson bandit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BetaDist {
    pub alpha: f64,
    pub beta: f64,
}

impl Default for BetaDist {
    fn default() -> Self {
        Self {
            alpha: 1.0,
            beta: 1.0,
        }
    }
}

impl BetaDist {
    fn sample<R: rand::Rng>(&self, rng: &mut R) -> f64 {
        let a = self.alpha.max(1e-6);
        let b = self.beta.max(1e-6);
        // Safety: a and b are clamped to ≥1e-6, so Beta::new never fails.
        Beta::new(a, b)
            .expect("clamped values ≥1e-6 are always valid Beta params")
            .sample(rng)
    }
}

/// Versioned on-disk format for `AdaptOrch` state.
#[derive(Debug, Serialize, Deserialize)]
struct PersistState {
    version: u32,
    arms: HashMap<String, BetaDist>,
}

/// Session-level metrics for `AdaptOrch` (atomic, not persisted).
#[derive(Debug, Default)]
pub struct AdaptOrchMetrics {
    /// Total classify calls.
    pub classify_calls: AtomicU64,
    /// Calls that timed out or failed — fell back to `Unknown`.
    pub classify_timeouts: AtomicU64,
    /// Hint distribution.
    pub hint_parallel: AtomicU64,
    pub hint_sequential: AtomicU64,
    pub hint_hierarchical: AtomicU64,
    pub hint_hybrid: AtomicU64,
    /// Times `record_outcome` was called.
    pub outcomes_recorded: AtomicU64,
}

fn arm_key(class: TaskClass, hint: TopologyHint) -> String {
    let c = match class {
        TaskClass::IndependentBatch => "independent_batch",
        TaskClass::SequentialPipeline => "sequential_pipeline",
        TaskClass::HierarchicalDecomp => "hierarchical_decomp",
        TaskClass::Unknown => "unknown",
    };
    let h = match hint {
        TopologyHint::Parallel => "parallel",
        TopologyHint::Sequential => "sequential",
        TopologyHint::Hierarchical => "hierarchical",
        TopologyHint::Hybrid => "hybrid",
    };
    format!("{c}:{h}")
}

const ALL_HINTS: [TopologyHint; 4] = [
    TopologyHint::Parallel,
    TopologyHint::Sequential,
    TopologyHint::Hierarchical,
    TopologyHint::Hybrid,
];

/// Bandit-driven topology advisor.
///
/// Classifies the user goal into a [`TaskClass`] via a cheap LLM call, samples
/// the best [`TopologyHint`] for that class via Thompson sampling, and injects
/// one sentence into the planner system prompt. Outcomes are recorded synchronously
/// and persisted once on shutdown.
pub struct TopologyAdvisor {
    classifier: Arc<AnyProvider>,
    arms: Arc<Mutex<HashMap<(TaskClass, TopologyHint), BetaDist>>>,
    state_path: PathBuf,
    classify_timeout: Duration,
    pub metrics: Arc<AdaptOrchMetrics>,
    rng: Arc<Mutex<rand::rngs::SmallRng>>,
}

impl std::fmt::Debug for TopologyAdvisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TopologyAdvisor")
            .field("state_path", &self.state_path)
            .field("classify_timeout", &self.classify_timeout)
            .finish_non_exhaustive()
    }
}

impl TopologyAdvisor {
    /// Construct a new advisor. Loads persisted state from `state_path` if present.
    ///
    /// When `state_path` is an empty string, the default path
    /// `~/.zeph/adaptorch_state.json` is used.
    #[must_use]
    pub fn new(
        classifier: Arc<AnyProvider>,
        state_path: impl Into<PathBuf>,
        classify_timeout: Duration,
    ) -> Self {
        let path: PathBuf = {
            let p = state_path.into();
            if p.as_os_str().is_empty() {
                Self::default_path()
            } else {
                p
            }
        };
        let arms = load_arms(&path);
        Self {
            classifier,
            arms: Arc::new(Mutex::new(arms)),
            state_path: path,
            classify_timeout,
            metrics: Arc::new(AdaptOrchMetrics::default()),
            rng: Arc::new(Mutex::new(rand::rngs::SmallRng::from_rng(&mut rand::rng()))),
        }
    }

    /// Default persistence path: `~/.zeph/adaptorch_state.json`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".zeph")
            .join("adaptorch_state.json")
    }

    /// Classify the goal and sample the best topology hint for this turn.
    ///
    /// Classification failures fall back to `TaskClass::Unknown` + `TopologyHint::Hybrid`.
    pub async fn recommend(&self, goal: &str) -> AdvisorVerdict {
        self.metrics.classify_calls.fetch_add(1, Ordering::Relaxed);

        let class = tokio::time::timeout(self.classify_timeout, self.classify(goal))
            .await
            .unwrap_or_else(|_| {
                self.metrics
                    .classify_timeouts
                    .fetch_add(1, Ordering::Relaxed);
                TaskClass::Unknown
            });

        let fallback = class == TaskClass::Unknown;
        let (hint, exploit) = self.sample_arm(class);

        match hint {
            TopologyHint::Parallel => {
                self.metrics.hint_parallel.fetch_add(1, Ordering::Relaxed);
            }
            TopologyHint::Sequential => {
                self.metrics.hint_sequential.fetch_add(1, Ordering::Relaxed);
            }
            TopologyHint::Hierarchical => {
                self.metrics
                    .hint_hierarchical
                    .fetch_add(1, Ordering::Relaxed);
            }
            TopologyHint::Hybrid => {
                self.metrics.hint_hybrid.fetch_add(1, Ordering::Relaxed);
            }
        }

        AdvisorVerdict {
            class,
            hint,
            exploit,
            fallback,
        }
    }

    /// Record the binary outcome of a plan guided by `(class, hint)`.
    ///
    /// **Synchronous** — acquires the in-memory `Mutex`, updates two `f64` counters, drops
    /// the guard. Never spawns. Never persists. Persistence happens in [`save`](Self::save).
    pub fn record_outcome(&self, class: TaskClass, hint: TopologyHint, reward: f64) {
        self.metrics
            .outcomes_recorded
            .fetch_add(1, Ordering::Relaxed);
        let key = (class, hint);
        let mut arms = self.arms.lock();
        let arm = arms.entry(key).or_default();
        if reward >= 1.0 {
            arm.alpha += 1.0;
        } else {
            arm.beta += 1.0;
        }
    }

    /// Persist the Beta-arm table to `state_path` atomically.
    ///
    /// Called from the agent shutdown hook (once per process), mirroring
    /// `AnyProvider::save_router_state`. Failures are logged and swallowed.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` when the write fails.
    pub fn save(&self) -> io::Result<()> {
        let arms_map: HashMap<String, BetaDist> = self
            .arms
            .lock()
            .iter()
            .map(|((class, hint), dist)| (arm_key(*class, *hint), dist.clone()))
            .collect();

        let state = PersistState {
            version: 1,
            arms: arms_map,
        };

        let json = serde_json::to_string_pretty(&state).map_err(io::Error::other)?;

        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        atomic_write(&self.state_path, json.as_bytes())?;
        Ok(())
    }

    // ─── private helpers ─────────────────────────────────────────────────────

    async fn classify(&self, goal: &str) -> TaskClass {
        let truncated: String = goal.chars().take(400).collect();
        let system = "\
You classify task decomposition patterns. Read the goal and answer with one of:\n\
- independent_batch  — fan-out work with no cross-deps (research, comparisons, multi-source queries)\n\
- sequential_pipeline — strict ordering (build → test → deploy, ETL)\n\
- hierarchical_decomp — tree of subgoals, divide-and-conquer\n\
- unknown            — does not clearly fit any of the above\n\n\
Respond with a single JSON object:\n\
{\"class\":\"...\",\"reason\":\"<one sentence>\"}";

        let messages = vec![
            Message::from_legacy(Role::System, system),
            Message::from_legacy(Role::User, format!("Goal:\n{truncated}")),
        ];

        let raw = match self.classifier.chat(&messages).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "adaptorch: classify call failed");
                return TaskClass::Unknown;
            }
        };

        parse_class(&raw)
    }

    fn sample_arm(&self, class: TaskClass) -> (TopologyHint, bool) {
        if class == TaskClass::Unknown {
            return (TopologyHint::Hybrid, false);
        }
        // Clone arm entries under arms lock, then release before acquiring rng lock.
        let arm_entries: Vec<(TopologyHint, BetaDist)> = {
            let arms = self.arms.lock();
            ALL_HINTS
                .iter()
                .map(|hint| {
                    (
                        *hint,
                        arms.get(&(class, *hint)).cloned().unwrap_or_default(),
                    )
                })
                .collect()
        };
        let mut rng = self.rng.lock();
        let scores: Vec<(TopologyHint, f64)> = arm_entries
            .iter()
            .map(|(hint, dist)| (*hint, dist.sample(&mut *rng)))
            .collect();

        let (hint, score) = scores
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map_or((TopologyHint::Hybrid, 0.0), |(h, s)| (*h, *s));

        // "exploit" = the arm's mean (alpha / (alpha+beta)) aligns with the sampled score
        let arm = arm_entries
            .iter()
            .find(|(h, _)| *h == hint)
            .map(|(_, d)| d.clone())
            .unwrap_or_default();
        let mean = arm.alpha / (arm.alpha + arm.beta);
        let exploit = (score - mean).abs() < 0.15;

        (hint, exploit)
    }
}

/// Parse the classifier's JSON response into a [`TaskClass`].
fn parse_class(raw: &str) -> TaskClass {
    // Try direct JSON parse first.
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw)
        && let Some(class) = val.get("class").and_then(|c| c.as_str())
    {
        return str_to_class(class);
    }
    // Extract first {...} substring.
    if let Some(start) = raw.find('{')
        && let Some(end) = raw[start..].find('}')
    {
        let chunk = &raw[start..=start + end];
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(chunk)
            && let Some(class) = val.get("class").and_then(|c| c.as_str())
        {
            return str_to_class(class);
        }
    }
    // Substring scan.
    for variant in &[
        "independent_batch",
        "sequential_pipeline",
        "hierarchical_decomp",
        "unknown",
    ] {
        if raw.contains(variant) {
            return str_to_class(variant);
        }
    }
    TaskClass::Unknown
}

fn str_to_class(s: &str) -> TaskClass {
    match s {
        "independent_batch" => TaskClass::IndependentBatch,
        "sequential_pipeline" => TaskClass::SequentialPipeline,
        "hierarchical_decomp" => TaskClass::HierarchicalDecomp,
        _ => TaskClass::Unknown,
    }
}

fn load_arms(path: &std::path::Path) -> HashMap<(TaskClass, TopologyHint), BetaDist> {
    let mut arms = default_arms();
    let Ok(data) = std::fs::read_to_string(path) else {
        return arms;
    };
    let Ok(state) = serde_json::from_str::<PersistState>(&data) else {
        tracing::warn!(path = %path.display(), "adaptorch: failed to parse state file, using defaults");
        return arms;
    };
    if state.version != 1 {
        tracing::warn!(
            version = state.version,
            "adaptorch: unknown state version, using defaults"
        );
        return arms;
    }
    for (key_str, dist) in state.arms {
        let mut parts = key_str.splitn(2, ':');
        let (Some(c), Some(h)) = (parts.next(), parts.next()) else {
            continue;
        };
        let class = str_to_class(c);
        let hint = match h {
            "parallel" => TopologyHint::Parallel,
            "sequential" => TopologyHint::Sequential,
            "hierarchical" => TopologyHint::Hierarchical,
            "hybrid" => TopologyHint::Hybrid,
            _ => continue,
        };
        arms.insert((class, hint), dist);
    }
    arms
}

fn default_arms() -> HashMap<(TaskClass, TopologyHint), BetaDist> {
    let classes = [
        TaskClass::IndependentBatch,
        TaskClass::SequentialPipeline,
        TaskClass::HierarchicalDecomp,
        TaskClass::Unknown,
    ];
    let mut map = HashMap::new();
    for class in classes {
        for hint in ALL_HINTS {
            map.insert((class, hint), BetaDist::default());
        }
    }
    map
}

fn atomic_write(path: &std::path::Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_class_direct_json() {
        let json = r#"{"class":"independent_batch","reason":"fan-out"}"#;
        assert_eq!(parse_class(json), TaskClass::IndependentBatch);
    }

    #[test]
    fn parse_class_fallback_substring() {
        assert_eq!(
            parse_class("  sequential_pipeline "),
            TaskClass::SequentialPipeline
        );
    }

    #[test]
    fn parse_class_unknown_for_garbage() {
        assert_eq!(parse_class("no idea"), TaskClass::Unknown);
    }

    #[test]
    fn topology_hint_sentence_hybrid_is_none() {
        assert!(TopologyHint::Hybrid.prompt_sentence().is_none());
    }

    #[test]
    fn record_outcome_updates_alpha_beta() {
        use std::sync::Arc;
        use zeph_llm::any::AnyProvider;
        let mock = zeph_llm::mock::MockProvider::default();
        let advisor = TopologyAdvisor::new(
            Arc::new(AnyProvider::Mock(mock)),
            PathBuf::new(),
            Duration::from_secs(4),
        );
        advisor.record_outcome(TaskClass::IndependentBatch, TopologyHint::Parallel, 1.0);
        advisor.record_outcome(TaskClass::IndependentBatch, TopologyHint::Parallel, 0.0);
        let arms = advisor.arms.lock();
        let arm = arms
            .get(&(TaskClass::IndependentBatch, TopologyHint::Parallel))
            .unwrap();
        assert!((arm.alpha - 2.0).abs() < f64::EPSILON);
        assert!((arm.beta - 2.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn recommend_with_valid_json_returns_correct_class() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        let mock = MockProvider::with_responses(vec![
            r#"{"class":"sequential_pipeline","reason":"strict ordering"}"#.into(),
        ]);
        let advisor = TopologyAdvisor::new(
            Arc::new(AnyProvider::Mock(mock)),
            PathBuf::new(),
            Duration::from_secs(4),
        );
        let verdict = advisor
            .recommend("Build, test, then deploy the service")
            .await;
        assert_eq!(verdict.class, TaskClass::SequentialPipeline);
        assert!(advisor.metrics.classify_timeouts.load(Ordering::Relaxed) == 0);
    }

    #[tokio::test]
    async fn recommend_timeout_returns_unknown_and_increments_metric() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        // Delay longer than classify_timeout so the call times out.
        let mut mock = MockProvider::default();
        mock.delay_ms = 200;
        mock.default_response = r#"{"class":"sequential_pipeline","reason":"x"}"#.into();
        let advisor = TopologyAdvisor::new(
            Arc::new(AnyProvider::Mock(mock)),
            PathBuf::new(),
            Duration::from_millis(50), // short timeout
        );
        let verdict = advisor.recommend("any goal").await;
        assert_eq!(verdict.class, TaskClass::Unknown);
        assert_eq!(advisor.metrics.classify_timeouts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn sample_arm_favours_reinforced_hint() {
        use zeph_llm::any::AnyProvider;
        let mock = zeph_llm::mock::MockProvider::default();
        let advisor = TopologyAdvisor::new(
            Arc::new(AnyProvider::Mock(mock)),
            PathBuf::new(),
            Duration::from_secs(4),
        );
        // Reinforce Sequential 20 times for SequentialPipeline class.
        for _ in 0..20 {
            advisor.record_outcome(TaskClass::SequentialPipeline, TopologyHint::Sequential, 1.0);
        }
        // Sample 50 times and verify Sequential wins most often.
        let mut counts = std::collections::HashMap::new();
        for _ in 0..50 {
            let (hint, _) = advisor.sample_arm(TaskClass::SequentialPipeline);
            *counts.entry(hint).or_insert(0u32) += 1;
        }
        let sequential_count = counts.get(&TopologyHint::Sequential).copied().unwrap_or(0);
        assert!(
            sequential_count > 30,
            "expected Sequential to win >30/50 times after reinforcement, got {sequential_count}"
        );
    }

    #[test]
    fn persistence_round_trip() {
        use zeph_llm::any::AnyProvider;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let mock = zeph_llm::mock::MockProvider::default();
            let advisor = TopologyAdvisor::new(
                Arc::new(AnyProvider::Mock(mock)),
                path.clone(),
                Duration::from_secs(4),
            );
            advisor.record_outcome(TaskClass::SequentialPipeline, TopologyHint::Sequential, 1.0);
            advisor.save().unwrap();
        }
        {
            let mock = zeph_llm::mock::MockProvider::default();
            let advisor = TopologyAdvisor::new(
                Arc::new(AnyProvider::Mock(mock)),
                path.clone(),
                Duration::from_secs(4),
            );
            let arms = advisor.arms.lock();
            let arm = arms
                .get(&(TaskClass::SequentialPipeline, TopologyHint::Sequential))
                .unwrap();
            // alpha was 1.0 (default) + 1 success = 2.0
            assert!((arm.alpha - 2.0).abs() < f64::EPSILON);
        }
    }
}
