// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared wizard input validators used across `init` steps.

/// Parses an optional positive integer field: blank input means "unset" (`None`); a `0`
/// is rejected (matches `Config::validate`'s rejection of `Some(0)` for `max_worktrees`/
/// `disk_quota_mb`/`default_idle_timeout_secs` — see `crates/zeph-config/src/loader.rs`) so
/// the wizard cannot emit a self-contradictory config.
pub(super) fn parse_optional_nonzero<T>(input: &str) -> Result<Option<T>, String>
where
    T: std::str::FromStr + PartialEq + Default,
{
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value: T = trimmed
        .parse()
        .map_err(|_| format!("'{trimmed}' is not a valid positive integer"))?;
    if value == T::default() {
        return Err("must be > 0, or blank to leave unset".to_owned());
    }
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_optional_nonzero_blank_is_none() {
        assert_eq!(parse_optional_nonzero::<usize>(""), Ok(None));
        assert_eq!(parse_optional_nonzero::<usize>("   "), Ok(None));
    }

    #[test]
    fn parse_optional_nonzero_positive_value_ok() {
        assert_eq!(parse_optional_nonzero::<usize>("5"), Ok(Some(5)));
        assert_eq!(parse_optional_nonzero::<u64>("2048"), Ok(Some(2048)));
    }

    #[test]
    fn parse_optional_nonzero_rejects_zero() {
        assert!(parse_optional_nonzero::<usize>("0").is_err());
        assert!(parse_optional_nonzero::<u64>("0").is_err());
    }

    #[test]
    fn parse_optional_nonzero_rejects_non_numeric() {
        assert!(parse_optional_nonzero::<usize>("abc").is_err());
    }

    #[test]
    fn parse_optional_nonzero_rejects_negative() {
        // Unsigned FromStr rejects a leading '-' outright, which is how negative input
        // is rejected for the u64/usize fields this helper is actually used for.
        assert!(parse_optional_nonzero::<u64>("-5").is_err());
    }
}
