// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for `FileExecutor` sandbox access controls.
//!
//! All tests use `tempfile::TempDir` for FS isolation and exercise the public
//! `FileExecutor::new` / `execute_file_tool` API from outside the crate.

use std::path::PathBuf;

use tempfile::TempDir;
use zeph_tools::{FileExecutor, ToolError};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_params(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn sandbox(dir: &TempDir) -> FileExecutor {
    FileExecutor::new(vec![dir.path().to_path_buf()])
}

// ---------------------------------------------------------------------------
// Basic allowed-path access
// ---------------------------------------------------------------------------

#[test]
fn allowed_path_permits_read_write() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("hello.txt");
    std::fs::write(&file, "hello world").unwrap();

    let exec = sandbox(&dir);

    let read_params = make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]);
    let result = exec
        .execute_file_tool("read", &read_params)
        .unwrap()
        .unwrap();
    assert!(
        result.summary.contains("hello world"),
        "expected file content in read summary, got: {}",
        result.summary
    );

    let write_params = make_params(&[
        ("path", serde_json::json!(file.to_str().unwrap())),
        ("content", serde_json::json!("overwritten")),
    ]);
    exec.execute_file_tool("write", &write_params)
        .unwrap()
        .unwrap();

    let verify_params = make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]);
    let verify = exec
        .execute_file_tool("read", &verify_params)
        .unwrap()
        .unwrap();
    assert!(
        verify.summary.contains("overwritten"),
        "expected overwritten content after write, got: {}",
        verify.summary
    );
}

// ---------------------------------------------------------------------------
// Disallowed path blocks all operations
// ---------------------------------------------------------------------------

