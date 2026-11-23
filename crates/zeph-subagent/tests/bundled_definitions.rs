// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for the bundled `.zeph/agents` definitions (#7153, #7154): they must parse,
//! raise no load warnings and not name a non-existent provider.

use std::path::PathBuf;

use zeph_subagent::{ModelSpec, SubAgentDef};

#[test]
fn test_bundled_definitions_parse_without_warnings_and_inherit_model() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.zeph/agents");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read definition");
        let def = SubAgentDef::parse(&text)
            .unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()));
        assert!(
            def.warnings().is_empty(),
            "{} has load warnings: {:?}",
            path.display(),
            def.warnings()
        );
        assert!(
            matches!(def.model, None | Some(ModelSpec::Inherit)),
            "{} names a model that is not a provider: {:?}",
            path.display(),
            def.model
        );
        seen += 1;
    }
    assert!(
        seen >= 9,
        "expected the 9 bundled definitions, found {seen}"
    );
}
