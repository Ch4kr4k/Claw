//! Text extraction from Word (`.docx`) and PowerPoint (`.pptx`) packages.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::xml::{attr, entity_text, read_entry};
use crate::Result;

type Archive = zip::ZipArchive<BufReader<File>>;

fn open(path: &Path) -> Result<Archive> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    zip::ZipArchive::new(BufReader::new(file))
        .map_err(|e| format!("{} is not a valid Office file: {e}", path.display()))
}

// ─────────────────────────────── Word ───────────────────────────────

#[derive(Default)]
struct Paragraph {
    text: String,
    style: Option<String>,
    numbered: bool,
}

impl Paragraph {
    /// Markdown line: headings become `#`, list items `- `.
    fn render(&self) -> String {
        let prefix = match self.style.as_deref() {
            Some(style) if style.eq_ignore_ascii_case("title") => "# ".to_string(),
            Some(style) if style.to_ascii_lowercase().starts_with("heading") => {
                style[7..].trim().parse::<usize>().map_or_else(
                    |_| String::new(),
                    |level| format!("{} ", "#".repeat(level.clamp(1, 6))),
                )
            }
            _ if self.numbered => "- ".to_string(),
            _ => String::new(),
        };
        format!("{prefix}{}", self.text)
    }
}

pub(crate) fn read_docx(path: &Path) -> Result<String> {
    let mut archive = open(path)?;
    let xml = read_entry(&mut archive, "word/document.xml")?
        .ok_or_else(|| format!("{} has no word/document.xml", path.display()))?;
    Ok(render_docx_xml(&xml))
}

fn apply_paragraph_property(
    paragraph: Option<&mut Paragraph>,
    element: &quick_xml::events::BytesStart<'_>,
) {
    let Some(p) = paragraph else { return };
    match element.local_name().as_ref() {
        b"pStyle" => p.style = attr(element, b"w:val"),
        b"numPr" => p.numbered = true,
        _ => {}
    }
}

