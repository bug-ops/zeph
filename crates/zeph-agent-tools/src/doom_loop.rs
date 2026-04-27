// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Doom-loop detection: hash message content while normalizing volatile tool IDs.

/// Hash message content for doom-loop detection, skipping volatile IDs in-place.
///
/// Normalizes `[tool_result: <id>]` → `[tool_result]` and
/// `[tool_use: <name>(<id>)]` → `[tool_use: <name>]` by feeding only stable segments
/// into the hasher without materializing the normalized string.
///
/// # Notes
///
/// `DefaultHasher` output is **not** stable across Rust versions — do not persist or
/// serialize these hashes. They are valid only for within-session equality comparison.
///
/// # Examples
///
/// ```
/// use zeph_agent_tools::doom_loop::doom_loop_hash;
///
/// let h1 = doom_loop_hash("hello world");
/// let h2 = doom_loop_hash("hello world");
/// assert_eq!(h1, h2);
///
/// // Volatile IDs are normalized
/// let h3 = doom_loop_hash("[tool_result: abc123] output");
/// let h4 = doom_loop_hash("[tool_result: xyz789] output");
/// assert_eq!(h3, h4);
/// ```
#[must_use]
pub fn doom_loop_hash(content: &str) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut hasher = DefaultHasher::new();
    let mut rest = content;
    while !rest.is_empty() {
        let r_pos = rest.find("[tool_result: ");
        let u_pos = rest.find("[tool_use: ");
        match (r_pos, u_pos) {
            (Some(r), Some(u)) if u < r => hash_tool_use_in_place(&mut hasher, &mut rest, u),
            (Some(r), _) => hash_tool_result_in_place(&mut hasher, &mut rest, r),
            (_, Some(u)) => hash_tool_use_in_place(&mut hasher, &mut rest, u),
            _ => {
                hasher.write(rest.as_bytes());
                break;
            }
        }
    }
    hasher.finish()
}

fn hash_tool_result_in_place(hasher: &mut impl std::hash::Hasher, rest: &mut &str, start: usize) {
    hasher.write(&rest.as_bytes()[..start]);
    if let Some(end) = rest[start..].find(']') {
        hasher.write(b"[tool_result]");
        *rest = &rest[start + end + 1..];
    } else {
        hasher.write(&rest.as_bytes()[start..]);
        *rest = "";
    }
}

fn hash_tool_use_in_place(hasher: &mut impl std::hash::Hasher, rest: &mut &str, start: usize) {
    hasher.write(&rest.as_bytes()[..start]);
    let tag = &rest[start..];
    // Format: "[tool_use: name(id)]" or "[tool_use: name]"
    // We want to emit "[tool_use: name]" stripping the ID.
    if let Some(paren) = tag.find('(') {
        if let Some(bracket) = tag.find(']') {
            // Emit "[tool_use: name]" (name is between ": " and "(")
            hasher.write(b"[tool_use: ");
            hasher.write(&tag.as_bytes()["[tool_use: ".len()..paren]);
            hasher.write(b"]");
            *rest = &rest[start + bracket + 1..];
        } else {
            hasher.write(&rest.as_bytes()[start..]);
            *rest = "";
        }
    } else if let Some(bracket) = tag.find(']') {
        // No parens — already in canonical form
        hasher.write(&tag.as_bytes()[..=bracket]);
        *rest = &rest[start + bracket + 1..];
    } else {
        hasher.write(&rest.as_bytes()[start..]);
        *rest = "";
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_same_hash() {
        assert_eq!(doom_loop_hash("hello"), doom_loop_hash("hello"));
    }

    #[test]
    fn different_tool_result_ids_same_hash() {
        let h1 = doom_loop_hash("[tool_result: abc] output");
        let h2 = doom_loop_hash("[tool_result: xyz] output");
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_content_different_hash() {
        let h1 = doom_loop_hash("content A");
        let h2 = doom_loop_hash("content B");
        assert_ne!(h1, h2);
    }

    #[test]
    fn empty_string_is_stable() {
        assert_eq!(doom_loop_hash(""), doom_loop_hash(""));
    }
}
