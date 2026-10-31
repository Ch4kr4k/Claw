//! ```mermaid blocks in replies drawn as diagrams: an image on terminals
//! that speak the kitty graphics protocol (see `term_image`), Unicode
//! box-drawing text elsewhere. The block's source is printed as usual
//! first; the diagram follows it.
//!
//! `CLAW_MERMAID=0` turns this off, `=text` always draws text.

use std::env;

/// Whether `language` (a fence's info string) names a Mermaid block.
pub fn is_mermaid(language: &str) -> bool {
    language
        .split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("mermaid"))
}

/// The diagram for `code`, at most `max_cols` wide; `None` when diagrams are
/// turned off.
pub fn draw(code: &str, max_cols: usize) -> Option<Result<String, String>> {
    let mode = env::var("CLAW_MERMAID").unwrap_or_default();
    if mode == "0" || code.trim().is_empty() {
        return None;
    }
    if mode != "text" && crate::term_image::enabled() {
        return Some(draw_image(code, max_cols));
    }
    Some(draw_text(code, max_cols))
}

fn draw_image(code: &str, max_cols: usize) -> Result<String, String> {
    let svg = mermaid_rs_renderer::render(code).map_err(|error| first_line(&error.to_string()))?;
    crate::term_image::show_bytes(&key(code), svg.as_bytes(), max_cols)
}

fn draw_text(code: &str, max_cols: usize) -> Result<String, String> {
    mermaid_text::render_with_width(code, Some(max_cols.max(20)))
        .map(|text| text.trim_end().to_string())
        .map_err(|error| first_line(&error.to_string()))
}

/// A name for the diagram's image, so a block drawn twice is sent once.
fn key(code: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    code.hash(&mut hasher);
    format!("mermaid-{:016x}.svg", hasher.finish())
}

fn first_line(error: &str) -> String {
    error.lines().next().unwrap_or(error).to_string()
}

#[cfg(test)]
mod tests {
    use super::{draw_text, is_mermaid, key};

    #[test]
    fn mermaid_fences_are_recognised() {
        assert!(is_mermaid("mermaid"));
        assert!(is_mermaid("Mermaid title=x"));
        assert!(!is_mermaid("rust"));
        assert!(!is_mermaid(""));
    }

    #[test]
    fn flowcharts_are_drawn_as_text() {
        let text = draw_text("flowchart LR\n  A[Start] --> B[End]\n", 80).unwrap();
        assert!(text.contains("Start"));
        assert!(text.contains("End"));
    }

    #[test]
    fn diagrams_render_to_svg() {
        let svg = mermaid_rs_renderer::render("flowchart TD; A-->B").unwrap();
        assert!(svg.contains("<svg"));
        assert!(key("x").ends_with(".svg"));
    }
}
