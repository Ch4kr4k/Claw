//! Reasoning written into the reply text as `<think>…</think>` (models
//! served without a reasoning parser, or with a template that does not
//! split it out) is moved to the thinking block instead of being shown and
//! kept as the answer.
//!
//! Streamed text arrives in pieces, so a tag can be split across two of
//! them (`<thi` + `nk>`): the scrubber holds back a piece's tail that could
//! still become a tag until the next one shows what it is. An opening tag
//! only counts at the start of a line, so prose that mentions `<think>`
//! is left alone; a closing tag is looked for only inside a block.

const TAGS: [(&str, &str); 2] = [("<think>", "</think>"), ("<thinking>", "</thinking>")];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Thinking(String),
    Text(String),
}

#[derive(Debug, Default)]
pub struct ThinkScrubber {
    /// The closing tag of the block being read, if inside one.
    open: Option<&'static str>,
    /// Text held back because it may be the start of a tag.
    held: String,
    /// Whether only whitespace has been seen since the last newline (or
    /// since the start).
    line_start: bool,
    started: bool,
}

impl ThinkScrubber {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Splits the next streamed piece of text into thinking and text.
    pub fn feed(&mut self, delta: &str) -> Vec<Piece> {
        if !self.started {
            self.started = true;
            self.line_start = true;
        }
        let mut buf = std::mem::take(&mut self.held);
        buf.push_str(delta);
        let mut out = Vec::new();
        loop {
            if let Some(close) = self.open {
                if let Some(at) = buf.find(close) {
                    push(&mut out, Piece::Thinking(buf[..at].to_string()));
                    buf = buf[at + close.len()..]
                        .trim_start_matches(['\n', '\r'])
                        .to_string();
                    self.open = None;
                    self.line_start = true;
                    continue;
                }
                let keep = partial_suffix(&buf, &[close]);
                push(
                    &mut out,
                    Piece::Thinking(buf[..buf.len() - keep].to_string()),
                );
                self.held = buf[buf.len() - keep..].to_string();
                return out;
            }
            if let Some((at, open, close)) = self.opening_tag(&buf) {
                let before = &buf[..at];
                // The whitespace on the tag's line before it is dropped too.
                let before = before.trim_end_matches([' ', '\t']);
                push(&mut out, Piece::Text(before.to_string()));
                buf = buf[at + open.len()..].to_string();
                self.open = Some(close);
                continue;
            }
            let opens = TAGS.map(|(open, _)| open);
            let keep = partial_suffix(&buf, &opens);
            let keep = if self.could_open_at(&buf, buf.len() - keep) {
                keep
            } else {
                0
            };
            let text = &buf[..buf.len() - keep];
            self.note_text(text);
            push(&mut out, Piece::Text(text.to_string()));
            self.held = buf[buf.len() - keep..].to_string();
            return out;
        }
    }

    /// What is still held back, at the end of the stream. Inside an
    /// unclosed block it is thinking.
    pub fn flush(&mut self) -> Vec<Piece> {
        let held = std::mem::take(&mut self.held);
        let mut out = Vec::new();
        if self.open.is_some() {
            push(&mut out, Piece::Thinking(held));
        } else {
            push(&mut out, Piece::Text(held));
        }
        out
    }

    /// The first opening tag in `buf` that starts a line.
    fn opening_tag(&self, buf: &str) -> Option<(usize, &'static str, &'static str)> {
        TAGS.iter()
            .filter_map(|(open, close)| {
                buf.match_indices(open)
                    .map(|(at, _)| at)
                    .find(|at| self.could_open_at(buf, *at))
                    .map(|at| (at, *open, *close))
            })
            .min_by_key(|(at, _, _)| *at)
    }

    /// Whether a tag starting at byte `at` of `buf` would start a line.
    fn could_open_at(&self, buf: &str, at: usize) -> bool {
        let before = &buf[..at];
        match before.rfind('\n') {
            Some(newline) => before[newline + 1..].trim().is_empty(),
            None => self.line_start && before.trim().is_empty(),
        }
    }

    fn note_text(&mut self, text: &str) {
        if let Some(newline) = text.rfind('\n') {
            self.line_start = text[newline + 1..].trim().is_empty();
        } else if !text.trim().is_empty() {
            self.line_start = false;
        }
    }
}

/// The length of the longest tail of `buf` that is the start of one of
/// `tags` (but not a whole tag).
fn partial_suffix(buf: &str, tags: &[&str]) -> usize {
    tags.iter()
        .flat_map(|tag| (1..tag.len()).map(move |len| &tag[..len]))
        .filter(|prefix| buf.ends_with(prefix))
        .map(str::len)
        .max()
        .unwrap_or(0)
}

fn push(out: &mut Vec<Piece>, piece: Piece) {
    let empty = match &piece {
        Piece::Thinking(text) | Piece::Text(text) => text.is_empty(),
    };
    if !empty {
        out.push(piece);
    }
}

/// `text` with its `<think>` blocks taken out: (thinking, the rest).
#[must_use]
pub fn split_think_blocks(text: &str) -> (String, String) {
    let mut scrubber = ThinkScrubber::new();
    let mut pieces = scrubber.feed(text);
    pieces.extend(scrubber.flush());
    let (mut thinking, mut rest) = (String::new(), String::new());
    for piece in pieces {
        match piece {
            Piece::Thinking(part) => thinking.push_str(&part),
            Piece::Text(part) => rest.push_str(&part),
        }
    }
    (thinking, rest)
}

#[cfg(test)]
mod tests {
    use super::{split_think_blocks, Piece, ThinkScrubber};

    fn run(deltas: &[&str]) -> (String, String) {
        let mut scrubber = ThinkScrubber::new();
        let mut pieces = Vec::new();
        for delta in deltas {
            pieces.extend(scrubber.feed(delta));
        }
        pieces.extend(scrubber.flush());
        let (mut thinking, mut text) = (String::new(), String::new());
        for piece in pieces {
            match piece {
                Piece::Thinking(part) => thinking.push_str(&part),
                Piece::Text(part) => text.push_str(&part),
            }
        }
        (thinking, text)
    }

    #[test]
    fn a_think_block_split_across_pieces_is_moved_to_thinking() {
        let (thinking, text) = run(&[
            "<thi",
            "nk>let me see",
            " more</th",
            "ink>\nThe answer is 4.",
        ]);
        assert_eq!(thinking, "let me see more");
        assert_eq!(text, "The answer is 4.");
    }

    #[test]
    fn prose_that_mentions_the_tag_is_left_alone() {
        let (thinking, text) = run(&["Models write a ", "<think> tag first."]);
        assert_eq!(thinking, "");
        assert_eq!(text, "Models write a <think> tag first.");
    }

    #[test]
    fn a_block_on_a_later_line_counts_and_an_unclosed_one_is_thinking() {
        let (thinking, text) =
            split_think_blocks("Intro\n<thinking>hmm</thinking>\nDone\n<think>still going");
        assert_eq!(thinking, "hmmstill going");
        assert_eq!(text, "Intro\nDone\n");
    }

    #[test]
    fn text_ending_in_a_lone_angle_bracket_is_not_lost() {
        let (thinking, text) = run(&["a <", "b"]);
        assert_eq!(thinking, "");
        assert_eq!(text, "a <b");
        let (_, text) = run(&["x\n<"]);
        assert_eq!(text, "x\n<");
    }
}
