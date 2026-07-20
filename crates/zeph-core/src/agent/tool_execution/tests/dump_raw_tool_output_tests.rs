// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression coverage for #6364: `DebugDumper::dump_tool_output` had no reachable production
//! call site — the only callers were `#[cfg(test)]`-gated legacy functions. These tests drive
//! the real `process_one_tool_result` path (not `DebugDumper::dump_tool_output` in isolation)
//! and assert a dump file is actually written to disk, which is the exact defect class the fix
//! (`dump_raw_tool_output`, `tool_result.rs`) closes. Reverting that call site makes
//! `process_one_tool_result_writes_raw_dump_file_on_success` fail.

use std::path::Path;
use std::time::Duration;

use zeph_llm::provider::MessagePart;
use zeph_sanitizer::pii::{PiiFilter, PiiFilterConfig};
use zeph_tools::OverflowConfig;
use zeph_tools::executor::{ToolError, ToolOutput};

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::debug_dump::{DebugDumper, DumpFormat};

fn make_tool_use_request(id: &str, name: &str) -> zeph_llm::provider::ToolUseRequest {
    zeph_llm::provider::ToolUseRequest {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({"command": "echo test"}),
    }
}

fn make_agent_with_dumper(dumper: DebugDumper) -> crate::agent::Agent<MockChannel> {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let mut agent = crate::agent::Agent::new(
        provider,
        channel,
        registry,
        None,
        5,
        MockToolExecutor::no_tools(),
    );
    agent.runtime.debug.debug_dumper = Some(dumper);
    agent
}

