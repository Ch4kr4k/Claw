//! Codex's patch format, for `apply_patch` (after deepseek-harness's
//! `fs-patch`): models trained on Codex (gpt-oss among them) write their
//! edits this way, several files in one call. This module only reads a
//! patch and applies its hunks to text; `lib.rs` checks permissions and
//! the read-before-edit rule for every file, then writes them, all or none.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path/new.rs
//! +line
//! *** Update File: path/old.rs
//! *** Move to: path/renamed.rs      (optional)
//! @@ fn context_line                 (optional anchor)
//!  unchanged
//! -removed
//! +added
//! *** Delete File: path/gone.rs
//! *** End Patch
//! ```

use runtime::PermissionMode;
use serde_json::json;

use super::ToolSpec;

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "apply_patch",
        description: "Apply a patch in Codex's format: add, update (with optional rename) and delete files in one call, all or nothing. Lines: `*** Begin Patch`, then for each file `*** Add File: <path>` (every line prefixed `+`), `*** Delete File: <path>`, or `*** Update File: <path>` (optionally `*** Move to: <path>`) followed by hunks: an optional `@@ <a line just before the change>`, then context lines prefixed with a space, removed lines with `-`, added lines with `+`; finally `*** End Patch`. Files you update or delete must have been read first with read_file. Prefer edit_file for a single small change.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "patch": { "type": "string", "description": "The whole patch, from *** Begin Patch to *** End Patch" }
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::WorkspaceWrite,
    }]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatchOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Hunk {
    /// The text after `@@`, a line to find before the hunk.
    anchor: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    /// The hunk ends at the end of the file.
    at_end: bool,
}

