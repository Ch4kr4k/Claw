//! SSRF hardening for every outbound request `ore::igd` makes — see the
//! parent module's doc comment for the full threat model. Every request,
//! regardless of whether its URL came from the caller-supplied post URL or
//! from Instagram's own HTML/JSON response, goes through `guarded_client`
//! before a single byte is sent.
//!
//! Deliberately its own hardening rather than reusing `ore::net_fetch`
//! directly: that module's `fetch_url_hardened`/`open_hardened_response`
//! only ever issue a plain unauthenticated `GET`, but `ore::igd` needs a
//! shared cookie jar across requests (Instagram's page fetch sets
//! `csrftoken`/`mid`, replayed on the later GraphQL `POST`) and a `POST`
//! with a form body and custom headers — reusing `ore::net_fetch::
//! is_blocked_ip` (the actual IP-range blocklist) keeps the one place that
//! matters, the blocklist itself, from drifting between the two modules.
use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::{Client, Url};

use crate::social::is_blocked_ip;

use super::types::{REQUEST_TIMEOUT, USER_AGENT};

/// Every real Instagram-controlled host this scraper legitimately talks to
/// — the page/API host itself, its static JS CDN, and its media CDN.
/// Anything else is rejected before a single byte is sent, regardless of
/// where the URL came from: the caller-supplied post URL, a `<script src>`
/// lifted out of the page's own HTML, or a `video_versions`/
/// `image_versions2` URL lifted out of Instagram's own GraphQL response.
const ALLOWED_HOST_SUFFIXES: &[&str] = &["instagram.com", "cdninstagram.com", "fbcdn.net"];

fn host_is_allowed(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    ALLOWED_HOST_SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

/// Resolves `url`'s host and rejects it if any resolved address is a
/// private/loopback/link-local/etc range (`ore::net_fetch::is_blocked_ip` —
/// the identical check `/dwnld`'s SSRF hardening uses), returning the first
/// resolved address to pin the connection to. Re-resolved fresh for every
/// single request rather than cached, closing the same DNS-rebinding gap
/// `ore::net_fetch::hardened_client` closes: a hostname passing this check
/// must be the exact address the request actually connects to, not
/// whatever it might resolve to a moment later.
async fn resolve_pinned(url: &Url) -> Result<SocketAddr, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "invalid URL".to_string())?
        .to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| "could not resolve host".to_string())?
        .collect();
    let Some(&addr) = resolved.first() else {
        return Err("could not resolve host".to_string());
    };
    if resolved.iter().any(|a| is_blocked_ip(&a.ip())) {
        tracing::warn!(
            target: "security",
            url = %url,
            "SECURITY_ALERT igd outbound fetch blocked — host resolved to a non-public address"
        );
        return Err("blocked destination".to_string());
    }
    Ok(addr)
}

/// Builds a one-off `reqwest::Client` pinned to `url`'s already-validated,
/// already-resolved address, sharing `jar` so cookies set by an earlier
/// request on a *different* host (e.g. `www.instagram.com` setting
/// `csrftoken`) are still sent here. Rebuilt per request rather than once
/// per session: Instagram's media CDN spreads requests across many
/// different `scontent-*.cdninstagram.com` hostnames, and a single
/// `reqwest::Client` can only pin one resolved address per hostname (via
/// `ClientBuilder::resolve`) — "build once" would only protect the first
/// host a session happened to see, not every host it goes on to reach.
/// Redirects are never followed (same stance `ore::net_fetch` takes): a
/// redirect target is unvalidated by definition, so treating one as a hard
/// failure is simpler and safer than re-running this whole check against
/// wherever it points.
pub async fn guarded_client(url: &Url, jar: Arc<reqwest::cookie::Jar>) -> Result<Client, String> {
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err("only http/https URLs are supported".to_string());
    }
    if !host_is_allowed(url) {
        tracing::warn!(
            target: "security",
            url = %url,
            "SECURITY_ALERT igd outbound fetch blocked — host outside the Instagram allowlist"
        );
        return Err("that host isn't Instagram — refusing to fetch it".to_string());
    }
    let addr = resolve_pinned(url).await?;
    let host = url
        .host_str()
        .expect("checked by host_is_allowed")
        .to_string();

    Client::builder()
        .cookie_provider(jar)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .connect_timeout(REQUEST_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .resolve(&host, addr)
        .build()
        .map_err(|e| format!("building request client: {e}"))
}

