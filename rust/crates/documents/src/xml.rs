//! Small helpers over `quick-xml` and `zip` shared by the OOXML readers and
//! writers.

use std::io::{Read, Seek, Write};

use quick_xml::events::{BytesRef, BytesStart};
use zip::write::SimpleFileOptions;

use crate::Result;

/// Value of the attribute with this qualified name (e.g. `w:val`, `r:id`).
pub(crate) fn attr(element: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .map(|a| {
            let raw = String::from_utf8_lossy(&a.value);
            quick_xml::escape::unescape(&raw)
                .map_or_else(|_| raw.to_string(), std::borrow::Cow::into_owned)
        })
}

/// Text of an entity reference such as `&amp;` or `&#8211;`.
pub(crate) fn entity_text(reference: &BytesRef<'_>) -> String {
    if let Ok(Some(c)) = reference.resolve_char_ref() {
        return c.to_string();
    }
    let name = reference.decode().unwrap_or_default();
    quick_xml::escape::unescape(&format!("&{name};"))
        .map_or_else(|_| String::new(), std::borrow::Cow::into_owned)
}

/// Escapes text for element content and attribute values.
pub(crate) fn escape(text: &str) -> String {
    quick_xml::escape::escape(text).into_owned()
}

/// Reads one archive entry as UTF-8, or `None` if it does not exist.
pub(crate) fn read_entry<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Option<String>> {
    let mut file = match archive.by_name(name) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(format!("cannot read {name}: {error}")),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| format!("cannot read {name}: {error}"))?;
    Ok(Some(text))
}

/// Writes a set of package parts as a zip archive.
pub(crate) fn write_package<W: Write + Seek>(writer: W, parts: &[(String, Vec<u8>)]) -> Result<()> {
    let mut zip = zip::ZipWriter::new(writer);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in parts {
        zip.start_file(name.as_str(), options)
            .map_err(|e| e.to_string())?;
        zip.write_all(data).map_err(|e| e.to_string())?;
    }
    zip.finish()
        .map_err(|e| e.to_string())?
        .flush()
        .map_err(|e| e.to_string())
}

/// Writes `parts` to `path` atomically (temp file in the same directory,
/// then rename), so a failed write never leaves a truncated document.
pub(crate) fn write_package_file(
    path: &std::path::Path,
    parts: &[(String, Vec<u8>)],
) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp{}",
        path.extension().and_then(|e| e.to_str()).unwrap_or("zip"),
        std::process::id()
    ));
    let result = std::fs::File::create(&tmp)
        .map_err(|e| e.to_string())
        .and_then(|file| write_package(std::io::BufWriter::new(file), parts))
        .and_then(|()| std::fs::rename(&tmp, path).map_err(|e| e.to_string()));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|error| format!("cannot write {}: {error}", path.display()))
}
