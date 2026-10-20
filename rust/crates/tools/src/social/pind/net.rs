//! SSRF hardening for every outbound request `ore::pind` makes — see the
//! parent module's doc comment for the full threat model. Every request
//! (short-link resolution, the pin metadata call, each media fetch) goes
//! through `guarded_client` before a byte is sent, and every redirect is
//! followed by `guarded_get` one hop at a time, re-validating each
//! `Location` the same way rather than letting reqwest chase it blindly.
//!
//! Deliberately its own hardening rather than reusing `ore::fbd::net` etc.
//! — each platform's allowlist stays local to the module that gates it, so
//! they never drift into each other.

use std::net::SocketAddr;

use reqwest::{Client, Response, Url};

use crate::social::is_blocked_ip;

use super::types::{MAX_API_BYTES, MAX_MEDIA_BYTES, MAX_REDIRECTS, REQUEST_TIMEOUT, USER_AGENT};

/// Every real Pinterest-controlled host this scraper legitimately talks to:
/// `pinterest.com` (pin pages, the `PinResource` endpoint, and
/// `api.pinterest.com`, which a `pin.it` link redirects through), `pin.it`
/// (the short-link host itself), and `pinimg.com` (image/video CDN). Anything
/// else is rejected before a byte is sent, wherever the URL came from.
const ALLOWED_HOST_SUFFIXES: &[&str] = &["pinterest.com", "pin.it", "pinimg.com"];

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
            "SECURITY_ALERT pind outbound fetch blocked — host resolved to a non-public address"
        );
        return Err("blocked destination".to_string());
    }
    Ok(addr)
}

/// Builds a one-off `reqwest::Client` pinned to `url`'s already-validated,
/// already-resolved address (rebuilt per request — see `ore::igd::net::
/// guarded_client`). Redirects are never auto-followed. No cookie jar:
/// every call here worked with none, so there's no session state to leak
/// between requests.
async fn guarded_client(url: &Url) -> Result<Client, String> {
    if url.scheme() != "https" {
        return Err("only https URLs are supported".to_string());
    }
    if !host_is_allowed(url) {
        tracing::warn!(
            target: "security",
            url = %url,
            "SECURITY_ALERT pind outbound fetch blocked — host outside the Pinterest allowlist"
        );
        return Err("that host isn't Pinterest — refusing to fetch it".to_string());
    }
    let addr = resolve_pinned(url).await?;
    let host = url
        .host_str()
        .expect("checked by host_is_allowed")
        .to_string();

    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .connect_timeout(REQUEST_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .resolve(&host, addr)
        .build()
        .map_err(|e| format!("building request client: {e}"))
}

/// Issues a `GET` at `start_url`, following redirects manually up to
/// `MAX_REDIRECTS` hops, each re-validated through `guarded_client`.
pub async fn guarded_get(
    start_url: &str,
    extra_headers: &[(&str, &str)],
) -> Result<Response, String> {
    let mut current = Url::parse(start_url).map_err(|_| "invalid Pinterest URL".to_string())?;

    for _ in 0..MAX_REDIRECTS {
        let client = guarded_client(&current).await?;
        let mut request = client.get(current.clone());
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| "failed to fetch Pinterest URL".to_string())?;

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

    Err("too many redirects resolving that Pinterest URL".to_string())
}

/// Follows `url`'s redirect chain and returns the final URL, never reading
/// the final response's body (a pin page is >1 MB; only its address matters
/// for a `pin.it` short link).
pub async fn resolve_final_url(url: &str) -> Result<String, String> {
    let response = guarded_get(
        url,
        &[("Accept", "text/html"), ("Accept-Encoding", "identity")],
    )
    .await?;
    Ok(response.url().to_string())
}

/// Fetches the pin's `PinResource` JSON, capped at `MAX_API_BYTES`.
/// `pin_id` must already be a validated all-digit string (`parse::
/// extract_pin_id`); the request URL is built through `query_pairs_mut`, so
/// even a bad value couldn't alter its structure. The `Accept-Encoding:
/// identity` header is required, not cosmetic: Pinterest gzips responses
/// regardless, and this workspace's reqwest has no decompression feature.
/// `X-Pinterest-PWS-Handler` is what separates a `200` from a `403` here.
pub async fn fetch_pin_json(pin_id: &str) -> Result<String, String> {
    let mut url =
        Url::parse("https://www.pinterest.com/resource/PinResource/get/").expect("static URL");
    let data = serde_json::json!({
        "options": { "id": pin_id, "field_set_key": "unauth_react_main_pin" },
        "context": {}
    })
    .to_string();
    url.query_pairs_mut()
        .append_pair("source_url", &format!("/pin/{pin_id}/"))
        .append_pair("data", &data);

    let mut response = guarded_get(
        url.as_str(),
        &[
            ("Accept", "application/json"),
            ("Accept-Encoding", "identity"),
            ("X-Pinterest-PWS-Handler", "www/pin/[id].js"),
        ],
    )
    .await?;
    if !response.status().is_success() {
        return Err(format!(
            "Pinterest returned status {} for that pin",
            response.status()
        ));
    }

    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Pinterest response stream error".to_string())?
    {
        if bytes.len() + chunk.len() > MAX_API_BYTES {
            return Err("Pinterest response too large".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| "unexpected response from Pinterest".to_string())
}

/// Fetches `url`'s body, streamed chunk-by-chunk and refused past `limit`
/// bytes (checked against `Content-Length` up front and again as bytes
/// arrive). `what` only labels the error.
pub async fn fetch_capped(url_str: &str, limit: u64, what: &str) -> Result<Vec<u8>, String> {
    let mut response = guarded_get(url_str, &[("Accept-Encoding", "identity")]).await?;
    if !response.status().is_success() {
        return Err(format!(
            "bad status {} downloading {what}",
            response.status()
        ));
    }
    if response.content_length().is_some_and(|len| len > limit) {
        return Err(format!("{what} too large"));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| format!("{what} stream error"))?
    {
        if bytes.len() as u64 + chunk.len() as u64 > limit {
            return Err(format!("{what} too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Fetches one media file's bytes, capped at `MAX_MEDIA_BYTES`.
pub async fn fetch_media_bytes(url_str: &str) -> Result<Vec<u8>, String> {
    fetch_capped(url_str, MAX_MEDIA_BYTES, "media file").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_pinterest_shortlink_and_cdn_hosts() {
        assert!(host_is_allowed(
            &Url::parse("https://www.pinterest.com/pin/1/").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://api.pinterest.com/url_shortener/x/redirect/").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://pin.it/4qRKpmw").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://i.pinimg.com/originals/a.jpg").unwrap()
        ));
        assert!(host_is_allowed(
            &Url::parse("https://v1.pinimg.com/videos/a.mp4").unwrap()
        ));
    }

    #[test]
    fn rejects_lookalike_and_unrelated_hosts() {
        assert!(!host_is_allowed(
            &Url::parse("https://evil-pinterest.com/x").unwrap()
        ));
        assert!(!host_is_allowed(
            &Url::parse("https://pinterest.com.evil.com/x").unwrap()
        ));
        assert!(!host_is_allowed(
            &Url::parse("https://notpin.it/x").unwrap()
        ));
        assert!(!host_is_allowed(
            &Url::parse("http://169.254.169.254/latest/meta-data/").unwrap()
        ));
        assert!(!host_is_allowed(
            &Url::parse("https://example.com/").unwrap()
        ));
    }
}
