// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;

use thiserror::Error;
use zeph_common::money::{Cents, CentsPer1k};
use zeph_config::ServiceSpeed;
use zeph_llm::provider::TokenUsage;

/// Daily spend cap: either unlimited or a limit in [`Cents`].
///
/// Replaces the `max_daily_cents > 0.0` sentinel check: `0` in config maps to
/// [`DailyBudget::Unlimited`] at the type boundary ([`DailyBudget::from_config`]).
///
/// # Examples
///
/// ```
/// use zeph_common::money::Cents;
/// use zeph_core::cost::DailyBudget;
///
/// assert_eq!(DailyBudget::from_config(0), DailyBudget::Unlimited);
/// let budget = DailyBudget::from_config(500);
/// assert_eq!(budget.remaining(Cents::new(120.0)), Some(Cents::new(380.0)));
/// assert!(budget.is_exhausted(Cents::new(500.0)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DailyBudget {
    /// No daily cap.
    Unlimited,
    /// Spending is blocked once the day's spend reaches this amount.
    Limited(Cents),
}

impl DailyBudget {
    /// Maps the `[cost] max_daily_cents` config value; `0` means unlimited.
    #[must_use]
    pub fn from_config(max_daily_cents: u32) -> Self {
        if max_daily_cents == 0 {
            Self::Unlimited
        } else {
            Self::Limited(Cents::new(f64::from(max_daily_cents)))
        }
    }

    /// The configured limit, or `None` when unlimited.
    #[must_use]
    pub fn limit(self) -> Option<Cents> {
        match self {
            Self::Unlimited => None,
            Self::Limited(limit) => Some(limit),
        }
    }

    /// Budget left after `spent`, floored at zero; `None` when unlimited.
    #[must_use]
    pub fn remaining(self, spent: Cents) -> Option<Cents> {
        self.limit().map(|limit| limit.saturating_sub(spent))
    }

    /// Whether `spent` has reached the limit. A non-positive limit is always exhausted.
    #[must_use]
    pub fn is_exhausted(self, spent: Cents) -> bool {
        match self {
            Self::Unlimited => false,
            // NaN limit compares as `None`, so it counts as exhausted (fail closed).
            Self::Limited(limit) => spent.partial_cmp(&limit) != Some(std::cmp::Ordering::Less),
        }
    }
}

/// Returned by [`CostTracker::check_budget`] when the daily budget is spent.
#[derive(Debug, Error)]
#[error(
    "daily budget exhausted: spent {:.2} / {:.2} cents \
     (raise or disable via `[cost] max_daily_cents` in config.toml; 0 = unlimited)",
    .spent_cents.as_f64(),
    .budget_cents.as_f64()
)]
pub struct BudgetExhausted {
    /// Spend so far today.
    pub spent_cents: Cents,
    /// The configured daily limit.
    pub budget_cents: Cents,
}

/// Per-provider usage and cost breakdown for the current session/day.
#[derive(Debug, Clone, Default)]
pub struct ProviderUsage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub cost_cents: Cents,
    pub request_count: u64,
    /// Last model seen for this provider (informational only — may change per-call).
    pub model: String,
}

/// Per-model token prices; every rate is in [`Cents`] per 1000 tokens ([`CentsPer1k`]).
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub prompt_cents_per_1k: CentsPer1k,
    pub completion_cents_per_1k: CentsPer1k,
    /// Cache read (cache hit) price. Claude: 10% of prompt; `OpenAI`: 50%; others: 0%.
    pub cache_read_cents_per_1k: CentsPer1k,
    /// Cache write (cache creation) price. Claude: 125% of prompt; others: 0%.
    pub cache_write_cents_per_1k: CentsPer1k,
}

struct CostState {
    spent_cents: Cents,
    session_cents: Cents,
    day: u32,
    providers: HashMap<String, ProviderUsage>,
    successful_tasks: u64,
}

pub struct CostTracker {
    pricing: HashMap<String, ModelPricing>,
    fast_pricing: HashMap<String, ModelPricing>,
    unpriced_models_warned: Mutex<HashSet<String>>,
    fast_unpriced_warned: Mutex<HashSet<String>>,
    state: Arc<Mutex<CostState>>,
    budget: DailyBudget,
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
    claude_pricing_with_cache_read(prompt, completion, 0.1)
}

/// Claude pricing with a model-specific cache-read ratio; cache write is 125% of prompt.
fn claude_pricing_with_cache_read(
    prompt: f64,
    completion: f64,
    cache_read_ratio: f64,
) -> ModelPricing {
    ModelPricing {
        prompt_cents_per_1k: CentsPer1k::new(prompt),
        completion_cents_per_1k: CentsPer1k::new(completion),
        cache_read_cents_per_1k: CentsPer1k::new(prompt * cache_read_ratio),
        cache_write_cents_per_1k: CentsPer1k::new(prompt * 1.25),
    }
}

