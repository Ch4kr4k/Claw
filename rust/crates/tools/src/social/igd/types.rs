//! Shared types for `ore::igd`. See `super`'s module doc comment for the
//! overall threat model.

use std::time::Duration;

/// Same UA `super::session` sends on every request — Instagram's web app
/// serves meaningfully different (often JS-free) markup to clients it
/// classifies as bots, so this has to look like a real desktop browser.
pub(crate) const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:141.0) Gecko/20100101 Firefox/141.0";
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// How many of the post page's own JS bundle URLs `discovery::
/// discover_graphql_operation` will download while hunting for the
/// persisted-query `doc_id` — bounded so a page shaped unexpectedly
/// differently than today's Instagram build can't turn one `/igd` command
/// into an unbounded number of outbound fetches. Raised from an original
/// `120`. The actual bug that made discovery intermittent was
/// `extract_javascript_bundle_urls` truncating a randomly-ordered
/// `HashSet` (see that function's doc comment, now fixed to preserve page
/// order instead) — on a real post page with 477 total bundle URLs, the
/// target operation's own module turned out to live at page-order index 1
/// and 2 (script tags load dependency-first, so the relevant Relay bundles
/// tend to sit early), well inside even the original `120`, and only the
/// random reordering was excluding them some fraction of the time. This
/// `300` isn't "477 was observed to be necessary" — it's just a wider
/// margin than `120` now that discovery is deterministic, in case some
/// other post's relevant bundle isn't as early in page order as this one's
/// was.
pub(crate) const MAX_BUNDLE_DOWNLOADS: usize = 300;
/// `graphql::execute_media_graphql`'s retry budget for a transient
/// (network-error or 5xx) failure only — never for a 4xx or an
/// application-level GraphQL error, see `classify_graphql_failure`.
pub(crate) const MAX_RETRY_COUNT: u32 = 2;
/// Caps how much of one media item's bytes `net::fetch_media_bytes` will
/// buffer in memory. Unlike `ore::herald::files::save_confined_streaming`
/// (which has no fixed cap at all — CLAUDE.md's own upload rule prefers a
/// live disk-space check there), this module buffers fully in memory before
/// ever touching disk, since the same bytes are also handed straight to
/// Telegram's `sendPhoto`/`sendVideo`. An unbounded in-memory buffer on a
/// Raspberry Pi 4 is a real memory-exhaustion risk a disk-space check
/// wouldn't catch, so this stays a fixed ceiling: generous for anything
/// Instagram's own encoder plausibly produces (reels/posts are overwhelmingly
/// well under this), and comfortably above Telegram Bot API's own 50MB
/// `sendVideo` upload ceiling — a file this module would refuse to fetch
/// could never have been relayed as a preview anyway.
pub(crate) const MAX_MEDIA_BYTES: u64 = 200 * 1024 * 1024;
/// How many redirect hops `net::fetch_media_bytes` will follow. Verified
/// empirically: a `video_versions`/`image_versions2` URL's own edge host
/// (e.g. `instagram.fdel18-1.fna.fbcdn.net`) 302s a video fetch to a
/// generic `video.xx.fbcdn.net` host before serving the actual bytes —
/// image URLs were not observed to do this, only video ones — so without
/// following this hop, every video item failed with "bad status 302
/// downloading media" while photo items (served directly, no redirect)
/// kept working, which is exactly the "`/igd` only works for photos" bug
/// this constant exists to fix. Bounded rather than followed indefinitely
/// for the same reason `ore::xrd`/`ore::fbd` bound their own redirect
/// following: so a redirect loop can't turn one download into an unbounded
/// number of fetches.
pub(crate) const MAX_MEDIA_REDIRECTS: usize = 5;

/// `PolarisPostRootQuery`'s Relay `providedVariables`, as of the last time
/// they were checked by hand — used only when `discovery::
/// extract_provided_variables` couldn't read the live list out of the
/// bundle. Instagram's persisted query rejects a request missing any one of
/// these with a bare `{"errors":[{"message":"execution error"}]}` even
/// when the `doc_id` itself is still current — that's exactly how `/igd`
/// broke when `PolarisThreadsStyleCommentsEnabled` was added without the
/// `doc_id` changing, and why the list is now discovered rather than only
/// hardcoded.
pub(crate) const FALLBACK_PROVIDED_VARIABLES: &[&str] = &[
    "__relay_internal__pv__PolarisShortDramaEnabledrelayprovider",
    "__relay_internal__pv__PolarisThreadsStyleCommentsEnabledrelayprovider",
    "__relay_internal__pv__PolarisMultiCaptionCarouselEnabledrelayprovider",
];
/// Upper bound on how many `providedVariables` names discovery will accept
/// from a bundle — a sanity cap so a malformed/unexpected bundle can't
/// balloon the GraphQL request body. Today's real count is 3.
pub(crate) const MAX_PROVIDED_VARIABLES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Image,
    Video,
}

impl MediaType {
    /// Filename extension for a saved item — matches `Config::
    /// telegram_folders`' `pictures`/`video` split one-for-one, since these
    /// two variants are exactly what `Herald::default_folder_for_content_type`
    /// maps `image/*`/`video/*` to.
    pub fn extension(self) -> &'static str {
        match self {
            MediaType::Video => "mp4",
            MediaType::Image => "jpg",
        }
    }
}

/// One fully-downloaded media item from a post/reel — bytes only, never a
/// path: the caller (`Herald::handle_igd`) decides where it lands.
#[derive(Debug, Clone)]
pub struct MediaItem {
    pub bytes: Vec<u8>,
    pub media_type: MediaType,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphQlOperation {
    pub operation_name: String,
    pub doc_id: String,
    /// The query's Relay `providedVariables` names, as read out of the same
    /// web build the `doc_id` came from — see `discovery::
    /// extract_provided_variables`. `None` when discovery couldn't find
    /// them, in which case `graphql` falls back to
    /// `FALLBACK_PROVIDED_VARIABLES`.
    pub provided_variables: Option<Vec<String>>,
    pub discovered_from_bundle: String,
    pub discovered_at: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedOperation {
    pub operation_name: String,
    pub doc_id: String,
    /// `#[serde(default)]` so a cache file written before this field
    /// existed still loads (as `None` → the fallback list) instead of being
    /// discarded wholesale.
    #[serde(default)]
    pub provided_variables: Option<Vec<String>>,
    pub saved_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphQlFailure {
    HttpError,
    InvalidJson,
    InvalidDocId,
    ExecutionError,
    NullData,
    RateLimited,
    LoginRequired,
    MediaUnavailable,
    Unknown,
}

pub struct GraphQlResult {
    pub success: bool,
    pub status: Option<u16>,
    pub json: Option<serde_json::Value>,
    #[allow(dead_code)] // kept for debugging a failed request
    pub error: Option<String>,
}
