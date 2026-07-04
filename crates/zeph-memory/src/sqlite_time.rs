// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared `SQLite` `datetime('now')` TEXT-timestamp parsing.
//!
//! Used by [`crate::graph::types`], [`crate::graph::activation`], and [`crate::eviction`] to
//! convert `SQLite` TEXT timestamps into Unix seconds without pulling in `chrono`.

/// Parse a `SQLite` `datetime('now')` string to Unix seconds.
///
/// Accepts:
/// - `"YYYY-MM-DD HH:MM:SS"` (19 chars, standard `SQLite` format)
/// - `"YYYY-MM-DD HH:MM:SS.fff"` (fractional seconds — truncated, not rounded)
/// - `"YYYY-MM-DD HH:MM:SSZ"` or `"YYYY-MM-DD HH:MM:SS+HH:MM"` (timezone suffix — treated as UTC)
///
/// Leading/trailing whitespace is trimmed before parsing.
///
/// Returns `None` if the string cannot be parsed.
#[must_use]
pub(crate) fn parse_sqlite_datetime_to_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    // Minimum: "YYYY-MM-DD HH:MM:SS" (19 chars)
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: i64 = s[5..7].parse().ok()?;
    let day: i64 = s[8..10].parse().ok()?;
    let hour: i64 = s[11..13].parse().ok()?;
    let min: i64 = s[14..16].parse().ok()?;
    // Only parse the base seconds; ignore fractional seconds and timezone suffix.
    let sec: i64 = s[17..19].parse().ok()?;

    // Days since Unix epoch (1970-01-01) via civil calendar algorithm.
    // Reference: https://howardhinnant.github.io/date_algorithms.html#days_from_civil
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86_400 + hour * 3_600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epoch() {
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-01 00:00:00"),
            Some(0)
        );
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-02 00:00:00"),
            Some(86_400)
        );
    }

    #[test]
    fn known_value() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        assert_eq!(
            parse_sqlite_datetime_to_unix("2024-01-01 00:00:00"),
            Some(1_704_067_200)
        );
    }

    #[test]
    fn invalid_returns_none() {
        assert_eq!(parse_sqlite_datetime_to_unix("not-a-date"), None);
        assert_eq!(parse_sqlite_datetime_to_unix(""), None);
    }

    #[test]
    fn pre_1970_produces_negative_value_not_none() {
        // A syntactically valid but pre-epoch date parses successfully to a negative
        // Unix timestamp — it is not treated as an error. Callers that need a `u64` (e.g.
        // `eviction::parse_sqlite_timestamp_secs`) must convert and handle the negative
        // case explicitly; they must not assume this function only ever returns `None` for
        // "bad" input.
        assert_eq!(
            parse_sqlite_datetime_to_unix("1969-12-31 23:59:59"),
            Some(-1)
        );
        assert!(parse_sqlite_datetime_to_unix("1900-01-01 00:00:00").unwrap() < 0);
    }

    #[test]
    fn fractional_seconds_truncated() {
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-01 00:00:00.999"),
            Some(0)
        );
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-02 00:00:00.123"),
            Some(86_400)
        );
    }

    #[test]
    fn timezone_suffix_treated_as_utc() {
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-01 00:00:00Z"),
            Some(0)
        );
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-01 00:00:00+05:30"),
            Some(0)
        );
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(
            parse_sqlite_datetime_to_unix("  1970-01-01 00:00:00  "),
            Some(0)
        );
    }
}
