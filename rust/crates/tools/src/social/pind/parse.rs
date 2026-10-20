//! Extracts the pin id from a Pinterest URL, and the downloadable media
//! URL(s) from that pin's `PinResource` JSON.
//!
//! Unlike `ore::fbd`, there's no regex scraping here: the pin page's own
//! server-rendered HTML carries only a cover image (`og:image`/ld+json), but
//! the same unauthenticated `PinResource` endpoint the web client itself
//! calls returns the full pin as one clean JSON object, so this module just
//! walks that with `serde_json::Value`. Every field read is optional and
//! shape-checked — a Pinterest-side schema change yields a clean `Err`, not
//! a panic.
//!
//! Scope: single-image pins, carousel pins (every slide), and video pins.
//! A video is the highest-resolution progressive `.mp4` when Pinterest
//! offers one; otherwise (verified live: story pins carry *only* an HLS
//! `.m3u8`, no `.mp4` at all) it's the HLS master playlist, flagged
//! `hls: true` for `super::hls` to turn into one `.mp4`.
//!
//! Story pins are read page by page, block by block, in order: a video
//! block yields its stream, an image block (`story_pin_image_block` —
//! photos *and* GIFs, verified live on a real photo story pin) yields its
//! `image.images.originals` URL. A page with no media blocks falls back to
//! that page's own `video`/`image`. The pin's top-level `images` is never
//! used for a story pin — it's only a cover frame, and silently saving it
//! would look like success while dropping the actual content; a story pin
//! with no readable media at all is an error instead.
//!
//! GIF pins: a plain GIF pin carries `embed: {"type":"gif","src":...}`
//! alongside an `images.orig` `.gif` (verified live in search results);
//! the `embed` source is preferred. GIFs come back as `MediaType::Image`
//! with a `gif` extension — `Herald::handle_pind` sends them as Telegram
//! animations based on the *sniffed* bytes, not this module's say-so.

use std::collections::HashSet;

use serde_json::Value;

use super::types::{MediaType, MAX_ITEMS};

#[derive(Debug)]
pub struct MediaRef {
    pub url: String,
    pub media_type: MediaType,
    /// `url` is an HLS master playlist (`.m3u8`), not a file — see
    /// `super::hls`.
    pub hls: bool,
}

/// A pin id is a decimal integer; 25 digits is comfortably above any real
/// one (they're ~18) and keeps the value harmless wherever it's embedded.
const MAX_PIN_ID_DIGITS: usize = 25;

fn valid_pin_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_PIN_ID_DIGITS && s.bytes().all(|b| b.is_ascii_digit())
}

/// Pulls the pin id out of a (post-redirect) Pinterest URL. Handles
/// `/pin/<id>/`, `/pin/<slug>--<id>/`, and the `/pin/<id>/sent/?...` shape a
/// `pin.it` link's redirect chain was observed to land on.
pub fn extract_pin_id(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let mut segments = url.path_segments()?;
    segments.find(|s| *s == "pin")?;
    let seg = segments.next()?;
    let candidate = seg.rsplit('-').next().unwrap_or(seg);
    valid_pin_id(candidate).then(|| candidate.to_string())
}

/// Maps a media URL's path suffix to a fixed extension, ignoring any query.
/// `None` for anything unrecognized (the caller falls back to the media
/// type's default). Output is always a `&'static str`.
pub fn extension_for(url: &str) -> Option<&'static str> {
    let path = reqwest::Url::parse(url).ok()?.path().to_ascii_lowercase();
    let ext = path.rsplit_once('.')?.1;
    match ext {
        "jpg" | "jpeg" => Some("jpg"),
        "png" => Some("png"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        "mp4" => Some("mp4"),
        _ => None,
    }
}

