//! Extracts the downloadable media URL(s) for one Facebook video/reel/photo
//! post out of its page's own embedded JSON.
//!
//! Unlike `ore::igd`'s GraphQL response or `ore::xrd`'s single
//! `__INITIAL_STATE__` object, Facebook's Comet page is a large tree of
//! separate Relay preloader records — a reel page in particular preloads a
//! whole vertical feed of "up next" reels alongside the one actually
//! requested, each with its own `browser_native_sd_url`/
//! `browser_native_hd_url` pair (verified empirically: a single real reel
//! page carried six such pairs, one per feed item), and a photo post's page
//! can likewise carry a neighboring/suggested post's own photo attachment
//! elsewhere in the same tree. Parsing the entire multi-megabyte tree as one
//! JSON document to find "the" post would be both fragile (it isn't
//! reliably valid JSON end-to-end) and wrong (there is no single post to
//! find — there are several, and picking the wrong one silently downloads
//! someone else's media instead of the one the caller asked for). So this
//! module does a targeted regex pull instead, scoped to the target post id
//! two different ways depending on media kind:
//! - **Video**: each video's own `dash_manifest_url` embeds that specific
//!   video's id in a `v=<id>` query parameter immediately alongside its
//!   `browser_native_sd_url`/`browser_native_hd_url` fields, so matching on
//!   that id is what picks the right one out of the several embedded in the
//!   page.
//! - **Photo**: a photo attachment (single photo or a multi-photo album —
//!   both go through the same "mediaset" renderer, verified empirically
//!   against a real 4-photo album post) is tagged with
//!   `"mediaset_token":"pcb.<post id>"` immediately before that post's own
//!   list of `image`/`viewer_image` URL pairs, one pair per photo. Slicing
//!   the page from that marker to the *next* `mediaset_token` marker of any
//!   id (or to the end of the page) bounds extraction to just this post's
//!   own photos, the same way the video id match keeps a neighboring reel's
//!   data out.
//! - **Single photo (non-album)**: a post with exactly one photo is *not*
//!   rendered through the mediaset renderer at all (verified live, Oct
//!   2026 — no `mediaset_token` anywhere on the page) but through
//!   `StoryAttachmentPhotoStyleRenderer`, whose photo node carries a
//!   `photo_image` URL plus a sibling `feedback` object whose `id` is the
//!   standard-base64 of `feedback:<post id>`. Matching that encoded id is
//!   the scoping key here — see `extract_single_photo_refs`. Note this
//!   shape's `viewer_image` has only dimensions, no `uri`, so the largest
//!   rendition the logged-out page exposes is `photo_image` (display-sized,
//!   not the original upload).

use std::collections::HashSet;

use regex::Regex;

use super::types::MediaType;

pub struct MediaRef {
    pub url: String,
    pub media_type: MediaType,
}

/// Pulls the target post's id out of the page's own final (post-redirect)
/// URL. Facebook has several live URL shapes for the same kind of content:
/// `/reel/<id>/`, `/<page>/videos/<id>/`, and `/<page>/posts/<id>/` (the
/// shape a photo/text/link post resolves to) carry the id as their last
/// path segment, while the older `/watch/?v=<id>` and `/video.php?v=<id>`
/// carry it as a `v` query parameter instead — so `v` is checked first,
/// then the last non-empty, all-digit path segment.
fn extract_target_post_id(final_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(final_url).ok()?;

    if let Some((_, v)) = url.query_pairs().find(|(k, _)| k == "v") {
        if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) {
            return Some(v.to_string());
        }
    }

    let last = url.path_segments()?.filter(|s| !s.is_empty()).next_back()?;
    if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
        Some(last.to_string())
    } else {
        None
    }
}

/// Reverses JSON string escaping (`\/`, `\"`, `\uXXXX`, ...) on a regex-
/// captured string body by wrapping it back in quotes and letting
/// `serde_json` do the actual unescaping, rather than hand-rolling it.
fn unescape_json_string(raw: &str) -> Option<String> {
    serde_json::from_str::<String>(&format!("\"{raw}\"")).ok()
}

