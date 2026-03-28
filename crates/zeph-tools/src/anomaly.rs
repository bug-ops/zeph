// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sliding-window anomaly detection for tool execution patterns.

use std::collections::VecDeque;

/// Severity of a detected anomaly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnomalySeverity {
    Warning,
    Critical,
}

/// A detected anomaly in tool execution patterns.
#[derive(Debug, Clone)]
pub struct Anomaly {
    pub severity: AnomalySeverity,
    pub description: String,
}

/// Tracks recent tool execution outcomes and detects anomalous patterns.
#[derive(Debug)]
pub struct AnomalyDetector {
    window: VecDeque<Outcome>,
    window_size: usize,
    error_threshold: f64,
    critical_threshold: f64,
}

#[derive(Debug, Clone, Copy)]
enum Outcome {
    Success,
    Error,
    Blocked,
}

impl AnomalyDetector {
    #[must_use]
    pub fn new(window_size: usize, error_threshold: f64, critical_threshold: f64) -> Self {
        Self {
            window: VecDeque::with_capacity(window_size),
            window_size,
            error_threshold,
            critical_threshold,
        }
    }

    /// Record a successful tool execution.
    pub fn record_success(&mut self) {
        self.push(Outcome::Success);
    }

    /// Record a failed tool execution.
    pub fn record_error(&mut self) {
        self.push(Outcome::Error);
    }

    /// Record a blocked tool execution.
    pub fn record_blocked(&mut self) {
        self.push(Outcome::Blocked);
    }

    /// Record a quality failure (`ToolNotFound`, `InvalidParameters`, `TypeMismatch`) that
    /// originated from a reasoning-enhanced model. Counts as an error for anomaly
    /// detection purposes and logs a `reasoning_amplification` warning.
    ///
    /// Per arXiv:2510.22977, reasoning models amplify tool hallucinations — this
    /// method makes such failures visible in the anomaly window.
    pub fn record_reasoning_quality_failure(&mut self, model_name: &str, tool_name: &str) {
        self.push(Outcome::Error);
        tracing::warn!(
            model = model_name,
            tool = tool_name,
            category = "reasoning_amplification",
            "quality failure from reasoning model — CoT may amplify tool hallucination (arXiv:2510.22977)"
        );
    }

    fn push(&mut self, outcome: Outcome) {
        if self.window.len() >= self.window_size {
            self.window.pop_front();
        }
        self.window.push_back(outcome);
    }

    /// Check the current window for anomalies.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn check(&self) -> Option<Anomaly> {
        if self.window.len() < 3 {
            return None;
        }

        let total = self.window.len();
        let errors = self
            .window
            .iter()
            .filter(|o| matches!(o, Outcome::Error | Outcome::Blocked))
            .count();

        let ratio = errors as f64 / total as f64;

        if ratio >= self.critical_threshold {
            Some(Anomaly {
                severity: AnomalySeverity::Critical,
                description: format!(
                    "error rate {:.0}% ({errors}/{total}) exceeds critical threshold",
                    ratio * 100.0,
                ),
            })
        } else if ratio >= self.error_threshold {
            Some(Anomaly {
                severity: AnomalySeverity::Warning,
                description: format!(
                    "error rate {:.0}% ({errors}/{total}) exceeds warning threshold",
                    ratio * 100.0,
                ),
            })
        } else {
            None
        }
    }

    /// Reset the sliding window.
    pub fn reset(&mut self) {
        self.window.clear();
    }
}

impl Default for AnomalyDetector {
    fn default() -> Self {
        Self::new(10, 0.5, 0.8)
    }
}