fn is_non_empty(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

fn orig_image_url(images: &Value) -> Option<String> {
    images.get("orig")?.get("url")?.as_str().map(str::to_string)
}

/// Picks the best stream out of a `video_list` map (`V_720P`, `V_EXP7`,
/// `V_HLSV4`, ... → `{url, width, height}`): the largest progressive `.mp4`
/// if any, else the largest HLS `.m3u8` (flagged `true`).
fn best_video(video_list: &Value) -> Option<(String, bool)> {
    let mut best_mp4: Option<(u64, String)> = None;
    let mut best_hls: Option<(u64, String)> = None;
    for v in video_list.as_object()?.values() {
        let Some(url) = v.get("url").and_then(Value::as_str) else {
            continue;
        };
        let Some(path) = reqwest::Url::parse(url)
            .ok()
            .map(|u| u.path().to_ascii_lowercase())
        else {
            continue;
        };
        let area = v.get("width").and_then(Value::as_u64).unwrap_or(0)
            * v.get("height").and_then(Value::as_u64).unwrap_or(0);
        let slot = if path.ends_with(".mp4") {
            &mut best_mp4
        } else if path.ends_with(".m3u8") {
            &mut best_hls
        } else {
            continue;
        };
        if slot
            .as_ref()
            .is_none_or(|(a, u)| (area, url) > (*a, u.as_str()))
        {
            *slot = Some((area, url.to_string()));
        }
    }
    best_mp4
        .map(|(_, u)| (u, false))
        .or_else(|| best_hls.map(|(_, u)| (u, true)))
}

/// A story image's best URL: `images.originals` (the full-size upload),
/// else the largest-area rendition offered.
fn story_image_url(image: &Value) -> Option<String> {
    let images = image.get("images")?;
    if let Some(url) = images.pointer("/originals/url").and_then(Value::as_str) {
        return Some(url.to_string());
    }
    images
        .as_object()?
        .values()
        .filter_map(|v| {
            let url = v.get("url")?.as_str()?;
            let area = v.get("width").and_then(Value::as_u64).unwrap_or(0)
                * v.get("height").and_then(Value::as_u64).unwrap_or(0);
            Some((area, url))
        })
        .max()
        .map(|(_, url)| url.to_string())
}

/// One story-pin node's media — a block, or a page used as its own
/// fallback. Video wins over image, since a video block/page's `image` is
/// just its poster frame.
fn story_node_ref(node: &Value) -> Option<MediaRef> {
    if let Some((url, hls)) = node.pointer("/video/video_list").and_then(best_video) {
        return Some(MediaRef {
            url,
            media_type: MediaType::Video,
            hls,
        });
    }
    let url = node.get("image").and_then(story_image_url)?;
    Some(MediaRef {
        url,
        media_type: MediaType::Image,
        hls: false,
    })
}

/// Every media item of a story pin, in page/block order — see this
/// module's doc comment.
fn story_refs(story: &Value) -> Vec<MediaRef> {
    let mut refs = Vec::new();
    let Some(pages) = story.get("pages").and_then(Value::as_array) else {
        return refs;
    };
    for page in pages {
        let blocks = page
            .get("blocks")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let page_refs: Vec<MediaRef> = blocks.iter().filter_map(story_node_ref).collect();
        if page_refs.is_empty() {
            refs.extend(story_node_ref(page));
        } else {
            refs.extend(page_refs);
        }
    }
    refs
}

/// A plain GIF pin's `embed.src`, when `embed.type` says it's a GIF.
fn gif_embed_url(pin: &Value) -> Option<String> {
    let embed = pin.get("embed")?;
    if embed.get("type").and_then(Value::as_str) != Some("gif") {
        return None;
    }
    embed.get("src")?.as_str().map(str::to_string)
}

pub fn extract_media_refs(api_json: &str) -> Result<Vec<MediaRef>, String> {
    let root: Value = serde_json::from_str(api_json)
        .map_err(|_| "unexpected response from Pinterest".to_string())?;
    let resp = root
        .get("resource_response")
        .ok_or_else(|| "unexpected response from Pinterest".to_string())?;
    if resp.get("status").and_then(Value::as_str) != Some("success") {
        return Err("Pinterest has no public pin for that link".to_string());
    }
    let pin = resp
        .get("data")
        .filter(|d| d.is_object())
        .ok_or_else(|| "Pinterest returned no pin data".to_string())?;

    let mut refs: Vec<MediaRef> = Vec::new();

    if let Some(story) = pin.get("story_pin_data").filter(|s| is_non_empty(s)) {
        refs = story_refs(story);
        if refs.is_empty() {
            return Err("that story pin has no downloadable photo, GIF, or video".to_string());
        }
    }

    if let Some(slots) = pin
        .pointer("/carousel_data/carousel_slots")
        .and_then(Value::as_array)
    {
        for slot in slots {
            if let Some(url) = slot.get("images").and_then(orig_image_url) {
                refs.push(MediaRef {
                    url,
                    media_type: MediaType::Image,
                    hls: false,
                });
            }
        }
    }

    if refs.is_empty() {
        if let Some(url) = gif_embed_url(pin) {
            refs.push(MediaRef {
                url,
                media_type: MediaType::Image,
                hls: false,
            });
        }
    }

    if refs.is_empty() {
        if let Some(video_list) = pin.pointer("/videos/video_list") {
            let (url, hls) = best_video(video_list)
                .ok_or_else(|| "that video pin has no downloadable stream".to_string())?;
            refs.push(MediaRef {
                url,
                media_type: MediaType::Video,
                hls,
            });
        } else if pin.get("is_video").and_then(Value::as_bool) == Some(true) {
            return Err("that video pin has no downloadable stream".to_string());
        }
    }

    if refs.is_empty() {
        if let Some(url) = pin.get("images").and_then(orig_image_url) {
            refs.push(MediaRef {
                url,
                media_type: MediaType::Image,
                hls: false,
            });
        }
    }

    let mut seen = HashSet::new();
    refs.retain(|r| seen.insert(r.url.clone()));
    refs.truncate(MAX_ITEMS);

    if refs.is_empty() {
        return Err("no downloadable media found in that pin".to_string());
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(data: &str) -> String {
        format!(r#"{{"resource_response":{{"status":"success","data":{data}}}}}"#)
    }

    #[test]
    fn pin_id_from_canonical_slug_and_shortlink_landing_urls() {
        assert_eq!(
            extract_pin_id("https://www.pinterest.com/pin/99360735500167749/"),
            Some("99360735500167749".into())
        );
        assert_eq!(
            extract_pin_id("https://www.pinterest.com/pin/cool-thing--99360735500167749/"),
            Some("99360735500167749".into())
        );
        assert_eq!(
            extract_pin_id(
                "https://www.pinterest.com/pin/3096293484910807/sent/?invite_code=abc&sender=1"
            ),
            Some("3096293484910807".into())
        );
    }

    #[test]
    fn pin_id_rejects_non_pin_and_non_numeric_urls() {
        assert_eq!(
            extract_pin_id("https://www.pinterest.com/someuser/board/"),
            None
        );
        assert_eq!(extract_pin_id("https://www.pinterest.com/pin/abc/"), None);
        assert_eq!(
            extract_pin_id("https://www.pinterest.com/pin/123%2F..%2F456/"),
            None
        );
        assert_eq!(
            extract_pin_id("https://www.pinterest.com/pin/12345678901234567890123456/"),
            None
        );
        assert_eq!(extract_pin_id("not a url"), None);
    }

    #[test]
    fn single_image_pin_uses_orig() {
        let j = wrap(r#"{"images":{"orig":{"url":"https://i.pinimg.com/originals/a7/66/x.jpg"}}}"#);
        let refs = extract_media_refs(&j).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://i.pinimg.com/originals/a7/66/x.jpg");
        assert_eq!(refs[0].media_type, MediaType::Image);
    }

    #[test]
    fn carousel_pin_yields_every_slide_deduped() {
        let j = wrap(
            r#"{"images":{"orig":{"url":"https://i.pinimg.com/originals/1.jpg"}},"carousel_data":{"carousel_slots":[
            {"images":{"orig":{"url":"https://i.pinimg.com/originals/1.jpg"}}},
            {"images":{"orig":{"url":"https://i.pinimg.com/originals/2.png"}}},
            {"images":{"orig":{"url":"https://i.pinimg.com/originals/2.png"}}}]}}"#,
        );
        let refs = extract_media_refs(&j).unwrap();
        assert_eq!(
            refs.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
            vec![
                "https://i.pinimg.com/originals/1.jpg",
                "https://i.pinimg.com/originals/2.png"
            ]
        );
    }

    #[test]
    fn video_pin_picks_largest_mp4_and_skips_hls() {
        let j = wrap(
            r#"{"is_video":true,"images":{"orig":{"url":"https://i.pinimg.com/originals/cover.jpg"}},"videos":{"video_list":{
            "V_HLSV4":{"url":"https://v1.pinimg.com/videos/x/hls/a.m3u8","width":1920,"height":1080},
            "V_480P":{"url":"https://v1.pinimg.com/videos/x/480p/a.mp4","width":480,"height":852},
            "V_720P":{"url":"https://v1.pinimg.com/videos/x/720p/a.mp4?t=1","width":720,"height":1280}}}}"#,
        );
        let refs = extract_media_refs(&j).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://v1.pinimg.com/videos/x/720p/a.mp4?t=1");
        assert_eq!(refs[0].media_type, MediaType::Video);
    }

    #[test]
    fn video_pin_with_only_hls_yields_the_playlist_not_the_cover_image() {
        let j = wrap(
            r#"{"is_video":true,"images":{"orig":{"url":"https://i.pinimg.com/originals/cover.jpg"}},"videos":{"video_list":{
            "V_HLSV4":{"url":"https://v1.pinimg.com/videos/x/hls/a.m3u8","width":1920,"height":1080}}}}"#,
        );
        let refs = extract_media_refs(&j).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://v1.pinimg.com/videos/x/hls/a.m3u8");
        assert!(refs[0].hls);
        assert_eq!(refs[0].media_type, MediaType::Video);
    }

    #[test]
    fn video_story_pin_yields_its_hls_playlist_once() {
        // Shape captured from a real video story pin: the same video_list
        // appears on the page and in its video block.
        let vl = r#"{"video_list":{"V_HLSV3_MOBILE":{"width":576,"height":1024,"url":"https://v1.pinimg.com/videos/iht/hls/7d/6b/69/x.m3u8"}}}"#;
        let j = wrap(&format!(
            r#"{{"story_pin_data":{{"pages":[{{"video":{vl},"blocks":[{{"block_type":3,"video":{vl}}}]}}]}},"images":{{"orig":{{"url":"https://i.pinimg.com/originals/c.jpg"}}}}}}"#
        ));
        let refs = extract_media_refs(&j).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].url,
            "https://v1.pinimg.com/videos/iht/hls/7d/6b/69/x.m3u8"
        );
        assert!(refs[0].hls);
    }

    #[test]
    fn story_pin_without_any_media_is_refused_not_saved_as_its_cover() {
        let j = wrap(
            r#"{"story_pin_data":{"pages":[{"blocks":[{"block_type":1}]}]},"images":{"orig":{"url":"https://i.pinimg.com/originals/c.jpg"}}}"#,
        );
        assert!(extract_media_refs(&j).unwrap_err().contains("story pin"));
    }

    fn image_block(original: &str) -> String {
        format!(
            r#"{{"type":"story_pin_image_block","block_type":2,"image":{{"images":{{"originals":{{"url":"{original}","width":736,"height":1111}},"236x":{{"url":"https://i.pinimg.com/236x/small.jpg","width":236,"height":356}}}}}}}}"#
        )
    }

    #[test]
    fn photo_story_pin_yields_every_image_block_in_order() {
        // Shape captured from a real photo story pin (page image duplicates
        // its one block's image).
        let j = wrap(&format!(
            r#"{{"story_pin_data":{{"pages":[
                {{"image":{{"images":{{"originals":{{"url":"https://i.pinimg.com/originals/da/1.jpg"}}}}}},"blocks":[{}]}},
                {{"blocks":[{},{}]}}]}},
              "images":{{"orig":{{"url":"https://i.pinimg.com/originals/cover.jpg"}}}}}}"#,
            image_block("https://i.pinimg.com/originals/da/1.jpg"),
            image_block("https://i.pinimg.com/originals/da/2.gif"),
            image_block("https://i.pinimg.com/originals/da/3.png"),
        ));
        let refs = extract_media_refs(&j).unwrap();
        assert_eq!(
            refs.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
            vec![
                "https://i.pinimg.com/originals/da/1.jpg",
                "https://i.pinimg.com/originals/da/2.gif",
                "https://i.pinimg.com/originals/da/3.png",
            ]
        );
        assert!(refs
            .iter()
            .all(|r| r.media_type == MediaType::Image && !r.hls));
        assert_eq!(extension_for(&refs[1].url), Some("gif"));
    }

    #[test]
    fn mixed_story_pin_keeps_video_and_image_blocks_in_order() {
        let vl = r#"{"video_list":{"V_HLSV3_MOBILE":{"width":576,"height":1024,"url":"https://v1.pinimg.com/videos/x.m3u8"}}}"#;
        let j = wrap(&format!(
            r#"{{"story_pin_data":{{"pages":[{{"blocks":[{{"block_type":3,"video":{vl},"image":{{"images":{{"originals":{{"url":"https://i.pinimg.com/originals/poster.jpg"}}}}}}}}]}},{{"blocks":[{}]}}]}}}}"#,
            image_block("https://i.pinimg.com/originals/p.jpg"),
        ));
        let refs = extract_media_refs(&j).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].media_type, MediaType::Video);
        assert_eq!(refs[1].url, "https://i.pinimg.com/originals/p.jpg");
    }

    #[test]
    fn story_page_without_media_blocks_falls_back_to_the_page_image() {
        let j = wrap(
            r#"{"story_pin_data":{"pages":[{"image":{"images":{"736x":{"url":"https://i.pinimg.com/736x/a.jpg","width":736,"height":1000},"236x":{"url":"https://i.pinimg.com/236x/a.jpg","width":236,"height":320}}},"blocks":[{"block_type":1}]}]}}"#,
        );
        let refs = extract_media_refs(&j).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://i.pinimg.com/736x/a.jpg");
    }

    #[test]
    fn gif_pin_prefers_the_gif_embed_over_a_video_rendition() {
        let j = wrap(
            r#"{"embed":{"type":"gif","src":"https://i.pinimg.com/originals/05/4c/a9/x.gif","width":"300","height":"208"},
               "images":{"orig":{"url":"https://i.pinimg.com/originals/05/4c/a9/x.gif"}},
               "videos":{"video_list":{"V_720P":{"url":"https://v1.pinimg.com/videos/x.mp4","width":300,"height":208}}}}"#,
        );
        let refs = extract_media_refs(&j).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://i.pinimg.com/originals/05/4c/a9/x.gif");
        assert_eq!(refs[0].media_type, MediaType::Image);
    }

    #[test]
    fn failure_status_and_garbage_are_errors() {
        assert!(
            extract_media_refs(r#"{"resource_response":{"status":"failure","data":null}}"#)
                .is_err()
        );
        assert!(extract_media_refs("<html>").is_err());
        assert!(extract_media_refs(&wrap("{}")).is_err());
    }

    #[test]
    fn extension_is_a_fixed_whitelist() {
        assert_eq!(
            extension_for("https://i.pinimg.com/originals/a.PNG?x=1"),
            Some("png")
        );
        assert_eq!(
            extension_for("https://i.pinimg.com/originals/a.jpeg"),
            Some("jpg")
        );
        assert_eq!(extension_for("https://i.pinimg.com/originals/a.php"), None);
        assert_eq!(extension_for("https://i.pinimg.com/originals/noext"), None);
    }
}
