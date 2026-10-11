//! Office documents, PDFs and Jupyter notebooks for the file tools.
//!
//! Reading turns a document into text the model can work with (spreadsheets
//! become tables with cell addresses, Word/PowerPoint become markdown-ish
//! text, PDFs are split into pages, notebooks show cells and outputs).
//! Writing builds `.docx` from markdown, `.pptx` from slide specs, and edits
//! `.xlsx` workbooks in place.
//!
//! Ported from `AzureAiHarness` (`Documents.cs`, `OfficeWriters.cs`,
//! `Notebook.cs`).

mod docx_write;
mod notebook;
mod ooxml_read;
mod pptx_write;
mod spreadsheet;
mod xml;

use std::path::Path;

pub use docx_write::write_docx;
pub use pptx_write::{write_pptx, SlideSpec};
pub use spreadsheet::{write_xlsx, XlsxEdit, XlsxSummary};

pub type Result<T> = std::result::Result<T, String>;

/// Spreadsheet formats readable through calamine.
const SPREADSHEET_EXTENSIONS: &[&str] = &["xlsx", "xlsm", "xltx", "xltm", "xlsb", "xls", "ods"];
const WORD_EXTENSIONS: &[&str] = &["docx", "docm", "dotx"];
const POWERPOINT_EXTENSIONS: &[&str] = &["pptx", "pptm"];

/// Default number of spreadsheet rows shown per sheet.
pub const DEFAULT_MAX_ROWS: usize = 200;
/// Pages shown when a PDF is read without a `pages` range.
const DEFAULT_PDF_PAGES: usize = 50;

/// The kind of document a path refers to, by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Spreadsheet,
    Word,
    PowerPoint,
    Pdf,
    Notebook,
}

impl DocumentKind {
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        let ext = ext.as_str();
        if SPREADSHEET_EXTENSIONS.contains(&ext) {
            Some(Self::Spreadsheet)
        } else if WORD_EXTENSIONS.contains(&ext) {
            Some(Self::Word)
        } else if POWERPOINT_EXTENSIONS.contains(&ext) {
            Some(Self::PowerPoint)
        } else if ext == "pdf" {
            Some(Self::Pdf)
        } else if ext == "ipynb" {
            Some(Self::Notebook)
        } else {
            None
        }
    }
}

/// Options for [`read_document`]; each applies only to some formats.
#[derive(Debug, Clone, Default)]
pub struct ReadOptions {
    /// Spreadsheet: only this sheet (case-insensitive).
    pub sheet: Option<String>,
    /// Spreadsheet: A1 range such as `A1:D50`.
    pub range: Option<String>,
    /// Spreadsheet: rows shown per sheet (default [`DEFAULT_MAX_ROWS`]).
    pub max_rows: Option<usize>,
    /// Spreadsheet: append `[=FORMULA]` to formula cells.
    pub formulas: bool,
    /// PDF: page range such as `3` or `1-10`.
    pub pages: Option<String>,
}

/// Renders a document as text. Returns `Ok(None)` if `path` is not a
/// document type handled here.
pub fn read_document(path: &Path, options: &ReadOptions) -> Result<Option<String>> {
    let Some(kind) = DocumentKind::from_path(path) else {
        return Ok(None);
    };
    let text = match kind {
        DocumentKind::Spreadsheet => spreadsheet::read(path, options)?,
        DocumentKind::Word => ooxml_read::read_docx(path)?,
        DocumentKind::PowerPoint => ooxml_read::read_pptx(path)?,
        DocumentKind::Pdf => read_pdf(path, options.pages.as_deref())?,
        DocumentKind::Notebook => {
            let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            match notebook::render(&raw) {
                Some(text) => text,
                // Not nbformat 4: let the caller show the raw JSON.
                None => return Ok(None),
            }
        }
    };
    Ok(Some(text))
}

/// Marker appended when no page in the requested range has extractable text.
pub const NO_TEXT_LAYER_MARKER: &str =
    "[No text layer found; this PDF is probably scanned. Convert it with OCR to read it.]";