/// Matches a `dash_manifest_url` (captures its embedded video id),
/// immediately followed by that same video's `browser_native_sd_url` and
/// `browser_native_hd_url` fields — the exact, consistently-adjacent field
/// order observed in a real page (see this module's doc comment). A
/// Facebook build that reorders these fields would make this stop
/// matching entirely; that's an honest failure (a clear "no video data
/// found" error to the caller), not silently guessing at a different shape.
fn video_pattern() -> Regex {
    Regex::new(
        r#""dash_manifest_url":"(?:[^"\\]|\\.)*v=(\d+)(?:[^"\\]|\\.)*","browser_native_sd_url":"((?:[^"\\]|\\.)*)","browser_native_hd_url":"((?:[^"\\]|\\.)*)""#,
    )
    .expect("valid regex")
}

/// Matches one photo attachment node's `image`/`viewer_image` URL pair —
/// same verified-field-order style as `video_pattern`, see this module's
/// doc comment. `image` is a small thumbnail rendition; `viewer_image` is
/// the full-resolution one, so extraction prefers it, falling back to
/// `image` if empty, exactly like the video path prefers HD over SD.
fn photo_pattern() -> Regex {
    Regex::new(
        r#""image":\{"uri":"((?:[^"\\]|\\.)*)","height":\d+,"width":\d+\},"viewer_image":\{"height":\d+,"width":\d+,"uri":"((?:[^"\\]|\\.)*)"\}"#,
    )
    .expect("valid regex")
}

/// Slices `html` down to just the target post's own photo-attachment block
/// — see this module's doc comment for why `mediaset_token` is the right
/// anchor and why bounding to the *next* marker (any id) matters.
fn photo_attachment_window<'a>(html: &'a str, target_id: &str) -> Option<&'a str> {
    let marker = Regex::new(r#""mediaset_token":"pcb\.(\d+)""#).expect("valid regex");
    let markers: Vec<(usize, &str)> = marker
        .captures_iter(html)
        .map(|c| {
            (
                c.get(0).expect("group 0 always matches").start(),
                c.get(1).expect("captured by (\\d+)").as_str(),
            )
        })
        .collect();
    let idx = markers.iter().position(|(_, id)| *id == target_id)?;
    let start = markers[idx].0;
    let end = markers
        .get(idx + 1)
        .map(|(pos, _)| *pos)
        .unwrap_or(html.len());
    Some(&html[start..end])
}

/// Pulls every photo in the target post's attachment (one photo, or a whole
/// album) out of its `mediaset_token`-scoped window. Two duplicate Relay
/// records embedding the same photos verbatim is expected (see this
/// module's doc comment and `video_pattern`'s), so results are deduped by
/// URL, preserving first-seen order. `None` when the target id has no
/// `mediaset_token` block at all (not a photo post) — the caller falls back
/// from this to the combined "nothing found" error, not this function.
fn extract_photo_refs(html: &str, target_id: &str) -> Option<Vec<MediaRef>> {
    let window = photo_attachment_window(html, target_id)?;

    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    for caps in photo_pattern().captures_iter(window) {
        let sd = unescape_json_string(&caps[1]).filter(|u| !u.is_empty());
        let hd = unescape_json_string(&caps[2]).filter(|u| !u.is_empty());
        let Some(url) = hd.or(sd) else { continue };
        if seen.insert(url.clone()) {
            refs.push(MediaRef {
                url,
                media_type: MediaType::Image,
            });
        }
    }
    if refs.is_empty() {
        None
    } else {
        Some(refs)
    }
}

