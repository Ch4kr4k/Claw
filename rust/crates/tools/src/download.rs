//! The `Download` tool: saves a file from the web (an image the reply shows,
//! a document a search found) or copies a local one, by default into
//! `~/Downloads`. The body is streamed to `<name>.part` and renamed when
//! complete, and an existing file is never overwritten: the new one gets a
//! ` (1)`, ` (2)`, … suffix.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{json, Value};

/// Largest download accepted.
const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// A whole download may take this long (large files on slow links).
const TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Deserialize)]
pub(crate) struct DownloadInput {
    /// `https://…`, `file://…` or a local path.
    #[serde(alias = "source")]
    url: String,
    /// A directory to save into, or a file path; default `~/Downloads`.
    #[serde(alias = "path", alias = "destination", alias = "dest")]
    save_to: Option<String>,
    /// The file name, when `save_to` is a directory; default from the
    /// server or the URL.
    #[serde(alias = "filename", alias = "file_name")]
    name: Option<String>,
}

pub(crate) fn run(input: DownloadInput) -> Result<Value, String> {
    let url = input.url.trim();
    if url.is_empty() {
        return Err("url is empty".to_string());
    }
    let (dir, file_name) = destination(input.save_to.as_deref(), input.name.as_deref())?;
    fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    if url.starts_with("http://") || url.starts_with("https://") {
        download(url, &dir, file_name)
    } else {
        copy_local(url, &dir, file_name)
    }
}

/// The directory to save into and the file name, if the caller chose one.
fn destination(
    save_to: Option<&str>,
    name: Option<&str>,
) -> Result<(PathBuf, Option<String>), String> {
    let name = name.map(sanitize).filter(|name| !name.is_empty());
    let Some(save_to) = save_to.map(str::trim).filter(|path| !path.is_empty()) else {
        return Ok((downloads_dir()?, name));
    };
    let path = expand_home(save_to);
    let looks_like_dir = save_to.ends_with('/') || path.is_dir();
    if looks_like_dir || name.is_some() {
        return Ok((path, name));
    }
    let file_name = path
        .file_name()
        .map(|name| sanitize(&name.to_string_lossy()))
        .filter(|name| !name.is_empty());
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    Ok((dir, file_name))
}

/// `$XDG_DOWNLOAD_DIR` (from the environment or `user-dirs.dirs`), else
/// `~/Downloads`.
fn downloads_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    let from_config = || {
        let config =
            std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
        let text = fs::read_to_string(config.join("user-dirs.dirs")).ok()?;
        text.lines()
            .filter_map(|line| line.trim().strip_prefix("XDG_DOWNLOAD_DIR="))
            .map(|value| {
                value
                    .trim_matches('"')
                    .replace("$HOME", &home.to_string_lossy())
            })
            .next()
    };
    Ok(std::env::var("XDG_DOWNLOAD_DIR")
        .ok()
        .or_else(from_config)
        .map_or_else(|| home.join("Downloads"), PathBuf::from))
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ if path == "~" => {
            std::env::var_os("HOME").map_or_else(|| PathBuf::from(path), PathBuf::from)
        }
        _ => PathBuf::from(path),
    }
}

/// A file name without path separators or control characters.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch == '/' || ch == '\\' || ch.is_control() {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    cleaned.chars().take(200).collect()
}

/// `dir/name`, or `dir/stem (n).ext` for the first `n` not taken yet.
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|path| !path.exists())
        .unwrap_or(candidate)
}

