// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use thiserror::Error;

#[derive(Debug, Error)]
#[error("daily budget exhausted: spent {spent_cents:.2} / {budget_cents:.2} cents")]
pub struct BudgetExhausted {
    pub spent_cents: f64,
    pub budget_cents: f64,
}

/// Per-provider usage and cost breakdown for the current session/day.
#[derive(Debug, Clone, Default)]
pub struct ProviderUsage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub cost_cents: f64,
    pub request_count: u64,
    /// Last model seen for this provider (informational only — may change per-call).
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub prompt_cents_per_1k: f64,
    pub completion_cents_per_1k: f64,
    /// Cache read (cache hit) price. Claude: 10% of prompt; `OpenAI`: 50%; others: 0%.
    pub cache_read_cents_per_1k: f64,
    /// Cache write (cache creation) price. Claude: 125% of prompt; others: 0%.
    pub cache_write_cents_per_1k: f64,
}

struct CostState {
    spent_cents: f64,
    day: u32,
    providers: HashMap<String, ProviderUsage>,
}

pub struct CostTracker {
    pricing: HashMap<String, ModelPricing>,
    state: Arc<Mutex<CostState>>,
    max_daily_cents: f64,
    enabled: bool,
}

fn current_day() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // UTC day number (days since epoch)
    u32::try_from(secs / 86_400).unwrap_or(0)
}

fn claude_pricing(prompt: f64, completion: f64) -> ModelPricing {
    ModelPricing {
        prompt_cents_per_1k: prompt,
        completion_cents_per_1k: completion,
        // Claude: cache read = 10% of prompt, cache write = 125% of prompt
        cache_read_cents_per_1k: prompt * 0.1,
        cache_write_cents_per_1k: prompt * 1.25,
    }
}

fn openai_pricing(prompt: f64, completion: f64) -> ModelPricing {
    ModelPricing {
        prompt_cents_per_1k: prompt,
        completion_cents_per_1k: completion,
        // OpenAI: cache read = 50% of prompt, no cache write charge
        cache_read_cents_per_1k: prompt * 0.5,
        cache_write_cents_per_1k: 0.0,
    }
}

fn default_pricing() -> HashMap<String, ModelPricing> {
    let mut m = HashMap::new();
    // Claude 4 (sonnet-4 / opus-4 base releases)
    m.insert("claude-sonnet-4-20250514".into(), claude_pricing(0.3, 1.5));
    m.insert("claude-opus-4-20250514".into(), claude_pricing(1.5, 7.5));
    // Claude 4.1 Opus ($15/$75 per 1M tokens)
    m.insert("claude-opus-4-1-20250805".into(), claude_pricing(1.5, 7.5));
    // Claude 4.5 family
    m.insert("claude-haiku-4-5-20251001".into(), claude_pricing(0.1, 0.5));
    m.insert(
        "claude-sonnet-4-5-20250929".into(),
        claude_pricing(0.3, 1.5),
    );
    m.insert("claude-opus-4-5-20251101".into(), claude_pricing(0.5, 2.5));
    // Claude 4.6 family
    m.insert("claude-sonnet-4-6".into(), claude_pricing(0.3, 1.5));
    m.insert("claude-opus-4-6".into(), claude_pricing(0.5, 2.5));
    // OpenAI
    m.insert("gpt-4o".into(), openai_pricing(0.25, 1.0));
    m.insert("gpt-4o-mini".into(), openai_pricing(0.015, 0.06));
    // GPT-5 family ($1.25/$10 per 1M tokens)
    m.insert("gpt-5".into(), openai_pricing(0.125, 1.0));
    // GPT-5 mini ($0.25/$2 per 1M tokens)
    m.insert("gpt-5-mini".into(), openai_pricing(0.025, 0.2));
    m
}

fn reset_if_new_day(state: &mut CostState) {
    let today = current_day();
    if state.day != today {
        state.spent_cents = 0.0;
        state.day = today;
        state.providers.clear();
    }
}

