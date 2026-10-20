//! Pinterest pin scraper backing the `/pind <url>` Telegram command
//! (`ore::herald::commands::TelegramCommand::Pind`, executed by `Herald::
//! handle_pind`) — same job as `ore::igd`/`ore::xrd`/`ore::fbd`, a fourth
//! platform. See `ore::igd`'s module doc comment for the shared design
//! rationale (this module owns every network call it makes; `ore::herald`
//! only ever calls `download_post` and then saves/categorizes/relays
//! whatever bytes come back).
//!
//! **Mechanism**: resolve the input to a numeric pin id (a `pin.it` short
//! link is followed through its redirect chain first), call the same
//! unauthenticated `PinResource` JSON endpoint Pinterest's own web client
//! uses, and pick the media URL(s) out of that object — see `parse` for the
//! supported pin kinds and `net::fetch_pin_json` for the two request
//! headers that endpoint is picky about. No cookies, no login, no
//! `ore::platform_cookies` entry.
//!
//! **Threat model** — `input_url` is attacker-influenced (the linked
//! Telegram chat's message text, same trust level as the sibling
//! commands). Every request goes through `net::guarded_client`'s fixed
//! `pinterest.com`/`pin.it`/`pinimg.com` https-only allowlist and
//! DNS-resolved/connect-pinned `ore::net_fetch::is_blocked_ip` check, with
//! every redirect hop re-validated. The pin id interpolated into the API
//! request is validated all-digit/length-bounded and URL-encoded. The
//! metadata JSON and each media file are size-capped, item count is capped
//! (`types::MAX_ITEMS`), and downloaded bytes are never trusted as
//! whatever type Pinterest's JSON claims: `ore::herald::sniff_content_type`
//! decides the destination folder, and the saved file's extension comes
//! from a fixed whitelist (`parse::extension_for`), never remote text.
//!
//! **HLS-only videos** (every video story pin, verified live) have no
//! progressive `.mp4`; `hls` fetches the two fMP4 tracks through the same
//! guarded client and joins them with a local-only `ffmpeg -c copy` — see
//! its module doc for the subprocess hardening.
//!
//! Fragile by nature, not by bug: this depends on an undocumented endpoint
//! and its `images.orig`/`videos.video_list`/`carousel_data` shapes.
//! Image pins, video story pins (HLS), and photo story pins were verified
//! live against real public pins; GIF story pins share the photo story
//! pin's block shape and plain GIF pins' `embed` shape was seen live in
//! search results, but neither was downloaded end to end; plain-`.mp4` video and carousel parsing follow Pinterest's
//! long-stable shapes but are covered by fixture tests only.

pub mod hls;
pub mod net;
pub mod parse;
pub mod types;

use types::MediaItem;

/// Fetches every media item (one image, one video, or each carousel slide)
/// for the Pinterest pin at `input_url`, fully in memory (see
/// `types::MAX_MEDIA_BYTES`).
pub async fn download_post(input_url: &str) -> Result<Vec<MediaItem>, String> {
    // A direct pin URL already carries its id; anything else (a `pin.it`
    // short link, or a pin URL with an unusual shape) is followed to its
    // landing URL first. The follow is itself host-allowlisted.
    let pin_id = match parse::extract_pin_id(input_url) {
        Some(id) if is_pinterest_pin_host(input_url) => id,
        _ => {
            let final_url = net::resolve_final_url(input_url).await?;
            parse::extract_pin_id(&final_url)
                .ok_or_else(|| "that doesn't look like a Pinterest pin link".to_string())?
        }
    };

    let json = net::fetch_pin_json(&pin_id).await?;
    let refs = parse::extract_media_refs(&json)?;

    let mut items = Vec::with_capacity(refs.len());
    for m in refs {
        let (bytes, extension) = if m.hls {
            (hls::download_hls(&m.url).await?, "mp4")
        } else {
            let ext =
                parse::extension_for(&m.url).unwrap_or_else(|| m.media_type.default_extension());
            (net::fetch_media_bytes(&m.url).await?, ext)
        };
        items.push(MediaItem {
            bytes,
            media_type: m.media_type,
            extension,
        });
    }
    Ok(items)
}

/// Whether `input_url` is a `pinterest.com` page, i.e. one whose own path
/// can be trusted as the id source without a network hop. The id is
/// extracted from the URL *path* only and the API call always goes to a
/// fixed host, so this is about not fabricating an id from a lookalike
/// host's URL, not about SSRF (every fetch is allowlisted regardless).
fn is_pinterest_pin_host(input_url: &str) -> bool {
    reqwest::Url::parse(input_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| h == "pinterest.com" || h.ends_with(".pinterest.com"))
}
