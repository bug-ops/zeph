// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::{Agent, Channel};
use std::sync::OnceLock;

/// Minimum evidence count required before a preference is emitted.
pub(super) const MIN_EVIDENCE: i64 = 3;
/// Minimum confidence threshold for persisting a preference.
pub(super) const PERSIST_THRESHOLD: f64 = 0.7;
/// Maximum number of new corrections to process per analysis run.
const CORRECTIONS_BATCH: u32 = 50;
/// Maximum number of preferences injected into the system prompt.
const MAX_INJECTED_PREFS: usize = 3;

/// A preference inferred from user corrections.
#[derive(Debug, PartialEq)]
pub(super) struct InferredPreference {
    pub key: String,
    pub value: String,
    pub confidence: f64,
    pub evidence_count: i64,
}

static CONCISE_RE: OnceLock<regex::Regex> = OnceLock::new();
static VERBOSE_RE: OnceLock<regex::Regex> = OnceLock::new();
static BULLET_RE: OnceLock<regex::Regex> = OnceLock::new();
static NO_MD_RE: OnceLock<regex::Regex> = OnceLock::new();
static HEADERS_RE: OnceLock<regex::Regex> = OnceLock::new();
static CODE_ONLY_RE: OnceLock<regex::Regex> = OnceLock::new();
static LANG_RE: OnceLock<regex::Regex> = OnceLock::new();

fn correction_weight(kind: &str) -> i64 {
    if kind == "alternative_request" { 2 } else { 1 }
}

struct EvidenceCounts {
    concise: i64,
    verbose: i64,
    bullet: i64,
    no_md: i64,
    headers: i64,
    code_only: i64,
    lang: std::collections::HashMap<String, i64>,
}

fn count_evidence(
    corrections: &[zeph_memory::store::corrections::UserCorrectionRow],
) -> EvidenceCounts {
    let concise_re = CONCISE_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(too\s+long|too\s+verbose|be\s+concise|be\s+brief|shorter\s+response|more\s+concise|less\s+verbose|tldr|tl;dr)\b",
        )
        .expect("static regex")
    });
    let verbose_re = VERBOSE_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(more\s+detail|explain\s+more|elaborate|expand\s+on|give\s+more\s+context)\b",
        )
        .expect("static regex")
    });
    let bullet_re = BULLET_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(use\s+bullet\s+points?|bullet\s+list|as\s+a\s+list)\b")
            .expect("static regex")
    });
    let no_md_re = NO_MD_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(no\s+markdown|plain\s+text|without\s+markdown|remove\s+formatting)\b",
        )
        .expect("static regex")
    });
    let headers_re = HEADERS_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(use\s+headers?|add\s+headers?|with\s+headers?)\b")
            .expect("static regex")
    });
    let code_only_re = CODE_ONLY_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(code\s+only|just\s+the\s+code|only\s+code|no\s+explanation)\b")
            .expect("static regex")
    });
    let lang_re = LANG_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(respond|answer|reply|write|speak)\s+in\s+([a-z]+)\b")
            .expect("static regex")
    });

    let mut counts = EvidenceCounts {
        concise: 0,
        verbose: 0,
        bullet: 0,
        no_md: 0,
        headers: 0,
        code_only: 0,
        lang: std::collections::HashMap::new(),
    };

    for row in corrections {
        if row.correction_kind == "self_correction" {
            continue;
        }
        let text = &row.correction_text;
        let w = correction_weight(&row.correction_kind);

        if concise_re.is_match(text) {
            counts.concise += w;
        }
        if verbose_re.is_match(text) {
            counts.verbose += w;
        }
        if bullet_re.is_match(text) {
            counts.bullet += w;
        }
        if no_md_re.is_match(text) {
            counts.no_md += w;
        }
        if headers_re.is_match(text) {
            counts.headers += w;
        }
        if code_only_re.is_match(text) {
            counts.code_only += w;
        }
        if let Some(caps) = lang_re.captures(text) {
            let lang = caps[2].to_lowercase();
            *counts.lang.entry(lang).or_default() += w;
        }
    }
    counts
}

