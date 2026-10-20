//! Facebook video/reel/photo scraper backing the `/fbd <url>` Telegram command
//! (`ore::herald::commands::TelegramCommand::Fbd`, executed by `Herald::
//! handle_fbd`) — same job as `ore::igd`/`ore::xrd`, a third platform. See
//! `ore::igd`'s module doc comment for the shared design rationale (this
//! module owns every network call it makes; `ore::herald` only ever calls
//! `download_post` and then saves/categorizes/relays whatever bytes come
//! back); only the platform-specific mechanics differ, documented here and
//! in `parse`.
//!
//! **Why this looks different from `ore::xrd`'s single-JSON-blob scrape**:
//! Facebook's Comet page doesn't server-render one clean state object the
//! way Xiaohongshu's does — it's a tree of many separate Relay preloader
//! records, and a reel page specifically preloads a whole feed of "up
//! next" reels alongside the one actually requested (six separate videos'
//! worth of data were embedded in one real page fetched while building
//! this module). So instead of parsing the page as JSON, `parse::
//! extract_media_refs` does a targeted regex pull keyed to the *specific*
//! video id the caller's URL resolved to — see `parse`'s module doc
//! comment for why that's necessary, not just simpler.
//!
//! **Threat model** — `input_url` is attacker-influenced (the linked
//! Telegram chat's message text, identical trust level `/igd`'s and
//! `/xrd`'s URLs already get). Every request this module makes — resolving
//! a `facebook.com/share/...` link, fetching whatever page each redirect
//! hop lands on, or fetching a media CDN URL lifted out of that page's
//! own embedded JSON (including *that* URL's own edge-host redirect — see
//! `net`'s doc comment) — goes through `net::guarded_client`'s fixed
//! `facebook.com`/`fbcdn.net` allowlist and DNS-resolved/connect-pinned
//! against `ore::net_fetch::is_blocked_ip` before a single byte is sent,
//! with every redirect hop re-validated the same way rather than ever
//! trusting reqwest's own blind redirect chasing. Downloaded bytes are
//! capped (`types::MAX_MEDIA_BYTES`) and never trusted as whatever type
//! Facebook's own JSON claims for them: `ore::herald::sniff_content_type`
//! (magic bytes via `infer`) is what actually decides the destination
//! folder and Telegram send method once `Herald::handle_fbd` has the
//! bytes, same as `/igd`/`/xrd`.
//!
//! Verified empirically against real public share links before writing this
//! module (reel) and before adding photo support (a 4-photo album post): a
//! plain `GET` with browser-shaped headers and a fresh cookie jar (no
//! login, no warm-up request) resolved the share link both times and
//! exposed the target's media URLs — `browser_native_hd_url` for the reel,
//! `image`/`viewer_image` pairs scoped by `mediaset_token` for the album —
//! and those CDN URLs' bytes were downloadable with no cookies or auth
//! beyond following the redirect(s) each needed.
//!
//! Fragile by nature, not by bug: this depends on Facebook continuing to
//! embed `browser_native_sd_url`/`browser_native_hd_url` next to a
//! `dash_manifest_url` carrying the same video's id (and, for photos,
//! `image`/`viewer_image` next to a `mediaset_token` carrying the same
//! post's id), and on a plain `GET` with these specific headers continuing
//! to be enough to avoid a login wall — Facebook's anti-scraping posture is
//! generally the most aggressive of the three platforms this app scrapes,
//! so expect this one to need maintenance the soonest, with no
//! cached-id-style self-healing (there's no persisted id here to
//! rediscover, since no signed API call is ever made in the first place —
//! same as `ore::xrd`).
//!
//! Scope: video/reel and photo posts. A video/reel's `browser_native_*_url`
//! and a photo post's `image`/`viewer_image` pair are two genuinely
//! different embedded shapes, so `parse::extract_media_refs` scopes each to
//! the target post id its own way — see `parse`'s module doc comment.

pub mod net;
pub mod parse;
pub mod session;
pub mod types;

use types::MediaItem;

/// Fetches the target post's media bytes — one video, or every photo in a
/// photo post/album — from the Facebook URL at `input_url`, fully in memory
/// (see `types::MAX_MEDIA_BYTES`'s doc comment for why this module buffers
/// rather than streams to disk).
pub async fn download_post(input_url: &str) -> Result<Vec<MediaItem>, String> {
    let sess = session::new_session();
    let page = session::fetch_share_page(&sess, input_url).await?;
    let refs = parse::extract_media_refs(&page.final_url, &page.html)?;

    let mut items = Vec::with_capacity(refs.len());
    for m in refs {
        let bytes = net::fetch_media_bytes(sess.jar.clone(), &m.url).await?;
        items.push(MediaItem {
            bytes,
            media_type: m.media_type,
        });
    }
    Ok(items)
}
