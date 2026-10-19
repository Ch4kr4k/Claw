//! How replies are drawn: iced's Markdown with code blocks that are syntax
//! highlighted (Solarized), wrapped instead of overflowing, and carry a
//! language label and a Copy button; paragraphs that break long words; and
//! images loaded from the web or from disk.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::widget::text::{Span, Wrapping};
use iced::widget::{button, column, container, image, markdown, rich_text, row, space, svg, text};
use iced::{Element, Font, Length};

use crate::theme;
use crate::Message;

/// An image a reply refers to.
#[derive(Debug, Clone)]
pub enum ImageState {
    Loading,
    Raster(image::Handle),
    Svg(svg::Handle),
    Failed(String),
}

type Line = Arc<[Span<'static, markdown::Uri>]>;

/// Highlighted code blocks, keyed by language, code and theme. Streaming
/// replies add a new block per update, so the cache is cleared when it
/// grows large.
#[derive(Default)]
pub struct HighlightCache {
    blocks: HashMap<u64, Arc<[Line]>>,
}

impl HighlightCache {
    fn get(&mut self, language: &str, code: &str, theme: iced::highlighter::Theme) -> Arc<[Line]> {
        let mut hasher = DefaultHasher::new();
        (language, code, format!("{theme:?}")).hash(&mut hasher);
        let key = hasher.finish();
        if let Some(lines) = self.blocks.get(&key) {
            return lines.clone();
        }
        if self.blocks.len() > 400 {
            self.blocks.clear();
        }
        let lines = highlight(language, code, theme);
        self.blocks.insert(key, lines.clone());
        lines
    }
}

fn highlight(language: &str, code: &str, theme: iced::highlighter::Theme) -> Arc<[Line]> {
    let token = match language.trim().to_lowercase().as_str() {
        "" | "text" | "plain" | "console" | "output" => "txt".to_string(),
        "shell" | "bash" | "zsh" | "sh" => "sh".to_string(),
        "rust" => "rs".to_string(),
        "python" | "py" => "py".to_string(),
        "javascript" | "js" => "js".to_string(),
        "typescript" | "ts" => "ts".to_string(),
        "c++" | "cpp" => "cpp".to_string(),
        "json" => "json".to_string(),
        "yaml" | "yml" => "yaml".to_string(),
        "toml" => "toml".to_string(),
        "markdown" | "md" => "md".to_string(),
        other => other.to_string(),
    };
    let mut stream = iced::highlighter::Stream::new(&iced::highlighter::Settings { theme, token });
    code.trim_end_matches('\n')
        .split('\n')
        .map(|line| {
            let spans: Vec<Span<'static, markdown::Uri>> = stream
                .highlight_line(line)
                .map(|(range, highlight)| {
                    let mut span = Span::new(line[range].to_string());
                    if let Some(color) = highlight.color() {
                        span = span.color(color);
                    }
                    if let Some(font) = highlight.font() {
                        span = span.font(font);
                    }
                    span
                })
                .collect();
            stream.commit();
            let spans: Line = if spans.is_empty() {
                // Keep empty lines one line tall.
                Arc::from(vec![Span::new(" ".to_string())])
            } else {
                Arc::from(spans)
            };
            spans
        })
        .collect::<Vec<_>>()
        .into()
}

pub struct Viewer<'a> {
    pub images: &'a HashMap<String, ImageState>,
    pub saved: &'a HashMap<String, Result<PathBuf, String>>,
    pub highlights: &'a RefCell<HighlightCache>,
    pub light: bool,
}

impl<'a> markdown::Viewer<'a, Message> for Viewer<'a> {
    fn on_link_click(url: markdown::Uri) -> Message {
        Message::Link(url)
    }

    fn paragraph(
        &self,
        settings: markdown::Settings,
        text: &markdown::Text,
    ) -> Element<'a, Message> {
        rich_text(text.spans(settings.style))
            .size(settings.text_size)
            .wrapping(Wrapping::WordOrGlyph)
            .on_link_click(Self::on_link_click)
            .into()
    }

    fn code_block(
        &self,
        settings: markdown::Settings,
        language: Option<&'a str>,
        code: &'a str,
        _lines: &'a [markdown::Text],
    ) -> Element<'a, Message> {
        let language = language.unwrap_or_default();
        let lines = self.highlights.borrow_mut().get(
            language,
            code,
            if self.light {
                iced::highlighter::Theme::InspiredGitHub
            } else {
                iced::highlighter::Theme::SolarizedDark
            },
        );
        let body = column(lines.iter().map(|line| {
            rich_text(line.clone())
                .font(Font::MONOSPACE)
                .size(settings.code_size)
                .wrapping(Wrapping::WordOrGlyph)
                .into()
        }));
        let label = if language.is_empty() {
            "code"
        } else {
            language
        };
        let header = row![
            text(label.to_string()).size(12).style(theme::muted),
            space::horizontal(),
            button(text("Copy").size(12))
                .style(theme::ghost)
                .padding([2, 8])
                .on_press(Message::Copy(code.to_string())),
        ]
        .align_y(iced::Alignment::Center);
        container(column![header, container(body).width(Length::Fill)].spacing(4))
            .padding([8, 12])
            .width(Length::Fill)
            .style(theme::code_box)
            .into()
    }

    fn image(
        &self,
        settings: markdown::Settings,
        url: &'a markdown::Uri,
        _title: &'a str,
        alt: &markdown::Text,
    ) -> Element<'a, Message> {
        let picture: Element<'a, Message> = match self.images.get(url.as_str()) {
            Some(ImageState::Raster(handle)) => image(handle.clone())
                .width(Length::Shrink)
                .content_fit(iced::ContentFit::ScaleDown)
                .into(),
            Some(ImageState::Svg(handle)) => svg(handle.clone()).width(Length::Fixed(480.0)).into(),
            Some(ImageState::Failed(error)) => text(format!("Image not loaded ({error}): {url}"))
                .size(12)
                .style(theme::muted)
                .wrapping(Wrapping::WordOrGlyph)
                .into(),
            _ => text("Loading image…").size(12).style(theme::muted).into(),
        };
        let caption = rich_text(alt.spans(settings.style))
            .size(settings.text_size * 0.8)
            .wrapping(Wrapping::WordOrGlyph)
            .on_link_click(Self::on_link_click);
        let save: Element<'a, Message> = match self.saved.get(url.as_str()) {
            Some(Ok(path)) => row![
                text(format!("✓ Saved to {}", crate::sessions::short_path(path)))
                    .size(12)
                    .style(theme::muted),
                button(text("Show").size(12))
                    .style(theme::ghost)
                    .padding([2, 8])
                    .on_press(Message::OpenFolder(
                        path.parent().map(Path::to_path_buf).unwrap_or_default(),
                    )),
            ]
            .align_y(iced::Alignment::Center)
            .into(),
            Some(Err(error)) => row![
                text(format!("Not saved: {error}"))
                    .size(12)
                    .style(theme::muted),
                button(text("Retry").size(12))
                    .style(theme::ghost)
                    .padding([2, 8])
                    .on_press(Message::SaveImage(url.to_string())),
            ]
            .align_y(iced::Alignment::Center)
            .into(),
            None => button(text("Save").size(12))
                .style(theme::ghost)
                .padding([2, 8])
                .on_press(Message::SaveImage(url.to_string()))
                .into(),
        };
        column![
            button(container(picture).max_width(720).max_height(520))
                .style(theme::ghost)
                .padding(2)
                .on_press(Message::Link(url.clone())),
            row![caption, space::horizontal(), save].align_y(iced::Alignment::Center),
        ]
        .spacing(4)
        .into()
    }
}