#[test]
fn disallowed_path_blocks_all_operations() {
    let sandbox_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    let outside_file = outside_dir.path().join("target.txt");
    std::fs::write(&outside_file, "secret").unwrap();
    let outside_dir2 = TempDir::new().unwrap();
    let inside_file = sandbox_dir.path().join("src.txt");
    std::fs::write(&inside_file, "data").unwrap();

    let exec = sandbox(&sandbox_dir);
    let outside_path = serde_json::json!(outside_file.to_str().unwrap());
    let outside_dir_path = serde_json::json!(outside_dir.path().to_str().unwrap());
    let dst_path = serde_json::json!(outside_dir2.path().join("dst.txt").to_str().unwrap());

    let cases: &[(&str, serde_json::Map<String, serde_json::Value>)] = &[
        ("read", make_params(&[("path", outside_path.clone())])),
        (
            "write",
            make_params(&[
                ("path", outside_path.clone()),
                ("content", serde_json::json!("pwned")),
            ]),
        ),
        (
            "edit",
            make_params(&[
                ("path", outside_path.clone()),
                ("old_string", serde_json::json!("secret")),
                ("new_string", serde_json::json!("pwned")),
            ]),
        ),
        (
            "list_directory",
            make_params(&[("path", outside_dir_path.clone())]),
        ),
        (
            "create_directory",
            make_params(&[(
                "path",
                serde_json::json!(outside_dir.path().join("new").to_str().unwrap()),
            )]),
        ),
        (
            "delete_path",
            make_params(&[("path", outside_path.clone())]),
        ),
        (
            "move_path",
            make_params(&[
                ("source", serde_json::json!(inside_file.to_str().unwrap())),
                ("destination", dst_path.clone()),
            ]),
        ),
        (
            "copy_path",
            make_params(&[
                ("source", serde_json::json!(inside_file.to_str().unwrap())),
                ("destination", dst_path.clone()),
            ]),
        ),
    ];

    for (tool, params) in cases {
        let result = exec.execute_file_tool(tool, params);
        assert!(
            matches!(result, Err(ToolError::SandboxViolation { .. })),
            "tool '{tool}': expected SandboxViolation for outside-sandbox path, got: {result:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Symlink escape (unix-only)
// ---------------------------------------------------------------------------

/// A symlink inside the sandbox pointing directly outside must be denied.
#[cfg(unix)]
#[test]
fn symlink_escape_blocked() {
    let sandbox_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    let outside_file = outside_dir.path().join("outside.txt");
    std::fs::write(&outside_file, "escaped").unwrap();

    let link = sandbox_dir.path().join("link.txt");
    std::os::unix::fs::symlink(&outside_file, &link).unwrap();

    let exec = sandbox(&sandbox_dir);
    let params = make_params(&[("path", serde_json::json!(link.to_str().unwrap()))]);
    let result = exec.execute_file_tool("read", &params);
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "expected SandboxViolation when reading a symlink that escapes the sandbox, \
         got: {result:?}"
    );
}

/// A multi-hop symlink chain that ultimately resolves outside the sandbox must be denied.
#[cfg(unix)]
#[test]
fn nested_symlink_chain_blocked() {
    let sandbox_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    let outside_file = outside_dir.path().join("deep.txt");
    std::fs::write(&outside_file, "deep escaped").unwrap();

    // link1 -> link2 -> outside_file
    let link2 = sandbox_dir.path().join("link2.txt");
    std::os::unix::fs::symlink(&outside_file, &link2).unwrap();
    let link1 = sandbox_dir.path().join("link1.txt");
    std::os::unix::fs::symlink(&link2, &link1).unwrap();

    let exec = sandbox(&sandbox_dir);
    let params = make_params(&[("path", serde_json::json!(link1.to_str().unwrap()))]);
    let result = exec.execute_file_tool("read", &params);
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "expected SandboxViolation for multi-hop symlink chain escaping sandbox, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Path traversal
// ---------------------------------------------------------------------------

#[test]
fn parent_traversal_blocked() {
    let sandbox_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    let outside_file = outside_dir.path().join("escape.txt");
    std::fs::write(&outside_file, "escaped").unwrap();

    // Build a real traversal path: <sandbox>/<depth * ../> + outside components.
    // All intermediate dirs exist, so resolve_via_ancestors canonicalizes fully
    // and the sandbox check fires on the real resolved path.
    let depth = sandbox_dir.path().components().count();
    let mut traversal = sandbox_dir.path().to_path_buf();
    for _ in 0..depth {
        traversal.push("..");
    }
    for component in outside_file.components().skip(1) {
        traversal.push(component);
    }

    let exec = sandbox(&sandbox_dir);
    let params = make_params(&[("path", serde_json::json!(traversal.to_str().unwrap()))]);
    let result = exec.execute_file_tool("read", &params);
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "expected SandboxViolation for path traversal ({}), got: {result:?}",
        traversal.display()
    );
}

// ---------------------------------------------------------------------------
// Multiple allowed paths
// ---------------------------------------------------------------------------

#[test]
fn multiple_allowed_paths() {
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    let file_a = dir_a.path().join("file_a.txt");
    let file_b = dir_b.path().join("file_b.txt");
    let outside_file = outside_dir.path().join("outside.txt");
    std::fs::write(&file_a, "from A").unwrap();
    std::fs::write(&file_b, "from B").unwrap();
    std::fs::write(&outside_file, "outside").unwrap();

    let exec = FileExecutor::new(vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()]);

    let result_a = exec
        .execute_file_tool(
            "read",
            &make_params(&[("path", serde_json::json!(file_a.to_str().unwrap()))]),
        )
        .unwrap()
        .unwrap();
    assert!(
        result_a.summary.contains("from A"),
        "expected to read file_a from dir_a, got: {}",
        result_a.summary
    );

    let result_b = exec
        .execute_file_tool(
            "write",
            &make_params(&[
                ("path", serde_json::json!(file_b.to_str().unwrap())),
                ("content", serde_json::json!("written to B")),
            ]),
        )
        .unwrap()
        .unwrap();
    assert!(
        result_b.summary.contains("Wrote"),
        "expected write to succeed in dir_b, got: {}",
        result_b.summary
    );

    let outside_result = exec.execute_file_tool(
        "read",
        &make_params(&[("path", serde_json::json!(outside_file.to_str().unwrap()))]),
    );
    assert!(
        matches!(outside_result, Err(ToolError::SandboxViolation { .. })),
        "expected SandboxViolation for outside_dir (not in allowed_paths), got: {outside_result:?}"
    );
}

// ---------------------------------------------------------------------------
// Empty allowed_paths defaults to cwd
// ---------------------------------------------------------------------------

#[test]
fn empty_allowed_paths_defaults_to_cwd() {
    // FileExecutor::new([]) falls back to current_dir() as the allowed path.
    // A file in the real cwd must be accessible.
    let cwd = std::env::current_dir().unwrap();
    let file = cwd.join("Cargo.toml"); // always present in the workspace root

    let exec = FileExecutor::new(vec![]);
    let params = make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]);
    let result = exec.execute_file_tool("read", &params);
    assert!(
        result.is_ok(),
        "expected Cargo.toml in cwd to be readable with empty allowed_paths, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Tilde in allowed_paths — regression for #2115
// ---------------------------------------------------------------------------

/// Regression test for #2115: `~/path` in `allowed_paths` is NOT expanded by
/// `FileExecutor::new` — tilde is a shell feature, not a `PathBuf` feature.
/// `canonicalize()` on a literal `~/path` fails, and `unwrap_or` keeps the
/// literal, which never matches any canonical path. Result: all access is
/// silently blocked when the executor has only a tilde path.
///
/// This test documents the CURRENT (buggy) behavior:
/// - tilde-only `allowed_paths` → `SandboxViolation` for any real file
/// - mixing tilde + real path → real path still works (tilde is dead weight)
///
/// When #2115 is fixed, update these assertions to verify tilde IS expanded.
#[test]
fn tilde_in_allowed_paths_regression() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("data.txt");
    std::fs::write(&file, "content").unwrap();

    // Tilde-only: no real path can match the literal "~/nonexistent" string.
    let exec_tilde_only = FileExecutor::new(vec![PathBuf::from("~/nonexistent")]);
    let result = exec_tilde_only.execute_file_tool(
        "read",
        &make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]),
    );
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "expected SandboxViolation when allowed_paths contains only a literal tilde path \
         (tilde not expanded — #2115), got: {result:?}"
    );

    // Tilde + real path: the real path still permits access.
    let exec_mixed = FileExecutor::new(vec![
        PathBuf::from("~/nonexistent"),
        dir.path().to_path_buf(),
    ]);
    let result_mixed = exec_mixed
        .execute_file_tool(
            "read",
            &make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]),
        )
        .unwrap()
        .unwrap();
    assert!(
        result_mixed.summary.contains("content"),
        "expected real path to be accessible even when tilde path is also in allowed_paths, \
         got: {}",
        result_mixed.summary
    );
}

