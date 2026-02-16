//! Recipe serialization for exploration replay.
//!
//! Recipes describe a sequence of fork points as `(rng_call_count, seed)` pairs.
//! They can be formatted as human-readable timeline strings for debugging and
//! parsed back for deterministic replay via RNG breakpoints.

use std::fmt;

/// Error parsing a timeline string.
#[derive(Debug)]
pub struct ParseTimelineError {
    /// Description of the parse error.
    pub message: String,
}

impl fmt::Display for ParseTimelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse timeline error: {}", self.message)
    }
}

impl std::error::Error for ParseTimelineError {}

/// Format a recipe as a human-readable timeline string.
///
/// Each segment is formatted as `count@seed`, joined by ` -> `.
///
/// # Example
///
/// ```
/// use moonpool_explorer::format_timeline;
///
/// let recipe = vec![(42, 12345), (17, 67890)];
/// assert_eq!(format_timeline(&recipe), "42@12345 -> 17@67890");
/// ```
pub fn format_timeline(recipe: &[(u64, u64)]) -> String {
    recipe
        .iter()
        .map(|(count, seed)| format!("{count}@{seed}"))
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// Parse a timeline string back into a recipe.
///
/// Accepts the format produced by [`format_timeline`]: segments of `count@seed`
/// joined by ` -> `.
///
/// # Errors
///
/// Returns an error if the string is malformed (missing `@`, non-numeric values).
///
/// # Example
///
/// ```
/// use moonpool_explorer::parse_timeline;
///
/// let recipe = parse_timeline("42@12345 -> 17@67890").unwrap();
/// assert_eq!(recipe, vec![(42, 12345), (17, 67890)]);
/// ```
pub fn parse_timeline(s: &str) -> Result<Vec<(u64, u64)>, ParseTimelineError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    trimmed
        .split(" -> ")
        .map(|segment| {
            let segment = segment.trim();
            let at_pos = segment.find('@').ok_or_else(|| ParseTimelineError {
                message: format!("missing '@' in segment: {segment}"),
            })?;

            let count_str = &segment[..at_pos];
            let seed_str = &segment[at_pos + 1..];

            let count = count_str.parse::<u64>().map_err(|e| ParseTimelineError {
                message: format!("invalid count '{count_str}': {e}"),
            })?;

            let seed = seed_str.parse::<u64>().map_err(|e| ParseTimelineError {
                message: format!("invalid seed '{seed_str}': {e}"),
            })?;

            Ok((count, seed))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_empty() {
        assert_eq!(format_timeline(&[]), "");
    }

    #[test]
    fn test_format_single() {
        assert_eq!(format_timeline(&[(42, 12345)]), "42@12345");
    }

    #[test]
    fn test_format_multiple() {
        let recipe = vec![(42, 12345), (17, 67890), (100, 999)];
        assert_eq!(format_timeline(&recipe), "42@12345 -> 17@67890 -> 100@999");
    }

    #[test]
    fn test_roundtrip() {
        let original = vec![(42, 12345), (17, 67890)];
        let formatted = format_timeline(&original);
        let parsed = parse_timeline(&formatted).expect("parse failed");
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_parse_empty() {
        let result = parse_timeline("").expect("parse failed");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_whitespace() {
        let result = parse_timeline("  ").expect("parse failed");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_error_missing_at() {
        let result = parse_timeline("42-12345");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_error_non_numeric() {
        let result = parse_timeline("abc@12345");
        assert!(result.is_err());
    }
}
