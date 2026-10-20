//! Instagram post/reel media scraper backing the `/igd <url>` Telegram
//! command (`ore::herald::commands::TelegramCommand::Igd`, executed by
//! `Herald::handle_igd`). Ported from a standalone research tool that
//! reverse-engineered how Instagram's own web client resolves a post's
//! media URLs — there is no official, authenticated API involved; this
//! mines the same public post page and persisted-query GraphQL endpoint a
//! logged-out browser tab already uses to render the page, using the exact
//! request shape (cookies + CSRF token, no bearer token, no login) a real
//! browser sends for a public post.
//!
//! This module owns every network call the scraper makes; `ore::herald`
//! only ever calls `download_post` and then saves/categorizes/relays
//! whatever bytes come back — the same "`ore` owns the external fetch,
//! `herald` owns the Telegram-facing orchestration" split `ore::codedb`'s
//! image proxy already follows (see `CLAUDE.md`'s `ore`/`server` boundary
//! note, extended here to an `ore`-internal module boundary).
//!
//! **Threat model** — `input_url` is attacker-influenced (it comes
//! straight from the linked Telegram chat's message text, the identical
//! trust level `/dwnld`'s URL already gets — see `ore::herald::commands`'s
//! module doc comment). Every request this module makes, whether to
//! `input_url` itself or to a URL lifted out of Instagram's own HTML/JSON
//! response (a JS bundle URL, a `video_versions`/`image_versions2` media
//! URL), is checked against `net::guarded_client`'s fixed
//! `instagram.com`/`cdninstagram.com`/`fbcdn.net` allowlist and DNS-
//! resolved/connect-pinned against `ore::net_fetch::is_blocked_ip` before a
//! single byte is sent — see `net`'s doc comment for why this doesn't just
//! reuse `ore::net_fetch` directly. That closes both the case where a
//! hostile `input_url` points somewhere other than Instagram at all, and
//! the (much less likely, but not assumed away) case of a compromised or
//! malformed response trying to smuggle a private-network callback through
//! a "JS bundle" or "media" URL. Downloaded bytes are capped
//! (`types::MAX_MEDIA_BYTES`) and never trusted as whatever type Instagram
//! claims for them: `ore::herald::sniff_content_type` (magic bytes via
//! `infer`) is what actually decides the destination folder and Telegram
//! send method once `Herald::handle_igd` has the bytes — Instagram's own
//! `video_versions`/`image_versions2` shape only decides which URLs are
//! worth fetching in the first place, and downloaded content is never
//! executed, interpreted, or rendered server-side; it is written to disk
//! and handed to Telegram's own `sendPhoto`/`sendVideo`, the same as any
//! other herald-saved file.
//!
//! Fragile by nature, not by bug: Instagram ships a new web build
//! periodically, which can change the page's own markup, invalidate the
//! cached `doc_id` (handled — see `download_post`'s cache-then-rediscover
//! flow), or change the GraphQL response shape entirely (not handled —
//! `graphql::extract_media_refs` returns a plain error in that case rather
//! than guessing). This is expected to need occasional maintenance to keep
//! working, the same as any unofficial-API integration.

pub mod cache;
pub mod discovery;
pub mod graphql;
mod net;
pub mod session;
pub mod types;
pub mod utils;

use std::path::Path;

use types::MediaItem;

const TARGET_OPERATION_NAME: &str = "PolarisPostRootQuery";

