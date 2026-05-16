// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! UTC timestamp helpers shared across the workspace.
//!
//! All functions use [`std::time::SystemTime`] — no external date/time crates required.
//! The O(1) Hinnant algorithm is used internally to convert Unix seconds to a
//! calendar date without iterating over years or months.

use std::time::{SystemTime, UNIX_EPOCH};

/// Converts seconds since the Unix epoch to `(year, month, day, hour, minute, second)` UTC.
///
/// Uses the proleptic-Gregorian Hinnant algorithm — O(1), no heap allocation.
/// Kept `pub(crate)` because the raw 6-tuple is an implementation detail; prefer
/// the higher-level functions in this module for public use.
pub(crate) fn secs_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    const SECS_PER_MIN: u64 = 60;
    const DAYS_PER_400Y: u64 = 146_097;

    let s = (secs % SECS_PER_MIN) as u32;
    let total_mins = secs / SECS_PER_MIN;
    let mi = (total_mins % 60) as u32;
    let total_hours = total_mins / 60;
    let h = (total_hours % 24) as u32;
    let mut days = total_hours / 24;

    // Shift epoch from 1970-01-01 to 0000-03-01 for Gregorian math.
    days += 719_468;
    let era = days / DAYS_PER_400Y;
    let doe = days % DAYS_PER_400Y;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    // All intermediate values are calendar-bounded (day ≤ 31, month ≤ 12, year ≤ ~u32::MAX/2).
    #[allow(clippy::cast_possible_truncation)]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    #[allow(clippy::cast_possible_truncation)]
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    #[allow(clippy::cast_possible_truncation)]
    let y = if mo <= 2 { y + 1 } else { y } as u32;
    (y, mo, d, h, mi, s)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Returns the current UTC time as a `(year, month, day, hour, minute, second)` tuple.
///
/// # Examples
///
/// ```rust
/// let (y, mo, d, h, mi, s) = zeph_common::timestamp::utc_now_datetime();
/// assert!(y >= 2024, "year should be at least 2024");
/// assert!((1..=12).contains(&mo));
/// assert!((1..=31).contains(&d));
/// assert!(h < 24 && mi < 60 && s < 60);
/// ```
#[must_use]
pub fn utc_now_datetime() -> (u32, u32, u32, u32, u32, u32) {
    secs_to_ymdhms(unix_secs())
}

/// Returns the current UTC time in RFC 3339 format: `YYYY-MM-DDTHH:MM:SSZ`.
///
/// # Examples
///
/// ```rust
/// let ts = zeph_common::timestamp::utc_now_rfc3339();
/// assert_eq!(ts.len(), 20, "expected format YYYY-MM-DDTHH:MM:SSZ");
/// assert!(ts.ends_with('Z'));
/// assert_eq!(&ts[10..11], "T");
/// ```
#[must_use]
pub fn utc_now_rfc3339() -> String {
    let (y, mo, d, h, mi, s) = utc_now_datetime();
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Returns the current UTC time in compact format: `YYYYMMDD_HHMMSS`.
///
/// Suitable for filenames and run identifiers where colons and hyphens are undesirable.
///
/// # Examples
///
/// ```rust
/// let ts = zeph_common::timestamp::utc_now_compact();
/// assert_eq!(ts.len(), 15, "expected format YYYYMMDD_HHMMSS");
/// assert_eq!(&ts[8..9], "_");
/// ```
#[must_use]
pub fn utc_now_compact() -> String {
    let (y, mo, d, h, mi, s) = utc_now_datetime();
    format!("{y:04}{mo:02}{d:02}_{h:02}{mi:02}{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_format_is_correct() {
        let ts = utc_now_rfc3339();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[10..11], "T");
        // Basic digit checks
        assert!(ts[..4].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn compact_format_is_correct() {
        let ts = utc_now_compact();
        assert_eq!(ts.len(), 15);
        assert_eq!(&ts[8..9], "_");
    }

    #[test]
    fn secs_to_ymdhms_known_epoch() {
        // Unix epoch = 1970-01-01 00:00:00
        let (y, mo, d, h, mi, s) = secs_to_ymdhms(0);
        assert_eq!((y, mo, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn secs_to_ymdhms_known_date() {
        // 2024-03-01 00:00:00 UTC = 1709251200
        let (y, mo, d, h, mi, s) = secs_to_ymdhms(1_709_251_200);
        assert_eq!((y, mo, d, h, mi, s), (2024, 3, 1, 0, 0, 0));
    }

    #[test]
    fn datetime_fields_are_in_range() {
        let (y, mo, d, h, mi, s) = utc_now_datetime();
        assert!(y >= 2024);
        assert!((1..=12).contains(&mo));
        assert!((1..=31).contains(&d));
        assert!(h < 24 && mi < 60 && s < 60);
    }
}