/// Returns `true` when `model_name` matches a known reasoning-enhanced model pattern.
///
/// Reasoning models (o1, o3, o4-mini, `QwQ`, `DeepSeek-R1`, etc.) are more prone to
/// tool hallucination than standard models per arXiv:2510.22977. This helper enables
/// callers to conditionally emit `reasoning_amplification` warnings.
#[must_use]
pub fn is_reasoning_model(model_name: &str) -> bool {
    let lower = model_name.to_ascii_lowercase();
    // OpenAI o-series: o1, o3, o4-mini, o1-mini, o1-preview, o3-mini
    let openai_o = lower.starts_with("o1") || lower.starts_with("o3") || lower.starts_with("o4");
    // QwQ reasoning models
    let qwq = lower.contains("qwq");
    // DeepSeek R1 and variants
    let deepseek_r1 = lower.contains("deepseek-r1") || lower.contains("deepseek_r1");
    // Claude extended thinking (prefixed with "claude" and contains "think")
    let claude_think = lower.starts_with("claude") && lower.contains("think");
    openai_o || qwq || deepseek_r1 || claude_think
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_anomaly_on_success() {
        let mut det = AnomalyDetector::default();
        for _ in 0..10 {
            det.record_success();
        }
        assert!(det.check().is_none());
    }

    #[test]
    fn warning_on_half_errors() {
        let mut det = AnomalyDetector::new(10, 0.5, 0.8);
        for _ in 0..5 {
            det.record_success();
        }
        for _ in 0..5 {
            det.record_error();
        }
        let anomaly = det.check().unwrap();
        assert_eq!(anomaly.severity, AnomalySeverity::Warning);
    }

    #[test]
    fn critical_on_high_errors() {
        let mut det = AnomalyDetector::new(10, 0.5, 0.8);
        for _ in 0..2 {
            det.record_success();
        }
        for _ in 0..8 {
            det.record_error();
        }
        let anomaly = det.check().unwrap();
        assert_eq!(anomaly.severity, AnomalySeverity::Critical);
    }

    #[test]
    fn blocked_counts_as_error() {
        let mut det = AnomalyDetector::new(10, 0.5, 0.8);
        for _ in 0..2 {
            det.record_success();
        }
        for _ in 0..8 {
            det.record_blocked();
        }
        let anomaly = det.check().unwrap();
        assert_eq!(anomaly.severity, AnomalySeverity::Critical);
    }

    #[test]
    fn window_slides() {
        let mut det = AnomalyDetector::new(5, 0.5, 0.8);
        for _ in 0..5 {
            det.record_error();
        }
        assert!(det.check().is_some());

        // Push 5 successes to slide out errors
        for _ in 0..5 {
            det.record_success();
        }
        assert!(det.check().is_none());
    }

    #[test]
    fn too_few_samples_returns_none() {
        let mut det = AnomalyDetector::default();
        det.record_error();
        det.record_error();
        assert!(det.check().is_none());
    }

    #[test]
    fn reset_clears_window() {
        let mut det = AnomalyDetector::new(5, 0.5, 0.8);
        for _ in 0..5 {
            det.record_error();
        }
        assert!(det.check().is_some());
        det.reset();
        assert!(det.check().is_none());
    }

    #[test]
    fn default_thresholds() {
        let det = AnomalyDetector::default();
        assert_eq!(det.window_size, 10);
        assert!((det.error_threshold - 0.5).abs() < f64::EPSILON);
        assert!((det.critical_threshold - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn is_reasoning_model_openai_o_series() {
        assert!(is_reasoning_model("o1"));
        assert!(is_reasoning_model("o1-mini"));
        assert!(is_reasoning_model("o1-preview"));
        assert!(is_reasoning_model("o3"));
        assert!(is_reasoning_model("o3-mini"));
        assert!(is_reasoning_model("o4-mini"));
        assert!(!is_reasoning_model("gpt-4o"));
        assert!(!is_reasoning_model("gpt-4o-mini"));
    }

    #[test]
    fn is_reasoning_model_other_families() {
        assert!(is_reasoning_model("QwQ-32B"));
        assert!(is_reasoning_model("deepseek-r1"));
        assert!(is_reasoning_model("deepseek-r1-distill-qwen-14b"));
        assert!(is_reasoning_model("claude-3-opus-think"));
        assert!(!is_reasoning_model("claude-3-opus"));
        assert!(!is_reasoning_model("qwen2.5:14b"));
    }

    #[test]
    fn record_reasoning_quality_failure_increments_error_count() {
        let mut det = AnomalyDetector::new(10, 0.5, 0.8);
        // Record 6 reasoning quality failures in window of 10
        for _ in 0..6 {
            det.record_reasoning_quality_failure("o1", "shell");
        }
        // 6/6 = 100% > critical threshold
        let anomaly = det.check().unwrap();
        assert_eq!(anomaly.severity, AnomalySeverity::Critical);
    }
}
