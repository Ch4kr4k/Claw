//! Markdown → `.docx` with real Word styles (Title, Heading 1-6, List
//! Paragraph, Quote, Code), lists, tables, inline formatting and links.

use std::fmt::Write as _;
use std::path::Path;

use crate::xml::{escape, write_package_file};
use crate::Result;

const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const REL_HYPERLINK: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";
/// Text width of an A4 page with the margins below, in twentieths of a point.
const TABLE_WIDTH_TWIPS: usize = 9_638;

/// Writes `markdown` to `path` as a Word document. `title` adds a Title
/// paragraph first; `letter` selects US Letter instead of A4.
pub fn write_docx(path: &Path, markdown: &str, title: Option<&str>, letter: bool) -> Result<()> {
    let mut builder = Builder::default();
    let body = builder.body(markdown, title, letter);
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W_NS}" xmlns:r="{R_NS}"><w:body>{body}</w:body></w:document>"#
    );
    let mut rels = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>"#,
    );
    for (index, url) in builder.links.iter().enumerate() {
        let _ = write!(
            rels,
            r#"<Relationship Id="rIdL{index}" Type="{REL_HYPERLINK}" Target="{}" TargetMode="External"/>"#,
            escape(url)
        );
    }
    rels.push_str("</Relationships>");

    let parts = vec![
        (
            "[Content_Types].xml".to_string(),
            CONTENT_TYPES.as_bytes().to_vec(),
        ),
        ("_rels/.rels".to_string(), PACKAGE_RELS.as_bytes().to_vec()),
        ("word/document.xml".to_string(), document.into_bytes()),
        (
            "word/_rels/document.xml.rels".to_string(),
            rels.into_bytes(),
        ),
        (
            "word/styles.xml".to_string(),
            STYLES_XML.as_bytes().to_vec(),
        ),
        (
            "word/numbering.xml".to_string(),
            numbering_xml(&builder.numbered_lists).into_bytes(),
        ),
    ];
    write_package_file(path, &parts)
}

#[derive(Default)]
struct Builder {
    /// Hyperlink targets; index `i` is relationship `rIdL{i}`.
    links: Vec<String>,
    /// numIds of numbered lists (each restarts at 1).
    numbered_lists: Vec<usize>,
}

impl Builder {
    fn body(&mut self, markdown: &str, title: Option<&str>, letter: bool) -> String {
        let mut body = String::new();
        if let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) {
            body.push_str(&self.paragraph(title, Some("Title"), None));
        }
        let lines: Vec<&str> = markdown.lines().collect();
        let mut pending: Vec<&str> = Vec::new();
        let mut numbered_list = 0usize;
        let mut i = 0;

        macro_rules! flush {
            () => {
                if !pending.is_empty() {
                    body.push_str(&self.paragraph(&pending.join(" "), None, None));
                    pending.clear();
                }
            };
        }

        while i < lines.len() {
            let line = lines[i];
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                flush!();
                let mut code = Vec::new();
                i += 1;
                while i < lines.len() && !lines[i].trim().starts_with("```") {
                    code.push(lines[i]);
                    i += 1;
                }
                if code.is_empty() {
                    code.push("");
                }
                for line in code {
                    body.push_str(&code_paragraph(line));
                }
                i += 1;
                continue;
            }
            if trimmed.is_empty() {
                flush!();
                numbered_list = 0;
                i += 1;
                continue;
            }
            if let Some((level, text)) = heading(trimmed) {
                flush!();
                body.push_str(&self.paragraph(text, Some(&format!("Heading{level}")), None));
                i += 1;
                continue;
            }
            if is_rule(trimmed) {
                flush!();
                body.push_str(r#"<w:p><w:pPr><w:pBdr><w:bottom w:val="single" w:sz="6" w:space="1" w:color="A6A6A6"/></w:pBdr></w:pPr></w:p>"#);
                i += 1;
                continue;
            }
            if trimmed.starts_with('|') {
                flush!();
                let mut rows = Vec::new();
                while i < lines.len() && lines[i].trim().starts_with('|') {
                    rows.push(lines[i].trim());
                    i += 1;
                }
                body.push_str(&self.table(&rows));
                body.push_str("<w:p/>");
                continue;
            }
            if let Some(quote) = trimmed.strip_prefix("> ") {
                flush!();
                body.push_str(&self.paragraph(quote, Some("Quote"), None));
                i += 1;
                continue;
            }
            if let Some((indent, ordered, text)) = list_item(line) {
                flush!();
                let level = (indent / 2).min(2);
                let num_id = if ordered {
                    if numbered_list == 0 {
                        numbered_list = 2 + self.numbered_lists.len();
                        self.numbered_lists.push(numbered_list);
                    }
                    numbered_list
                } else {
                    1
                };
                body.push_str(&self.paragraph(text, Some("ListParagraph"), Some((level, num_id))));
                i += 1;
                continue;
            }
            pending.push(trimmed);
            i += 1;
        }
        flush!();

