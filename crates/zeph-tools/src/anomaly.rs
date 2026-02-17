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
}
