//! Builds a 16:9 `.pptx` from scratch: an optional title slide, then content
//! slides with a title, bullets, a table and/or an image.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::xml::{escape, write_package_file};
use crate::Result;

/// EMUs per inch.
const EMU: f64 = 914_400.0;
/// 13.333 x 7.5 in.
const SLIDE_W: i64 = 12_192_000;
const SLIDE_H: i64 = 6_858_000;

const NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;
const REL_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const REL_TYPE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// One content slide.
#[derive(Debug, Clone, Default)]
pub struct SlideSpec {
    pub title: Option<String>,
    /// Bullet lines; two leading spaces per sub-level, `**bold**` allowed.
    pub bullets: Vec<String>,
    pub table_headers: Vec<String>,
    pub table_rows: Vec<Vec<String>>,
    /// PNG, JPEG or GIF file shown on the slide.
    pub image: Option<PathBuf>,
}

#[allow(clippy::cast_possible_truncation)]
fn inches(value: f64) -> i64 {
    (value * EMU) as i64
}

struct Image {
    bytes: Vec<u8>,
    extension: &'static str,
    size: Option<(i64, i64)>,
}

fn load_image(path: &Path) -> Result<Image> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("Image not found: {} ({e})", path.display()))?;
    let extension = match bytes.as_slice() {
        [0x89, b'P', b'N', b'G', ..] => "png",
        [0xFF, 0xD8, ..] => "jpeg",
        [b'G', b'I', b'F', ..] => "gif",
        _ => {
            return Err(format!(
                "Unsupported image type: {} (use PNG, JPEG or GIF)",
                path.display()
            ))
        }
    };
    let size = image_size(&bytes);
    Ok(Image {
        bytes,
        extension,
        size,
    })
}

/// Pixel size from PNG/GIF/JPEG headers, to keep the aspect ratio.
fn image_size(b: &[u8]) -> Option<(i64, i64)> {
    let be16 = |i: usize| i64::from(u16::from_be_bytes([b[i], b[i + 1]]));
    if b.len() > 24 && b[0] == 0x89 {
        let be32 = |i: usize| i64::from(u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]));
        return Some((be32(16), be32(20)));
    }
    if b.len() > 10 && b[0] == b'G' {
        let le16 = |i: usize| i64::from(u16::from_le_bytes([b[i], b[i + 1]]));
        return Some((le16(6), le16(8)));
    }
    if b.len() > 4 && b[0] == 0xFF && b[1] == 0xD8 {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                return Some((be16(i + 7), be16(i + 5)));
            }
            i += 2 + len;
        }
    }
    None
}