/// Strips a trailing `-YYYYMMDD` snapshot suffix so a dated id resolves to its base entry.
fn strip_date_suffix(model: &str) -> &str {
    match model.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => model,
    }
}

fn openai_pricing(prompt: f64, completion: f64) -> ModelPricing {
    ModelPricing {
        prompt_cents_per_1k: CentsPer1k::new(prompt),
        completion_cents_per_1k: CentsPer1k::new(completion),
        // OpenAI: cache read = 50% of prompt, no cache write charge
        cache_read_cents_per_1k: CentsPer1k::new(prompt * 0.5),
        cache_write_cents_per_1k: CentsPer1k::new(0.0),
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
    // Claude 5 / Opus 4.8 family
    m.insert("claude-opus-4-8".into(), claude_pricing(0.5, 2.5));
    // Claude 5 family
    m.insert("claude-haiku-5-5".into(), claude_pricing(0.01, 0.05));
    m.insert("claude-sonnet-5".into(), claude_pricing(0.2, 1.0));
    m.insert("claude-sonnet-5-5".into(), claude_pricing(0.2, 1.0));
    m.insert("claude-opus-5".into(), claude_pricing(0.5, 2.5));
    m.insert(
        "claude-opus-5-5".into(),
        claude_pricing_with_cache_read(0.4, 2.0, 0.05),
    );
    m.insert("claude-fable-5".into(), claude_pricing(1.0, 5.0));
    m.insert(
        "claude-fable-5-1".into(),
        claude_pricing_with_cache_read(1.0, 5.0, 0.025),
    );
    m.insert(
        "claude-fable-5-2".into(),
        claude_pricing_with_cache_read(0.8, 4.0, 0.025),
    );
    m.insert("claude-mythos-5".into(), claude_pricing(1.0, 5.0));
    m.insert(
        "claude-mythos-5-1".into(),
        claude_pricing_with_cache_read(1.0, 5.0, 0.025),
    );
    m.insert(
        "claude-mythos-5-2".into(),
        claude_pricing_with_cache_read(0.8, 4.0, 0.025),
    );
    // OpenAI
    m.insert("gpt-4o".into(), openai_pricing(0.25, 1.0));
    m.insert("gpt-4o-mini".into(), openai_pricing(0.015, 0.06));
    // GPT-5 family ($1.25/$10 per 1M tokens)
    m.insert("gpt-5".into(), openai_pricing(0.125, 1.0));
    // GPT-5 mini ($0.25/$2 per 1M tokens)
    m.insert("gpt-5-mini".into(), openai_pricing(0.025, 0.2));
    m
}

/// Fast-mode price rows: twice the standard row of the same model; cache multipliers stack.
fn default_fast_pricing() -> HashMap<String, ModelPricing> {
    let mut m = HashMap::new();
    m.insert(
        "claude-opus-5-5".into(),
        claude_pricing_with_cache_read(0.8, 4.0, 0.05),
    );
    m.insert(
        "claude-fable-5-2".into(),
        claude_pricing_with_cache_read(1.6, 8.0, 0.025),
    );
    m.insert(
        "claude-mythos-5-2".into(),
        claude_pricing_with_cache_read(1.6, 8.0, 0.025),
    );
    m
}

fn reset_if_new_day(state: &mut CostState) {
    let today = current_day();
    if state.day != today {
        state.spent_cents = Cents::ZERO;
        state.day = today;
        state.providers.clear();
        state.successful_tasks = 0;
    }
}

impl CostTracker {
    /// Creates a tracker that enforces `budget` against today's accumulated spend.
    ///
    /// There is no enabled flag: a caller that does not want cost tracking simply does not
    /// build a tracker. A [`DailyBudget::Limited`] limit that is zero, negative or NaN counts
    /// as already exhausted, so [`check_budget`](Self::check_budget) fails closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::money::Cents;
    /// use zeph_core::cost::{CostTracker, DailyBudget};
    ///
    /// assert!(CostTracker::new(DailyBudget::Unlimited).check_budget().is_ok());
    /// assert!(CostTracker::new(DailyBudget::Limited(Cents::new(f64::NAN))).check_budget().is_err());
    /// ```
    #[must_use]
    pub fn new(budget: DailyBudget) -> Self {
        Self {
            pricing: default_pricing(),
            fast_pricing: default_fast_pricing(),
            unpriced_models_warned: Mutex::new(HashSet::new()),
            fast_unpriced_warned: Mutex::new(HashSet::new()),
            state: Arc::new(Mutex::new(CostState {
                spent_cents: Cents::ZERO,
                session_cents: Cents::ZERO,
                day: current_day(),
                providers: HashMap::new(),
                successful_tasks: 0,
            })),
            budget,
        }
    }

    #[must_use]
    pub fn with_pricing(mut self, model: &str, pricing: ModelPricing) -> Self {
        self.pricing.insert(model.to_owned(), pricing);
        self
    }

    /// Resolves `model` to its pricing entry, falling back to the id without a `-YYYYMMDD`
    /// snapshot suffix.
    fn pricing_for(&self, model: &str, speed: ServiceSpeed) -> Option<&ModelPricing> {
        let table = match speed {
            ServiceSpeed::Fast => &self.fast_pricing,
            ServiceSpeed::Standard => &self.pricing,
        };
        let found = table
            .get(model)
            .or_else(|| table.get(strip_date_suffix(model)));
        if found.is_none() && speed == ServiceSpeed::Fast {
            let mut warned = self.fast_unpriced_warned.lock();
            if !warned.contains(model) {
                warned.insert(model.to_owned());
                tracing::warn!(model, "no fast-mode price row; billing at standard price");
            }
            return self.pricing_for(model, ServiceSpeed::Standard);
        }
        found
    }

    /// Compute the cost for a single LLM call's token counts, without recording it.
    ///
    /// Pure pricing arithmetic, extracted from [`Self::record_usage`] (issue #6549) so the
    /// per-message usage ledger can compute the exact same cost as the live daily aggregate
    /// for the same call — single pricing source of truth. Replicates `record_usage`'s
    /// missing-model fallback: an unknown model prices at zero, silently. `record_usage`
    /// additionally emits a WARN/debug log for that case (it has `provider_kind`, which this
    /// pure signature does not); callers that want the log must check pricing themselves.
    #[must_use]
    pub fn price_of(&self, model: &str, usage: TokenUsage, speed: ServiceSpeed) -> Cents {
        let Some(pricing) = self.pricing_for(model, speed) else {
            return Cents::ZERO;
        };
        pricing.prompt_cents_per_1k.cost(usage.input)
            + pricing.completion_cents_per_1k.cost(usage.output)
            + pricing.cache_read_cents_per_1k.cost(usage.cache_read)
            + pricing.cache_write_cents_per_1k.cost(usage.cache_write)
    }

    /// Record auxiliary usage (for example Claude server-side compaction iterations) for the
    /// call just recorded with [`Self::record_usage`].
    ///
    /// Adds the tokens and cost to the provider's totals and the daily spend, without counting
    /// another request.
    pub fn record_auxiliary_usage(
        &self,
        provider_name: &str,
        provider_kind: &str,
        model: &str,
        usage: TokenUsage,
        speed: ServiceSpeed,
    ) {
        self.add_usage(provider_name, provider_kind, model, usage, speed, 0);
    }

    /// Record token usage for a single LLM call, attributed to `provider_name`.
    ///
    /// `provider_kind` must be the value returned by `AnyProvider::provider_kind_str()`:
    /// `"ollama"` or `"candle"` for local providers, `"cloud"` for API providers.
    /// Local providers always have zero cost by design; the missing-pricing WARN is
    /// suppressed for them to avoid log floods on every Ollama call.
    ///
    /// Cache token counts are zero when not available. Cost is computed using
    /// model-specific pricing including cache read/write rates.
    pub fn record_usage(
        &self,
        provider_name: &str,
        provider_kind: &str,
        model: &str,
        usage: TokenUsage,
        speed: ServiceSpeed,
    ) {
        self.add_usage(provider_name, provider_kind, model, usage, speed, 1);
    }

    /// Record the usage of a prompt-cache keepalive ping and return its cost in cents.
    ///
    /// The tokens and cost count toward the provider totals and the daily spend, but a ping is
    /// not a provider request, so `request_count` is unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::money::Cents;
    /// use zeph_core::cost::{CostTracker, DailyBudget};
    /// use zeph_llm::provider::{CacheUsage, ResponseUsage};
    ///
    /// let tracker = CostTracker::new(DailyBudget::Unlimited);
    /// let usage = ResponseUsage {
    ///     cache: Some(CacheUsage { creation: 0, read: 10_000 }),
    ///     ..ResponseUsage::default()
    /// };
    /// let cents = tracker.record_keepalive_usage("claude", "cloud", "claude-sonnet-4-6", usage);
    /// assert!(cents > Cents::ZERO);
    /// assert_eq!(tracker.current_spend(), cents);
    /// ```
    pub fn record_keepalive_usage(
        &self,
        provider_name: &str,
        provider_kind: &str,
        model: &str,
        usage: zeph_llm::provider::ResponseUsage,
    ) -> Cents {
        let cache = usage.cache.unwrap_or_default();
        let tokens = TokenUsage {
            input: usage.input,
            output: usage.output,
            cache_read: cache.read,
            cache_write: cache.creation,
        };
        self.add_usage(provider_name, provider_kind, model, tokens, usage.speed, 0);
        self.price_of(model, tokens, usage.speed)
    }

    /// Shared body of [`Self::record_usage`] and [`Self::record_auxiliary_usage`].
    fn add_usage(
        &self,
        provider_name: &str,
        provider_kind: &str,
        model: &str,
        usage: TokenUsage,
        speed: ServiceSpeed,
        requests: u64,
    ) {
        if self.pricing_for(model, ServiceSpeed::Standard).is_none() {
            let is_local = matches!(provider_kind, "ollama" | "candle" | "local");
            if is_local {
                tracing::debug!(model, "local model; cost recorded as zero");
            } else if self.unpriced_models_warned.lock().insert(model.to_owned()) {
                tracing::warn!(
                    model,
                    "model not found in pricing table; cost recorded as zero"
                );
            }
        }
        let cost = self.price_of(model, usage, speed);

        let mut state = self.state.lock();
        reset_if_new_day(&mut state);
        state.spent_cents += cost;
        state.session_cents += cost;

        let entry = state.providers.entry(provider_name.to_owned()).or_default();
        entry.input_tokens += usage.input;
        entry.cache_read_tokens += usage.cache_read;
        entry.cache_write_tokens += usage.cache_write;
        entry.output_tokens += usage.output;
        entry.cost_cents += cost;
        entry.request_count += requests;
        model.clone_into(&mut entry.model);
    }

    /// # Errors
    ///
    /// Returns `BudgetExhausted` when daily spend exceeds the configured limit.
    pub fn check_budget(&self) -> Result<(), BudgetExhausted> {
        let mut state = self.state.lock();
        reset_if_new_day(&mut state);
        if self.budget.is_exhausted(state.spent_cents)
            && let Some(budget_cents) = self.budget.limit()
        {
            return Err(BudgetExhausted {
                spent_cents: state.spent_cents,
                budget_cents,
            });
        }
        Ok(())
    }

    /// Returns the configured daily budget.
    #[must_use]
    pub fn budget(&self) -> DailyBudget {
        self.budget
    }

    /// Returns today's accumulated spend.
    #[must_use]
    pub fn current_spend(&self) -> Cents {
        self.state.lock().spent_cents
    }

    /// Returns the total cost recorded since the tracker was created.
    ///
    /// Unlike [`Self::current_spend`] this never resets at a day boundary and is not tied to the
    /// budget, so it is the value to show as the session's cumulative cost.
    #[must_use]
    pub fn session_spend(&self) -> Cents {
        self.state.lock().session_cents
    }

    /// Increment the successful-task counter.
    ///
    /// Call after each turn that completes without error and produces a usable agent response.
    pub fn record_successful_task(&self) {
        let mut state = self.state.lock();
        reset_if_new_day(&mut state);
        state.successful_tasks += 1;
    }

    /// Returns cost-per-successful-task, or `None` if no tasks recorded yet.
    #[must_use]
    pub fn cps(&self) -> Option<Cents> {
        let state = self.state.lock();
        if state.successful_tasks == 0 {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "successful_tasks uses approximate floating-point values for ranking, display, or telemetry, where integer precision is not part of the contract"
        )]
        Some(Cents::new(
            state.spent_cents.as_f64() / state.successful_tasks as f64,
        ))
    }

    /// Returns total number of successful tasks recorded today.
    #[must_use]
    pub fn successful_tasks(&self) -> u64 {
        self.state.lock().successful_tasks
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

    #[test]
    fn nan_limit_is_always_exhausted() {
        assert!(DailyBudget::Limited(Cents::new(f64::NAN)).is_exhausted(Cents::ZERO));
    }

    fn record(tracker: &CostTracker, provider: &str, model: &str, input: u64, output: u64) {
        tracker.record_usage(
            provider,
            "cloud",
            model,
            TokenUsage {
                input,
                cache_read: 0,
                cache_write: 0,
                output,
            },
            ServiceSpeed::Standard,
        );
    }

    #[test]
    fn cost_tracker_records_usage_and_calculates_cost() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        record(&tracker, "openai", "gpt-4o", 1000, 1000);
        // 0.25 + 1.0 = 1.25
        let spend = tracker.current_spend().as_f64();
        assert!((spend - 1.25).abs() < 0.001);
    }

    #[test]
    fn keepalive_usage_counts_toward_spend_but_not_request_count() {
        use zeph_llm::provider::{CacheUsage, ResponseUsage};

        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        let usage = ResponseUsage {
            input: 10,
            output: 1,
            cache: Some(CacheUsage {
                creation: 0,
                read: 10_000,
            }),
            ..ResponseUsage::default()
        };
        let cost = tracker.record_keepalive_usage("claude", "cloud", "claude-sonnet-4-6", usage);
        assert!(cost > Cents::ZERO);
        assert_eq!(tracker.current_spend(), cost);
        let breakdown = tracker.provider_breakdown();
        let (_, claude) = breakdown.iter().find(|(n, _)| n == "claude").unwrap();
        assert_eq!(claude.request_count, 0);
        assert_eq!(claude.cache_read_tokens, 10_000);
    }

    #[test]
    fn check_budget_passes_when_under_limit() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
        record(&tracker, "openai", "gpt-4o-mini", 100, 100);
        assert!(tracker.check_budget().is_ok());
    }

    #[test]
    fn check_budget_fails_when_over_limit() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(0.01)));
        record(&tracker, "claude", "claude-opus-4-20250514", 10000, 10000);
        assert!(tracker.check_budget().is_err());
    }

    #[test]
    fn daily_reset_clears_spending() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
        record(&tracker, "openai", "gpt-4o", 1000, 1000);
        assert!(tracker.current_spend().as_f64() > 0.0);
        // Simulate day change
        {
            let mut state = tracker.state.lock();
            state.day = 0; // force a past day
        }
        // check_budget should reset
        assert!(tracker.check_budget().is_ok());
        assert!((tracker.current_spend().as_f64() - 0.0).abs() < 0.001);
    }

    #[test]
    fn daily_reset_clears_provider_breakdown() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
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
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
        record(&tracker, "ollama", "llama3:8b", 10000, 10000);
        assert!((tracker.current_spend().as_f64() - 0.0).abs() < 0.001);
    }

    #[test]
    fn ollama_unknown_model_no_warn_no_panic() {
        // Local providers should silently record zero cost for unknown models.
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
        tracker.record_usage(
            "local",
            "ollama",
            "totally-unknown-ollama-model",
            TokenUsage {
                input: 5000,
                cache_read: 0,
                cache_write: 0,
                output: 5000,
            },
            ServiceSpeed::Standard,
        );
        assert!((tracker.current_spend().as_f64() - 0.0).abs() < 0.001);
    }

    #[test]
    fn cloud_unknown_model_still_records_zero_cost() {
        // Cloud providers record zero cost for unknown models (WARN emitted separately).
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
        tracker.record_usage(
            "openai",
            "cloud",
            "totally-unknown-cloud-model",
            TokenUsage {
                input: 5000,
                cache_read: 0,
                cache_write: 0,
                output: 5000,
            },
            ServiceSpeed::Standard,
        );
        assert!((tracker.current_spend().as_f64() - 0.0).abs() < 0.001);
    }

    #[test]
    fn unknown_model_zero_cost() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
        record(&tracker, "unknown", "totally-unknown-model", 5000, 5000);
        assert!((tracker.current_spend().as_f64() - 0.0).abs() < 0.001);
    }

    #[test]
    fn price_of_distinguishes_cache_read_from_cache_write() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        let price = |cache_read, cache_write| {
            tracker
                .price_of(
                    "claude-opus-4-6",
                    TokenUsage {
                        cache_read,
                        cache_write,
                        ..TokenUsage::default()
                    },
                    ServiceSpeed::Standard,
                )
                .as_f64()
        };
        let read_only = price(1000, 0);
        let write_only = price(0, 1000);
        assert!((write_only - 0.625).abs() < 1e-9);
        assert!(read_only < write_only);
        assert!((price(1000, 2000) - (read_only + 2.0 * write_only)).abs() < 1e-9);
    }

    /// M2 (issue #6549): `price_of` must be the exact pricing source of truth `record_usage`
    /// itself uses — a per-message ledger row computed via `price_of` must always match the
    /// cost `record_usage` folded into the daily aggregate for the same call, including the
    /// missing-model fallback (unpriced/local models price at zero in both places).
    #[test]
    fn price_of_matches_record_usage_for_known_model() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        let expected = tracker
            .price_of(
                "gpt-4o",
                TokenUsage {
                    input: 1000,
                    cache_read: 0,
                    cache_write: 0,
                    output: 1000,
                },
                ServiceSpeed::Standard,
            )
            .as_f64();
        record(&tracker, "openai", "gpt-4o", 1000, 1000);
        assert!(
            (tracker.current_spend().as_f64() - expected).abs() < 1e-9,
            "price_of={expected} current_spend={}",
            tracker.current_spend().as_f64()
        );
    }

    #[test]
    fn price_of_zero_for_unpriced_model_matches_record_usage_fallback() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        let price = tracker
            .price_of(
                "totally-unknown-model",
                TokenUsage {
                    input: 5000,
                    cache_read: 0,
                    cache_write: 0,
                    output: 5000,
                },
                ServiceSpeed::Standard,
            )
            .as_f64();
        assert!(
            (price - 0.0).abs() < 1e-9,
            "unpriced model must price at zero"
        );

        record(&tracker, "unknown", "totally-unknown-model", 5000, 5000);
        assert!((tracker.current_spend().as_f64() - price).abs() < 1e-9);
    }

    #[test]
    fn price_of_includes_cache_tokens() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        // Mirrors cache_write_cost_included_in_total: claude-opus-4-6 cache_write = 125% of
        // prompt price (0.5 cents/1k) -> 1000 tokens = 0.625 cents.
        let price = tracker
            .price_of(
                "claude-opus-4-6",
                TokenUsage {
                    input: 0,
                    cache_read: 0,
                    cache_write: 1000,
                    output: 0,
                },
                ServiceSpeed::Standard,
            )
            .as_f64();
        assert!((price - 0.625).abs() < 0.001);
    }

    #[test]
    fn known_claude_model_has_nonzero_cost() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        record(&tracker, "claude", "claude-haiku-4-5-20251001", 1000, 1000);
        assert!(tracker.current_spend().as_f64() > 0.0);
    }

    #[test]
    fn gpt5_pricing_is_correct() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        record(&tracker, "openai", "gpt-5", 1000, 1000);
        // 0.125 + 1.0 = 1.125
        let spend = tracker.current_spend().as_f64();
        assert!((spend - 1.125).abs() < 0.001);
    }

    #[test]
    fn gpt5_mini_pricing_is_correct() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        record(&tracker, "openai", "gpt-5-mini", 1000, 1000);
        // 0.025 + 0.2 = 0.225
        let spend = tracker.current_spend().as_f64();
        assert!((spend - 0.225).abs() < 0.001);
    }

    #[test]
    fn check_budget_unlimited_when_max_daily_cents_is_zero() {
        let tracker = CostTracker::new(DailyBudget::Unlimited);
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
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
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
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        // gpt-4o: cheap; claude-opus: expensive
        record(&tracker, "cheap", "gpt-4o-mini", 100, 100);
        record(&tracker, "expensive", "claude-opus-4-20250514", 10000, 5000);

        let breakdown = tracker.provider_breakdown();
        assert_eq!(breakdown[0].0, "expensive");
    }

    #[test]
    fn cache_tokens_included_in_cost() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        // claude-haiku prompt=0.1, cache_read=0.01 per 1k
        // 1000 cache_read tokens = 0.01 cents; 0 input/output for isolation
        tracker.record_usage(
            "claude",
            "cloud",
            "claude-haiku-4-5-20251001",
            TokenUsage {
                input: 0,
                cache_read: 1000,
                cache_write: 0,
                output: 0,
            },
            ServiceSpeed::Standard,
        );
        let spend = tracker.current_spend().as_f64();
        assert!(spend > 0.0, "cache read should contribute to cost");
    }

    #[test]
    fn cache_write_cost_included_in_total() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        // Claude pricing: cache_write = 125% of prompt price
        // claude-opus-4-6: prompt = 0.5 cents/1k
        // 1000 cache_write tokens = (0.5 * 1.25 * 1000) / 1000 = 0.625 cents
        tracker.record_usage(
            "claude-provider",
            "cloud",
            "claude-opus-4-6",
            TokenUsage {
                input: 0,
                cache_read: 0,
                cache_write: 1000,
                output: 0,
            },
            ServiceSpeed::Standard,
        );
        let cost = tracker.current_spend().as_f64();
        assert!((cost - 0.625).abs() < 0.001);
    }

    #[test]
    fn opus_4_8_prices_like_retained_opus_4_6() {
        let price = |model: &str| {
            CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)))
                .price_of(
                    model,
                    TokenUsage {
                        input: 1000,
                        cache_read: 0,
                        cache_write: 0,
                        output: 1000,
                    },
                    ServiceSpeed::Standard,
                )
                .as_f64()
        };
        assert!((price("claude-opus-4-8") - price("claude-opus-4-6")).abs() < f64::EPSILON);
        assert!(price("claude-opus-4-8") > 0.0);
    }

    #[test]
    fn claude_5_generation_prices_per_million_tokens() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        // (model, $/MTok input, $/MTok output); 1 cent per 1k tokens = $10 per MTok.
        let rows = [
            ("claude-sonnet-5", 2.0, 10.0),
            ("claude-sonnet-5-5", 2.0, 10.0),
            ("claude-opus-5", 5.0, 25.0),
            ("claude-opus-5-5", 4.0, 20.0),
            ("claude-haiku-5-5", 0.1, 0.5),
            ("claude-fable-5", 10.0, 50.0),
            ("claude-fable-5-1", 10.0, 50.0),
            ("claude-fable-5-2", 8.0, 40.0),
            ("claude-mythos-5", 10.0, 50.0),
            ("claude-mythos-5-1", 10.0, 50.0),
            ("claude-mythos-5-2", 8.0, 40.0),
        ];
        for (model, input, output) in rows {
            let expected = (input + output) / 10.0;
            let got = tracker
                .price_of(
                    model,
                    TokenUsage {
                        input: 1000,
                        cache_read: 0,
                        cache_write: 0,
                        output: 1000,
                    },
                    ServiceSpeed::Standard,
                )
                .as_f64();
            assert!(
                (got - expected).abs() < 1e-9,
                "{model}: {got} != {expected}"
            );
        }
    }

    #[test]
    fn cache_read_ratio_is_model_specific() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        let read = |model: &str| {
            tracker
                .price_of(
                    model,
                    TokenUsage {
                        input: 0,
                        cache_read: 1000,
                        cache_write: 0,
                        output: 0,
                    },
                    ServiceSpeed::Standard,
                )
                .as_f64()
        };
        assert!((read("claude-sonnet-5-5") - 0.02).abs() < 1e-9);
        assert!((read("claude-opus-5-5") - 0.02).abs() < 1e-9);
        assert!((read("claude-fable-5-1") - 0.025).abs() < 1e-9);
        assert!((read("claude-fable-5-2") - 0.02).abs() < 1e-9);
        assert!((read("claude-mythos-5") - 0.1).abs() < 1e-9);
        assert!((read("claude-mythos-5-1") - 0.025).abs() < 1e-9);
        assert!((read("claude-mythos-5-2") - 0.02).abs() < 1e-9);
    }

    #[test]
    fn dated_snapshot_id_resolves_to_base_entry() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        assert!(
            (tracker
                .price_of(
                    "claude-sonnet-5-5-20261001",
                    TokenUsage {
                        input: 1000,
                        cache_read: 0,
                        cache_write: 0,
                        output: 0
                    },
                    ServiceSpeed::Standard
                )
                .as_f64()
                - tracker
                    .price_of(
                        "claude-sonnet-5-5",
                        TokenUsage {
                            input: 1000,
                            cache_read: 0,
                            cache_write: 0,
                            output: 0
                        },
                        ServiceSpeed::Standard
                    )
                    .as_f64())
            .abs()
                < f64::EPSILON
        );
        assert!(
            tracker
                .price_of(
                    "claude-sonnet-5-5-20261001",
                    TokenUsage {
                        input: 1000,
                        cache_read: 0,
                        cache_write: 0,
                        output: 0
                    },
                    ServiceSpeed::Standard
                )
                .as_f64()
                > 0.0
        );
        assert!(
            tracker
                .price_of(
                    "claude-unknown-20261001",
                    TokenUsage {
                        input: 1000,
                        cache_read: 0,
                        cache_write: 0,
                        output: 0
                    },
                    ServiceSpeed::Standard
                )
                .as_f64()
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn unknown_model_warns_once_per_model_id() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        for _ in 0..3 {
            tracker.record_usage(
                "p",
                "cloud",
                "mystery-model",
                TokenUsage {
                    input: 10,
                    cache_read: 0,
                    cache_write: 0,
                    output: 10,
                },
                ServiceSpeed::Standard,
            );
        }
        tracker.record_usage(
            "p",
            "cloud",
            "other-mystery",
            TokenUsage {
                input: 10,
                cache_read: 0,
                cache_write: 0,
                output: 10,
            },
            ServiceSpeed::Standard,
        );
        let warned = tracker.unpriced_models_warned.lock();
        assert_eq!(warned.len(), 2);
    }

    #[test]
    fn auxiliary_usage_adds_cost_without_counting_a_request() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        tracker.record_usage(
            "claude",
            "cloud",
            "claude-sonnet-5-5",
            TokenUsage {
                input: 1000,
                cache_read: 0,
                cache_write: 0,
                output: 1000,
            },
            ServiceSpeed::Standard,
        );
        let base = tracker.current_spend().as_f64();
        let aux = TokenUsage {
            input: 2000,
            output: 500,
            cache_write: 0,
            cache_read: 0,
        };
        tracker.record_auxiliary_usage(
            "claude",
            "cloud",
            "claude-sonnet-5-5",
            aux,
            ServiceSpeed::Standard,
        );
        let expected = tracker
            .price_of("claude-sonnet-5-5", aux, ServiceSpeed::Standard)
            .as_f64();
        assert!(expected > 0.0);
        assert!((tracker.current_spend().as_f64() - base - expected).abs() < 1e-9);
        let breakdown = tracker.provider_breakdown();
        assert_eq!(breakdown[0].1.request_count, 1);
        assert_eq!(breakdown[0].1.input_tokens, 3000);
        assert_eq!(breakdown[0].1.output_tokens, 1500);
    }

    #[test]
    fn cps_none_when_no_tasks() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
        assert!(tracker.cps().is_none());
        assert_eq!(tracker.successful_tasks(), 0);
    }

    #[test]
    fn cps_calculated_correctly() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
        // 0.25 (input) + 1.0 (output) = 1.25 cents
        record(&tracker, "openai", "gpt-4o", 1000, 1000);
        tracker.record_successful_task();
        tracker.record_successful_task();
        assert_eq!(tracker.successful_tasks(), 2);
        let cps = tracker
            .cps()
            .expect("cps should be Some after tasks")
            .as_f64();
        // 1.25 / 2 = 0.625
        assert!((cps - 0.625).abs() < 0.001, "cps={cps}");
    }

    #[test]
    fn cps_resets_on_new_day() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
        record(&tracker, "openai", "gpt-4o", 1000, 1000);
        tracker.record_successful_task();
        assert_eq!(tracker.successful_tasks(), 1);
        // Force day change
        {
            let mut state = tracker.state.lock();
            state.day = 0;
        }
        // Any state-touching call triggers reset
        assert!(tracker.check_budget().is_ok());
        assert_eq!(tracker.successful_tasks(), 0);
        assert!(tracker.cps().is_none());
    }

    #[test]
    fn session_spend_survives_day_rollover() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(100.0)));
        record(&tracker, "openai", "gpt-4o", 1000, 1000);
        let before = tracker.session_spend();
        assert!(before > Cents::ZERO);
        tracker.state.lock().day = 0;
        assert!(tracker.check_budget().is_ok());
        assert_eq!(tracker.current_spend(), Cents::ZERO);
        assert_eq!(tracker.session_spend(), before);
    }

    #[test]
    fn dated_mythos_snapshot_prices_like_base() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        let std = ServiceSpeed::Standard;
        assert!(
            (tracker
                .price_of(
                    "claude-mythos-5-2-20261109",
                    TokenUsage {
                        input: 1000,
                        cache_read: 0,
                        cache_write: 0,
                        output: 1000
                    },
                    std
                )
                .as_f64()
                - tracker
                    .price_of(
                        "claude-mythos-5-2",
                        TokenUsage {
                            input: 1000,
                            cache_read: 0,
                            cache_write: 0,
                            output: 1000
                        },
                        std
                    )
                    .as_f64())
            .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn fast_mode_prices_at_double_standard_with_stacked_cache() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        for model in ["claude-opus-5-5", "claude-fable-5-2", "claude-mythos-5-2"] {
            for (input, cache_read, cache_write, output) in [
                (1000, 0, 0, 0),
                (0, 1000, 0, 0),
                (0, 0, 1000, 0),
                (0, 0, 0, 1000),
            ] {
                let std = tracker
                    .price_of(
                        model,
                        TokenUsage {
                            input,
                            cache_read,
                            cache_write,
                            output,
                        },
                        ServiceSpeed::Standard,
                    )
                    .as_f64();
                let fast = tracker
                    .price_of(
                        model,
                        TokenUsage {
                            input,
                            cache_read,
                            cache_write,
                            output,
                        },
                        ServiceSpeed::Fast,
                    )
                    .as_f64();
                assert!((fast - 2.0 * std).abs() < 1e-9, "{model}: {fast} vs {std}");
            }
        }
    }

    #[test]
    fn fast_mode_without_row_falls_back_to_standard_price() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        let std = tracker
            .price_of(
                "claude-sonnet-5-5",
                TokenUsage {
                    input: 1000,
                    cache_read: 0,
                    cache_write: 0,
                    output: 1000,
                },
                ServiceSpeed::Standard,
            )
            .as_f64();
        let fast = tracker
            .price_of(
                "claude-sonnet-5-5",
                TokenUsage {
                    input: 1000,
                    cache_read: 0,
                    cache_write: 0,
                    output: 1000,
                },
                ServiceSpeed::Fast,
            )
            .as_f64();
        assert!((fast - std).abs() < f64::EPSILON);
        assert!(fast > 0.0);
    }

    #[test]
    fn record_usage_applies_fast_price() {
        let tracker = CostTracker::new(DailyBudget::Limited(Cents::new(1000.0)));
        tracker.record_usage(
            "p",
            "cloud",
            "claude-fable-5-2",
            TokenUsage {
                input: 1000,
                cache_read: 0,
                cache_write: 0,
                output: 1000,
            },
            ServiceSpeed::Fast,
        );
        // (1.6 + 8.0) cents
        assert!((tracker.current_spend().as_f64() - 9.6).abs() < 1e-9);
    }
}