// ---------------------------------------------------------------------------
// Nonexistent allowed path handled gracefully
// ---------------------------------------------------------------------------

#[test]
fn nonexistent_allowed_path_handled_gracefully() {
    // A nonexistent path in allowed_paths must not panic during construction.
    // canonicalize() will fail and unwrap_or keeps the raw path. Access to
    // other valid paths in allowed_paths still works.
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("ok.txt");
    std::fs::write(&file, "accessible").unwrap();

    let exec = FileExecutor::new(vec![
        PathBuf::from("/tmp/zeph_test_nonexistent_path_xyz123"),
        dir.path().to_path_buf(),
    ]);

    let result = exec
        .execute_file_tool(
            "read",
            &make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]),
        )
        .unwrap()
        .unwrap();
    assert!(
        result.summary.contains("accessible"),
        "expected valid path to work even when another allowed_path doesn't exist, \
         got: {}",
        result.summary
    );
}

// ---------------------------------------------------------------------------
// delete_path refuses sandbox root
// ---------------------------------------------------------------------------

#[test]
fn delete_sandbox_root_blocked() {
    let dir = TempDir::new().unwrap();
    let exec = sandbox(&dir);

    let params = make_params(&[
        ("path", serde_json::json!(dir.path().to_str().unwrap())),
        ("recursive", serde_json::json!(true)),
    ]);
    let result = exec.execute_file_tool("delete_path", &params);
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "expected SandboxViolation when attempting to delete the sandbox root, got: {result:?}"
    );
    assert!(
        dir.path().exists(),
        "sandbox root must not have been deleted"
    );
}

// ---------------------------------------------------------------------------
// Cross-boundary move and copy — all four cases
// ---------------------------------------------------------------------------

#[test]
fn cross_boundary_move_blocked() {
    let sandbox_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    let inside_file = sandbox_dir.path().join("inside.txt");
    let outside_file = outside_dir.path().join("outside.txt");
    std::fs::write(&inside_file, "data").unwrap();
    std::fs::write(&outside_file, "outside").unwrap();

    let exec = sandbox(&sandbox_dir);

    // 1. move from sandbox to outside — destination violation
    let result = exec.execute_file_tool(
        "move_path",
        &make_params(&[
            ("source", serde_json::json!(inside_file.to_str().unwrap())),
            (
                "destination",
                serde_json::json!(outside_dir.path().join("moved.txt").to_str().unwrap()),
            ),
        ]),
    );
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "move sandbox->outside: expected SandboxViolation, got: {result:?}"
    );

    // 2. move from outside to sandbox — source violation
    let result = exec.execute_file_tool(
        "move_path",
        &make_params(&[
            ("source", serde_json::json!(outside_file.to_str().unwrap())),
            (
                "destination",
                serde_json::json!(sandbox_dir.path().join("dst.txt").to_str().unwrap()),
            ),
        ]),
    );
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "move outside->sandbox: expected SandboxViolation, got: {result:?}"
    );

    // 3. copy from sandbox to outside — destination violation
    let result = exec.execute_file_tool(
        "copy_path",
        &make_params(&[
            ("source", serde_json::json!(inside_file.to_str().unwrap())),
            (
                "destination",
                serde_json::json!(outside_dir.path().join("copied.txt").to_str().unwrap()),
            ),
        ]),
    );
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "copy sandbox->outside: expected SandboxViolation, got: {result:?}"
    );

    // 4. copy from outside to sandbox — source violation
    let result = exec.execute_file_tool(
        "copy_path",
        &make_params(&[
            ("source", serde_json::json!(outside_file.to_str().unwrap())),
            (
                "destination",
                serde_json::json!(sandbox_dir.path().join("dst2.txt").to_str().unwrap()),
            ),
        ]),
    );
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "copy outside->sandbox: expected SandboxViolation, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// write creates parent directories within sandbox
// ---------------------------------------------------------------------------

