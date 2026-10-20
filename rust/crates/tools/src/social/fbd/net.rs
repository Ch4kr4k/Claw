//! SSRF hardening for every outbound request `ore::fbd` makes — see the
//! parent module's doc comment for the full threat model. Every request —
//! resolving a share link, fetching whatever page a redirect hop lands on,
//! or fetching the video CDN URL lifted out of that page's own embedded
//! JSON — goes through `guarded_client` before a single byte is sent, and
//! every redirect (both the share-link chain and the CDN's own edge-host
//! redirect) is followed by `guarded_get`, one hop at a time, re-validating
//! each `Location` the exact same way rather than ever letting reqwest
//! chase one blindly.
//!
//! Deliberately its own hardening rather than reusing `ore::igd::net` or
//! `ore::xrd::net` — same rationale as those modules' own doc comments:
//! each platform's allowlist stays local to the module that actually gates
//! it, so the three never drift into each other.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::{Client, Response, Url};

use crate::social::is_blocked_ip;

use super::types::{MAX_REDIRECTS, REQUEST_TIMEOUT, USER_AGENT};

/// Every real Facebook-controlled host this scraper legitimately talks to:
/// the share-link/page host (`facebook.com`, covers `www.facebook.com` and
/// a bare `facebook.com`) and its video CDN (`fbcdn.net` — the edge host
/// named in the page, e.g. `video.fdel18-1.fna.fbcdn.net`, and whatever
/// `*.fbcdn.net` host its own redirect points at, e.g. `video.xx.fbcdn.net`
/// — both live under this one suffix). Anything else is rejected before a
/// single byte is sent, regardless of where the URL came from.
const ALLOWED_HOST_SUFFIXES: &[&str] = &["facebook.com", "fbcdn.net"];

fn host_is_allowed(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    ALLOWED_HOST_SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

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
            "SECURITY_ALERT fbd outbound fetch blocked — host resolved to a non-public address"
        );
        return Err("blocked destination".to_string());
    }
    Ok(addr)
}

/// Builds a one-off `reqwest::Client` pinned to `url`'s already-validated,
/// already-resolved address — see `ore::igd::net::guarded_client`'s doc
/// comment for why this is rebuilt per request rather than once per
/// session. Redirects are never auto-followed here: `guarded_get` follows
/// them itself, one hop at a time, so every hop passes back through this
/// exact host-allowlist and IP-pinning check before it's requested.
async fn guarded_client(url: &Url, jar: Arc<reqwest::cookie::Jar>) -> Result<Client, String> {
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err("only http/https URLs are supported".to_string());
    }
    if !host_is_allowed(url) {
        tracing::warn!(
            target: "security",
            url = %url,
            "SECURITY_ALERT fbd outbound fetch blocked — host outside the Facebook allowlist"
        );
        return Err("that host isn't Facebook — refusing to fetch it".to_string());
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

/// Issues a `GET` at `start_url`, following redirects manually up to
/// `MAX_REDIRECTS` hops — each hop's `Location` is resolved against the
/// current URL and re-validated through `guarded_client`'s host allowlist
/// and IP pinning exactly like the first request, so a redirect can never
/// smuggle a fetch to a host outside this module's allowlist. Used for
/// both the share-link → canonical-page resolution (which needs it because
/// a share link always redirects) and the video CDN fetch (which needs it
/// because Facebook's own edge-host `Location` redirect is a real, observed
/// part of actually getting the bytes — see `types::MAX_REDIRECTS`'s doc
/// comment). `extra_headers` lets the caller send page-fetch-only headers
/// (`Accept`, `Accept-Language`, `Sec-Fetch-*`) without this function
/// needing to know which kind of request it's making.
pub async fn guarded_get(
    jar: Arc<reqwest::cookie::Jar>,
    start_url: &str,
    extra_headers: &[(&str, &str)],
) -> Result<Response, String> {
    let mut current = Url::parse(start_url).map_err(|_| "invalid Facebook URL".to_string())?;

    for _ in 0..MAX_REDIRECTS {
        let client = guarded_client(&current, jar.clone()).await?;
        let mut request = client.get(current.clone());
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| "failed to fetch Facebook URL".to_string())?;

        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| "redirect with no Location header".to_string())?
                .to_string();
            current = current
                .join(&location)
                .map_err(|_| "invalid redirect target".to_string())?;
            continue;
        }

        return Ok(response);
    }

    Err("too many redirects resolving that Facebook URL".to_string())
}

/// Fetches the video's bytes, capped at `super::types::MAX_MEDIA_BYTES` and
/// streamed chunk-by-chunk — identical shape to `ore::igd::net::
/// fetch_media_bytes`/`ore::xrd::net::fetch_media_bytes`, just routed
/// through `guarded_get` rather than a single direct request, since this
/// URL's own edge-host redirect (see `types::MAX_REDIRECTS`'s doc comment)
/// has to be followed to reach the actual bytes at all.
pub async fn fetch_media_bytes(
    jar: Arc<reqwest::cookie::Jar>,
    url_str: &str,
) -> Result<Vec<u8>, String> {
    use super::types::MAX_MEDIA_BYTES;

    let mut response = guarded_get(jar, url_str, &[]).await?;
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
    fn allows_facebook_and_its_video_cdn_hosts() {
        assert!(host_is_allowed(
            &Url::parse("https://www.facebook.com/reel/123/").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://facebook.com/share/r/abc/").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://video.fdel18-1.fna.fbcdn.net/x.mp4").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://video.xx.fbcdn.net/x.mp4").unwrap()
        ));
    }

    #[test]
    fn rejects_lookalike_and_unrelated_hosts() {
        assert!(!host_is_allowed(
            &Url::parse("https://evil-facebook.com/x").unwrap()
        ));
        assert!(!host_is_allowed(
            &Url::parse("https://facebook.com.evil.com/x").unwrap()
        ));
        assert!(!host_is_allowed(
            &Url::parse("http://169.254.169.254/latest/meta-data/").unwrap()
        ));
        assert!(!host_is_allowed(
            &Url::parse("https://example.com/").unwrap()
        ));
    }
}
