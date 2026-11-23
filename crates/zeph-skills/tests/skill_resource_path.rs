// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7159: model-supplied skill resource paths are parsed into a newtype
//! and loaded with containment, size and encoding checks.

use std::fs;

use zeph_skills::resource::{
    MAX_SKILL_RESOURCE_BYTES, SkillResourcePath, SkillResources, format_resource_listing,
    load_skill_resource,
};

fn skill_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("references")).expect("mkdir");
    fs::write(dir.path().join("references/doc.md"), "# Doc").expect("write");
    dir
}

#[test]
fn test_skill_resource_path_parse_accepts_known_subdirectories() {
    for ok in [
        "references/doc.md",
        "scripts/run.sh",
        "assets/logo.png",
        "references/sub/deep.md",
    ] {
        let p = SkillResourcePath::parse(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        assert_eq!(p.as_str(), ok);
    }
}

#[test]
fn test_skill_resource_path_parse_rejects_unsafe_shapes() {
    let too_long = format!("references/{}", "a".repeat(513));
    for bad in [
        "",
        "doc.md",
        "references",
        "references/../../etc/passwd",
        "../references/doc.md",
        "/etc/passwd",
        "/references/doc.md",
        "other/doc.md",
        "SKILL.md",
        "references\\doc.md",
        "references/doc\0.md",
        too_long.as_str(),
    ] {
        assert!(
            SkillResourcePath::parse(bad).is_err(),
            "must reject {bad:?}"
        );
    }
}

#[test]
fn test_load_skill_resource_reads_file_inside_skill_dir() {
    let dir = skill_dir();
    let p = SkillResourcePath::parse("references/doc.md").expect("path");
    assert_eq!(load_skill_resource(dir.path(), &p).expect("load"), "# Doc");
}

#[test]
fn test_load_skill_resource_missing_file_is_error() {
    let dir = skill_dir();
    let p = SkillResourcePath::parse("references/nope.md").expect("path");
    assert!(load_skill_resource(dir.path(), &p).is_err());
}

#[cfg(unix)]
#[test]
fn test_load_skill_resource_symlink_escaping_skill_dir_is_error() {
    let dir = skill_dir();
    let outside = tempfile::tempdir().expect("outside");
    fs::write(outside.path().join("secret.md"), "secret").expect("write");
    std::os::unix::fs::symlink(
        outside.path().join("secret.md"),
        dir.path().join("references/link.md"),
    )
    .expect("symlink");
    let p = SkillResourcePath::parse("references/link.md").expect("path");
    assert!(load_skill_resource(dir.path(), &p).is_err());
}

#[test]
fn test_load_skill_resource_over_size_limit_is_error() {
    let dir = skill_dir();
    let limit = usize::try_from(MAX_SKILL_RESOURCE_BYTES).expect("limit fits usize");
    fs::write(dir.path().join("references/big.md"), vec![b'a'; limit + 1]).expect("write");
    fs::write(dir.path().join("references/edge.md"), vec![b'a'; limit]).expect("write");
    let big = SkillResourcePath::parse("references/big.md").expect("path");
    let edge = SkillResourcePath::parse("references/edge.md").expect("path");
    assert!(load_skill_resource(dir.path(), &big).is_err());
    assert_eq!(
        load_skill_resource(dir.path(), &edge).expect("edge").len(),
        limit
    );
}

#[test]
fn test_load_skill_resource_non_utf8_is_error() {
    let dir = skill_dir();
    fs::write(dir.path().join("references/bin.md"), [0xff, 0xfe, 0x00]).expect("write");
    let p = SkillResourcePath::parse("references/bin.md").expect("path");
    assert!(load_skill_resource(dir.path(), &p).is_err());
}

#[test]
fn test_format_resource_listing_uses_skill_relative_paths_and_hint() {
    let dir = skill_dir();
    let resources = SkillResources {
        references: vec![dir.path().join("references/doc.md")],
        ..SkillResources::default()
    };
    let listing = format_resource_listing(&resources, dir.path()).expect("listing");
    assert!(listing.contains("references/doc.md"), "{listing}");
    assert!(
        !listing.contains(&dir.path().display().to_string()),
        "absolute path leaked: {listing}"
    );
    assert!(listing.contains("load_skill"), "{listing}");
    assert!(listing.contains("path"), "{listing}");
}

#[test]
fn test_format_resource_listing_none_when_no_resources() {
    let dir = skill_dir();
    assert!(format_resource_listing(&SkillResources::default(), dir.path()).is_none());
}
