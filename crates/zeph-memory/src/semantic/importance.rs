//! Write-time importance scoring for memory retrieval.
//!
//! Combines three heuristic signals:
//! - Marker detection (50%): explicit salience markers like "remember:", "important:"
//! - Content density (30%): message length as proxy for information density
//! - Role adjustment (20%): user messages weighted higher than assistant/system

/// Compute importance score for a message at write time.
///
/// Returns a score in [0.0, 1.0] based on content and role.
/// Default 0.5 represents neutral importance.
pub fn compute_importance(content: &str, role: &str) -> f64 {
    let marker = marker_score(content);
    let density = density_score(content);
    let role_adj = role_adjustment(role);

    // Weighted combination: 50% marker + 30% density + 20% role
    let raw = 0.50 * marker + 0.30 * density + 0.20 * role_adj;
    raw.clamp(0.0, 1.0)
}

/// Return the byte offset of the `n`-th character boundary, or `s.len()` if shorter.
///
/// This avoids `&s[..n]` panicking when `n` falls mid-UTF-8 sequence.
fn char_byte_boundary(s: &str, n: usize) -> usize {
    s.char_indices()
        .nth(n)
        .map_or(s.len(), |(byte_offset, _)| byte_offset)
}

/// Detect explicit salience markers in content.
///
/// Returns 1.0 if any marker found at start or line-start, 0.0 otherwise.
/// Case-insensitive. Checks first 200 chars for start, first 500 for line markers.
fn marker_score(content: &str) -> f64 {
    let markers = [
        "remember:",
        "important:",
        "always:",
        "never forget:",
        "key point:",
        "critical:",
    ];

    let content_lower = content.to_lowercase();
    // Find safe byte boundaries at Unicode char boundaries to avoid mid-char slice panics.
    let search_end = char_byte_boundary(&content_lower, 500);
    let searchable = &content_lower[..search_end];

    // Check start of content (first 200 chars)
    let start_end = char_byte_boundary(searchable, 200);
    let start_section = &searchable[..start_end];
    for marker in &markers {
        if start_section.starts_with(marker) || start_section.trim_start().starts_with(marker) {
            return 1.0;
        }
    }

    // Check for markers at line starts in full 500-char window
    for marker in &markers {
        for line in searchable.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with(marker) {
                return 1.0;
            }
        }
    }

    0.0
}

/// Content length as proxy for information density.
///
/// Returns score in [0.0, 1.0] using sigmoid-like curve.
/// 0 chars → 0.0, 300 chars → ~0.5, 3000+ chars → ~0.91
fn density_score(content: &str) -> f64 {
    #[expect(clippy::cast_precision_loss)]
    let chars = content.chars().count() as f64;
    let x = chars / 300.0;
    (x / (1.0 + x)).min(1.0)
}

/// Role-based importance adjustment.
///
/// User messages weighted higher than assistant/system.
/// Unknown roles default to neutral (0.5).
fn role_adjustment(role: &str) -> f64 {
    match role.to_lowercase().as_str() {
        "user" => 0.7,
        "assistant" => 0.4,
        _ => 0.5, // system and unknown → neutral
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_importance_empty_content() {
        // Empty content: marker=0, density=0, role=0.7 → raw = 0.14
        let score = compute_importance("", "user");
        assert!(
            (0.10..=0.20).contains(&score),
            "empty content user score should be around role_adj contribution, got {score}"
        );
    }

    #[test]
    fn test_compute_importance_empty_content_system_role() {
        // system role adj = 0.5, density=0, marker=0 → raw = 0.10
        let score = compute_importance("", "system");
        assert!(
            (0.05..=0.15).contains(&score),
            "empty content system score expected ~0.10, got {score}"
        );
    }

    #[test]
    fn test_compute_importance_with_marker() {
        let content = "remember: the API key rotates weekly";
        let score = compute_importance(content, "user");
        // marker(1.0)*0.5 + density(~0.11)*0.3 + role(0.7)*0.2 ≈ 0.67
        assert!(
            score > 0.60,
            "marker should boost score above 0.60, got {score}"
        );
    }

    #[test]
    fn test_compute_importance_no_marker_short() {
        let content = "hello";
        let score = compute_importance(content, "user");
        // density(~0.016)*0.3 + role(0.7)*0.2 ≈ 0.145
        assert!(
            (0.10..=0.20).contains(&score),
            "short, unmarked user msg expected in 0.10-0.20, got {score}"
        );
    }

    #[test]
    fn test_compute_importance_no_marker_long() {
        let content = "a".repeat(500);
        let score = compute_importance(&content, "user");
        // density(0.625)*0.3 + role(0.7)*0.2 ≈ 0.328
        assert!(
            (0.28..=0.40).contains(&score),
            "long, unmarked user msg expected in 0.28-0.40, got {score}"
        );
    }

    #[test]
    fn test_marker_score_case_insensitive() {
        assert!((marker_score("Remember: test") - 1.0).abs() < f64::EPSILON);
        assert!((marker_score("REMEMBER: test") - 1.0).abs() < f64::EPSILON);
        assert!((marker_score("rEmEmBeR: test") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_marker_score_no_marker() {
        assert!(marker_score("just a regular message").abs() < f64::EPSILON);
    }

    #[test]
    fn test_density_score_empty() {
        assert!(density_score("").abs() < f64::EPSILON);
    }

    #[test]
    fn test_density_score_100_chars() {
        let score = density_score(&"a".repeat(100));
        // x=1/3, score=0.25 exactly
        assert!((0.24..=0.28).contains(&score));
    }

    #[test]
    fn test_density_score_3000_chars() {
        let score = density_score(&"a".repeat(3000));
        assert!(score > 0.9);
    }

    #[test]
    fn test_marker_score_multibyte_no_panic() {
        // 'é' is 2 bytes; a 250-char string of 'é' is 500 bytes.
        // Before the fix this would panic when slicing [..500] on the lowercased string.
        let content = "é".repeat(250);
        let score = marker_score(&content);
        assert!(
            score.abs() < f64::EPSILON,
            "no marker expected in multibyte string"
        );
    }

    #[test]
    fn test_marker_score_multibyte_with_marker() {
        // Marker followed by multibyte content — must not panic and must detect marker.
        let tail = "é".repeat(300);
        let content = format!("remember: {tail}");
        assert!((marker_score(&content) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_char_byte_boundary_within() {
        // "abc" — 3 chars, 3 bytes; boundary at 2 should be byte offset 2.
        assert_eq!(char_byte_boundary("abc", 2), 2);
    }

    #[test]
    fn test_char_byte_boundary_beyond() {
        // n > string length should return s.len()
        assert_eq!(char_byte_boundary("hi", 100), 2);
    }

    #[test]
    fn test_char_byte_boundary_multibyte() {
        // "éé" = 4 bytes; boundary at char 1 = byte offset 2.
        assert_eq!(char_byte_boundary("éé", 1), 2);
    }

    #[test]
    fn test_role_adjustment() {
        assert!((role_adjustment("user") - 0.7).abs() < f64::EPSILON);
        assert!((role_adjustment("assistant") - 0.4).abs() < f64::EPSILON);
        assert!((role_adjustment("system") - 0.5).abs() < f64::EPSILON);
        assert!((role_adjustment("unknown") - 0.5).abs() < f64::EPSILON);
        assert!((role_adjustment("User") - 0.7).abs() < f64::EPSILON); // case-insensitive
    }
}
