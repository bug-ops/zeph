// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Guards the shipped default config against model ids Anthropic has retired (#7094).

use zeph_config::defaults::RETIRED_CLAUDE_MODELS;

#[test]
fn shipped_default_toml_names_no_retired_model() {
    let src = include_str!("../config/default.toml");
    for (id, _) in RETIRED_CLAUDE_MODELS {
        assert!(
            !src.contains(id),
            "default.toml still names retired model {id}"
        );
    }
}
