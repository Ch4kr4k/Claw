//! Spreadsheets: read any calamine format as markdown tables with Excel
//! cell addresses; edit `.xlsx` workbooks in place with umya.

use std::path::Path;

use calamine::{open_workbook_auto, Data, Range, Reader};
use serde_json::Value;

use crate::{column_letters, file_name, parse_cell, ReadOptions, Result, DEFAULT_MAX_ROWS};

/// Hard ceiling on rows shown per sheet, whatever the caller asks for.
const MAX_ROWS_CEILING: usize = 5_000;

pub(crate) fn read(path: &Path, options: &ReadOptions) -> Result<String> {
    let mut workbook = open_workbook_auto(path)
        .map_err(|e| format!("cannot open workbook {}: {e}", path.display()))?;
    let names = workbook.sheet_names();
    let mut out = format!(
        "Workbook {}: {} sheet(s): {}\n",
        file_name(path),
        names.len(),
        names
            .iter()
            .map(|n| format!("'{n}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let selected: Vec<String> = match options.sheet.as_deref() {
        Some(wanted) => vec![names
            .iter()
            .find(|n| n.eq_ignore_ascii_case(wanted))
            .cloned()
            .ok_or_else(|| format!("Sheet '{wanted}' not found."))?],
        None => names.clone(),
    };
    let window = options
        .range
        .as_deref()
        .map(|range| {
            parse_range(range)
                .ok_or_else(|| format!("Invalid range '{range}'; use A1 notation like A1:D50."))
        })
        .transpose()?;
    let max_rows = options
        .max_rows
        .unwrap_or(DEFAULT_MAX_ROWS)
        .clamp(1, MAX_ROWS_CEILING);

    for name in selected {
        let data = workbook
            .worksheet_range(&name)
            .map_err(|e| format!("cannot read sheet '{name}': {e}"))?;
        let formulas = if options.formulas {
            workbook.worksheet_formula(&name).ok()
        } else {
            None
        };
        render_sheet(&mut out, &name, &data, formulas.as_ref(), window, max_rows);
    }
    Ok(out)
}

/// Cell window, 0-based inclusive: (first row, first col, last row, last col).
type Window = (u32, u32, u32, u32);

fn parse_range(range: &str) -> Option<Window> {
    let (first, last) = range.split_once(':').unwrap_or((range, range));
    let (c0, r0) = parse_cell(first)?;
    let (c1, r1) = parse_cell(last)?;
    Some((
        r0.min(r1) - 1,
        c0.min(c1) - 1,
        r0.max(r1) - 1,
        c0.max(c1) - 1,
    ))
}

fn render_sheet(
    out: &mut String,
    name: &str,
    data: &Range<Data>,
    formulas: Option<&Range<String>>,
    window: Option<Window>,
    max_rows: usize,
) {
    out.push_str(&format!("\n## Sheet '{name}'"));
    // Formulas without a cached result (e.g. written by a library) are
    // outside the data range, so the used range covers both.
    let used = [bounds(data), formulas.and_then(bounds)]
        .into_iter()
        .flatten()
        .reduce(|(a0, b0, a1, b1), (c0, d0, c1, d1)| {
            (a0.min(c0), b0.min(d0), a1.max(c1), b1.max(d1))
        });
    let Some((r0, c0, r1, c1)) = window.or(used) else {
        out.push_str(" (empty)\n");
        return;
    };
    out.push_str(&format!(
        "  range {}{}:{}{} ({} rows x {} cols)\n",
        column_letters(c0 + 1),
        r0 + 1,
        column_letters(c1 + 1),
        r1 + 1,
        r1 - r0 + 1,
        c1 - c0 + 1
    ));

    // Markdown table with Excel column letters and row numbers so the model
    // can cite cells.
    out.push_str("| row |");
    for c in c0..=c1 {
        out.push_str(&format!(" {} |", column_letters(c + 1)));
    }
    out.push_str("\n|---|");
    for _ in c0..=c1 {
        out.push_str("---|");
    }
    out.push('\n');

    let mut shown = 0;
    for r in r0..=r1 {
        let cells: Vec<String> = (c0..=c1)
            .map(|c| {
                cell_text(
                    data.get_value((r, c)),
                    formulas.and_then(|f| f.get_value((r, c))),
                )
            })
            .collect();
        if window.is_none() && cells.iter().all(String::is_empty) {
            continue;
        }
        if shown >= max_rows {
            let end = (r + u32::try_from(max_rows).unwrap_or(u32::MAX) - 1).min(r1);
            out.push_str(&format!(
                "… {} more rows. Use range (e.g. \"{}{}:{}{}\") to read further.\n",
                r1 - r + 1,
                column_letters(c0 + 1),
                r + 1,
                column_letters(c1 + 1),
                end + 1
            ));
            break;
        }
        out.push_str(&format!("| {} |", r + 1));
        for cell in cells {
            out.push_str(&format!(" {cell} |"));
        }
        out.push('\n');
        shown += 1;
    }
}

fn bounds<T: calamine::CellType>(range: &Range<T>) -> Option<Window> {
    match (range.start(), range.end()) {
        (Some((r0, c0)), Some((r1, c1))) => Some((r0, c0, r1, c1)),
        _ => None,
    }
}

fn cell_text(value: Option<&Data>, formula: Option<&String>) -> String {
    let mut text = match value {
        Some(Data::DateTime(dt)) => dt.as_datetime().map_or_else(
            || dt.as_f64().to_string(),
            |d| {
                if d.time() == chrono::NaiveTime::MIN {
                    d.format("%Y-%m-%d").to_string()
                } else {
                    d.format("%Y-%m-%d %H:%M").to_string()
                }
            },
        ),
        Some(Data::Error(e)) => format!("#{e:?}"),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    if let Some(formula) = formula.filter(|f| !f.is_empty()) {
        text.push_str(&format!(" [={formula}]"));
    }
    text.replace('|', "\\|").replace(['\r', '\n'], " ")
}

// ─────────────────────────────── writing ───────────────────────────────

/// One `write_xlsx` request.
#[derive(Debug, Clone, Default)]
pub struct XlsxEdit {
    /// Target sheet (default: first sheet, or `Sheet1` for new files).
    pub sheet: Option<String>,
    /// Individual cells: address → value (`null` clears, `"=..."` is a formula).
    pub cells: Vec<(String, Value)>,
    /// Block of rows written from `start_cell`.
    pub rows: Vec<Vec<Value>>,
    /// Top-left cell for `rows` (default `A1`).
    pub start_cell: Option<String>,
    /// Bold the first written row.
    pub bold_first_row: bool,
    /// Sheet to delete before writing.
    pub delete_sheet: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XlsxSummary {
    pub created: bool,
    pub cells_written: usize,
    pub sheet: String,
}

/// Opens (or creates) the workbook at `path`, applies `edit`, and saves.
pub fn write_xlsx(path: &Path, edit: &XlsxEdit) -> Result<XlsxSummary> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if !matches!(ext.as_deref(), Some("xlsx" | "xlsm")) {
        return Err("Only .xlsx/.xlsm files are supported.".to_string());
    }
    // Validate every address before touching the file.
    let cells = edit
        .cells
        .iter()
        .map(|(address, value)| {
            parse_cell(address)
                .map(|coord| (coord, value))
                .ok_or_else(|| format!("Invalid cell address '{address}'."))
        })
        .collect::<Result<Vec<_>>>()?;
    let start_address = edit.start_cell.as_deref().unwrap_or("A1");
    let (start_col, start_row) = parse_cell(start_address)
        .ok_or_else(|| format!("Invalid start_cell '{start_address}'."))?;

    let created = !path.exists();
    let mut book = if created {
        umya_spreadsheet::new_file_empty_worksheet()
    } else {
        umya_spreadsheet::reader::xlsx::read(path)
            .map_err(|e| format!("cannot open workbook {}: {e}", path.display()))?
    };

    if let Some(name) = edit.delete_sheet.as_deref() {
        if let Some(actual) = sheet_name_matching(&book, name) {
            book.remove_sheet_by_name(&actual)
                .map_err(|e| format!("cannot delete sheet '{name}': {e}"))?;
        }
    }

    let sheet_name = match edit.sheet.as_deref() {
        Some(name) => match sheet_name_matching(&book, name) {
            Some(actual) => actual,
            None => {
                book.new_sheet(name)
                    .map_err(|e| format!("cannot add sheet '{name}': {e}"))?;
                name.to_string()
            }
        },
        None => match book.sheet_collection().first() {
            Some(first) => first.name().to_string(),
            None => {
                book.new_sheet("Sheet1").map_err(|e| e.to_string())?;
                "Sheet1".to_string()
            }
        },
    };
    let sheet = book
        .sheet_by_name_mut(&sheet_name)
        .map_err(|e| format!("sheet '{sheet_name}': {e}"))?;

    let mut written = 0;
    for ((col, row), value) in cells {
        set_cell(sheet, (col, row), value);
        written += 1;
    }
    for (r, row) in edit.rows.iter().enumerate() {
        let row_index = start_row + u32::try_from(r).map_err(|e| e.to_string())?;
        for (c, value) in row.iter().enumerate() {
            let col_index = start_col + u32::try_from(c).map_err(|e| e.to_string())?;
            set_cell(sheet, (col_index, row_index), value);
            if r == 0 && edit.bold_first_row {
                sheet
                    .style_mut((col_index, row_index))
                    .font_mut()
                    .set_bold(true);
            }
            written += 1;
        }
    }

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    umya_spreadsheet::writer::xlsx::write(&book, path)
        .map_err(|e| format!("cannot save {}: {e}", path.display()))?;
    Ok(XlsxSummary {
        created,
        cells_written: written,
        sheet: sheet_name,
    })
}

fn sheet_name_matching(book: &umya_spreadsheet::Workbook, name: &str) -> Option<String> {
    book.sheet_collection()
        .iter()
        .map(|s| s.name().to_string())
        .find(|n| n.eq_ignore_ascii_case(name))
}

fn set_cell(sheet: &mut umya_spreadsheet::Worksheet, coord: (u32, u32), value: &Value) {
    match value {
        Value::Null => {
            sheet.remove_cell(coord);
        }
        Value::Bool(b) => {
            sheet.cell_mut(coord).set_value_bool(*b);
        }
        Value::Number(n) => {
            sheet
                .cell_mut(coord)
                .set_value_number(n.as_f64().unwrap_or(0.0));
        }
        Value::String(s) if s.len() > 1 && s.starts_with('=') => {
            sheet.cell_mut(coord).set_formula(&s[1..]);
        }
        Value::String(s) => {
            sheet.cell_mut(coord).set_value_string(s.as_str());
        }
        other => {
            sheet.cell_mut(coord).set_value_string(other.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ranges_parse_to_zero_based_windows() {
        assert_eq!(parse_range("A1:C10"), Some((0, 0, 9, 2)));
        assert_eq!(parse_range("C10:A1"), Some((0, 0, 9, 2)));
        assert_eq!(parse_range("B2"), Some((1, 1, 1, 1)));
        assert_eq!(parse_range("nope"), None);
    }

    #[test]
    fn write_then_read_round_trips_values_and_formulas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        let summary = write_xlsx(
            &path,
            &XlsxEdit {
                sheet: Some("Data".to_string()),
                rows: vec![
                    vec![json!("Name"), json!("Qty")],
                    vec![json!("apples"), json!(3)],
                    vec![json!("pears|x"), json!(4.5)],
                ],
                cells: vec![("B4".to_string(), json!("=SUM(B2:B3)"))],
                bold_first_row: true,
                ..XlsxEdit::default()
            },
        )
        .unwrap();
        assert!(summary.created);
        assert_eq!(summary.cells_written, 7);
        assert_eq!(summary.sheet, "Data");

        let text = read(
            &path,
            &ReadOptions {
                formulas: true,
                ..ReadOptions::default()
            },
        )
        .unwrap();
        assert!(text.contains("1 sheet(s): 'Data'"), "{text}");
        assert!(text.contains("| 2 | apples | 3 |"), "{text}");
        assert!(text.contains("pears\\|x"), "{text}");
        assert!(text.contains("[=SUM(B2:B3)]"), "{text}");

        // Update in place: clear a cell, add a sheet, keep the rest.
        let summary = write_xlsx(
            &path,
            &XlsxEdit {
                sheet: Some("data".to_string()),
                cells: vec![("A2".to_string(), Value::Null)],
                ..XlsxEdit::default()
            },
        )
        .unwrap();
        assert!(!summary.created);
        assert_eq!(summary.sheet, "Data");
        let text = read(&path, &ReadOptions::default()).unwrap();
        assert!(!text.contains("apples"), "{text}");
        assert!(text.contains("pears"), "{text}");
    }

    #[test]
    fn write_rejects_bad_addresses_and_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let bad = XlsxEdit {
            cells: vec![("1A".to_string(), json!(1))],
            ..XlsxEdit::default()
        };
        assert!(write_xlsx(&dir.path().join("x.xlsx"), &bad)
            .unwrap_err()
            .contains("Invalid cell address"));
        assert!(!dir.path().join("x.xlsx").exists());
        assert!(write_xlsx(&dir.path().join("x.csv"), &XlsxEdit::default()).is_err());
    }
}