/// Image references in a reply that are not loaded or loading yet.
pub fn wanted_images<'c>(
    content: &'c markdown::Content,
    images: &HashMap<String, ImageState>,
) -> Vec<&'c String> {
    content
        .images()
        .iter()
        .filter(|url| !images.contains_key(url.as_str()))
        .collect()
}

/// Loads one image from the web or the disk.
pub async fn load_image(url: String) -> (String, ImageState) {
    let state = fetch(&url).await.unwrap_or_else(ImageState::Failed);
    (url, state)
}

/// Saves the image at `url` into the downloads folder; returns its path.
pub async fn save_image(url: String) -> Result<PathBuf, String> {
    let (bytes, kind) = fetch_bytes(&url).await?;
    let dir = downloads_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| error.to_string())?;
    let mut name = url
        .split(['?', '#'])
        .next()
        .and_then(|path| path.rsplit('/').find(|segment| !segment.is_empty()))
        .map(|segment| {
            segment
                .replace('\\', "_")
                .trim_start_matches('.')
                .to_string()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "image".to_string());
    if !name.contains('.') {
        let ext = match kind.split(';').next().unwrap_or_default().trim() {
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/svg+xml" => "svg",
            _ => "png",
        };
        name = format!("{name}.{ext}");
    }
    let path = free_path(&dir, &name);
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|error| error.to_string())?;
    Ok(path)
}

/// `$XDG_DOWNLOAD_DIR`, else `~/Downloads`.
fn downloads_dir() -> PathBuf {
    std::env::var_os("XDG_DOWNLOAD_DIR").map_or_else(
        || crate::sessions::home_dir().join("Downloads"),
        PathBuf::from,
    )
}

/// `dir/name`, or `dir/stem (n).ext` for the first `n` not taken.
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    std::iter::once(dir.join(name))
        .chain((1..).map(|n| dir.join(format!("{stem} ({n}){ext}"))))
        .find(|path| !path.exists())
        .unwrap_or_else(|| dir.join(name))
}

async fn fetch(url: &str) -> Result<ImageState, String> {
    let (bytes, kind) = fetch_bytes(url).await?;
    image_state(url, bytes, &kind)
}

/// The bytes at `url` (web or disk) and their content type, if known.
async fn fetch_bytes(url: &str) -> Result<(Vec<u8>, String), String> {
    let (bytes, kind) = if url.starts_with("http://") || url.starts_with("https://") {
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) claw-desktop")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|error| error.to_string())?;
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let kind = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = response.bytes().await.map_err(|error| error.to_string())?;
        (bytes.to_vec(), kind)
    } else {
        let path = url.strip_prefix("file://").unwrap_or(url);
        let path = crate::expand_path(path);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| error.to_string())?;
        (bytes, String::new())
    };
    if bytes.len() > 25 * 1024 * 1024 {
        return Err("larger than 25 MB".to_string());
    }
    Ok((bytes, kind))
}

fn image_state(url: &str, bytes: Vec<u8>, kind: &str) -> Result<ImageState, String> {
    let is_svg = kind.contains("svg")
        || url
            .to_lowercase()
            .split('?')
            .next()
            .unwrap_or_default()
            .ends_with(".svg")
        || bytes.starts_with(b"<svg")
        || bytes.starts_with(b"<?xml");
    if is_svg {
        Ok(ImageState::Svg(svg::Handle::from_memory(bytes)))
    } else if kind.starts_with("text/") {
        Err("not an image".to_string())
    } else {
        Ok(ImageState::Raster(image::Handle::from_bytes(bytes)))
    }
}