/// Writes the deck and returns the number of slides.
pub fn write_pptx(
    path: &Path,
    title: Option<&str>,
    subtitle: Option<&str>,
    slides: &[SlideSpec],
) -> Result<usize> {
    let title = title.map(str::trim).filter(|t| !t.is_empty());
    if slides.is_empty() && title.is_none() {
        return Err("No slides given.".to_string());
    }
    // Load every image first so a bad path fails before anything is written.
    let images = slides
        .iter()
        .map(|s| s.image.as_deref().map(load_image).transpose())
        .collect::<Result<Vec<_>>>()?;

    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    let mut slide_xml = Vec::new();
    if let Some(title) = title {
        slide_xml.push((title_slide(title, subtitle), None));
    }
    for (spec, image) in slides.iter().zip(images) {
        let xml = content_slide(spec, image.as_ref());
        slide_xml.push((xml, image));
    }

    let mut content_types = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Default Extension="jpeg" ContentType="image/jpeg"/><Default Extension="gif" ContentType="image/gif"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/><Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>"#,
    );
    let mut presentation_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{REL_NS}"><Relationship Id="rId1" Type="{REL_TYPE}/slideMaster" Target="slideMasters/slideMaster1.xml"/><Relationship Id="rId2" Type="{REL_TYPE}/theme" Target="theme/theme1.xml"/>"#
    );
    let mut slide_ids = String::new();
    let count = slide_xml.len();
    for (index, (xml, image)) in slide_xml.into_iter().enumerate() {
        let n = index + 1;
        let _ = write!(
            content_types,
            r#"<Override PartName="/ppt/slides/slide{n}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
        );
        let _ = write!(
            presentation_rels,
            r#"<Relationship Id="rId{}" Type="{REL_TYPE}/slide" Target="slides/slide{n}.xml"/>"#,
            10 + n
        );
        let _ = write!(
            slide_ids,
            r#"<p:sldId id="{}" r:id="rId{}"/>"#,
            255 + n,
            10 + n
        );

        let mut rels = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{REL_NS}"><Relationship Id="rId1" Type="{REL_TYPE}/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>"#
        );
        if let Some(image) = image {
            let media = format!("image{n}.{}", image.extension);
            let _ = write!(
                rels,
                r#"<Relationship Id="rId2" Type="{REL_TYPE}/image" Target="../media/{media}"/>"#
            );
            parts.push((format!("ppt/media/{media}"), image.bytes));
        }
        rels.push_str("</Relationships>");
        parts.push((format!("ppt/slides/slide{n}.xml"), xml.into_bytes()));
        parts.push((
            format!("ppt/slides/_rels/slide{n}.xml.rels"),
            rels.into_bytes(),
        ));
    }
    content_types.push_str("</Types>");
    presentation_rels.push_str("</Relationships>");

    let presentation = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:presentation {NS} saveSubsetFonts="1"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst>{slide_ids}</p:sldIdLst><p:sldSz cx="{SLIDE_W}" cy="{SLIDE_H}"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#
    );
    let package_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{REL_NS}"><Relationship Id="rId1" Type="{REL_TYPE}/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#
    );
    let master_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{REL_NS}"><Relationship Id="rId1" Type="{REL_TYPE}/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="{REL_TYPE}/theme" Target="../theme/theme1.xml"/></Relationships>"#
    );
    let layout_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{REL_NS}"><Relationship Id="rId1" Type="{REL_TYPE}/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#
    );

    let mut all = vec![
        (
            "[Content_Types].xml".to_string(),
            content_types.into_bytes(),
        ),
        ("_rels/.rels".to_string(), package_rels.into_bytes()),
        (
            "ppt/presentation.xml".to_string(),
            presentation.into_bytes(),
        ),
        (
            "ppt/_rels/presentation.xml.rels".to_string(),
            presentation_rels.into_bytes(),
        ),
        (
            "ppt/slideMasters/slideMaster1.xml".to_string(),
            master_xml().into_bytes(),
        ),
        (
            "ppt/slideMasters/_rels/slideMaster1.xml.rels".to_string(),
            master_rels.into_bytes(),
        ),
        (
            "ppt/slideLayouts/slideLayout1.xml".to_string(),
            layout_xml().into_bytes(),
        ),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels".to_string(),
            layout_rels.into_bytes(),
        ),
        (
            "ppt/theme/theme1.xml".to_string(),
            THEME_XML.as_bytes().to_vec(),
        ),
    ];
    all.extend(parts);
    write_package_file(path, &all)?;
    Ok(count)
}

fn title_slide(title: &str, subtitle: Option<&str>) -> String {
    let width = SLIDE_W - inches(1.8);
    let mut shapes = text_box(
        2,
        "Title",
        (inches(0.9), inches(2.3), width, inches(1.6)),
        &paragraph(
            title,
            &TextStyle {
                size: 4400,
                bold: true,
                align: Some("ctr"),
                color: Some("1F3864"),
                bullet_level: None,
            },
        ),
        "b",
        false,
    );
    if let Some(subtitle) = subtitle.map(str::trim).filter(|s| !s.is_empty()) {
        shapes.push_str(&text_box(
            3,
            "Subtitle",
            (inches(0.9), inches(4.0), width, inches(1.0)),
            &paragraph(
                subtitle,
                &TextStyle {
                    size: 2400,
                    bold: false,
                    align: Some("ctr"),
                    color: Some("595959"),
                    bullet_level: None,
                },
            ),
            "t",
            false,
        ));
    }
    slide(&shapes)
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn content_slide(spec: &SlideSpec, image: Option<&Image>) -> String {
    let mut shapes = String::new();
    let mut id = 2;
    let (top, left) = (inches(0.35), inches(0.6));
    let width = SLIDE_W - 2 * left;
    if let Some(title) = spec
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        shapes.push_str(&text_box(
            id,
            "Title",
            (left, top, width, inches(1.0)),
            &paragraph(
                title,
                &TextStyle {
                    size: 3200,
                    bold: true,
                    align: None,
                    color: Some("1F3864"),
                    bullet_level: None,
                },
            ),
            "ctr",
            false,
        ));
        id += 1;
    }
    let mut body_top = inches(1.55);
    let mut body_h = SLIDE_H - body_top - inches(0.5);
    let has_table = !spec.table_headers.is_empty() || !spec.table_rows.is_empty();
    let has_bullets = !spec.bullets.is_empty();
    let text_w = if (has_table || image.is_some()) && has_bullets {
        width / 2 - inches(0.15)
    } else {
        width
    };

    if has_bullets {
        let paragraphs: String = spec
            .bullets
            .iter()
            .map(|bullet| {
                let indent = bullet.len() - bullet.trim_start().len();
                let level = (indent / 2).min(2);
                let text = bullet.trim();
                let text = ["- ", "* ", "• "]
                    .iter()
                    .find_map(|p| text.strip_prefix(p))
                    .unwrap_or(text);
                let size = if level == 0 { 2000 } else { 1800 };
                paragraph(
                    text,
                    &TextStyle {
                        size,
                        bold: false,
                        align: None,
                        color: None,
                        bullet_level: Some(level),
                    },
                )
            })
            .collect();
        shapes.push_str(&text_box(
            id,
            "Content",
            (left, body_top, text_w, body_h),
            &paragraphs,
            "t",
            true,
        ));
        id += 1;
    }

    let (right_left, right_w) = if has_bullets {
        (left + text_w + inches(0.3), width - text_w - inches(0.3))
    } else {
        (left, width)
    };
    if has_table {
        let table_h = if image.is_some() {
            body_h / 2 - inches(0.1)
        } else {
            body_h
        };
        shapes.push_str(&table(
            id,
            (right_left, body_top, right_w, table_h),
            &spec.table_headers,
            &spec.table_rows,
        ));
        id += 1;
        if image.is_some() {
            body_top += table_h + inches(0.2);
            body_h -= table_h + inches(0.2);
        }
    }
    if let Some(image) = image {
        let (iw, ih) = image
            .size
            .filter(|&(w, h)| w > 0 && h > 0)
            .unwrap_or((4, 3));
        let scale = (right_w as f64 / iw as f64).min(body_h as f64 / ih as f64);
        let (w, h) = ((iw as f64 * scale) as i64, (ih as f64 * scale) as i64);
        let _ = write!(
            shapes,
            r#"<p:pic><p:nvPicPr><p:cNvPr id="{id}" name="Picture"/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x="{}" y="{}"/><a:ext cx="{w}" cy="{h}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>"#,
            right_left + (right_w - w) / 2,
            body_top + (body_h - h) / 2
        );
    }
    slide(&shapes)
}