/// The operations of a patch, in order.
pub(crate) fn parse_patch(text: &str) -> Result<Vec<PatchOp>, String> {
    let text = text.trim();
    let mut lines = text.lines().peekable();
    if lines.next().map(str::trim) != Some("*** Begin Patch") {
        return Err("a patch starts with `*** Begin Patch`".to_string());
    }
    let mut ops = Vec::new();
    let mut ended = false;
    while let Some(line) = lines.next() {
        if line.trim() == "*** End Patch" {
            ended = true;
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let mut content = String::new();
            while let Some(next) = lines.peek() {
                if next.starts_with("*** ") {
                    break;
                }
                let next = lines.next().unwrap_or_default();
                let added = next.strip_prefix('+').ok_or_else(|| {
                    format!("in Add File {path}: every line must start with `+`: {next:?}")
                })?;
                content.push_str(added);
                content.push('\n');
            }
            ops.push(PatchOp::Add {
                path: path.trim().to_string(),
                content,
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(PatchOp::Delete {
                path: path.trim().to_string(),
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            let mut move_to = None;
            if let Some(next) = lines
                .peek()
                .and_then(|next| next.strip_prefix("*** Move to: "))
            {
                move_to = Some(next.trim().to_string());
                lines.next();
            }
            let mut hunks = Vec::new();
            let mut hunk = Hunk::default();
            while let Some(next) = lines.peek() {
                if next.starts_with("*** ") && next.trim() != "*** End of File" {
                    break;
                }
                let next = lines.next().unwrap_or_default();
                if next.trim() == "*** End of File" {
                    hunk.at_end = true;
                } else if let Some(anchor) = next.strip_prefix("@@") {
                    if !hunk.old.is_empty() || !hunk.new.is_empty() {
                        hunks.push(std::mem::take(&mut hunk));
                    }
                    let anchor = anchor.trim().trim_end_matches("@@").trim();
                    hunk.anchor = (!anchor.is_empty()).then(|| anchor.to_string());
                } else if let Some(rest) = next.strip_prefix('-') {
                    hunk.old.push(rest.to_string());
                } else if let Some(rest) = next.strip_prefix('+') {
                    hunk.new.push(rest.to_string());
                } else {
                    // Context: ' ' prefix, or an empty line models write
                    // without the space.
                    let rest = next.strip_prefix(' ').unwrap_or(next);
                    hunk.old.push(rest.to_string());
                    hunk.new.push(rest.to_string());
                }
            }
            if !hunk.old.is_empty() || !hunk.new.is_empty() {
                hunks.push(hunk);
            }
            if hunks.is_empty() && move_to.is_none() {
                return Err(format!("Update File {path} has no changes"));
            }
            ops.push(PatchOp::Update {
                path: path.trim().to_string(),
                move_to,
                hunks,
            });
        } else if !line.trim().is_empty() {
            return Err(format!("unexpected line in the patch: {line:?}"));
        }
    }
    if !ended {
        return Err("the patch has no `*** End Patch` line (was it cut off?)".to_string());
    }
    if ops.is_empty() {
        return Err("the patch changes nothing".to_string());
    }
    Ok(ops)
}

/// `content` with `hunks` applied in order; an error names the hunk that
/// does not match.
pub(crate) fn apply_hunks(content: &str, hunks: &[Hunk]) -> Result<String, String> {
    let ends_with_newline = content.ends_with('\n') || content.is_empty();
    let mut lines = content.lines().map(str::to_string).collect::<Vec<_>>();
    let mut cursor = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        if let Some(anchor) = &hunk.anchor {
            if let Some(found) = find_line(&lines, cursor, anchor) {
                cursor = found;
            }
        }
        let at = if hunk.old.is_empty() {
            if hunk.at_end || hunk.anchor.is_none() {
                lines.len()
            } else {
                (cursor + 1).min(lines.len())
            }
        } else if hunk.at_end {
            let start = lines.len().saturating_sub(hunk.old.len());
            find_block(&lines, start, &hunk.old)
                .filter(|found| *found == start)
                .or_else(|| find_block(&lines, cursor, &hunk.old))
                .ok_or_else(|| mismatch(index, hunk))?
        } else {
            find_block(&lines, cursor, &hunk.old)
                .or_else(|| find_block(&lines, 0, &hunk.old))
                .ok_or_else(|| mismatch(index, hunk))?
        };
        lines.splice(at..at + hunk.old.len(), hunk.new.iter().cloned());
        cursor = at + hunk.new.len();
    }
    let mut out = lines.join("\n");
    if ends_with_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

fn mismatch(index: usize, hunk: &Hunk) -> String {
    format!(
        "hunk {} does not match the file: these lines were not found:\n{}\nRead the file again and copy the context and `-` lines exactly.",
        index + 1,
        hunk.old.join("\n")
    )
}

fn find_line(lines: &[String], from: usize, needle: &str) -> Option<usize> {
    let needle = needle.trim();
    (from..lines.len()).find(|at| lines[*at].contains(needle))
}

/// Where `block` starts at or after `from`: exact lines first, then
/// ignoring trailing, then all surrounding whitespace.
fn find_block(lines: &[String], from: usize, block: &[String]) -> Option<usize> {
    if block.len() > lines.len() {
        return None;
    }
    let normalizers: [fn(&str) -> &str; 3] = [|line| line, str::trim_end, str::trim];
    normalizers.iter().find_map(|normalize| {
        (from..=lines.len() - block.len()).find(|start| {
            block
                .iter()
                .zip(&lines[*start..])
                .all(|(want, have)| normalize(want) == normalize(have))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{apply_hunks, parse_patch, PatchOp};

    const PATCH: &str = "*** Begin Patch
*** Add File: notes.txt
+hello
+world
*** Update File: src/lib.rs
*** Move to: src/core.rs
@@ fn add
 fn add(a: i32, b: i32) -> i32 {
-    a - b
+    a + b
 }
*** Delete File: old.rs
*** End Patch";

    #[test]
    fn a_codex_patch_is_parsed_into_operations() {
        let ops = parse_patch(PATCH).expect("parses");
        assert_eq!(
            ops[0],
            PatchOp::Add {
                path: "notes.txt".into(),
                content: "hello\nworld\n".into()
            }
        );
        assert!(
            matches!(&ops[1], PatchOp::Update { path, move_to: Some(to), hunks } if path == "src/lib.rs" && to == "src/core.rs" && hunks.len() == 1)
        );
        assert_eq!(
            ops[2],
            PatchOp::Delete {
                path: "old.rs".into()
            }
        );
        assert!(
            parse_patch("*** Begin Patch\n*** Delete File: x").is_err(),
            "cut off"
        );
    }

    #[test]
    fn hunks_apply_in_order_and_tolerate_whitespace() {
        let file = "fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n\nfn add(a: i32, b: i32) -> i32 {\n    a - b   \n}\n";
        let PatchOp::Update { hunks, .. } = parse_patch(PATCH).unwrap().remove(1) else {
            panic!("update");
        };
        let out = apply_hunks(file, &hunks).expect("applies");
        assert_eq!(
            out,
            "fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            "the anchor picks the second `a - b`"
        );
        let bad = parse_patch("*** Begin Patch\n*** Update File: x\n-missing line\n*** End Patch")
            .unwrap();
        let PatchOp::Update { hunks, .. } = &bad[0] else {
            panic!()
        };
        assert!(apply_hunks(file, hunks)
            .unwrap_err()
            .contains("hunk 1 does not match"));
    }
}