        let (width, height) = if letter {
            (12_240, 15_840)
        } else {
            (11_906, 16_838)
        };
        let _ = write!(
            body,
            r#"<w:sectPr><w:pgSz w:w="{width}" w:h="{height}"/><w:pgMar w:top="1134" w:right="1134" w:bottom="1134" w:left="1134" w:header="709" w:footer="709" w:gutter="0"/></w:sectPr>"#
        );
        body
    }

    fn paragraph(
        &mut self,
        text: &str,
        style: Option<&str>,
        numbering: Option<(usize, usize)>,
    ) -> String {
        let mut properties = String::new();
        if let Some(style) = style {
            let _ = write!(properties, r#"<w:pStyle w:val="{style}"/>"#);
        }
        if let Some((level, num_id)) = numbering {
            let _ = write!(
                properties,
                r#"<w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="{num_id}"/></w:numPr>"#
            );
        }
        let runs = self.inline(text);
        if properties.is_empty() {
            format!("<w:p>{runs}</w:p>")
        } else {
            format!("<w:p><w:pPr>{properties}</w:pPr>{runs}</w:p>")
        }
    }

    /// `**bold**`, `__bold__`, `*italic*`, `_italic_`, `` `code` ``, `[text](url)`.
    fn inline(&mut self, text: &str) -> String {
        let mut out = String::new();
        for span in parse_inline(text) {
            match span {
                Span::Plain(t) => out.push_str(&run(&t, "")),
                Span::Bold(t) => out.push_str(&run(&t, "<w:b/>")),
                Span::Italic(t) => out.push_str(&run(&t, "<w:i/>")),
                Span::Code(t) => out.push_str(&run(&t, r#"<w:rStyle w:val="InlineCode"/>"#)),
                Span::Link(label, url) if is_absolute_url(&url) => {
                    let id = self.links.len();
                    self.links.push(url);
                    let _ = write!(
                        out,
                        r#"<w:hyperlink r:id="rIdL{id}">{}</w:hyperlink>"#,
                        run(&label, r#"<w:rStyle w:val="Hyperlink"/>"#)
                    );
                }
                Span::Link(label, _) => out.push_str(&run(&label, "")),
            }
        }
        out
    }

    fn table(&mut self, rows: &[&str]) -> String {
        let data: Vec<Vec<String>> = rows
            .iter()
            .filter(|row| !is_separator_row(row))
            .map(|row| table_cells(row))
            .collect();
        let cols = data.iter().map(Vec::len).max().unwrap_or(1).max(1);
        let width = TABLE_WIDTH_TWIPS / cols;
        let border = |side: &str| {
            format!(r#"<w:{side} w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/>"#)
        };
        // Schema order matters: top, left, bottom, right, insideH, insideV.
        let mut out = format!(
            r#"<w:tbl><w:tblPr><w:tblW w:w="5000" w:type="pct"/><w:tblBorders>{}{}{}{}{}{}</w:tblBorders></w:tblPr><w:tblGrid>"#,
            border("top"),
            border("left"),
            border("bottom"),
            border("right"),
            border("insideH"),
            border("insideV")
        );
        for _ in 0..cols {
            let _ = write!(out, r#"<w:gridCol w:w="{width}"/>"#);
        }
        out.push_str("</w:tblGrid>");
        for (r, row) in data.iter().enumerate() {
            out.push_str("<w:tr>");
            if r == 0 {
                out.push_str("<w:trPr><w:tblHeader/></w:trPr>");
            }
            for c in 0..cols {
                let text = row.get(c).map_or("", String::as_str);
                let shading = if r == 0 {
                    r#"<w:shd w:val="clear" w:color="auto" w:fill="D9E2F3"/>"#
                } else {
                    ""
                };
                let content = if r == 0 && !text.is_empty() {
                    self.paragraph(&format!("**{text}**"), None, None)
                } else {
                    self.paragraph(text, None, None)
                };
                let _ = write!(
                    out,
                    r#"<w:tc><w:tcPr><w:tcW w:w="{width}" w:type="dxa"/>{shading}</w:tcPr>{content}</w:tc>"#
                );
            }
            out.push_str("</w:tr>");
        }
        out.push_str("</w:tbl>");
        out
    }
}

fn run(text: &str, properties: &str) -> String {
    let properties = if properties.is_empty() {
        String::new()
    } else {
        format!("<w:rPr>{properties}</w:rPr>")
    };
    format!(
        r#"<w:r>{properties}<w:t xml:space="preserve">{}</w:t></w:r>"#,
        escape(text)
    )
}

fn code_paragraph(line: &str) -> String {
    format!(
        r#"<w:p><w:pPr><w:pStyle w:val="Code"/></w:pPr><w:r><w:t xml:space="preserve">{}</w:t></w:r></w:p>"#,
        escape(&line.replace('\t', "    "))
    )
}

fn is_absolute_url(url: &str) -> bool {
    url.split_once("://").is_some_and(|(scheme, rest)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            && !rest.is_empty()
    }) || url.starts_with("mailto:")
}

/// `## Title` → (2, "Title").
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    rest.starts_with(char::is_whitespace)
        .then(|| (level, rest.trim()))
}

/// `---`, `***`, `___` (optionally spaced).
fn is_rule(line: &str) -> bool {
    let marks: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    marks.len() >= 3 && matches!(marks[0], '-' | '*' | '_') && marks.iter().all(|&c| c == marks[0])
}

/// `  - item` → (indent, ordered, text); also `1.` and `1)`.
fn list_item(line: &str) -> Option<(usize, bool, &str)> {
    let indent_str: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let indent = indent_str.replace('\t', "    ").len();
    let rest = &line[indent_str.len()..];
    if let Some(text) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        return Some((indent, false, text.trim()));
    }
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let after = &rest[digits..];
    after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))
        .map(|text| (indent, true, text.trim()))
}