/// Fetches every media item (image/video, including every item of a
/// carousel post) from the Instagram post/reel at `input_url`, fully in
/// memory (see `types::MAX_MEDIA_BYTES`'s doc comment for why this module
/// buffers rather than streams to disk). `cache_file` should sit under an
/// already-configured herald data directory (see `ore::config`'s
/// `herald_data_dir`) — no new `paths.json` entry needed, the same "a file
/// under an already-configured data dir" shape `Herald`'s own
/// `state.json` uses. `cookies`, from `ore::platform_cookies`' encrypted
/// store, is optional — `None` keeps this module's original anonymous
/// behavior; see `session::new_session` for why a real logged-in session
/// may be required for some posts regardless of query shape.
pub async fn download_post(
    cache_file: &Path,
    input_url: &str,
    cookies: Option<&str>,
) -> Result<Vec<MediaItem>, String> {
    let shortcode = utils::extract_shortcode(input_url)
        .ok_or_else(|| "not a recognizable Instagram post/reel URL".to_string())?;

    let mut sess = session::new_session(cookies);
    let page = session::fetch_media_page(&sess, input_url).await?;

    let page_url =
        reqwest::Url::parse(&page.final_url).map_err(|_| "invalid final page URL".to_string())?;
    sess.csrf_token = session::extract_csrf_from_cookie_jar(&sess, &page_url);
    sess.referer = Some(page.final_url.clone());

    if let Some(cached) = cache::read_operation_cache(cache_file, TARGET_OPERATION_NAME).await {
        let result = graphql::execute_media_graphql(
            &sess,
            &shortcode,
            &cached.doc_id,
            cached.provided_variables.as_deref(),
        )
        .await;
        if result.success {
            let data = result
                .json
                .as_ref()
                .and_then(|j| j.get("data"))
                .expect("checked success above");
            return fetch_all_media(&sess, &shortcode, data).await;
        }

        let failure = graphql::classify_graphql_failure(&result);
        if graphql::is_stale_doc_id_failure(failure) {
            cache::delete_cached_operation(cache_file, TARGET_OPERATION_NAME).await;
        } else {
            return Err(describe_terminal_failure(failure));
        }
    }

    let operation = discovery::discover_graphql_operation(
        &sess,
        &page.html,
        TARGET_OPERATION_NAME,
        types::MAX_BUNDLE_DOWNLOADS,
    )
    .await
    .map_err(|e| format!("could not locate Instagram's current GraphQL operation id: {e}"))?;
    cache::save_operation_cache(cache_file, &operation).await;

    let result = graphql::execute_media_graphql(
        &sess,
        &shortcode,
        &operation.doc_id,
        operation.provided_variables.as_deref(),
    )
    .await;
    if result.success {
        let data = result
            .json
            .as_ref()
            .and_then(|j| j.get("data"))
            .expect("checked success above");
        return fetch_all_media(&sess, &shortcode, data).await;
    }

    Err(describe_terminal_failure(
        graphql::classify_graphql_failure(&result),
    ))
}

fn describe_terminal_failure(failure: types::GraphQlFailure) -> String {
    use types::GraphQlFailure::*;
    match failure {
        RateLimited => "Instagram rate-limited this request — try again later".to_string(),
        LoginRequired => "Instagram is requiring a logged-in session to view this post".to_string(),
        MediaUnavailable => "post unavailable, private, or deleted".to_string(),
        // Not a login problem: an id Instagram does not know, or a change
        // in its web API. The old wording sent a model off to set up cookies.
        other => format!(
            "Instagram did not return the post ({other:?}): check that the link is a post or reel \
             (instagram.com/p/…, /reel/…); if it is, Instagram's web API may have changed"
        ),
    }
}

/// Downloads every media item's bytes concurrently rather than one at a
/// time — each item is an independent request to Instagram's CDN (through
/// its own `net::guarded_client`-built connection, since that's pinned per
/// resolved address, not shared), so there's no correctness reason to
/// serialize them, and a real browser rendering the same carousel would
/// already be fetching all of its images/video in parallel. Carousels are
/// capped by Instagram itself at a small item count, so this is bounded
/// concurrency in practice without needing an explicit limit. On the first
/// failure, every still-running download is aborted rather than left to
/// finish and be discarded.
async fn fetch_all_media(
    sess: &session::Session,
    shortcode: &str,
    data: &serde_json::Value,
) -> Result<Vec<MediaItem>, String> {
    let refs = graphql::extract_media_refs(shortcode, data)?;
    let total = refs.len();

    let mut set = tokio::task::JoinSet::new();
    for (idx, m) in refs.into_iter().enumerate() {
        let jar = sess.jar.clone();
        set.spawn(async move {
            let result = net::fetch_media_bytes(jar, &m.url).await;
            (idx, result, m.media_type)
        });
    }

    let mut slots: Vec<Option<MediaItem>> = (0..total).map(|_| None).collect();
    let mut first_error = None;
    while let Some(joined) = set.join_next().await {
        let (idx, result, media_type) = match joined {
            Ok(v) => v,
            Err(e) => {
                first_error.get_or_insert(format!("media download task panicked: {e}"));
                continue;
            }
        };
        match result {
            Ok(bytes) => slots[idx] = Some(MediaItem { bytes, media_type }),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }

    if let Some(e) = first_error {
        return Err(e);
    }
    Ok(slots
        .into_iter()
        .map(|s| s.expect("no error recorded, every slot must be filled"))
        .collect())
}