fn render_docx_xml(xml: &str) -> String {
    let mut reader = Reader::from_str(xml);
    let mut out = String::new();
    let mut paragraph: Option<Paragraph> = None;
    let mut in_text = false;
    // Table nesting: rows of cells of paragraph texts.
    let mut table_depth = 0usize;
    let mut row: Vec<String> = Vec::new();
    let mut cell: Vec<String> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"p" => paragraph = Some(Paragraph::default()),
                b"t" => in_text = true,
                b"tbl" => table_depth += 1,
                b"tr" => row.clear(),
                b"tc" => cell.clear(),
                b"pStyle" | b"numPr" => apply_paragraph_property(paragraph.as_mut(), &e),
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                let Some(p) = paragraph.as_mut() else {
                    continue;
                };
                match e.local_name().as_ref() {
                    b"pStyle" | b"numPr" => apply_paragraph_property(Some(p), &e),
                    b"tab" => p.text.push('\t'),
                    b"br" | b"cr" => p.text.push('\n'),
                    _ => {}
                }
            }
            Ok(Event::Text(e)) if in_text => {
                if let Some(p) = paragraph.as_mut() {
                    p.text.push_str(&e.decode().unwrap_or_default());
                }
            }
            Ok(Event::GeneralRef(e)) if in_text => {
                if let Some(p) = paragraph.as_mut() {
                    p.text.push_str(&entity_text(&e));
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" => {
                    if let Some(p) = paragraph.take() {
                        if table_depth > 0 {
                            cell.push(p.text);
                        } else {
                            out.push_str(&p.render());
                            out.push('\n');
                        }
                    }
                }
                b"tc" => row.push(cell.join(" ").replace('|', "\\|").replace('\n', " ")),
                b"tr" => {
                    out.push_str(&format!("| {} |\n", row.join(" | ")));
                }
                b"tbl" => {
                    table_depth = table_depth.saturating_sub(1);
                    if table_depth == 0 {
                        out.push('\n');
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    if out.trim().is_empty() {
        "(empty document)".to_string()
    } else {
        out
    }
}

// ─────────────────────────────── PowerPoint ───────────────────────────────

pub(crate) fn read_pptx(path: &Path) -> Result<String> {
    let mut archive = open(path)?;
    let presentation = read_entry(&mut archive, "ppt/presentation.xml")?
        .ok_or_else(|| format!("{} has no ppt/presentation.xml", path.display()))?;
    let rels = read_entry(&mut archive, "ppt/_rels/presentation.xml.rels")?.unwrap_or_default();
    let targets = relationship_targets(&rels);

    let slides: Vec<String> = slide_rel_ids(&presentation)
        .iter()
        .filter_map(|id| targets.get(id))
        .map(|(_, target)| resolve_target("ppt", target))
        .collect();
    if slides.is_empty() {
        return Ok("(no slides)".to_string());
    }

    let mut out = String::new();
    for (index, slide) in slides.iter().enumerate() {
        out.push_str(&format!("## Slide {}\n", index + 1));
        if let Some(xml) = read_entry(&mut archive, slide)? {
            for line in drawing_paragraphs(&xml) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        let notes = notes_part_for(&mut archive, slide)?;
        if let Some(notes) = notes.and_then(|part| read_entry(&mut archive, &part).ok().flatten()) {
            // Notes pages repeat the slide number as a paragraph; skip it.
            let lines: Vec<String> = drawing_paragraphs(&notes)
                .into_iter()
                .filter(|line| line.parse::<u32>().is_err())
                .collect();
            if !lines.is_empty() {
                out.push_str(&format!("Notes: {}\n", lines.join(" ")));
            }
        }
        out.push('\n');
    }
    Ok(out)
}

/// `r:id`s of `<p:sldId>` in presentation order.
fn slide_rel_ids(presentation: &str) -> Vec<String> {
    let mut reader = Reader::from_str(presentation);
    let mut ids = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Empty(e) | Event::Start(e)) if e.local_name().as_ref() == b"sldId" => {
                if let Some(id) = attr(&e, b"r:id") {
                    ids.push(id);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    ids
}

/// Relationship id → (type, target).
fn relationship_targets(rels: &str) -> HashMap<String, (String, String)> {
    let mut reader = Reader::from_str(rels);
    let mut map = HashMap::new();
    loop {
        match reader.read_event() {
            Ok(Event::Empty(e) | Event::Start(e)) if e.local_name().as_ref() == b"Relationship" => {
                if let (Some(id), Some(target)) = (attr(&e, b"Id"), attr(&e, b"Target")) {
                    map.insert(id, (attr(&e, b"Type").unwrap_or_default(), target));
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    map
}

/// Resolves a relationship target relative to the part's directory.
fn resolve_target(base_dir: &str, target: &str) -> String {
    if let Some(absolute) = target.strip_prefix('/') {
        return absolute.to_string();
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
    for segment in target.split('/') {
        match segment {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn notes_part_for(archive: &mut Archive, slide: &str) -> Result<Option<String>> {
    let (dir, file) = slide.rsplit_once('/').unwrap_or(("", slide));
    let rels = read_entry(archive, &format!("{dir}/_rels/{file}.rels"))?;
    Ok(rels.and_then(|rels| {
        relationship_targets(&rels)
            .into_values()
            .find(|(kind, _)| kind.ends_with("/notesSlide"))
            .map(|(_, target)| resolve_target(dir, &target))
    }))
}

/// Text of each non-empty `<a:p>` in a DrawingML part.
fn drawing_paragraphs(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    let mut lines = Vec::new();
    let mut current: Option<String> = None;
    let mut in_text = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"p" => current = Some(String::new()),
                b"t" => in_text = true,
                _ => {}
            },
            Ok(Event::Empty(e)) if e.local_name().as_ref() == b"br" => {
                if let Some(line) = current.as_mut() {
                    line.push(' ');
                }
            }
            Ok(Event::Text(e)) if in_text => {
                if let Some(line) = current.as_mut() {
                    line.push_str(&e.decode().unwrap_or_default());
                }
            }
            Ok(Event::GeneralRef(e)) if in_text => {
                if let Some(line) = current.as_mut() {
                    line.push_str(&entity_text(&e));
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" => {
                    if let Some(line) = current.take().filter(|l| !l.trim().is_empty()) {
                        lines.push(line);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docx_xml_renders_headings_lists_tables_and_entities() {
        let xml = r#"<w:document xmlns:w="w"><w:body>
            <w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Intro</w:t></w:r></w:p>
            <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/></w:numPr></w:pPr><w:r><w:t>A &amp; B</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>x|y</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>2</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
            <w:p><w:r><w:t xml:space="preserve">tail</w:t><w:tab/><w:t>end</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let text = render_docx_xml(xml);
        assert_eq!(text, "## Intro\n- A & B\n| x\\|y | 2 |\n\ntail\tend\n");
    }

    #[test]
    fn relationship_targets_resolve_relative_paths() {
        assert_eq!(
            resolve_target("ppt", "slides/slide2.xml"),
            "ppt/slides/slide2.xml"
        );
        assert_eq!(
            resolve_target("ppt/slides", "../notesSlides/notesSlide1.xml"),
            "ppt/notesSlides/notesSlide1.xml"
        );
        assert_eq!(
            resolve_target("ppt", "/ppt/slides/s.xml"),
            "ppt/slides/s.xml"
        );
    }
}
