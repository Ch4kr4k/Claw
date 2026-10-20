//! Shared types for `ore::fbd`. See `super`'s module doc comment for the
//! overall threat model.

use std::time::Duration;

pub(crate) const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// How many redirect hops `net::guarded_get` will follow — used both for
/// resolving a share link down to its canonical `/reel/<id>/` (or
/// `/<page>/videos/<id>/`) page, and for the video CDN URL itself, which
/// (unlike Instagram's or Xiaohongshu's) 302s from the edge host named in
/// the page to a different `*.fbcdn.net` host before serving the actual
/// bytes. Bounded so a redirect loop, or an unexpectedly long chain, can't
/// turn one `/fbd` command into an unbounded number of fetches.
pub(crate) const MAX_REDIRECTS: usize = 5;
/// Caps how much of the video's bytes `net::fetch_media_bytes` will buffer
/// in memory — same value and rationale as `ore::igd::types::
/// MAX_MEDIA_BYTES`/`ore::xrd::types::MAX_MEDIA_BYTES`.
pub(crate) const MAX_MEDIA_BYTES: u64 = 200 * 1024 * 1024;

/// A video/reel post yields exactly one `Video` item (its
/// `browser_native_*_url`); a photo post yields one `Image` item per
/// attached photo (its `image`/`viewer_image` pair) — see `parse`'s module
/// doc comment for how each is scoped to the target post id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Video,
    Image,
}

impl MediaType {
    pub fn extension(self) -> &'static str {
        match self {
            MediaType::Video => "mp4",
            MediaType::Image => "jpg",
        }
    }
}

/// One fully-downloaded media item — bytes only, never a path: the caller
/// (`Herald::handle_fbd`) decides where it lands.
#[derive(Debug, Clone)]
pub struct MediaItem {
    pub bytes: Vec<u8>,
    pub media_type: MediaType,
}