fn read_pdf(path: &Path, pages: Option<&str>) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let texts = pdf_extract::extract_text_from_mem_by_pages(&bytes)
        .map_err(|e| format!("cannot read PDF {}: {e}", path.display()))?;
    let total = texts.len();
    let name = file_name(path);
    if total == 0 {
        return Ok(format!("PDF {name}: no pages"));
    }
    let (from, to) = parse_page_range(pages, total);
    let mut out = format!("PDF {name}: {total} page(s); showing {from}-{to}\n");
    let mut empty = 0;
    for (index, text) in texts.iter().enumerate().take(to).skip(from - 1) {
        let text = text.trim();
        if text.is_empty() {
            empty += 1;
        }
        out.push_str(&format!("\n--- page {} ---\n{text}\n", index + 1));
    }
    if empty == to - from + 1 {
        out.push('\n');
        out.push_str(NO_TEXT_LAYER_MARKER);
        out.push('\n');
    }
    Ok(out)
}

/// `"3"` → (3, 3); `"2-5"` → (2, 5); `"4-"` → (4, total); clamped to the
/// document. No range shows the first [`DEFAULT_PDF_PAGES`] pages.
fn parse_page_range(pages: Option<&str>, total: usize) -> (usize, usize) {
    let Some(pages) = pages.map(str::trim).filter(|p| !p.is_empty()) else {
        return (1, total.min(DEFAULT_PDF_PAGES));
    };
    let (first, last) = pages.split_once('-').unwrap_or((pages, pages));
    let from = first.trim().parse().unwrap_or(1).clamp(1, total);
    let to = match last.trim() {
        "" => total,
        value => value.parse().unwrap_or(total),
    }
    .clamp(from, total);
    (from, to)
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// 1-based column number → letters (`1` → `A`, `28` → `AB`).
#[must_use]
pub fn column_letters(mut column: u32) -> String {
    let mut letters = Vec::new();
    while column > 0 {
        let rem = (column - 1) % 26;
        letters.push(char::from(b'A' + u8::try_from(rem).unwrap_or(0)));
        column = (column - 1) / 26;
    }
    letters.iter().rev().collect()
}

/// `"B12"` → `(col 2, row 12)`, both 1-based.
#[must_use]
pub fn parse_cell(address: &str) -> Option<(u32, u32)> {
    let address = address.trim().replace('$', "");
    let split = address.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = address.split_at(split);
    if letters.is_empty() || !letters.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut column: u32 = 0;
    for c in letters.chars() {
        column = column
            .checked_mul(26)?
            .checked_add(u32::from(c.to_ascii_uppercase() as u8 - b'A' + 1))?;
    }
    let row: u32 = digits.parse().ok()?;
    (column > 0 && row > 0 && column <= 16_384 && row <= 1_048_576).then_some((column, row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_addresses_round_trip() {
        assert_eq!(parse_cell("A1"), Some((1, 1)));
        assert_eq!(parse_cell("$AB$12"), Some((28, 12)));
        assert_eq!(parse_cell("xfd1048576"), Some((16_384, 1_048_576)));
        assert_eq!(parse_cell("A0"), None);
        assert_eq!(parse_cell("12"), None);
        assert_eq!(parse_cell("A1B"), None);
        assert_eq!(column_letters(1), "A");
        assert_eq!(column_letters(26), "Z");
        assert_eq!(column_letters(28), "AB");
        assert_eq!(column_letters(16_384), "XFD");
    }

    #[test]
    fn page_ranges_clamp_to_document() {
        assert_eq!(parse_page_range(None, 80), (1, 50));
        assert_eq!(parse_page_range(Some("3"), 10), (3, 3));
        assert_eq!(parse_page_range(Some("2-5"), 10), (2, 5));
        assert_eq!(parse_page_range(Some("4-"), 10), (4, 10));
        assert_eq!(parse_page_range(Some("8-99"), 10), (8, 10));
        assert_eq!(parse_page_range(Some("50"), 10), (10, 10));
    }

    #[test]
    fn kinds_follow_extensions() {
        assert_eq!(
            DocumentKind::from_path(Path::new("a/b.XLSX")),
            Some(DocumentKind::Spreadsheet)
        );
        assert_eq!(
            DocumentKind::from_path(Path::new("deck.pptx")),
            Some(DocumentKind::PowerPoint)
        );
        assert_eq!(DocumentKind::from_path(Path::new("main.rs")), None);
    }
}
