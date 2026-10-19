//! Images in replies (`![alt](url)`) drawn inline on terminals that speak
//! the kitty graphics protocol (kitty, Ghostty).
//!
//! The image is sent once, quietly (`q=2`), as a virtual placement; it is
//! then shown by printing rows of the placeholder character U+10EEEE whose
//! foreground colour is the image id and whose first cell carries a row
//! diacritic. Being ordinary text, the placeholders also survive full-screen
//! mode, which redraws the conversation from its own copy of the cells.
//!
//! `CLAW_INLINE_IMAGES=0` turns this off, `=1` forces it on.

use std::collections::HashMap;
use std::env;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat};

use crate::repl_commands::{base64_encode, expand_home};

/// The image placeholder character.
const PLACEHOLDER: char = '\u{10EEEE}';

/// Diacritics for rows 0, 1, 2, … (kitty's `rowcolumn-diacritics.txt`).
const ROW_DIACRITICS: [char; 24] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
];

/// Widest an image is drawn, in columns.
const MAX_COLS: usize = 80;
/// Tallest an image is drawn, in rows (one diacritic per row).
const MAX_ROWS: usize = ROW_DIACRITICS.len();
const MAX_BYTES: usize = 25 * 1024 * 1024;
/// Base64 bytes per graphics escape, the protocol's limit.
const CHUNK: usize = 4096;

/// An image the terminal already holds.
#[derive(Debug, Clone, Copy)]
struct Sent {
    id: u32,
    cols: usize,
    rows: usize,
}

fn sent() -> &'static Mutex<HashMap<String, Sent>> {
    static SENT: OnceLock<Mutex<HashMap<String, Sent>>> = OnceLock::new();
    SENT.get_or_init(Mutex::default)
}

/// Whether replies show images inline.
pub fn enabled() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    let supported = *SUPPORTED.get_or_init(|| match env::var("CLAW_INLINE_IMAGES").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => {
            let term = env::var("TERM").unwrap_or_default();
            let program = env::var("TERM_PROGRAM").unwrap_or_default();
            // tmux would need the escapes wrapped and passthrough allowed.
            env::var_os("TMUX").is_none()
                && (term.contains("kitty")
                    || term.contains("ghostty")
                    || program.eq_ignore_ascii_case("ghostty")
                    || env::var_os("KITTY_WINDOW_ID").is_some())
        }
    });
    supported && runtime::turn_keys::stdout_is_terminal()
}

/// What shows the image at `url` at most `max_cols` wide: the escapes that
/// send it (the first time only) and the placeholder rows, one per line.
pub fn show(url: &str, max_cols: usize) -> Result<String, String> {
    let known = sent()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(url)
        .copied();
    let mut out = String::new();
    let image = match known {
        Some(image) => image,
        None => {
            let bytes = load(url)?;
            let (png, cols, rows) = fit(&bytes, url, max_cols.clamp(1, MAX_COLS))?;
            let image = Sent {
                id: image_id(url),
                cols,
                rows,
            };
            transmit(&mut out, &png, image);
            sent()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(url.to_string(), image);
            image
        }
    };
    let (r, g, b) = (image.id >> 16, (image.id >> 8) & 0xff, image.id & 0xff);
    for (row, diacritic) in ROW_DIACRITICS.iter().take(image.rows).enumerate() {
        if row > 0 {
            out.push('\n');
        }
        out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{PLACEHOLDER}{diacritic}"));
        out.extend(std::iter::repeat_n(PLACEHOLDER, image.cols - 1));
        out.push_str("\x1b[39m");
    }
    Ok(out)
}

/// A 24-bit id (the placeholder colour) unlikely to clash with images
/// other programs put in the same window.
fn image_id(url: &str) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    #[allow(clippy::cast_possible_truncation)]
    let id = (hasher.finish() as u32) & 0x00ff_ffff;
    id.max(1)
}

/// Sends the PNG and creates its virtual placement of `cols` x `rows`.
fn transmit(out: &mut String, png: &[u8], image: Sent) {
    let data = base64_encode(png);
    let chunks: Vec<&str> = data
        .as_bytes()
        .chunks(CHUNK)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        if index == 0 {
            out.push_str(&format!(
                "\x1b_Ga=T,U=1,f=100,q=2,i={},c={},r={},m={more};{chunk}\x1b\\",
                image.id, image.cols, image.rows
            ));
        } else {
            out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }
}

/// The image's bytes, from the web or the disk.
fn load(url: &str) -> Result<Vec<u8>, String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        // A thread of its own: the blocking client must not run inside the
        // async runtime that streams the reply.
        let url = url.to_string();
        return std::thread::spawn(move || fetch(&url))
            .join()
            .unwrap_or_else(|_| Err("download failed".to_string()));
    }
    let path = url.strip_prefix("file://").unwrap_or(url);
    let path: PathBuf = expand_home(path);
    let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err("larger than 25 MB".to_string());
    }
    Ok(bytes)
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) claw")
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client.get(url).send().map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BYTES as u64)
    {
        return Err("larger than 25 MB".to_string());
    }
    let bytes = response.bytes().map_err(|error| error.to_string())?;
    Ok(bytes.to_vec())
}

/// Terminal cell size in pixels; a common size when the terminal won't say.
fn cell_pixels() -> (f64, f64) {
    crossterm::terminal::window_size()
        .ok()
        .filter(|size| size.width > 0 && size.height > 0 && size.columns > 0 && size.rows > 0)
        .map_or((9.0, 18.0), |size| {
            (
                f64::from(size.width) / f64::from(size.columns),
                f64::from(size.height) / f64::from(size.rows),
            )
        })
}