fn is_separator_row(row: &str) -> bool {
    let inner = row.trim().trim_matches('|');
    !inner.is_empty() && inner.chars().all(|c| matches!(c, '-' | ':' | '|' | ' '))
}

fn table_cells(row: &str) -> Vec<String> {
    let inner = row.trim();
    let inner = inner.strip_prefix('|').unwrap_or(inner);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    // Split on unescaped pipes.
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                current.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut current).trim().to_string()),
            other => current.push(other),
        }
    }
    cells.push(current.trim().to_string());
    cells
}

#[derive(Debug, PartialEq, Eq)]
enum Span {
    Plain(String),
    Bold(String),
    Italic(String),
    Code(String),
    Link(String, String),
}

/// Splits inline markdown into styled spans (no nesting).
fn parse_inline(text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    let find = |from: usize, pat: &[char]| -> Option<usize> {
        (from..chars.len().saturating_sub(pat.len() - 1)).find(|&j| chars[j..j + pat.len()] == *pat)
    };
    let word_char = |index: Option<usize>| {
        index
            .and_then(|k| chars.get(k))
            .is_some_and(|c| c.is_alphanumeric())
    };

    while i < chars.len() {
        let c = chars[i];
        let mut matched: Option<(Span, usize)> = None;
        if c == '`' {
            if let Some(end) = find(i + 1, &['`']).filter(|&e| e > i + 1) {
                matched = Some((Span::Code(chars[i + 1..end].iter().collect()), end + 1));
            }
        } else if (c == '*' || c == '_') && chars.get(i + 1) == Some(&c) {
            if let Some(end) = find(i + 2, &[c, c]).filter(|&e| e > i + 2) {
                matched = Some((Span::Bold(chars[i + 2..end].iter().collect()), end + 2));
            }
        } else if (c == '*' || c == '_')
            && !word_char(i.checked_sub(1))
            && chars
                .get(i + 1)
                .is_some_and(|n| !n.is_whitespace() && *n != c)
        {
            if let Some(end) = find(i + 1, &[c]) {
                if c == '*' || !word_char(Some(end + 1)) {
                    matched = Some((Span::Italic(chars[i + 1..end].iter().collect()), end + 1));
                }
            }
        } else if c == '[' {
            if let Some(close) = find(i + 1, &[']', '(']) {
                if let Some(end) = find(close + 2, &[')']) {
                    let url: String = chars[close + 2..end].iter().collect();
                    if !url.is_empty() && !url.contains(char::is_whitespace) {
                        let label = chars[i + 1..close].iter().collect();
                        matched = Some((Span::Link(label, url), end + 1));
                    }
                }
            }
        }
        if let Some((span, next)) = matched {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(span);
            i = next;
        } else {
            plain.push(c);
            i += 1;
        }
    }
    if !plain.is_empty() {
        spans.push(Span::Plain(plain));
    }
    spans
}