/// Infer user preferences from a batch of correction rows.
///
/// Scans `correction_text` for recognizable patterns.
/// Rows with `correction_kind == "self_correction"` are skipped.
///
/// Returns at most one `InferredPreference` per preference category; the
/// caller is responsible for merging across batches via UPSERT semantics.
pub(super) fn infer_preferences(
    corrections: &[zeph_memory::store::corrections::UserCorrectionRow],
) -> Vec<InferredPreference> {
    let c = count_evidence(corrections);
    let mut out = Vec::new();

    // Verbosity: require 3:1 dominance and minimum evidence.
    // Allow precision loss: evidence counts fit easily in f64 mantissa at realistic values.
    #[allow(clippy::cast_precision_loss)]
    if c.concise >= MIN_EVIDENCE && c.concise >= c.verbose * 3 {
        let total = c.concise + c.verbose;
        out.push(InferredPreference {
            key: "verbosity".to_string(),
            value: "concise".to_string(),
            confidence: c.concise as f64 / total as f64,
            evidence_count: c.concise,
        });
    } else if c.verbose >= MIN_EVIDENCE && c.verbose >= c.concise * 3 {
        #[allow(clippy::cast_precision_loss)]
        let total = c.concise + c.verbose;
        out.push(InferredPreference {
            key: "verbosity".to_string(),
            value: "verbose".to_string(),
            confidence: c.verbose as f64 / total as f64,
            evidence_count: c.verbose,
        });
    }

    // Format: pick the dominant format signal.
    let format_candidates = [
        ("bullet points", c.bullet),
        ("no markdown", c.no_md),
        ("use headers", c.headers),
        ("code only", c.code_only),
    ];
    if let Some((value, evidence)) = format_candidates
        .iter()
        .filter(|(_, e)| *e >= MIN_EVIDENCE)
        .max_by_key(|(_, e)| *e)
    {
        #[allow(clippy::cast_precision_loss)]
        let conf = (*evidence as f64 / (*evidence as f64 + 1.0)).min(0.95);
        out.push(InferredPreference {
            key: "format_preference".to_string(),
            value: (*value).to_string(),
            confidence: conf,
            evidence_count: *evidence,
        });
    }

    // Language: most-mentioned explicit language with minimum evidence.
    if let Some((lang, &count)) = c.lang.iter().max_by_key(|(_, v)| *v)
        && count >= MIN_EVIDENCE
    {
        #[allow(clippy::cast_precision_loss)]
        let conf = (count as f64 / (count as f64 + 1.0)).min(0.95);
        out.push(InferredPreference {
            key: "response_language".to_string(),
            value: lang.clone(),
            confidence: conf,
            evidence_count: count,
        });
    }

    out
}

impl<C: Channel> Agent<C> {
    /// Run one preference analysis cycle.
    ///
    /// Loads corrections stored since the last watermark, infers preferences,
    /// and persists high-confidence ones to the `learned_preferences` table.
    /// The watermark (`last_analyzed_correction_id`) is advanced so the same
    /// corrections are never processed twice.
    pub(crate) async fn analyze_and_learn(&mut self) {
        if !self.learning_engine.should_analyze() {
            return;
        }
        let Some(memory) = &self.memory_state.memory else {
            self.learning_engine.mark_analyzed();
            return;
        };
        let after_id = self.learning_engine.last_analyzed_correction_id;
        let corrections = match memory
            .sqlite()
            .load_corrections_after(after_id, CORRECTIONS_BATCH)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("learning engine: failed to load corrections: {e:#}");
                self.learning_engine.mark_analyzed();
                return;
            }
        };

        if corrections.is_empty() {
            self.learning_engine.mark_analyzed();
            return;
        }

        // Advance watermark to the highest id in this batch.
        if let Some(max_id) = corrections.iter().map(|r| r.id).max() {
            self.learning_engine.last_analyzed_correction_id = max_id;
        }

        let preferences = infer_preferences(&corrections);

        for pref in preferences
            .iter()
            .filter(|p| p.confidence >= PERSIST_THRESHOLD)
        {
            if let Err(e) = memory
                .sqlite()
                .upsert_learned_preference(
                    &pref.key,
                    &pref.value,
                    pref.confidence,
                    pref.evidence_count,
                )
                .await
            {
                tracing::warn!(key = %pref.key, "learning engine: failed to persist preference: {e:#}");
            }
        }

        if !preferences.is_empty() {
            tracing::info!(
                count = preferences.len(),
                watermark = self.learning_engine.last_analyzed_correction_id,
                "learning engine: analyzed corrections, persisted preferences"
            );
        }

        self.learning_engine.mark_analyzed();
    }

    /// Load high-confidence learned preferences and inject them into the
    /// system prompt after the `<!-- cache:volatile -->` marker.
    pub(crate) async fn inject_learned_preferences(&self, prompt: &mut String) {
        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        let prefs = match memory.sqlite().load_learned_preferences().await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("learning engine: failed to load preferences for injection: {e:#}");
                return;
            }
        };

        let high_confidence: Vec<_> = prefs
            .into_iter()
            .filter(|p| p.confidence >= PERSIST_THRESHOLD)
            // TODO(skill-affinity): implement when skill_outcomes tracking is wired
            .take(MAX_INJECTED_PREFS)
            .collect();

        if high_confidence.is_empty() {
            return;
        }

        prompt.push_str("\n\n## Learned User Preferences\n");
        for pref in &high_confidence {
            // Sanitize value to prevent prompt injection via embedded newlines.
            let sanitized_value = pref.preference_value.replace(['\n', '\r'], " ");
            prompt.push_str("- ");
            prompt.push_str(&pref.preference_key);
            prompt.push_str(": ");
            prompt.push_str(&sanitized_value);
            prompt.push('\n');
        }
    }
}