#[test]
fn write_creates_parent_dirs_within_sandbox() {
    let dir = TempDir::new().unwrap();
    let deep = dir.path().join("a").join("b").join("c").join("file.txt");

    let exec = sandbox(&dir);
    let params = make_params(&[
        ("path", serde_json::json!(deep.to_str().unwrap())),
        ("content", serde_json::json!("deep content")),
    ]);
    exec.execute_file_tool("write", &params).unwrap().unwrap();

    assert!(deep.exists(), "deeply nested file must exist after write");
    assert_eq!(
        std::fs::read_to_string(&deep).unwrap(),
        "deep content",
        "file content must match what was written"
    );
}

// ---------------------------------------------------------------------------
// grep path validated
// ---------------------------------------------------------------------------

#[test]
fn grep_path_validated() {
    let sandbox_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    std::fs::write(outside_dir.path().join("secret.txt"), "password: hunter2").unwrap();

    let exec = sandbox(&sandbox_dir);
    let params = make_params(&[
        ("pattern", serde_json::json!("password")),
        (
            "path",
            serde_json::json!(outside_dir.path().to_str().unwrap()),
        ),
    ]);
    let result = exec.execute_file_tool("grep", &params);
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "expected SandboxViolation when grep path is outside sandbox, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// find_path excludes files outside sandbox
// ---------------------------------------------------------------------------

#[test]
fn find_path_stays_in_sandbox() {
    let sandbox_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();

    std::fs::write(sandbox_dir.path().join("inside.txt"), "in").unwrap();
    std::fs::write(outside_dir.path().join("outside.txt"), "out").unwrap();

    let exec = sandbox(&sandbox_dir);

    // Pattern anchored in sandbox — inside.txt must appear in results.
    let pattern_inside = format!("{}/*.txt", sandbox_dir.path().display());
    let result_inside = exec
        .execute_file_tool(
            "find_path",
            &make_params(&[("pattern", serde_json::json!(pattern_inside))]),
        )
        .unwrap()
        .unwrap();
    assert!(
        result_inside.summary.contains("inside.txt"),
        "expected inside.txt in find_path results, got: {}",
        result_inside.summary
    );

    // Pattern explicitly targets the outside dir — post-filter must exclude it.
    let pattern_outside = format!("{}/*.txt", outside_dir.path().display());
    let result_outside = exec
        .execute_file_tool(
            "find_path",
            &make_params(&[("pattern", serde_json::json!(pattern_outside))]),
        )
        .unwrap()
        .unwrap();
    assert!(
        !result_outside.summary.contains("outside.txt"),
        "find_path must not return files outside the sandbox, got: {}",
        result_outside.summary
    );
}

// ---------------------------------------------------------------------------
// grep with no path param uses "." which must be validated against sandbox
// ---------------------------------------------------------------------------

#[test]
fn grep_default_path_stays_in_sandbox() {
    // sandbox_dir is NOT the current working directory, so grep without a path
    // param defaults to "." (CWD) which is outside sandbox — must be rejected.
    let sandbox_dir = TempDir::new().unwrap();
    let exec = sandbox(&sandbox_dir);

    // No "path" key — defaults to "."
    let params = make_params(&[("pattern", serde_json::json!("anything"))]);
    let result = exec.execute_file_tool("grep", &params);
    assert!(
        matches!(result, Err(ToolError::SandboxViolation { .. })),
        "grep without path param defaults to CWD which is outside sandbox — \
         expected SandboxViolation, got: {result:?}"
    );
}