/// The image as a PNG scaled to fit `max_cols` by [`MAX_ROWS`] cells, and the
/// cells it takes.
fn fit(bytes: &[u8], url: &str, max_cols: usize) -> Result<(Vec<u8>, usize, usize), String> {
    let (cell_w, cell_h) = cell_pixels();
    #[allow(clippy::cast_precision_loss)]
    let (box_w, box_h) = (max_cols as f64 * cell_w, MAX_ROWS as f64 * cell_h);
    let image = if is_svg(bytes, url) {
        render_svg(bytes, box_w, box_h)?
    } else {
        let image = image::load_from_memory(bytes).map_err(|error| error.to_string())?;
        let (w, h) = (f64::from(image.width()), f64::from(image.height()));
        let scale = (box_w / w).min(box_h / h);
        if scale < 1.0 {
            let (w, h) = scaled(w, h, scale);
            image.resize_exact(w, h, FilterType::Triangle)
        } else {
            image
        }
    };
    let (w, h) = (f64::from(image.width()), f64::from(image.height()));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cols = ((w / cell_w).ceil() as usize).clamp(1, max_cols);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rows = ((h / cell_h).ceil() as usize).clamp(1, MAX_ROWS);
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok((png, cols, rows))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn scaled(w: f64, h: f64, scale: f64) -> (u32, u32) {
    (
        ((w * scale).round() as u32).max(1),
        ((h * scale).round() as u32).max(1),
    )
}

fn is_svg(bytes: &[u8], url: &str) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_lowercase();
    url.to_lowercase()
        .split(['?', '#'])
        .next()
        .is_some_and(|path| path.ends_with(".svg"))
        || head.trim_start().starts_with("<svg")
        || (head.trim_start().starts_with("<?xml") && head.contains("<svg"))
}

/// Draws an SVG as large as fits the box, on white: models draw dark
/// strokes and text that would vanish on a dark terminal.
fn render_svg(bytes: &[u8], box_w: f64, box_h: f64) -> Result<DynamicImage, String> {
    use resvg::{tiny_skia, usvg};
    static FONTS: OnceLock<std::sync::Arc<usvg::fontdb::Database>> = OnceLock::new();
    let fonts = FONTS.get_or_init(|| {
        let mut fonts = usvg::fontdb::Database::new();
        fonts.load_system_fonts();
        // fontdb's generic families (Arial, Times New Roman, …) are often
        // missing; point them at fonts that are installed.
        let installed = |fonts: &usvg::fontdb::Database, names: &[&str]| {
            names
                .iter()
                .find(|name| {
                    fonts
                        .faces()
                        .any(|face| face.families.iter().any(|(family, _)| family == *name))
                })
                .map(|name| (*name).to_string())
        };
        let sans = installed(
            &fonts,
            &["DejaVu Sans", "Noto Sans", "Liberation Sans", "Arial"],
        );
        let serif = installed(&fonts, &["DejaVu Serif", "Noto Serif", "Liberation Serif"]);
        let mono = installed(
            &fonts,
            &["DejaVu Sans Mono", "Noto Sans Mono", "Liberation Mono"],
        );
        if let Some(name) = sans {
            fonts.set_sans_serif_family(name);
        }
        if let Some(name) = serif {
            fonts.set_serif_family(name);
        }
        if let Some(name) = mono {
            fonts.set_monospace_family(name);
        }
        std::sync::Arc::new(fonts)
    });
    let options = usvg::Options {
        fontdb: fonts.clone(),
        // Text without a font-family is drawn sans-serif, not Times.
        font_family: "sans-serif".to_string(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options).map_err(|error| error.to_string())?;
    let size = tree.size();
    let (w, h) = (f64::from(size.width()), f64::from(size.height()));
    let scale = (box_w / w).min(box_h / h);
    let (pw, ph) = scaled(w, h, scale);
    let mut pixmap = tiny_skia::Pixmap::new(pw, ph).ok_or("empty image")?;
    pixmap.fill(tiny_skia::Color::WHITE);
    #[allow(clippy::cast_possible_truncation)]
    let transform = tiny_skia::Transform::from_scale(scale as f32, scale as f32);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let rgba = image::RgbaImage::from_raw(pw, ph, pixmap.take()).ok_or("bad pixel buffer")?;
    Ok(DynamicImage::ImageRgba8(rgba))
}

#[cfg(test)]
mod tests {
    use super::{fit, is_svg, transmit, Sent, PLACEHOLDER, ROW_DIACRITICS};

    #[test]
    fn svg_is_recognised_by_name_or_content() {
        assert!(is_svg(b"", "/tmp/chart.svg"));
        assert!(is_svg(b"", "https://x/a.SVG?v=1"));
        assert!(is_svg(b"<?xml version=\"1.0\"?>\n<svg>", "x"));
        assert!(!is_svg(b"\x89PNG", "x.png"));
    }

    #[test]
    fn svg_is_rendered_to_a_png_within_the_box() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><rect width="100" height="50" fill="red"/></svg>"#;
        let (png, cols, rows) = fit(svg, "a.svg", 40).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert!((1..=40).contains(&cols));
        assert!((1..=ROW_DIACRITICS.len()).contains(&rows));
    }

    #[test]
    fn large_images_are_sent_in_chunks() {
        let mut out = String::new();
        transmit(
            &mut out,
            &[7; 10_000],
            Sent {
                id: 5,
                cols: 3,
                rows: 2,
            },
        );
        assert!(out.starts_with("\x1b_Ga=T,U=1,f=100,q=2,i=5,c=3,r=2,m=1;"));
        assert_eq!(out.matches("\x1b_G").count(), 4);
        assert!(out.ends_with("\x1b\\") && out.contains("\x1b_Gm=0;"));
        assert!(!out.contains(PLACEHOLDER));
    }
}