fn download(url: &str, dir: &Path, chosen: Option<String>) -> Result<Value, String> {
    let client = Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) claw")
        .build()
        .map_err(|error| error.to_string())?;
    let mut response = client.get(url).send().map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {} for {url}", status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BYTES)
    {
        return Err("larger than 4 GB".to_string());
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let name = chosen
        .or_else(|| {
            response
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|value| value.to_str().ok())
                .and_then(disposition_name)
        })
        .or_else(|| url_name(response.url().path()))
        .unwrap_or_else(|| "download".to_string());
    let name = with_extension(&name, &content_type);
    let path = free_path(dir, &name);
    let partial = path.with_file_name(format!(
        "{}.part",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let written = (|| -> io::Result<u64> {
        let mut file = File::create(&partial)?;
        let written = io::copy(&mut (&mut response).take(MAX_BYTES + 1), &mut file)?;
        file.flush()?;
        Ok(written)
    })();
    let written = match written {
        Ok(written) if written > MAX_BYTES => {
            let _ = fs::remove_file(&partial);
            return Err("larger than 4 GB".to_string());
        }
        Ok(written) => written,
        Err(error) => {
            let _ = fs::remove_file(&partial);
            return Err(format!("download failed: {error}"));
        }
    };
    fs::rename(&partial, &path).map_err(|error| error.to_string())?;
    Ok(json!({
        "path": path.display().to_string(),
        "bytes": written,
        "contentType": content_type,
        "url": response.url().to_string(),
    }))
}

fn copy_local(source: &str, dir: &Path, chosen: Option<String>) -> Result<Value, String> {
    let source = expand_home(source.strip_prefix("file://").unwrap_or(source));
    let metadata =
        fs::metadata(&source).map_err(|error| format!("{}: {error}", source.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file", source.display()));
    }
    let name = chosen
        .or_else(|| {
            source
                .file_name()
                .map(|name| sanitize(&name.to_string_lossy()))
        })
        .unwrap_or_else(|| "download".to_string());
    let path = free_path(dir, &name);
    let bytes = fs::copy(&source, &path).map_err(|error| error.to_string())?;
    Ok(json!({
        "path": path.display().to_string(),
        "bytes": bytes,
        "copiedFrom": source.display().to_string(),
    }))
}

/// The file name in a `Content-Disposition` header.
fn disposition_name(header: &str) -> Option<String> {
    let mut plain = None;
    for part in header.split(';').map(str::trim) {
        if let Some(value) = part.strip_prefix("filename*=") {
            // RFC 5987: charset''percent-encoded
            let encoded = value.split("''").nth(1).unwrap_or(value);
            let name = sanitize(&percent_decode(encoded.trim_matches('"')));
            if !name.is_empty() {
                return Some(name);
            }
        } else if let Some(value) = part.strip_prefix("filename=") {
            plain = Some(sanitize(value.trim_matches('"'))).filter(|name| !name.is_empty());
        }
    }
    plain
}

/// The last segment of a URL path, decoded.
fn url_name(path: &str) -> Option<String> {
    let segment = path.rsplit('/').find(|segment| !segment.is_empty())?;
    Some(sanitize(&percent_decode(segment))).filter(|name| !name.is_empty())
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or_default();
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `name` with an extension for `content_type` when it has none.
fn with_extension(name: &str, content_type: &str) -> String {
    if name.contains('.') {
        return name.to_string();
    }
    let ext = match content_type.split(';').next().unwrap_or_default().trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/json" => "json",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/csv" => "csv",
        _ => return name.to_string(),
    };
    format!("{name}.{ext}")
}

#[cfg(test)]
mod tests {
    use super::{disposition_name, free_path, run, url_name, with_extension, DownloadInput};

    #[test]
    fn names_come_from_the_header_or_the_url() {
        assert_eq!(
            disposition_name(r#"attachment; filename="report.pdf""#).as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            disposition_name("attachment; filename=a.txt; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf")
                .as_deref(),
            Some("résumé.pdf")
        );
        assert_eq!(url_name("/a/b/My%20Pic.png").as_deref(), Some("My Pic.png"));
        assert_eq!(url_name("/").as_deref(), None);
        assert_eq!(url_name("/../..%2Fetc").as_deref(), Some("_etc"));
        assert_eq!(with_extension("photo", "image/jpeg; q=1"), "photo.jpg");
        assert_eq!(with_extension("a.bin", "image/png"), "a.bin");
    }

    #[test]
    fn local_files_are_copied_without_overwriting() {
        let dir = std::env::temp_dir().join(format!("claw-download-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("src.txt");
        std::fs::write(&source, "hi").unwrap();
        let out = dir.join("out");
        for expected in ["src.txt", "src (1).txt"] {
            let result = run(DownloadInput {
                url: source.display().to_string(),
                save_to: Some(format!("{}/", out.display())),
                name: None,
            })
            .unwrap();
            assert_eq!(result["path"], out.join(expected).display().to_string());
        }
        assert_eq!(free_path(&out, "src.txt"), out.join("src (2).txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
