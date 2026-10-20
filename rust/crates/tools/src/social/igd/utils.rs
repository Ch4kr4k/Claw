//! Small standalone helpers with no network/filesystem dependency of their
//! own — kept separate so they're trivially unit-testable.

/// Pulls the shortcode out of an Instagram post/reel URL: the path segment
/// after `p`, `reel`, `reels` or `tv`, e.g.
/// `https://www.instagram.com/reel/DdeRQz8q2HJ/` and the profile grid's
/// `https://www.instagram.com/<user>/p/DdeRQz8q2HJ/` both give
/// `DdeRQz8q2HJ`. (herald took the fourth `/` piece, which for the grid's
/// form is `p`; Instagram then answered "execution error".) `None` when
/// there is no such segment.
pub fn extract_shortcode(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let kind = segments
        .iter()
        .position(|segment| matches!(*segment, "p" | "reel" | "reels" | "tv"))?;
    let candidate = segments.get(kind + 1)?;
    candidate
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        .then(|| candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_shortcode_from_a_reel_url() {
        assert_eq!(
            extract_shortcode("https://www.instagram.com/reel/DdeRQz8q2HJ/"),
            Some("DdeRQz8q2HJ".to_string())
        );
    }

    #[test]
    fn extracts_shortcode_from_a_post_url_without_trailing_slash() {
        assert_eq!(
            extract_shortcode("https://www.instagram.com/p/DdeRQz8q2HJ"),
            Some("DdeRQz8q2HJ".to_string())
        );
    }

    #[test]
    fn extracts_shortcode_from_a_profile_grid_url() {
        assert_eq!(
            extract_shortcode("https://www.instagram.com/i._.hammock/p/DZnLlR-CsHS/"),
            Some("DZnLlR-CsHS".to_string())
        );
        assert_eq!(
            extract_shortcode("https://www.instagram.com/i._.hammock/reel/DZIExggumQO/?igsh=x"),
            Some("DZIExggumQO".to_string())
        );
    }

    #[test]
    fn returns_none_for_a_bare_domain() {
        assert_eq!(extract_shortcode("https://www.instagram.com/"), None);
    }

    #[test]
    fn returns_none_for_a_non_instagram_shaped_url() {
        assert_eq!(extract_shortcode("not a url"), None);
    }
}
