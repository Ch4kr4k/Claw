//! Per-request state for one `download_post` call: a cookie jar (Instagram
//! sets `csrftoken`/`mid` on the first page fetch, which the later GraphQL
//! `POST` must replay) plus the CSRF token and `Referer` derived from that
//! first fetch. Never persisted across calls or shared between concurrent
//! `/igd` invocations — a fresh `Session` per `download_post` call, same as
//! the original standalone tool's per-run session.

use std::sync::Arc;

use reqwest::cookie::{CookieStore, Jar};
use reqwest::Url;

use super::net::guarded_client;

pub const IG_BASE_URL: &str = "https://www.instagram.com";

pub struct Session {
    pub jar: Arc<Jar>,
    pub csrf_token: Option<String>,
    pub referer: Option<String>,
}

pub struct PageFetch {
    pub html: String,
    pub final_url: String,
}

/// `cookies`, when given, is a real logged-in Instagram session (Netscape
/// cookie-jar text, from `ore::platform_cookies`' encrypted store) —
/// seeded into the jar before the very first request via `ore::
/// platform_cookies::seed_jar`, same mechanism `ore::xrd::session::
/// new_session` uses. Needed for exactly the case Instagram's own
/// anonymous GraphQL response can't be told apart from "doesn't exist":
/// a private account's post, or anything else gated from logged-out
/// viewers — confirmed live (see the `/igd` diagnosis session this was
/// added for) that even the *exact* request Instagram's own logged-in web
/// client sends for such a post still 404s in all but name when replayed
/// anonymously; the query shape was never the problem for that case.
pub fn new_session(cookies: Option<&str>) -> Session {
    let jar = Jar::default();
    if let Some(text) = cookies {
        crate::social::seed_jar(&jar, text, "www.instagram.com");
    }
    Session {
        jar: Arc::new(jar),
        csrf_token: None,
        referer: None,
    }
}

/// Fetches the post/reel page itself — the SSR HTML this scraper mines for
/// JS bundle URLs (`super::discovery`) when the cached `doc_id` is missing
/// or stale, and the response whose `Set-Cookie` headers seed `session.jar`
/// for the GraphQL `POST` that follows. Redirects are never followed (see
/// `super::net::guarded_client`'s doc comment), so `input_url` must already
/// be a direct `instagram.com` post/reel URL — a share short-link or a
/// bare-domain URL that Instagram would otherwise 30x is reported as a
/// plain fetch failure rather than silently chased.
pub async fn fetch_media_page(session: &Session, input_url: &str) -> Result<PageFetch, String> {
    let url = Url::parse(input_url).map_err(|_| "invalid Instagram URL".to_string())?;
    let client = guarded_client(&url, session.jar.clone()).await?;

    let response = client
        .get(url)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.5")
        .send()
        .await
        .map_err(|_| "failed to fetch Instagram page".to_string())?;
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

/// Reads `csrftoken` back out of the cookie jar the page fetch just
/// populated, for replay as the GraphQL `POST`'s `X-CSRFToken` header.
pub fn extract_csrf_from_cookie_jar(session: &Session, url: &Url) -> Option<String> {
    let header_value = session.jar.cookies(url)?;
    let cookie_str = header_value.to_str().ok()?;
    cookie_str.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("csrftoken=").map(|v| v.to_string())
    })
}
