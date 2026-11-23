// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7159: `load_skill` accepts an optional `path` argument that reads a
//! skill-local resource through the same trust gate as the skill body.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use parking_lot::RwLock;
use zeph_common::SkillTrustLevel;
use zeph_core::{SkillLoaderExecutor, SkillTrustSnapshot};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolCall, ToolExecutor};

const NAME: &str = "res-skill";

fn registry(dir: &Path, doc: &[u8]) -> SkillRegistry {
    let skill = dir.join(NAME);
    std::fs::create_dir_all(skill.join("references")).expect("mkdir");
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {NAME}\ndescription: test skill\n---\nBODY-MARKER"),
    )
    .expect("skill");
    std::fs::write(skill.join("references/doc.md"), doc).expect("doc");
    std::fs::write(dir.join("outside.md"), "OUTSIDE-SECRET").expect("outside");
    SkillRegistry::load(&[dir.to_path_buf()])
}

fn executor(reg: SkillRegistry, level: SkillTrustLevel) -> SkillLoaderExecutor {
    let snapshot = HashMap::from([(
        NAME.to_owned(),
        SkillTrustSnapshot {
            trust_level: level,
            requires_trust_check: false,
            blake3_hash: String::new(),
        },
    )]);
    SkillLoaderExecutor::new(Arc::new(RwLock::new(reg)), Arc::new(RwLock::new(snapshot)))
}

fn call(path: Option<&str>) -> ToolCall {
    let mut params = serde_json::json!({ "skill_name": NAME });
    if let Some(p) = path {
        params["path"] = serde_json::Value::String(p.to_owned());
    }
    ToolCall {
        tool_id: zeph_common::ToolName::new("load_skill"),
        params: params.as_object().expect("object").clone(),
        attribution: zeph_tools::CallAttribution::default(),
        context: None,
        tool_call_id: None,
    }
}

async fn summary(exec: &SkillLoaderExecutor, path: Option<&str>) -> String {
    exec.execute_tool_call(&call(path))
        .await
        .expect("no tool error")
        .expect("output")
        .summary
}

#[tokio::test]
async fn test_load_skill_with_path_returns_resource_content_not_body() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exec = executor(
        registry(dir.path(), b"RESOURCE-CONTENT"),
        SkillTrustLevel::Trusted,
    );
    let out = summary(&exec, Some("references/doc.md")).await;
    assert!(out.contains("RESOURCE-CONTENT"), "{out}");
    assert!(!out.contains("BODY-MARKER"), "{out}");
}

#[tokio::test]
async fn test_load_skill_without_path_still_returns_body() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exec = executor(
        registry(dir.path(), b"RESOURCE-CONTENT"),
        SkillTrustLevel::Trusted,
    );
    let out = summary(&exec, None).await;
    assert!(out.contains("BODY-MARKER"), "{out}");
}

#[tokio::test]
async fn test_load_skill_path_traversal_and_unlisted_paths_never_leak_content() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exec = executor(
        registry(dir.path(), b"RESOURCE-CONTENT"),
        SkillTrustLevel::Trusted,
    );
    for bad in [
        "../outside.md",
        "references/../../outside.md",
        "/etc/passwd",
        "references/missing.md",
        "SKILL.md",
    ] {
        let out = exec
            .execute_tool_call(&call(Some(bad)))
            .await
            .ok()
            .flatten()
            .map_or_default(|o| o.summary);
        assert!(!out.contains("OUTSIDE-SECRET"), "{bad}: {out}");
        assert!(!out.contains("RESOURCE-CONTENT"), "{bad}: {out}");
        assert!(!out.contains("BODY-MARKER"), "{bad}: {out}");
    }
}

#[tokio::test]
async fn test_load_skill_path_blocked_skill_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exec = executor(
        registry(dir.path(), b"RESOURCE-CONTENT"),
        SkillTrustLevel::Blocked,
    );
    let out = summary(&exec, Some("references/doc.md")).await;
    assert!(out.contains("blocked by policy"), "{out}");
    assert!(!out.contains("RESOURCE-CONTENT"), "{out}");
}

#[tokio::test]
async fn test_load_skill_path_quarantined_resource_is_wrapped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exec = executor(
        registry(dir.path(), b"RESOURCE-CONTENT"),
        SkillTrustLevel::Quarantined,
    );
    let out = summary(&exec, Some("references/doc.md")).await;
    assert!(out.contains("QUARANTINED"), "{out}");
    assert!(out.contains("RESOURCE-CONTENT"), "{out}");
}

#[tokio::test]
async fn test_load_skill_path_over_256_kib_is_not_returned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut big = vec![b'a'; 256 * 1024 + 1];
    big[..9].copy_from_slice(b"BIG-START");
    let exec = executor(registry(dir.path(), &big), SkillTrustLevel::Trusted);
    let out = summary(&exec, Some("references/doc.md")).await;
    assert!(!out.contains("BIG-START"), "oversized resource leaked");
    assert!(!out.contains("BODY-MARKER"), "path was ignored: {out}");
}

#[tokio::test]
async fn test_load_skill_path_non_utf8_resource_is_not_returned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend_from_slice(b"NONUTF8-MARK");
    let exec = executor(registry(dir.path(), &bytes), SkillTrustLevel::Trusted);
    let out = summary(&exec, Some("references/doc.md")).await;
    assert!(!out.contains("NONUTF8-MARK"), "lossy decode leaked: {out}");
    assert!(!out.contains("BODY-MARKER"), "path was ignored: {out}");
}

#[test]
fn test_load_skill_definition_documents_skill_name_and_path_params() {
    let exec = SkillLoaderExecutor::new(
        Arc::new(RwLock::new(SkillRegistry::empty())),
        Arc::new(RwLock::new(HashMap::new())),
    );
    let def = &exec.tool_definitions()[0];
    assert!(
        def.description.contains("skill_name"),
        "{}",
        def.description
    );
    assert!(def.description.contains("path"), "{}", def.description);
    assert!(
        !def.description.contains("{\"name\""),
        "stale example: {}",
        def.description
    );
}