/// Polls the dumper's timestamped session subdirectory for `filename`, up to ~1s — writes go
/// through `DebugDumper::write`'s `spawn_blocking` fire-and-forget task, so they are not
/// necessarily on disk the instant `process_one_tool_result` returns. Mirrors the
/// `read_dump_file` helper in `debug_dump::tests`.
async fn wait_for_dump_file(base_dir: &Path, filename: &str) -> Option<String> {
    let session = std::fs::read_dir(base_dir).ok()?.next()?.ok()?.path();
    let path = session.join(filename);
    for _ in 0..200 {
        if let Ok(content) = std::fs::read_to_string(&path)
            && !content.is_empty()
        {
            return Some(content);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    None
}

/// Lists every filename actually written to the dumper's session subdirectory, after a brief
/// settle delay for any in-flight `spawn_blocking` writes. Used for negative assertions ("this
/// filename must never appear") where polling-until-present doesn't apply.
async fn dump_dir_filenames(base_dir: &Path) -> Vec<String> {
    tokio::time::sleep(Duration::from_millis(100)).await;
    let Some(Ok(session_entry)) = std::fs::read_dir(base_dir).ok().and_then(|mut e| e.next())
    else {
        return Vec::new();
    };
    std::fs::read_dir(session_entry.path())
        .map(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn process_one_tool_result_writes_raw_dump_file_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let dumper = DebugDumper::new(dir.path(), DumpFormat::Json).unwrap();
    let mut agent = make_agent_with_dumper(dumper);

    let tc = make_tool_use_request("id-dump-ok", "bash");
    agent
        .process_one_tool_result(
            &tc,
            "id-dump-ok",
            &std::time::Instant::now(),
            Ok(Some(ToolOutput {
                tool_name: "bash".into(),
                summary: "hello from bash".into(),
                ..Default::default()
            })),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
        )
        .await
        .unwrap();

    let content = wait_for_dump_file(dir.path(), "0000-tool-bash.txt")
        .await
        .expect(
            "process_one_tool_result must write a raw tool-output dump file via \
             dump_raw_tool_output (#6364) — no file appeared within 1s",
        );
    assert!(
        content.contains("hello from bash"),
        "dump file must contain the raw tool output, got: {content}"
    );
}

#[tokio::test]
async fn process_one_tool_result_dump_captures_raw_output_before_truncation() {
    // Proves the dump is the *raw, pre-summarization* output (per dump_raw_tool_output's doc
    // comment), not the truncated text actually sent to the LLM: a low overflow threshold
    // forces truncation of the channel-facing content, while the dump file must still hold
    // the full, untruncated output.
    let dir = tempfile::tempdir().unwrap();
    let dumper = DebugDumper::new(dir.path(), DumpFormat::Json).unwrap();
    let mut agent = make_agent_with_dumper(dumper);
    agent.tool_orchestrator.overflow_config = OverflowConfig {
        threshold: 100,
        retention_days: 7,
        max_overflow_bytes: 0,
        max_per_call_override: 1_000,
    };

    // Interspersed with spaces so it never forms a long contiguous base64-alphabet run —
    // `DebugDumper::dump_tool_output` independently applies `redact_binary_blobs`, which would
    // otherwise redact a purely-repeated-letter blob before this assertion ever sees it.
    let long_output = "raw output segment ".repeat(400);
    let tc = make_tool_use_request("id-dump-raw", "srv:big_tool");
    let mut result_parts = Vec::new();
    agent
        .process_one_tool_result(
            &tc,
            "id-dump-raw",
            &std::time::Instant::now(),
            Ok(Some(ToolOutput {
                tool_name: "srv:big_tool".into(),
                summary: long_output.clone(),
                ..Default::default()
            })),
            &mut result_parts,
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
        )
        .await
        .unwrap();

    let content = wait_for_dump_file(dir.path(), "0000-tool-srv_big_tool.txt")
        .await
        .expect("dump file must be written for a successful tool call");
    assert!(
        content.contains(&long_output),
        "dump must contain the FULL raw output, not the truncated version sent to the LLM"
    );

    let sent_content = result_parts
        .iter()
        .find_map(|p| match p {
            MessagePart::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .expect("expected a ToolResult message part");
    assert!(
        sent_content.len() < long_output.len(),
        "sanity check: LLM-facing content must actually be truncated relative to the raw \
         output, otherwise this test isn't exercising the raw-vs-summarized distinction"
    );
}

#[tokio::test]
async fn process_one_tool_result_dump_scrubs_pii_when_filter_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let dumper = DebugDumper::new(dir.path(), DumpFormat::Json).unwrap();
    let mut agent = make_agent_with_dumper(dumper);
    agent.services.security.pii_filter = PiiFilter::new(PiiFilterConfig {
        enabled: true,
        ..Default::default()
    });

    let tc = make_tool_use_request("id-pii-on", "bash");
    agent
        .process_one_tool_result(
            &tc,
            "id-pii-on",
            &std::time::Instant::now(),
            Ok(Some(ToolOutput {
                tool_name: "bash".into(),
                summary: "contact me at alice@example.com".into(),
                ..Default::default()
            })),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
        )
        .await
        .unwrap();

    let content = wait_for_dump_file(dir.path(), "0000-tool-bash.txt")
        .await
        .expect("dump file must be written for a successful tool call");
    assert!(
        !content.contains("alice@example.com"),
        "raw email must not reach disk when the agent's PII filter is enabled, got: {content}"
    );
}

#[tokio::test]
async fn process_one_tool_result_dump_keeps_content_when_pii_filter_disabled() {
    // Explicitly disabled (the agent's actual default has the PII filter *enabled* — see
    // `SecurityState::default()`). `scrub_content`/`redact_binary_blobs` (applied
    // unconditionally inside `DebugDumper::dump_tool_output`) target secrets/paths/binary
    // blobs, not emails, so a plain email must survive to disk verbatim in this configuration —
    // this is the negative control proving the PII-enabled test above is actually exercising
    // `dump_raw_tool_output`'s PII branch, not some other redaction layer.
    let dir = tempfile::tempdir().unwrap();
    let dumper = DebugDumper::new(dir.path(), DumpFormat::Json).unwrap();
    let mut agent = make_agent_with_dumper(dumper);
    agent.services.security.pii_filter = PiiFilter::new(PiiFilterConfig {
        enabled: false,
        ..Default::default()
    });

    let tc = make_tool_use_request("id-pii-off", "bash");
    agent
        .process_one_tool_result(
            &tc,
            "id-pii-off",
            &std::time::Instant::now(),
            Ok(Some(ToolOutput {
                tool_name: "bash".into(),
                summary: "contact me at alice@example.com".into(),
                ..Default::default()
            })),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
        )
        .await
        .unwrap();

    let content = wait_for_dump_file(dir.path(), "0000-tool-bash.txt")
        .await
        .expect("dump file must be written for a successful tool call");
    assert!(
        content.contains("alice@example.com"),
        "with the PII filter disabled (default), the dump must retain the original content, \
         got: {content}"
    );
}

#[tokio::test]
async fn process_one_tool_result_skips_raw_dump_on_error() {
    let dir = tempfile::tempdir().unwrap();
    let dumper = DebugDumper::new(dir.path(), DumpFormat::Json).unwrap();
    let mut agent = make_agent_with_dumper(dumper);

    let tc = make_tool_use_request("id-err-dump", "bash");
    let err = ToolError::InvalidParams {
        message: "missing required field 'command'".into(),
    };
    agent
        .process_one_tool_result(
            &tc,
            "id-err-dump",
            &std::time::Instant::now(),
            Err(err),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
        )
        .await
        .unwrap();

    let filenames = dump_dir_filenames(dir.path()).await;
    assert!(
        filenames.iter().any(|f| f.contains("tool-error-bash")),
        "sanity check: the pre-existing (unrelated) dump_tool_error call site in \
         classify_tool_result should still fire on error, got: {filenames:?}"
    );
    assert!(
        !filenames.iter().any(|f| f.ends_with("-tool-bash.txt")),
        "dump_raw_tool_output must be a no-op for is_error=true results — errors are dumped via \
         dump_tool_error instead, got: {filenames:?}"
    );
}

#[tokio::test]
async fn process_one_tool_result_dump_is_noop_when_debug_dumps_disabled() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let mut agent = crate::agent::Agent::new(
        provider,
        channel,
        registry,
        None,
        5,
        MockToolExecutor::no_tools(),
    );
    assert!(
        agent.runtime.debug.debug_dumper.is_none(),
        "precondition: debug dumps must be disabled by default"
    );

    let tc = make_tool_use_request("id-no-dump", "bash");
    // Must complete without panicking when there is no dumper to write through.
    agent
        .process_one_tool_result(
            &tc,
            "id-no-dump",
            &std::time::Instant::now(),
            Ok(Some(ToolOutput {
                tool_name: "bash".into(),
                summary: "no dumper attached".into(),
                ..Default::default()
            })),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut false,
            &mut None,
            &mut Vec::new(),
            &mut 0,
            &mut zeph_sanitizer::ContentTrustLevel::Trusted,
        )
        .await
        .unwrap();
}
