//! Shared types for `ore::pind`. See `super`'s module doc comment for the
//! overall threat model.

use std::time::Duration;

pub(crate) const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// How many redirect hops `net::guarded_get` will follow. A `pin.it` short
/// link observed in testing takes two (`pin.it` → `api.pinterest.com/
/// url_shortener/...` → `www.pinterest.com/pin/<id>/...`); bounded so a
/// redirect loop can't turn one `/pind` command into unbounded fetches.
pub(crate) const MAX_REDIRECTS: usize = 5;
/// Caps how much of the media's bytes `net::fetch_media_bytes` will buffer
/// in memory — same value and rationale as `ore::fbd::types::MAX_MEDIA_BYTES`.
pub(crate) const MAX_MEDIA_BYTES: u64 = 200 * 1024 * 1024;
/// Caps the pin metadata JSON `net::fetch_pin_json` will buffer. A real
/// response is a few KB; this only exists so a hostile/buggy upstream can't
/// stream unbounded data into memory through the one non-media fetch.
pub(crate) const MAX_API_BYTES: usize = 2 * 1024 * 1024;
/// Caps each HLS track (video, audio) `hls::download_hls` buffers before
/// handing them to `ffmpeg` — the two are held in memory at once, plus the
/// muxed output, so this is half `MAX_MEDIA_BYTES`.
pub(crate) const MAX_HLS_TRACK_BYTES: u64 = MAX_MEDIA_BYTES / 2;
/// Caps an HLS playlist's size — real ones are well under 5 KB.
pub(crate) const MAX_PLAYLIST_BYTES: u64 = 256 * 1024;
/// Bounds the whole `ffmpeg` mux — a stream copy of two local files, so
/// this only ever trips on a hang.
pub(crate) const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);
/// Caps how many items one `/pind` command downloads (a carousel pin's
/// slide count) — the carousel list comes from remote JSON, so it's
/// bounded here rather than trusted.
pub(crate) const MAX_ITEMS: usize = 20;

/// A video pin yields exactly one `Video` item; an image pin yields one
/// `Image`; a carousel pin yields one `Image` per slide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Video,
    Image,
}

impl MediaType {
    /// Fallback extension when the media URL's own suffix isn't one of the
    /// few `parse::extension_for` recognizes.
    pub fn default_extension(self) -> &'static str {
        match self {
            MediaType::Video => "mp4",
            MediaType::Image => "jpg",
        }
    }
}

/// One fully-downloaded media item — bytes only, never a path: the caller
/// (`Herald::handle_pind`) decides where it lands. `extension` is always
/// one of a fixed set of `&'static str`s (see `parse::extension_for`),
/// never text lifted from the remote URL, since it becomes part of a real
/// filename.
#[derive(Debug, Clone)]
pub struct MediaItem {
    pub bytes: Vec<u8>,
    pub media_type: MediaType,
    pub extension: &'static str,
}