/// Fetches one media item's bytes (an image or video URL out of
/// Instagram's own GraphQL response), capped at `super::types::
/// MAX_MEDIA_BYTES` and streamed chunk-by-chunk rather than buffered from a
/// (possibly absent, possibly dishonest) `Content-Length` — same shape as
/// `ore::net_fetch::fetch_url_hardened`, just against this module's own
/// Instagram-only allowlist instead of "any public host."
///
/// Unlike `guarded_client`'s general "never follow a redirect" stance
/// (still correct for the page fetch, the GraphQL call, and JS bundle
/// downloads — a redirect from any of those really would be unexpected),
/// a media URL's own edge host is verified to 302 a video fetch to a
/// generic `video.xx.fbcdn.net` host before serving the actual bytes (an
/// image URL was not observed to do this) — see `types::
/// MAX_MEDIA_REDIRECTS`'s doc comment for how this was diagnosed. So this
/// function follows that redirect itself, bounded, re-validating each hop
/// through `guarded_client`'s exact same host allowlist and IP pinning
/// rather than ever trusting reqwest's own blind redirect chasing.
pub async fn fetch_media_bytes(
    jar: Arc<reqwest::cookie::Jar>,
    url_str: &str,
) -> Result<Vec<u8>, String> {
    use super::types::{MAX_MEDIA_BYTES, MAX_MEDIA_REDIRECTS};

    let mut current = Url::parse(url_str).map_err(|_| "invalid media URL".to_string())?;
    let mut response = None;

    for _ in 0..MAX_MEDIA_REDIRECTS {
        let client = guarded_client(&current, jar.clone()).await?;
        let resp = client
            .get(current.clone())
            .send()
            .await
            .map_err(|_| "failed to fetch media".to_string())?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| "media redirect with no Location header".to_string())?
                .to_string();
            current = current
                .join(&location)
                .map_err(|_| "invalid media redirect target".to_string())?;
            continue;
        }
        response = Some(resp);
        break;
    }
    let mut response =
        response.ok_or_else(|| "too many redirects downloading media".to_string())?;

    if !response.status().is_success() {
        return Err(format!(
            "bad status {} downloading media",
            response.status()
        ));
    }
    if let Some(len) = response.content_length() {
        if len > MAX_MEDIA_BYTES {
            return Err("media file too large".to_string());
        }
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "media stream error".to_string())?
    {
        if bytes.len() as u64 + chunk.len() as u64 > MAX_MEDIA_BYTES {
            return Err("media file too large".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_instagram_and_its_cdn_hosts() {
        assert!(host_is_allowed(
            &Url::parse("https://www.instagram.com/reel/abc/").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://instagram.com/reel/abc/").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://static.cdninstagram.com/x.js").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://scontent-lax3-1.cdninstagram.com/v/x.mp4").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://video.xx.fbcdn.net/v/x.mp4").unwrap()
        ));
    }

    #[test]
    fn rejects_lookalike_and_unrelated_hosts() {
        assert!(!host_is_allowed(
            &Url::parse("https://evil-instagram.com/reel/abc/").unwrap()
        ));
        assert!(!host_is_allowed(
            &Url::parse("https://instagram.com.evil.com/reel/abc/").unwrap()
        ));
        assert!(!host_is_allowed(
            &Url::parse("http://169.254.169.254/latest/meta-data/").unwrap()
        ));
        assert!(!host_is_allowed(
            &Url::parse("https://example.com/").unwrap()
        ));
    }
}
