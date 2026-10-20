//! Per-request state for one `download_post` call: a cookie jar, plus
//! `fetch_share_page` — the page-fetch-flavored call over `net::
//! guarded_get`. A share link (`facebook.com/share/r/...`) always
//! redirects to the canonical `/reel/<id>/` (or `/<page>/videos/<id>/`)
//! page. Unlike Xiaohongshu (see `ore::xrd::session`'s doc comment for its
//! own observed anti-bot soft-wall), a plain `GET` here needs no cookie
//! warm-up beforehand — verified empirically: a fresh cookie jar with no
//! prior request still resolved a real share link on the first try, as
//! long as the request carried browser-shaped `Accept`/`Sec-Fetch-*`
//! headers rather than curl's bare defaults (a bare request with no such
//! headers was rejected outright with a `400`).

use std::sync::Arc;

use reqwest::cookie::Jar;

use super::net::guarded_get;

pub struct Session {
    pub jar: Arc<Jar>,
}

pub struct PageFetch {
    pub html: String,
    pub final_url: String,
}

pub fn new_session() -> Session {
    Session {
        jar: Arc::new(Jar::default()),
    }
}

/// Headers that made the difference between Facebook's `400` and a real
/// page in testing — shaped like an actual browser's top-level navigation,
/// not curl's bare defaults.
const PAGE_HEADERS: &[(&str, &str)] = &[
    ("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8"),
    ("Accept-Language", "en-US,en;q=0.9"),
    ("Sec-Fetch-Mode", "navigate"),
    ("Sec-Fetch-Site", "none"),
    ("Sec-Fetch-User", "?1"),
    ("Sec-Fetch-Dest", "document"),
    ("Upgrade-Insecure-Requests", "1"),
];

/// Fetches the page a share link (or an already-canonical URL) ultimately
/// resolves to, following redirects via `net::guarded_get`.
pub async fn fetch_share_page(session: &Session, input_url: &str) -> Result<PageFetch, String> {
    let response = guarded_get(session.jar.clone(), input_url, PAGE_HEADERS).await?;
    if !response.status().is_success() {
        return Err(format!(
            "unexpected status fetching page: {}",
            response.status()
        ));
    }
    let final_url = response.url().to_string();
    let html = response
        .text()
        .await
        .map_err(|_| "failed to read page body".to_string())?;
    Ok(PageFetch { html, final_url })
}