impl CostTracker {
    #[must_use]
    pub fn new(enabled: bool, max_daily_cents: f64) -> Self {
        Self {
            pricing: default_pricing(),
            state: Arc::new(Mutex::new(CostState {
                spent_cents: 0.0,
                day: current_day(),
                providers: HashMap::new(),
            })),
            max_daily_cents,
            enabled,
        }
    }

    #[must_use]
    pub fn with_pricing(mut self, model: &str, pricing: ModelPricing) -> Self {
        self.pricing.insert(model.to_owned(), pricing);
        self
    }

    /// Record token usage for a single LLM call, attributed to `provider_name`.
    ///
    /// `provider_kind` must be the value returned by `AnyProvider::provider_kind_str()`:
    /// `"ollama"` or `"candle"` for local providers, `"cloud"` for API providers.
    /// Local providers always have zero cost by design; the missing-pricing WARN is
    /// suppressed for them to avoid log floods on every Ollama call.
    ///
    /// Cache token counts are optional (pass 0 when not available). Cost is computed
    /// using model-specific pricing including cache read/write rates.
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage(
        &self,
        provider_name: &str,
        provider_kind: &str,
        model: &str,
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
    ) {
        if !self.enabled {
            return;
        }
        let pricing = if let Some(p) = self.pricing.get(model).cloned() {
            p
        } else {
            let is_local = matches!(provider_kind, "ollama" | "candle" | "local");
            if is_local {
                tracing::debug!(model, "local model; cost recorded as zero");
            } else {
                tracing::warn!(
                    model,
                    "model not found in pricing table; cost recorded as zero"
                );
            }
            ModelPricing {
                prompt_cents_per_1k: 0.0,
                completion_cents_per_1k: 0.0,
                cache_read_cents_per_1k: 0.0,
                cache_write_cents_per_1k: 0.0,
            }
        };
        #[allow(clippy::cast_precision_loss)]
        let cost = pricing.prompt_cents_per_1k * (input_tokens as f64) / 1000.0
            + pricing.completion_cents_per_1k * (output_tokens as f64) / 1000.0
            + pricing.cache_read_cents_per_1k * (cache_read_tokens as f64) / 1000.0
            + pricing.cache_write_cents_per_1k * (cache_write_tokens as f64) / 1000.0;

        let mut state = self.state.lock();
        reset_if_new_day(&mut state);
        state.spent_cents += cost;

        let entry = state.providers.entry(provider_name.to_owned()).or_default();
        entry.input_tokens += input_tokens;
        entry.cache_read_tokens += cache_read_tokens;
        entry.cache_write_tokens += cache_write_tokens;
        entry.output_tokens += output_tokens;
        entry.cost_cents += cost;
        entry.request_count += 1;
        model.clone_into(&mut entry.model);
    }

    /// # Errors
    ///
    /// Returns `BudgetExhausted` when daily spend exceeds the configured limit.
    pub fn check_budget(&self) -> Result<(), BudgetExhausted> {
        if !self.enabled {
            return Ok(());
        }
        let mut state = self.state.lock();
        reset_if_new_day(&mut state);
        if self.max_daily_cents > 0.0 && state.spent_cents >= self.max_daily_cents {
            return Err(BudgetExhausted {
                spent_cents: state.spent_cents,
                budget_cents: self.max_daily_cents,
            });
        }
        Ok(())
    }

    /// Returns the configured daily budget in cents. Zero means unlimited.
    #[must_use]
    pub fn max_daily_cents(&self) -> f64 {
        self.max_daily_cents
    }

    #[must_use]
    pub fn current_spend(&self) -> f64 {
        let state = self.state.lock();
        state.spent_cents
    }