fn numbering_xml(numbered: &[usize]) -> String {
    let levels = |ordered: bool| {
        let bullets = ["•", "◦", "▪"];
        let formats = ["decimal", "lowerLetter", "lowerRoman"];
        (0..3)
            .map(|i| {
                let (fmt, text) = if ordered {
                    (formats[i], format!("%{}.", i + 1))
                } else {
                    ("bullet", bullets[i].to_string())
                };
                format!(
                    r#"<w:lvl w:ilvl="{i}"><w:start w:val="1"/><w:numFmt w:val="{fmt}"/><w:lvlText w:val="{text}"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="{}" w:hanging="360"/></w:pPr></w:lvl>"#,
                    720 + 360 * i
                )
            })
            .collect::<String>()
    };
    let mut out = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:numbering xmlns:w="{W_NS}"><w:abstractNum w:abstractNumId="0"><w:multiLevelType w:val="hybridMultilevel"/>{}</w:abstractNum><w:abstractNum w:abstractNumId="1"><w:multiLevelType w:val="hybridMultilevel"/>{}</w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
        levels(false),
        levels(true)
    );
    for n in numbered {
        let _ = write!(
            out,
            r#"<w:num w:numId="{n}"><w:abstractNumId w:val="1"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="1"/></w:lvlOverride></w:num>"#
        );
    }
    out.push_str("</w:numbering>");
    out
}

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/><Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/></Types>"#;

const PACKAGE_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

