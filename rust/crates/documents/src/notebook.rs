//! Jupyter notebooks (nbformat 4) rendered as cells with their outputs; the
//! raw JSON buries the code under metadata and base64 output.

use serde_json::Value;

const MAX_OUTPUT_CHARS_PER_CELL: usize = 3_000;

/// Notebook text fields are either a string or a list of lines.
fn join_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(lines)) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

fn language(notebook: &Value) -> String {
    let metadata = &notebook["metadata"];
    metadata["kernelspec"]["language"]
        .as_str()
        .or_else(|| metadata["language_info"]["name"].as_str())
        .unwrap_or("python")
        .to_string()
}

/// Strips ANSI escape sequences (tracebacks are colored).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Keeps the start and end of long text.
fn truncate_middle(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let half = max / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(count - half).collect();
    format!(
        "{head}\n… [{} characters omitted] …\n{tail}",
        count - 2 * half
    )
}

/// Renders a notebook, or `None` if `raw` is not an nbformat 4 notebook.
pub(crate) fn render(raw: &str) -> Option<String> {
    let notebook: Value = serde_json::from_str(raw).ok()?;
    let cells = notebook.get("cells")?.as_array()?;
    let lang = language(&notebook);
    let mut out = format!(
        "Jupyter notebook, {} cell(s), language {lang}. NotebookEdit refers to cells by the id shown here.\n",
        cells.len()
    );
    for (index, cell) in cells.iter().enumerate() {
        let kind = cell["cell_type"].as_str().unwrap_or("code");
        let id = cell["id"]
            .as_str()
            .map_or_else(String::new, |id| format!(" id={id}"));
        let exec = cell["execution_count"]
            .as_u64()
            .map_or_else(String::new, |n| format!(" (execution {n})"));
        out.push_str(&format!("\n## Cell {} [{kind}]{id}{exec}\n", index + 1));
        let source = join_text(cell.get("source"));
        let source = source.trim_end();
        if kind == "code" {
            out.push_str(&format!("```{lang}\n{source}\n```\n"));
        } else {
            out.push_str(source);
            out.push('\n');
        }

        let Some(outputs) = cell["outputs"].as_array().filter(|o| !o.is_empty()) else {
            continue;
        };
        let mut output = String::new();
        for item in outputs {
            match item["output_type"].as_str() {
                Some("stream") => output.push_str(&join_text(item.get("text"))),
                Some("error") => {
                    output.push_str(&format!(
                        "{}: {}\n",
                        item["ename"].as_str().unwrap_or("Error"),
                        item["evalue"].as_str().unwrap_or("")
                    ));
                    for line in item["traceback"].as_array().into_iter().flatten() {
                        output.push_str(&strip_ansi(line.as_str().unwrap_or("")));
                        output.push('\n');
                    }
                }
                _ => {
                    // execute_result / display_data
                    let Some(data) = item["data"].as_object() else {
                        continue;
                    };
                    let has_image =
                        data.contains_key("image/png") || data.contains_key("image/jpeg");
                    if has_image {
                        output.push_str("[image output]\n");
                    }
                    if let Some(plain) = data.get("text/plain") {
                        output.push_str(&join_text(Some(plain)));
                        output.push('\n');
                    } else if !has_image {
                        let kinds: Vec<&str> = data.keys().map(String::as_str).collect();
                        output.push_str(&format!("[{} output]\n", kinds.join(", ")));
                    }
                }
            }
        }
        let output = output.trim_end();
        if !output.is_empty() {
            out.push_str("Output:\n");
            out.push_str(&truncate_middle(output, MAX_OUTPUT_CHARS_PER_CELL));
            out.push('\n');
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_cells_outputs_and_errors() {
        let raw = serde_json::json!({
            "metadata": {"kernelspec": {"language": "python"}},
            "nbformat": 4,
            "cells": [
                {"cell_type": "markdown", "id": "m1", "source": ["# Title\n", "text"]},
                {"cell_type": "code", "id": "c1", "execution_count": 2,
                 "source": "print(1)\n1+1",
                 "outputs": [
                    {"output_type": "stream", "text": ["1\n"]},
                    {"output_type": "execute_result", "data": {"text/plain": ["2"]}},
                    {"output_type": "display_data", "data": {"image/png": "AAAA", "text/plain": "<Figure>"}},
                    {"output_type": "error", "ename": "ValueError", "evalue": "bad",
                     "traceback": ["\u{1b}[0;31mValueError\u{1b}[0m: bad"]}
                 ]}
            ]
        })
        .to_string();
        let text = render(&raw).unwrap();
        assert!(text.starts_with("Jupyter notebook, 2 cell(s), language python."));
        assert!(text.contains("## Cell 1 [markdown] id=m1\n# Title\ntext\n"));
        assert!(
            text.contains("## Cell 2 [code] id=c1 (execution 2)\n```python\nprint(1)\n1+1\n```")
        );
        assert!(text
            .contains("Output:\n1\n2\n[image output]\n<Figure>\nValueError: bad\nValueError: bad"));
        assert!(!text.contains("AAAA"));
    }

    #[test]
    fn non_notebooks_are_rejected() {
        assert!(render("not json").is_none());
        assert!(render(r#"{"worksheets": []}"#).is_none());
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let text = "a".repeat(10) + &"b".repeat(10);
        let cut = truncate_middle(&text, 10);
        assert!(cut.starts_with("aaaaa") && cut.ends_with("bbbbb"));
        assert!(cut.contains("10 characters omitted"));
    }
}
