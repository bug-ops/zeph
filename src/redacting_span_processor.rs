// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Span processor wrapper that scrubs secrets from string attributes before export.
//!
//! This module is feature-gated on `otel` and is compiled only when the OTLP pipeline is active.
//! The processor is transparent when `security.redact_secrets = false`; only the wiring code in
//! [`crate::tracing_init`] enables it conditionally.

use opentelemetry::Context;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanProcessor};
use std::time::Duration;

/// Scrub string-valued attributes in-place using [`zeph_core::redact::scrub_content`].
fn scrub_string_attributes(attrs: &mut Vec<opentelemetry::KeyValue>) {
    for attr in attrs {
        if let opentelemetry::Value::String(ref mut s) = attr.value {
            let scrubbed = zeph_core::redact::scrub_content(s.as_str());
            if let std::borrow::Cow::Owned(new_val) = scrubbed {
                *s = new_val.into();
            }
        }
    }
}

/// Wraps any [`SpanProcessor`] and scrubs string-valued span attributes and event attributes
/// before forwarding.
///
/// Calls [`zeph_core::redact::scrub_content`] on every `Value::String` in both
/// `SpanData.attributes` and each `Event.attributes` within `SpanData.events`. Non-string
/// attribute types (`Int`, `Bool`, `Double`, `Array`) pass through unchanged.
///
/// # Why
///
/// The OTLP span pipeline forwards span attributes verbatim to the collector. Without redaction,
/// a tool call that includes an API key in its output would appear in Jaeger or any downstream
/// analytics system. Span events (e.g., from `tracing::info!` attached to a span) carry their own
/// attribute bags and must be scrubbed separately. This processor acts as a last-resort backstop —
/// application code should avoid recording secrets as span attributes in the first place.
#[derive(Debug)]
pub(crate) struct RedactingSpanProcessor<P: SpanProcessor> {
    inner: P,
}

impl<P: SpanProcessor> RedactingSpanProcessor<P> {
    /// Create a new [`RedactingSpanProcessor`] wrapping `inner`.
    pub(crate) fn new(inner: P) -> Self {
        Self { inner }
    }
}

impl<P: SpanProcessor> SpanProcessor for RedactingSpanProcessor<P> {
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, mut span: SpanData) {
        // Scrub span-level attributes.
        scrub_string_attributes(&mut span.attributes);
        // Scrub event-level attributes (e.g., from tracing::info! attached to this span).
        for event in &mut span.events.events {
            scrub_string_attributes(&mut event.attributes);
        }
        self.inner.on_end(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::{KeyValue, Value};
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SpanData, SpanProcessor};
    use std::sync::{Arc, Mutex};

    /// Minimal no-op SpanProcessor that captures the last span passed to on_end.
    #[derive(Debug, Default)]
    struct CapturingProcessor {
        captured: Arc<Mutex<Option<SpanData>>>,
    }

    impl SpanProcessor for CapturingProcessor {
        fn on_start(
            &self,
            _span: &mut opentelemetry_sdk::trace::Span,
            _cx: &opentelemetry::Context,
        ) {
        }

        fn on_end(&self, span: SpanData) {
            *self.captured.lock().unwrap() = Some(span);
        }

        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            Ok(())
        }
    }

    /// Build a minimal SpanData with given span attributes, no events.
    fn make_span_data(attrs: Vec<KeyValue>) -> SpanData {
        use opentelemetry::trace::{
            SpanContext, SpanId, SpanKind, TraceFlags, TraceId, TraceState,
        };
        use opentelemetry_sdk::trace::{SpanEvents, SpanLinks};
        SpanData {
            span_context: SpanContext::new(
                TraceId::from(1_u128),
                SpanId::from(1_u64),
                TraceFlags::SAMPLED,
                false,
                TraceState::default(),
            ),
            dropped_attributes_count: 0,
            parent_span_id: SpanId::INVALID,
            parent_span_is_remote: false,
            span_kind: SpanKind::Internal,
            name: "test".into(),
            start_time: std::time::SystemTime::UNIX_EPOCH,
            end_time: std::time::SystemTime::UNIX_EPOCH,
            attributes: attrs,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: opentelemetry::trace::Status::Unset,
            instrumentation_scope: opentelemetry::InstrumentationScope::builder("test").build(),
        }
    }

    /// String attribute containing a secret pattern is scrubbed after on_end.
    #[test]
    fn string_attribute_with_secret_is_scrubbed() {
        let captured = Arc::new(Mutex::new(None));
        let inner = CapturingProcessor {
            captured: Arc::clone(&captured),
        };
        let processor = RedactingSpanProcessor::new(inner);

        // Use a value that zeph_core::redact::scrub_content recognises as a secret.
        // The redactor matches patterns like `sk-...` (OpenAI keys), bearer tokens, etc.
        // Use a realistic-looking API key that the redactor will match.
        let secret_val = "Authorization: Bearer sk-proj-abcdefghijklmnopqrstuvwxyz0123456789";
        let span = make_span_data(vec![KeyValue::new("auth_header", secret_val)]);
        processor.on_end(span);

        let guard = captured.lock().unwrap();
        let result = guard.as_ref().unwrap();
        if let Value::String(ref s) = result.attributes[0].value {
            assert!(
                !s.as_str().contains("sk-proj-"),
                "secret must be redacted, got: {s}"
            );
        } else {
            panic!("expected String attribute");
        }
    }

    /// Non-string attributes (Int, Bool, Double) pass through unchanged.
    #[test]
    fn non_string_attributes_pass_through_unchanged() {
        let captured = Arc::new(Mutex::new(None));
        let inner = CapturingProcessor {
            captured: Arc::clone(&captured),
        };
        let processor = RedactingSpanProcessor::new(inner);

        let span = make_span_data(vec![
            KeyValue::new("count", 42_i64),
            KeyValue::new("flag", true),
            KeyValue::new("ratio", 0.5_f64),
        ]);
        processor.on_end(span);

        let guard = captured.lock().unwrap();
        let result = guard.as_ref().unwrap();
        assert_eq!(result.attributes.len(), 3);
        assert!(matches!(result.attributes[0].value, Value::I64(42)));
        assert!(matches!(result.attributes[1].value, Value::Bool(true)));
        assert!(
            matches!(result.attributes[2].value, Value::F64(v) if (v - 0.5).abs() < f64::EPSILON)
        );
    }

    /// String attributes inside span events are also scrubbed.
    #[test]
    fn event_string_attributes_are_scrubbed() {
        let captured = Arc::new(Mutex::new(None));
        let inner = CapturingProcessor {
            captured: Arc::clone(&captured),
        };
        let processor = RedactingSpanProcessor::new(inner);

        let secret_val = "token=sk-proj-abcdefghijklmnopqrstuvwxyz0123456789";
        let event = opentelemetry::trace::Event::new(
            "log",
            std::time::SystemTime::UNIX_EPOCH,
            vec![KeyValue::new("message", secret_val)],
            0,
        );
        let mut span = make_span_data(vec![]);
        span.events.events.push(event);
        processor.on_end(span);

        let guard = captured.lock().unwrap();
        let result = guard.as_ref().unwrap();
        let event_attr = &result.events.events[0].attributes[0];
        if let Value::String(ref s) = event_attr.value {
            assert!(
                !s.as_str().contains("sk-proj-"),
                "event attribute secret must be redacted, got: {s}"
            );
        } else {
            panic!("expected String attribute in event");
        }
    }
}
