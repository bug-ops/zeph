// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

fn main() {
    #[cfg(feature = "bundled-skills")]
    {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let skills_path = std::path::Path::new(&manifest_dir).join("skills");
        assert!(
            skills_path.exists(),
            "bundled-skills feature is enabled but `skills/` symlink or directory \
             was not found at `{}`. Ensure `crates/zeph-skills/skills` points to the \
             workspace `.zeph/skills/` directory before building with this feature.",
            skills_path.display()
        );
        // Re-run if skills dir changes.
        println!("cargo:rerun-if-changed=skills");
    }
}