/// Standard-alphabet, padded base64 — used only to *encode* a known
/// `feedback:<post id>` string for comparison against a page's embedded
/// `feedback.id`, never to decode page content, so no new dependency and no
/// parsing of attacker-shaped base64.
fn base64_std(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Matches a single-photo attachment node's `photo_image` URL and the
/// `feedback.id` of the *same* JSON object. The middle part only skips
/// sibling scalar fields (quoted strings with escapes, or bare
/// non-brace tokens), so it can't wander into a nested or neighboring
/// object and pair one post's photo with another post's feedback id.
fn single_photo_pattern() -> Regex {
    Regex::new(
        r#""photo_image":\{"uri":"((?:[^"\\]|\\.)*)","height":\d+,"width":\d+\}(?:[^{}"]|"(?:[^"\\]|\\.)*")*?"feedback":\{(?:[^{}"]|"(?:[^"\\]|\\.)*")*?"id":"([A-Za-z0-9+/=]+)""#,
    )
    .expect("valid regex")
}

/// The single (non-album) photo post shape — see this module's doc comment.
/// `None` when no photo node's `feedback.id` matches the target post.
fn extract_single_photo_refs(html: &str, target_id: &str) -> Option<Vec<MediaRef>> {
    let expected_feedback_id = base64_std(format!("feedback:{target_id}").as_bytes());

    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    for caps in single_photo_pattern().captures_iter(html) {
        if &caps[2] != expected_feedback_id.as_str() {
            continue;
        }
        let Some(url) = unescape_json_string(&caps[1]).filter(|u| !u.is_empty()) else {
            continue;
        };
        if seen.insert(url.clone()) {
            refs.push(MediaRef {
                url,
                media_type: MediaType::Image,
            });
        }
    }
    if refs.is_empty() {
        None
    } else {
        Some(refs)
    }
}

/// Pulls the target post's downloadable media out of the page: a video/reel
/// yields its one `browser_native_*_url` (HD preferred, falling back to SD);
/// a photo post yields one item per attached photo (full-resolution
/// preferred, falling back to the thumbnail). Tries video first since its
/// match is unambiguous (id embedded right in the same record); falls back
/// to photo only when no video record matches. `Err` when the resolved URL
/// doesn't look like a post URL at all, or when neither a matching video nor
/// a matching photo attachment is found (private, deleted, a login wall, or
/// an unsupported post type — e.g. a plain text/link post has neither).
pub fn extract_media_refs(final_url: &str, html: &str) -> Result<Vec<MediaRef>, String> {
    let target_id = extract_target_post_id(final_url)
        .ok_or_else(|| "could not determine a post id from the resolved URL".to_string())?;

    let pattern = video_pattern();
    for caps in pattern.captures_iter(html) {
        if &caps[1] != target_id.as_str() {
            continue;
        }
        let hd = unescape_json_string(&caps[3]).filter(|u| !u.is_empty());
        let sd = unescape_json_string(&caps[2]).filter(|u| !u.is_empty());
        return match hd.or(sd) {
            Some(url) => Ok(vec![MediaRef {
                url,
                media_type: MediaType::Video,
            }]),
            None => Err(format!(
                "found video {target_id} but it had no downloadable URL"
            )),
        };
    }

    if let Some(refs) = extract_photo_refs(html, &target_id) {
        return Ok(refs);
    }

    if let Some(refs) = extract_single_photo_refs(html, &target_id) {
        return Ok(refs);
    }

    Err(format!("no video or photo data found for {target_id} — private, deleted, a login wall, or an unsupported post type"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_json(id: &str, sd: &str, hd: &str) -> String {
        format!(
            r#"{{"dash_manifest_url":"https:\/\/www.facebook.com\/dash_mpd_debug.mpd?v={id}&dummy=.mpd","browser_native_sd_url":"{sd}","browser_native_hd_url":"{hd}"}}"#
        )
    }

    fn photo_json(image_uri: &str, viewer_uri: &str) -> String {
        format!(
            r#"{{"image":{{"uri":"{image_uri}","height":590,"width":332}},"viewer_image":{{"height":1920,"width":1080,"uri":"{viewer_uri}"}}}}"#
        )
    }

    fn mediaset_block(post_id: &str, photos: &[String]) -> String {
        format!(
            r#"{{"mediaset_token":"pcb.{post_id}","all_subattachments":{{"count":{},"nodes":[{}]}}}}"#,
            photos.len(),
            photos.join(",")
        )
    }

    #[test]
    fn extract_target_post_id_from_a_reel_path() {
        assert_eq!(
            extract_target_post_id("https://www.facebook.com/reel/819727804410500/?rdid=abc"),
            Some("819727804410500".to_string())
        );
    }

    #[test]
    fn extract_target_post_id_from_a_page_videos_path() {
        assert_eq!(
            extract_target_post_id(
                "https://www.facebook.com/61554124040049/videos/819727804410500/"
            ),
            Some("819727804410500".to_string())
        );
    }

    #[test]
    fn extract_target_post_id_from_a_page_posts_path() {
        assert_eq!(
            extract_target_post_id(
                "https://www.facebook.com/61553704094502/posts/122290950710123469/?rdid=abc"
            ),
            Some("122290950710123469".to_string())
        );
    }

    #[test]
    fn extract_target_post_id_from_a_watch_query_param() {
        assert_eq!(
            extract_target_post_id("https://www.facebook.com/watch/?v=819727804410500"),
            Some("819727804410500".to_string())
        );
    }

    #[test]
    fn extract_target_post_id_none_for_a_non_numeric_slug() {
        assert_eq!(
            extract_target_post_id("https://www.facebook.com/some.page.name/"),
            None
        );
    }

    #[test]
    fn picks_the_matching_video_out_of_several_embedded_ones() {
        let html = format!(
            "<script>{}{}{}</script>",
            video_json(
                "111",
                "https://video.xx.fbcdn.net\\/sd1.mp4",
                "https://video.xx.fbcdn.net\\/hd1.mp4"
            ),
            video_json(
                "819727804410500",
                "https://video.xx.fbcdn.net\\/sd2.mp4",
                "https://video.xx.fbcdn.net\\/hd2.mp4"
            ),
            video_json(
                "333",
                "https://video.xx.fbcdn.net\\/sd3.mp4",
                "https://video.xx.fbcdn.net\\/hd3.mp4"
            ),
        );
        let refs =
            extract_media_refs("https://www.facebook.com/reel/819727804410500/", &html).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://video.xx.fbcdn.net/hd2.mp4");
        assert_eq!(refs[0].media_type, MediaType::Video);
    }

    #[test]
    fn falls_back_to_sd_when_hd_is_empty() {
        let html = format!(
            "<script>{}</script>",
            video_json("819727804410500", "https://video.xx.fbcdn.net\\/sd.mp4", "")
        );
        let refs =
            extract_media_refs("https://www.facebook.com/reel/819727804410500/", &html).unwrap();
        assert_eq!(refs[0].url, "https://video.xx.fbcdn.net/sd.mp4");
    }

    #[test]
    fn errors_when_the_id_is_not_present_in_the_page_at_all() {
        let html = format!(
            "<script>{}</script>",
            video_json(
                "111",
                "https://video.xx.fbcdn.net\\/sd.mp4",
                "https://video.xx.fbcdn.net\\/hd.mp4"
            )
        );
        assert!(
            extract_media_refs("https://www.facebook.com/reel/819727804410500/", &html).is_err()
        );
    }

    #[test]
    fn errors_when_the_resolved_url_has_no_recognizable_video_id() {
        let html = "<script>no video data here</script>";
        assert!(extract_media_refs("https://www.facebook.com/some.page.name/", html).is_err());
    }

    #[test]
    fn extracts_every_photo_in_an_album_preferring_the_full_resolution_url() {
        let photos = vec![
            photo_json(
                "https://scontent.xx.fbcdn.net/sd1.jpg",
                "https://scontent.xx.fbcdn.net/hd1.jpg",
            ),
            photo_json(
                "https://scontent.xx.fbcdn.net/sd2.jpg",
                "https://scontent.xx.fbcdn.net/hd2.jpg",
            ),
        ];
        let html = format!(
            "<script>{}</script>",
            mediaset_block("122290950710123469", &photos)
        );
        let refs = extract_media_refs(
            "https://www.facebook.com/61553704094502/posts/122290950710123469/",
            &html,
        )
        .unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].url, "https://scontent.xx.fbcdn.net/hd1.jpg");
        assert_eq!(refs[1].url, "https://scontent.xx.fbcdn.net/hd2.jpg");
        assert!(refs.iter().all(|r| r.media_type == MediaType::Image));
    }

    #[test]
    fn extracts_a_single_photo_post() {
        let photos = vec![photo_json(
            "https://scontent.xx.fbcdn.net/sd.jpg",
            "https://scontent.xx.fbcdn.net/hd.jpg",
        )];
        let html = format!(
            "<script>{}</script>",
            mediaset_block("122290950710123469", &photos)
        );
        let refs = extract_media_refs(
            "https://www.facebook.com/61553704094502/posts/122290950710123469/",
            &html,
        )
        .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://scontent.xx.fbcdn.net/hd.jpg");
    }

    #[test]
    fn photo_falls_back_to_the_thumbnail_when_the_full_resolution_url_is_empty() {
        let photos = vec![photo_json("https://scontent.xx.fbcdn.net/sd.jpg", "")];
        let html = format!(
            "<script>{}</script>",
            mediaset_block("122290950710123469", &photos)
        );
        let refs = extract_media_refs(
            "https://www.facebook.com/61553704094502/posts/122290950710123469/",
            &html,
        )
        .unwrap();
        assert_eq!(refs[0].url, "https://scontent.xx.fbcdn.net/sd.jpg");
    }

    #[test]
    fn dedupes_photos_repeated_across_duplicate_relay_records() {
        let photos = vec![photo_json(
            "https://scontent.xx.fbcdn.net/sd1.jpg",
            "https://scontent.xx.fbcdn.net/hd1.jpg",
        )];
        let block = mediaset_block("122290950710123469", &photos);
        let html = format!("<script>{block}{block}</script>");
        let refs = extract_media_refs(
            "https://www.facebook.com/61553704094502/posts/122290950710123469/",
            &html,
        )
        .unwrap();
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn does_not_leak_a_neighboring_posts_photos_into_the_target_posts_results() {
        let other_photos = vec![photo_json(
            "https://scontent.xx.fbcdn.net/other_sd.jpg",
            "https://scontent.xx.fbcdn.net/other_hd.jpg",
        )];
        let target_photos = vec![photo_json(
            "https://scontent.xx.fbcdn.net/sd.jpg",
            "https://scontent.xx.fbcdn.net/hd.jpg",
        )];
        let html = format!(
            "<script>{}{}</script>",
            mediaset_block("999", &other_photos),
            mediaset_block("122290950710123469", &target_photos)
        );
        let refs = extract_media_refs(
            "https://www.facebook.com/61553704094502/posts/122290950710123469/",
            &html,
        )
        .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://scontent.xx.fbcdn.net/hd.jpg");
    }

    fn single_photo_json(uri: &str, post_id: &str) -> String {
        let feedback = base64_std(format!("feedback:{post_id}").as_bytes());
        format!(
            r#"{{"viewer_image":{{"height":1905,"width":1428}},"photo_image":{{"uri":"{uri}","height":702,"width":526}},"accessibility_caption":"May be {{an}} \"image\"","url":"https:\/\/www.facebook.com\/photo\/?fbid=1","id":"1","feedback":{{"can_viewer_comment":false,"id":"{feedback}"}},"__isNode":"Photo"}}"#
        )
    }

    #[test]
    fn base64_std_matches_known_vectors() {
        assert_eq!(base64_std(b""), "");
        assert_eq!(base64_std(b"f"), "Zg==");
        assert_eq!(base64_std(b"fo"), "Zm8=");
        assert_eq!(base64_std(b"foo"), "Zm9v");
        // The real value observed on a live single-photo post page.
        assert_eq!(
            base64_std(b"feedback:122289594920114104"),
            "ZmVlZGJhY2s6MTIyMjg5NTk0OTIwMTE0MTA0"
        );
    }

    #[test]
    fn extracts_a_single_photo_post_without_a_mediaset_block() {
        let html = format!(
            "<script>{}{}</script>",
            single_photo_json("https://scontent.xx.fbcdn.net\\/other.jpg", "999"),
            single_photo_json(
                "https://scontent.xx.fbcdn.net\\/p.jpg",
                "122289594920114104"
            ),
        );
        let refs = extract_media_refs(
            "https://www.facebook.com/61553423137019/posts/122289594920114104/",
            &html,
        )
        .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://scontent.xx.fbcdn.net/p.jpg");
        assert_eq!(refs[0].media_type, MediaType::Image);
    }

    #[test]
    fn single_photo_does_not_pair_with_a_feedback_id_from_another_object() {
        // Target post's feedback id sits in a *different* object than the photo.
        let feedback = base64_std(b"feedback:122289594920114104");
        let html = format!(
            r#"<script>{{"photo_image":{{"uri":"https://scontent.xx.fbcdn.net/x.jpg","height":1,"width":1}},"nested":{{"a":1}},"feedback":{{"id":"{feedback}"}}}}</script>"#
        );
        assert!(extract_media_refs(
            "https://www.facebook.com/61553423137019/posts/122289594920114104/",
            &html
        )
        .is_err());
    }

    #[test]
    fn errors_when_neither_video_nor_photo_data_matches_the_target_id() {
        let html = "<script>no matching data here</script>";
        assert!(extract_media_refs(
            "https://www.facebook.com/61553704094502/posts/122290950710123469/",
            html
        )
        .is_err());
    }
}