    /// Returns per-provider breakdown sorted by cost descending.
    #[must_use]
    pub fn provider_breakdown(&self) -> Vec<(String, ProviderUsage)> {
        let state = self.state.lock();
        let mut breakdown: Vec<(String, ProviderUsage)> = state
            .providers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        breakdown.sort_by(|a, b| {
            b.1.cost_cents
                .partial_cmp(&a.1.cost_cents)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        breakdown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tracker: &CostTracker, provider: &str, model: &str, input: u64, output: u64) {
        tracker.record_usage(provider, "cloud", model, input, 0, 0, output);
    }

    #[test]
    fn cost_tracker_records_usage_and_calculates_cost() {
        let tracker = CostTracker::new(true, 1000.0);
        record(&tracker, "openai", "gpt-4o", 1000, 1000);
        // 0.25 + 1.0 = 1.25
        let spend = tracker.current_spend();
        assert!((spend - 1.25).abs() < 0.001);
    }

    #[test]
    fn check_budget_passes_when_under_limit() {
        let tracker = CostTracker::new(true, 100.0);
        record(&tracker, "openai", "gpt-4o-mini", 100, 100);
        assert!(tracker.check_budget().is_ok());
    }

    #[test]
    fn check_budget_fails_when_over_limit() {
        let tracker = CostTracker::new(true, 0.01);
        record(&tracker, "claude", "claude-opus-4-20250514", 10000, 10000);
        assert!(tracker.check_budget().is_err());
    }

    #[test]
    fn daily_reset_clears_spending() {
        let tracker = CostTracker::new(true, 100.0);
        record(&tracker, "openai", "gpt-4o", 1000, 1000);
        assert!(tracker.current_spend() > 0.0);
        // Simulate day change
        {
            let mut state = tracker.state.lock();
            state.day = 0; // force a past day
        }
        // check_budget should reset
        assert!(tracker.check_budget().is_ok());
        assert!((tracker.current_spend() - 0.0).abs() < 0.001);
    }

    #[test]
    fn daily_reset_clears_provider_breakdown() {
        let tracker = CostTracker::new(true, 100.0);
        record(&tracker, "openai", "gpt-4o", 1000, 1000);
        assert!(!tracker.provider_breakdown().is_empty());
        // Simulate day change
        {
            let mut state = tracker.state.lock();
            state.day = 0;
        }
        assert!(tracker.check_budget().is_ok());
        assert!(tracker.provider_breakdown().is_empty());
    }

    #[test]
    fn ollama_zero_cost() {
        let tracker = CostTracker::new(true, 100.0);
        record(&tracker, "ollama", "llama3:8b", 10000, 10000);
        assert!((tracker.current_spend() - 0.0).abs() < 0.001);
    }

    #[test]
    fn ollama_unknown_model_no_warn_no_panic() {
        // Local providers should silently record zero cost for unknown models.
        let tracker = CostTracker::new(true, 100.0);
        tracker.record_usage(
            "local",
            "ollama",
            "totally-unknown-ollama-model",
            5000,
            0,
            0,
            5000,
        );
        assert!((tracker.current_spend() - 0.0).abs() < 0.001);
    }

    #[test]
    fn cloud_unknown_model_still_records_zero_cost() {
        // Cloud providers record zero cost for unknown models (WARN emitted separately).
        let tracker = CostTracker::new(true, 100.0);
        tracker.record_usage(
            "openai",
            "cloud",
            "totally-unknown-cloud-model",
            5000,
            0,
            0,
            5000,
        );
        assert!((tracker.current_spend() - 0.0).abs() < 0.001);
    }

    #[test]
    fn unknown_model_zero_cost() {
        let tracker = CostTracker::new(true, 100.0);
        record(&tracker, "unknown", "totally-unknown-model", 5000, 5000);
        assert!((tracker.current_spend() - 0.0).abs() < 0.001);
    }

    #[test]
    fn known_claude_model_has_nonzero_cost() {
        let tracker = CostTracker::new(true, 1000.0);
        record(&tracker, "claude", "claude-haiku-4-5-20251001", 1000, 1000);
        assert!(tracker.current_spend() > 0.0);
    }

    #[test]
    fn gpt5_pricing_is_correct() {
        let tracker = CostTracker::new(true, 1000.0);
        record(&tracker, "openai", "gpt-5", 1000, 1000);
        // 0.125 + 1.0 = 1.125
        let spend = tracker.current_spend();
        assert!((spend - 1.125).abs() < 0.001);
    }

    #[test]
    fn gpt5_mini_pricing_is_correct() {
        let tracker = CostTracker::new(true, 1000.0);
        record(&tracker, "openai", "gpt-5-mini", 1000, 1000);
        // 0.025 + 0.2 = 0.225
        let spend = tracker.current_spend();
        assert!((spend - 0.225).abs() < 0.001);
    }

    #[test]
    fn disabled_tracker_always_passes() {
        let tracker = CostTracker::new(false, 0.0);
        record(
            &tracker,
            "claude",
            "claude-opus-4-20250514",
            1_000_000,
            1_000_000,
        );
        assert!(tracker.check_budget().is_ok());
        assert!((tracker.current_spend() - 0.0).abs() < 0.001);
    }

    #[test]
    fn check_budget_unlimited_when_max_daily_cents_is_zero() {
        let tracker = CostTracker::new(true, 0.0);
        record(
            &tracker,
            "claude",
            "claude-opus-4-20250514",
            100_000,
            100_000,
        );
        assert!(tracker.check_budget().is_ok());
    }

    #[test]
    fn per_provider_accumulation() {
        let tracker = CostTracker::new(true, 1000.0);
        record(&tracker, "claude", "claude-haiku-4-5-20251001", 1000, 500);
        record(&tracker, "openai", "gpt-4o", 2000, 1000);
        record(&tracker, "claude", "claude-haiku-4-5-20251001", 500, 200);

        let breakdown = tracker.provider_breakdown();
        assert_eq!(breakdown.len(), 2);

        let claude = breakdown.iter().find(|(n, _)| n == "claude").unwrap();
        assert_eq!(claude.1.request_count, 2);
        assert_eq!(claude.1.input_tokens, 1500);
        assert_eq!(claude.1.output_tokens, 700);

        let openai = breakdown.iter().find(|(n, _)| n == "openai").unwrap();
        assert_eq!(openai.1.request_count, 1);
        assert_eq!(openai.1.input_tokens, 2000);
    }

    #[test]
    fn provider_breakdown_sorted_by_cost_desc() {
        let tracker = CostTracker::new(true, 1000.0);
        // gpt-4o: cheap; claude-opus: expensive
        record(&tracker, "cheap", "gpt-4o-mini", 100, 100);
        record(&tracker, "expensive", "claude-opus-4-20250514", 10000, 5000);

        let breakdown = tracker.provider_breakdown();
        assert_eq!(breakdown[0].0, "expensive");
    }

    #[test]
    fn cache_tokens_included_in_cost() {
        let tracker = CostTracker::new(true, 1000.0);
        // claude-haiku prompt=0.1, cache_read=0.01 per 1k
        // 1000 cache_read tokens = 0.01 cents; 0 input/output for isolation
        tracker.record_usage(
            "claude",
            "cloud",
            "claude-haiku-4-5-20251001",
            0,
            1000,
            0,
            0,
        );
        let spend = tracker.current_spend();
        assert!(spend > 0.0, "cache read should contribute to cost");
    }

    #[test]
    fn cache_write_cost_included_in_total() {
        let tracker = CostTracker::new(true, 1000.0);
        // Claude pricing: cache_write = 125% of prompt price
        // claude-opus-4-6: prompt = 0.5 cents/1k
        // 1000 cache_write tokens = (0.5 * 1.25 * 1000) / 1000 = 0.625 cents
        tracker.record_usage("claude-provider", "cloud", "claude-opus-4-6", 0, 0, 1000, 0);
        let cost = tracker.current_spend();
        assert!((cost - 0.625).abs() < 0.001);
    }

    #[test]
    fn provider_breakdown_empty_when_disabled() {
        let tracker = CostTracker::new(false, 100.0);
        tracker.record_usage(
            "claude",
            "cloud",
            "claude-haiku-4-5-20251001",
            1000,
            0,
            0,
            1000,
        );
        assert!(tracker.provider_breakdown().is_empty());
    }
}
