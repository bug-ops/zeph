// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use thiserror::Error;

#[derive(Debug, Error)]
#[error("daily budget exhausted: spent {spent_cents:.2} / {budget_cents:.2} cents")]
pub struct BudgetExhausted {
    pub spent_cents: f64,
    pub budget_cents: f64,
}

#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub prompt_cents_per_1k: f64,
    pub completion_cents_per_1k: f64,
}

struct CostState {
    spent_cents: f64,
    day: u32,
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

fn default_pricing() -> HashMap<String, ModelPricing> {
    let mut m = HashMap::new();
    // Claude 4 (sonnet-4 / opus-4 base releases)
    m.insert(
        "claude-sonnet-4-20250514".into(),
        ModelPricing {
            prompt_cents_per_1k: 0.3,
            completion_cents_per_1k: 1.5,
        },
    );
    m.insert(
        "claude-opus-4-20250514".into(),
        ModelPricing {
            prompt_cents_per_1k: 1.5,
            completion_cents_per_1k: 7.5,
        },
    );
    // Claude 4.1 Opus ($15/$75 per 1M tokens)
    m.insert(
        "claude-opus-4-1-20250805".into(),
        ModelPricing {
            prompt_cents_per_1k: 1.5,
            completion_cents_per_1k: 7.5,
        },
    );
    // Claude 4.5 family
    m.insert(
        "claude-haiku-4-5-20251001".into(),
        ModelPricing {
            prompt_cents_per_1k: 0.1,
            completion_cents_per_1k: 0.5,
        },
    );
    m.insert(
        "claude-sonnet-4-5-20250929".into(),
        ModelPricing {
            prompt_cents_per_1k: 0.3,
            completion_cents_per_1k: 1.5,
        },
    );
    m.insert(
        "claude-opus-4-5-20251101".into(),
        ModelPricing {
            prompt_cents_per_1k: 0.5,
            completion_cents_per_1k: 2.5,
        },
    );
    // Claude 4.6 family
    m.insert(
        "claude-sonnet-4-6".into(),
        ModelPricing {
            prompt_cents_per_1k: 0.3,
            completion_cents_per_1k: 1.5,
        },
    );
    m.insert(
        "claude-opus-4-6".into(),
        ModelPricing {
            prompt_cents_per_1k: 0.5,
            completion_cents_per_1k: 2.5,
        },
    );
    m.insert(
        "gpt-4o".into(),
        ModelPricing {
            prompt_cents_per_1k: 0.25,
            completion_cents_per_1k: 1.0,
        },
    );
    m.insert(
        "gpt-4o-mini".into(),
        ModelPricing {
            prompt_cents_per_1k: 0.015,
            completion_cents_per_1k: 0.06,
        },
    );
    // GPT-5 family ($1.25/$10 per 1M tokens)
    m.insert(
        "gpt-5".into(),
        ModelPricing {
            prompt_cents_per_1k: 0.125,
            completion_cents_per_1k: 1.0,
        },
    );
    // GPT-5 mini ($0.25/$2 per 1M tokens)
    m.insert(
        "gpt-5-mini".into(),
        ModelPricing {
            prompt_cents_per_1k: 0.025,
            completion_cents_per_1k: 0.2,
        },
    );
    m
}

impl CostTracker {
    #[must_use]
    pub fn new(enabled: bool, max_daily_cents: f64) -> Self {
        Self {
            pricing: default_pricing(),
            state: Arc::new(Mutex::new(CostState {
                spent_cents: 0.0,
                day: current_day(),
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

    pub fn record_usage(&self, model: &str, prompt_tokens: u64, completion_tokens: u64) {
        if !self.enabled {
            return;
        }
        let pricing = if let Some(p) = self.pricing.get(model).cloned() {
            p
        } else {
            tracing::warn!(
                model,
                "model not found in pricing table; cost recorded as zero"
            );
            ModelPricing {
                prompt_cents_per_1k: 0.0,
                completion_cents_per_1k: 0.0,
            }
        };
        #[allow(clippy::cast_precision_loss)]
        let cost = pricing.prompt_cents_per_1k * (prompt_tokens as f64) / 1000.0
            + pricing.completion_cents_per_1k * (completion_tokens as f64) / 1000.0;

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let today = current_day();
        if state.day != today {
            state.spent_cents = 0.0;
            state.day = today;
        }
        state.spent_cents += cost;
    }

    /// # Errors
    ///
    /// Returns `BudgetExhausted` when daily spend exceeds the configured limit.
    pub fn check_budget(&self) -> Result<(), BudgetExhausted> {
        if !self.enabled {
            return Ok(());
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let today = current_day();
        if state.day != today {
            state.spent_cents = 0.0;
            state.day = today;
        }
        if self.max_daily_cents > 0.0 && state.spent_cents >= self.max_daily_cents {
            return Err(BudgetExhausted {
                spent_cents: state.spent_cents,
                budget_cents: self.max_daily_cents,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn current_spend(&self) -> f64 {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.spent_cents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_tracker_records_usage_and_calculates_cost() {
        let tracker = CostTracker::new(true, 1000.0);
        tracker.record_usage("gpt-4o", 1000, 1000);
        // 0.25 + 1.0 = 1.25
        let spend = tracker.current_spend();
        assert!((spend - 1.25).abs() < 0.001);
    }

    #[test]
    fn check_budget_passes_when_under_limit() {
        let tracker = CostTracker::new(true, 100.0);
        tracker.record_usage("gpt-4o-mini", 100, 100);
        assert!(tracker.check_budget().is_ok());
    }

    #[test]
    fn check_budget_fails_when_over_limit() {
        let tracker = CostTracker::new(true, 0.01);
        tracker.record_usage("claude-opus-4-20250514", 10000, 10000);
        assert!(tracker.check_budget().is_err());
    }

    #[test]
    fn daily_reset_clears_spending() {
        let tracker = CostTracker::new(true, 100.0);
        tracker.record_usage("gpt-4o", 1000, 1000);
        assert!(tracker.current_spend() > 0.0);
        // Simulate day change
        {
            let mut state = tracker.state.lock().unwrap();
            state.day = 0; // force a past day
        }
        // check_budget should reset
        assert!(tracker.check_budget().is_ok());
        assert!((tracker.current_spend() - 0.0).abs() < 0.001);
    }

    #[test]
    fn ollama_zero_cost() {
        let tracker = CostTracker::new(true, 100.0);
        tracker.record_usage("llama3:8b", 10000, 10000);
        assert!((tracker.current_spend() - 0.0).abs() < 0.001);
    }

    #[test]
    fn unknown_model_zero_cost() {
        let tracker = CostTracker::new(true, 100.0);
        tracker.record_usage("totally-unknown-model", 5000, 5000);
        assert!((tracker.current_spend() - 0.0).abs() < 0.001);
    }

    #[test]
    fn known_claude_model_has_nonzero_cost() {
        let tracker = CostTracker::new(true, 1000.0);
        tracker.record_usage("claude-haiku-4-5-20251001", 1000, 1000);
        assert!(tracker.current_spend() > 0.0);
    }

    #[test]
    fn gpt5_pricing_is_correct() {
        let tracker = CostTracker::new(true, 1000.0);
        tracker.record_usage("gpt-5", 1000, 1000);
        // 0.125 + 1.0 = 1.125
        let spend = tracker.current_spend();
        assert!((spend - 1.125).abs() < 0.001);
    }

    #[test]
    fn gpt5_mini_pricing_is_correct() {
        let tracker = CostTracker::new(true, 1000.0);
        tracker.record_usage("gpt-5-mini", 1000, 1000);
        // 0.025 + 0.2 = 0.225
        let spend = tracker.current_spend();
        assert!((spend - 0.225).abs() < 0.001);
    }

    #[test]
    fn disabled_tracker_always_passes() {
        let tracker = CostTracker::new(false, 0.0);
        tracker.record_usage("claude-opus-4-20250514", 1_000_000, 1_000_000);
        assert!(tracker.check_budget().is_ok());
        assert!((tracker.current_spend() - 0.0).abs() < 0.001);
    }

    #[test]
    fn check_budget_unlimited_when_max_daily_cents_is_zero() {
        let tracker = CostTracker::new(true, 0.0);
        tracker.record_usage("claude-opus-4-20250514", 100_000, 100_000);
        assert!(tracker.check_budget().is_ok());
    }
}