fn slide(shapes: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sld {NS}><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#
    )
}

fn text_box(
    id: usize,
    name: &str,
    (x, y, w, h): (i64, i64, i64, i64),
    paragraphs: &str,
    anchor: &str,
    autofit: bool,
) -> String {
    let autofit = if autofit { "<a:normAutofit/>" } else { "" };
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{name}"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{w}" cy="{h}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/></p:spPr><p:txBody><a:bodyPr wrap="square" rtlCol="0" anchor="{anchor}">{autofit}</a:bodyPr><a:lstStyle/>{paragraphs}</p:txBody></p:sp>"#
    )
}

struct TextStyle {
    size: u32,
    bold: bool,
    align: Option<&'static str>,
    color: Option<&'static str>,
    bullet_level: Option<usize>,
}

fn paragraph(text: &str, style: &TextStyle) -> String {
    let properties = match (style.bullet_level, style.align) {
        (Some(level), _) => {
            let margin = 342_900 * (level + 1);
            let bullet = if level == 0 { "•" } else { "–" };
            format!(
                r#"<a:pPr marL="{margin}" lvl="{level}" indent="-342900"><a:spcBef><a:spcPts val="600"/></a:spcBef><a:buFont typeface="Arial"/><a:buChar char="{bullet}"/></a:pPr>"#
            )
        }
        (None, Some(align)) => format!(r#"<a:pPr algn="{align}"/>"#),
        (None, None) => String::new(),
    };
    let color = style.color.map_or_else(String::new, |c| {
        format!(r#"<a:solidFill><a:srgbClr val="{c}"/></a:solidFill>"#)
    });
    // `**bold**` spans inside the text.
    let mut runs = String::new();
    for (index, part) in text.split("**").enumerate() {
        if part.is_empty() {
            continue;
        }
        let bold = style.bold || index % 2 == 1;
        let _ = write!(
            runs,
            r#"<a:r><a:rPr lang="en-US" sz="{}"{} dirty="0">{color}</a:rPr><a:t>{}</a:t></a:r>"#,
            style.size,
            if bold { r#" b="1""# } else { "" },
            escape(part)
        );
    }
    format!("<a:p>{properties}{runs}</a:p>")
}

fn table(
    id: usize,
    (x, y, w, h): (i64, i64, i64, i64),
    headers: &[String],
    rows: &[Vec<String>],
) -> String {
    let mut all: Vec<&[String]> = Vec::new();
    if !headers.is_empty() {
        all.push(headers);
    }
    all.extend(rows.iter().map(Vec::as_slice));
    let cols = all.iter().map(|r| r.len()).max().unwrap_or(1).max(1);
    let n_rows = i64::try_from(all.len().max(1)).unwrap_or(1);
    let n_cols = i64::try_from(cols).unwrap_or(1);
    let col_w = w / n_cols;
    let row_h = inches(0.45).min(h / n_rows);
    let mut out = format!(
        r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="{id}" name="Table"/><p:cNvGraphicFramePr><a:graphicFrameLocks noGrp="1"/></p:cNvGraphicFramePr><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{}" cy="{}"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table"><a:tbl><a:tblPr firstRow="1" bandRow="1"/><a:tblGrid>"#,
        col_w * n_cols,
        row_h * n_rows
    );
    for _ in 0..cols {
        let _ = write!(out, r#"<a:gridCol w="{col_w}"/>"#);
    }
    out.push_str("</a:tblGrid>");
    let line = r#"<a:solidFill><a:srgbClr val="BFBFBF"/></a:solidFill>"#;
    for (r, row) in all.iter().enumerate() {
        let header = r == 0 && !headers.is_empty();
        let _ = write!(out, r#"<a:tr h="{row_h}">"#);
        for c in 0..cols {
            let text = row.get(c).map_or("", String::as_str);
            let fill = if header {
                "D9E2F3"
            } else if r % 2 == 0 {
                "F2F2F2"
            } else {
                "FFFFFF"
            };
            let _ = write!(
                out,
                r#"<a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" sz="1400"{} dirty="0"/><a:t>{}</a:t></a:r></a:p></a:txBody><a:tcPr><a:lnL w="9525">{line}</a:lnL><a:lnR w="9525">{line}</a:lnR><a:lnT w="9525">{line}</a:lnT><a:lnB w="9525">{line}</a:lnB><a:solidFill><a:srgbClr val="{fill}"/></a:solidFill></a:tcPr></a:tc>"#,
                if header { r#" b="1""# } else { "" },
                escape(text)
            );
        }
        out.push_str("</a:tr>");
    }
    out.push_str("</a:tbl></a:graphicData></a:graphic></p:graphicFrame>");
    out
}

fn master_xml() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sldMaster {NS}><p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>"#
    )
}

fn layout_xml() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sldLayout {NS} type="blank" preserve="1"><p:cSld name="Blank"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#
    )
}

const THEME_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="claw">
<a:themeElements>
<a:clrScheme name="claw">
<a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
<a:dk2><a:srgbClr val="1F3864"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
<a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
<a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4>
<a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6>
<a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
</a:clrScheme>
<a:fontScheme name="claw">
<a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont>
<a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont>
</a:fontScheme>
<a:fmtScheme name="claw">
<a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst>
<a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst>
<a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst>
<a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst>
</a:fmtScheme>
</a:themeElements>
</a:theme>"#;

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_PNG: [u8; 33] = [
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0,
        0, 40, 0, 0, 0, 20, 8, 6, 0, 0, 0, 0, 0, 0, 0,
    ];

    #[test]
    fn reads_image_sizes() {
        assert_eq!(image_size(&TINY_PNG), Some((40, 20)));
        let gif = [b'G', b'I', b'F', b'8', b'9', b'a', 7, 0, 3, 0, 0];
        assert_eq!(image_size(&gif), Some((7, 3)));
        assert_eq!(image_size(b"nope"), None);
    }

    #[test]
    fn writes_a_deck_that_reads_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("pic.png");
        std::fs::write(&image, TINY_PNG).unwrap();
        let path = dir.path().join("deck.pptx");
        let slides = vec![
            SlideSpec {
                title: Some("Plan & goals".into()),
                bullets: vec!["first **key** point".into(), "  nested".into()],
                image: Some(image),
                ..SlideSpec::default()
            },
            SlideSpec {
                title: Some("Numbers".into()),
                table_headers: vec!["A".into(), "B".into()],
                table_rows: vec![vec!["1".into(), "2".into()]],
                ..SlideSpec::default()
            },
        ];
        let count = write_pptx(&path, Some("Deck"), Some("Sub"), &slides).unwrap();
        assert_eq!(count, 3);
        let text = crate::read_document(&path, &crate::ReadOptions::default())
            .unwrap()
            .unwrap();
        assert_eq!(
            text,
            "## Slide 1\nDeck\nSub\n\n## Slide 2\nPlan & goals\nfirst key point\nnested\n\n## Slide 3\nNumbers\nA\nB\n1\n2\n\n"
        );
    }

    #[test]
    fn rejects_empty_decks_and_missing_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.pptx");
        assert!(write_pptx(&path, None, None, &[]).is_err());
        let missing = SlideSpec {
            image: Some(dir.path().join("none.png")),
            ..SlideSpec::default()
        };
        assert!(write_pptx(&path, None, None, &[missing])
            .unwrap_err()
            .contains("Image not found"));
        assert!(!path.exists());
    }
}