const STYLES_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:eastAsia="Calibri" w:cs="Calibri"/><w:sz w:val="22"/><w:szCs w:val="22"/><w:lang w:val="en-US"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120" w:line="264" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style>
<w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:spacing w:after="240"/></w:pPr><w:rPr><w:b/><w:color w:val="1F3864"/><w:sz w:val="52"/><w:szCs w:val="52"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="360" w:after="120"/><w:outlineLvl w:val="0"/></w:pPr><w:rPr><w:b/><w:color w:val="1F3864"/><w:sz w:val="36"/><w:szCs w:val="36"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="240" w:after="80"/><w:outlineLvl w:val="1"/></w:pPr><w:rPr><w:b/><w:color w:val="2F5496"/><w:sz w:val="30"/><w:szCs w:val="30"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="heading 3"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="200" w:after="60"/><w:outlineLvl w:val="2"/></w:pPr><w:rPr><w:b/><w:color w:val="2F5496"/><w:sz w:val="26"/><w:szCs w:val="26"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading4"><w:name w:val="heading 4"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:outlineLvl w:val="3"/></w:pPr><w:rPr><w:b/><w:i/><w:color w:val="2F5496"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading5"><w:name w:val="heading 5"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:outlineLvl w:val="4"/></w:pPr><w:rPr><w:b/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading6"><w:name w:val="heading 6"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:outlineLvl w:val="5"/></w:pPr><w:rPr><w:i/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="ListParagraph"><w:name w:val="List Paragraph"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:spacing w:after="60"/><w:contextualSpacing/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:ind w:left="567"/></w:pPr><w:rPr><w:i/><w:color w:val="595959"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Code"><w:name w:val="Code"/><w:basedOn w:val="Normal"/><w:pPr><w:shd w:val="clear" w:color="auto" w:fill="F2F2F2"/><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:rPr><w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas"/><w:sz w:val="19"/><w:szCs w:val="19"/></w:rPr></w:style>
<w:style w:type="character" w:styleId="InlineCode"><w:name w:val="Inline Code"/><w:rPr><w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas"/><w:shd w:val="clear" w:color="auto" w:fill="F2F2F2"/></w:rPr></w:style>
<w:style w:type="character" w:styleId="Hyperlink"><w:name w:val="Hyperlink"/><w:rPr><w:color w:val="0563C1"/><w:u w:val="single"/></w:rPr></w:style>
</w:styles>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_spans_parse() {
        assert_eq!(
            parse_inline("a **b** *c* `d` [e](https://x.y) snake_case_word _f_"),
            vec![
                Span::Plain("a ".into()),
                Span::Bold("b".into()),
                Span::Plain(" ".into()),
                Span::Italic("c".into()),
                Span::Plain(" ".into()),
                Span::Code("d".into()),
                Span::Plain(" ".into()),
                Span::Link("e".into(), "https://x.y".into()),
                Span::Plain(" snake_case_word ".into()),
                Span::Italic("f".into()),
            ]
        );
        assert_eq!(
            parse_inline("2 * 3 * 4"),
            vec![Span::Plain("2 * 3 * 4".into())]
        );
    }

    #[test]
    fn block_helpers_classify_lines() {
        assert_eq!(heading("### Deep"), Some((3, "Deep")));
        assert_eq!(heading("#hashtag"), None);
        assert!(is_rule("- - -") && is_rule("***") && !is_rule("--x"));
        assert_eq!(list_item("  - x"), Some((2, false, "x")));
        assert_eq!(list_item("10. y"), Some((0, true, "y")));
        assert_eq!(list_item("-x"), None);
        assert!(is_separator_row("|---|:--:|"));
        assert_eq!(table_cells("| a | b\\|c |"), vec!["a", "b|c"]);
    }

    #[test]
    fn writes_a_document_that_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.docx");
        let markdown = "# Heading\n\nSome **bold** & <text>.\n\n- one\n- two\n\n1. first\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n```\nlet x = 1;\n```\n\nSee [docs](https://example.com).";
        write_docx(&path, markdown, Some("My Title"), false).unwrap();
        let text = crate::read_document(&path, &crate::ReadOptions::default())
            .unwrap()
            .unwrap();
        assert!(
            text.starts_with("# My Title\n# Heading\nSome bold & <text>.\n- one\n- two\n- first\n"),
            "{text}"
        );
        assert!(text.contains("| A | B |\n| 1 | 2 |"), "{text}");
        assert!(text.contains("let x = 1;"), "{text}");
        assert!(text.contains("See docs."), "{text}");
    }
}
