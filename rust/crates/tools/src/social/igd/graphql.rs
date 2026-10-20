//! Executes Instagram's own persisted-query GraphQL call
//! (`/graphql/query`, the "legacy" endpoint — Instagram's newer
//! `/api/graphql/` silently 200s with the SPA shell instead of dispatching
//! unless the request carries browser-computed signals plain HTTP can't
//! reproduce, but this older endpoint dispatches correctly with just
//! cookies + a CSRF token) and extracts the downloadable media URLs from
//! its response.

use reqwest::Url;
use serde_json::Value;

use super::net::guarded_client;
use super::session::{Session, IG_BASE_URL};
use super::types::{
    GraphQlFailure, GraphQlResult, MediaType, FALLBACK_PROVIDED_VARIABLES, MAX_RETRY_COUNT,
};

const GRAPHQL_URL: &str = "https://www.instagram.com/graphql/query";

/// `provided_variables` is the query's Relay provider list from discovery /
/// the cache; `None` falls back to `FALLBACK_PROVIDED_VARIABLES`. Every
/// provider is sent as `false` — they're client-side feature flags, and
/// `false` is the shape a logged-out web client gets.
pub async fn execute_media_graphql(
    session: &Session,
    shortcode: &str,
    doc_id: &str,
    provided_variables: Option<&[String]>,
) -> GraphQlResult {
    let variables = build_variables(shortcode, provided_variables);
    let attempts = MAX_RETRY_COUNT.max(1);
    let mut last_result = None;

    for attempt in 0..attempts {
        let result = execute_media_graphql_once(session, &variables, doc_id).await;
        let is_transient =
            result.status.is_none() || result.status.map(|s| s >= 500).unwrap_or(false);

        if result.success || !is_transient || attempt + 1 == attempts {
            return result;
        }
        last_result = Some(result);
    }
    last_result.expect("loop runs at least once")
}

fn build_variables(shortcode: &str, provided_variables: Option<&[String]>) -> Value {
    let mut variables = serde_json::Map::new();
    variables.insert(
        "shortcode".to_string(),
        Value::String(shortcode.to_string()),
    );
    match provided_variables {
        Some(names) => names.iter().for_each(|n| {
            variables.insert(n.clone(), Value::Bool(false));
        }),
        None => FALLBACK_PROVIDED_VARIABLES.iter().for_each(|n| {
            variables.insert((*n).to_string(), Value::Bool(false));
        }),
    }
    Value::Object(variables)
}

async fn execute_media_graphql_once(
    session: &Session,
    variables: &Value,
    doc_id: &str,
) -> GraphQlResult {
    let form: Vec<(&str, String)> = vec![
        ("doc_id", doc_id.to_string()),
        ("variables", variables.to_string()),
        ("server_timestamps", "true".to_string()),
    ];
    let referer = session
        .referer
        .clone()
        .unwrap_or_else(|| format!("{IG_BASE_URL}/"));

    let url = match Url::parse(GRAPHQL_URL) {
        Ok(u) => u,
        Err(e) => return failure_result(e.to_string()),
    };
    let client = match guarded_client(&url, session.jar.clone()).await {
        Ok(c) => c,
        Err(e) => return failure_result(e),
    };

    let mut request = client
        .post(url)
        .header("Accept", "*/*")
        .header("Origin", IG_BASE_URL)
        .header("Referer", referer)
        .form(&form);
    if let Some(csrf) = &session.csrf_token {
        request = request.header("X-CSRFToken", csrf.clone());
    }

    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => return failure_result(e.to_string()),
    };
    let status = response.status();
    let body = match response.text().await {
        Ok(b) => b,
        Err(e) => {
            return GraphQlResult {
                success: false,
                status: Some(status.as_u16()),
                json: None,
                error: Some(e.to_string()),
            }
        }
    };

    let json: Option<Value> = serde_json::from_str(&body).ok();
    let success = status.is_success()
        && json
            .as_ref()
            .and_then(|j| j.get("data"))
            .map(|d| !d.is_null())
            .unwrap_or(false);

    GraphQlResult {
        success,
        status: Some(status.as_u16()),
        json,
        error: if success { None } else { Some(body) },
    }
}

fn failure_result(error: String) -> GraphQlResult {
    GraphQlResult {
        success: false,
        status: None,
        json: None,
        error: Some(error),
    }
}

pub fn classify_graphql_failure(result: &GraphQlResult) -> GraphQlFailure {
    let Some(status) = result.status else {
        return GraphQlFailure::HttpError;
    };
    if status == 429 {
        return GraphQlFailure::RateLimited;
    }
    if !(200..300).contains(&status) {
        return GraphQlFailure::HttpError;
    }

    let Some(json) = &result.json else {
        return GraphQlFailure::InvalidJson;
    };

    if let Some(errors) = json.get("errors").and_then(|e| e.as_array()) {
        let combined: String = errors
            .iter()
            .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
            .collect::<Vec<_>>()
            .join(" ");
        let lower = combined.to_lowercase();
        if lower.contains("login") || lower.contains("not authorized") {
            return GraphQlFailure::LoginRequired;
        }
        if lower.contains("rate") {
            return GraphQlFailure::RateLimited;
        }
        return GraphQlFailure::ExecutionError;
    }

    let data = json.get("data");
    if data.map(|d| d.is_null()).unwrap_or(true) {
        return GraphQlFailure::NullData;
    }

    let web_info = data.and_then(|d| d.get("xdt_api__v1__media__shortcode__web_info"));
    match web_info {
        None => GraphQlFailure::InvalidDocId,
        Some(Value::Null) => GraphQlFailure::MediaUnavailable,
        Some(_) => GraphQlFailure::Unknown,
    }
}

/// Failures where the persisted-query `doc_id` is likely stale and
/// rediscovery should be attempted, rather than failing outright.
pub fn is_stale_doc_id_failure(failure: GraphQlFailure) -> bool {
    !matches!(
        failure,
        GraphQlFailure::RateLimited
            | GraphQlFailure::LoginRequired
            | GraphQlFailure::MediaUnavailable
    )
}

/// One downloadable media item's URL and type, still unfetched — `super::
/// fetch_all_media` is what actually downloads the bytes, through `net`'s
/// SSRF-hardened client.
pub struct MediaRef {
    pub url: String,
    pub media_type: MediaType,
}

fn best_video_url(item: &Value) -> Option<String> {
    item.get("video_versions")
        .and_then(|v| v.as_array())
        .and_then(|versions| versions.first())
        .and_then(|v| v.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

fn best_image_url(item: &Value) -> Option<String> {
    item.get("image_versions2")
        .and_then(|v| v.get("candidates"))
        .and_then(|c| c.as_array())
        .and_then(|candidates| candidates.first())
        .and_then(|c| c.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

fn collect_media_items(item: &Value) -> Vec<MediaRef> {
    if let Some(carousel) = item.get("carousel_media").and_then(|c| c.as_array()) {
        return carousel
            .iter()
            .flat_map(|child| {
                best_video_url(child)
                    .map(|url| MediaRef {
                        url,
                        media_type: MediaType::Video,
                    })
                    .or_else(|| {
                        best_image_url(child).map(|url| MediaRef {
                            url,
                            media_type: MediaType::Image,
                        })
                    })
            })
            .collect();
    }

    if let Some(url) = best_video_url(item) {
        return vec![MediaRef {
            url,
            media_type: MediaType::Video,
        }];
    }
    if let Some(url) = best_image_url(item) {
        return vec![MediaRef {
            url,
            media_type: MediaType::Image,
        }];
    }
    Vec::new()
}

/// Pulls every downloadable media reference out of a successful GraphQL
/// response's `data` field. `Err` for a response shaped in a way this
/// module doesn't recognize at all (Instagram changed the schema) — a
/// *reachable, empty* result (a post with no media Instagram will show) is
/// already ruled out one level up, via `classify_graphql_failure`'s
/// `MediaUnavailable`/`NullData` cases.
pub fn extract_media_refs(shortcode: &str, data: &Value) -> Result<Vec<MediaRef>, String> {
    let item = data
        .get("xdt_api__v1__media__shortcode__web_info")
        .and_then(|w| w.get("items"))
        .and_then(|items| items.as_array())
        .and_then(|items| items.first())
        .ok_or_else(|| format!("no media items in response for {shortcode}"))?;

    let media = collect_media_items(item);
    if media.is_empty() {
        return Err("no downloadable media found in response".to_string());
    }
    Ok(media)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_media_refs_reads_a_single_video() {
        let data = serde_json::json!({
            "xdt_api__v1__media__shortcode__web_info": {
                "items": [{ "video_versions": [{ "url": "https://scontent.cdninstagram.com/v.mp4" }] }]
            }
        });
        let refs = extract_media_refs("abc", &data).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://scontent.cdninstagram.com/v.mp4");
        assert_eq!(refs[0].media_type, MediaType::Video);
    }

    #[test]
    fn extract_media_refs_reads_a_single_image() {
        let data = serde_json::json!({
            "xdt_api__v1__media__shortcode__web_info": {
                "items": [{ "image_versions2": { "candidates": [{ "url": "https://scontent.cdninstagram.com/p.jpg" }] } }]
            }
        });
        let refs = extract_media_refs("abc", &data).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].media_type, MediaType::Image);
    }

    #[test]
    fn extract_media_refs_reads_a_mixed_carousel() {
        let data = serde_json::json!({
            "xdt_api__v1__media__shortcode__web_info": {
                "items": [{
                    "carousel_media": [
                        { "image_versions2": { "candidates": [{ "url": "https://scontent.cdninstagram.com/p1.jpg" }] } },
                        { "video_versions": [{ "url": "https://scontent.cdninstagram.com/v1.mp4" }] },
                    ]
                }]
            }
        });
        let refs = extract_media_refs("abc", &data).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].media_type, MediaType::Image);
        assert_eq!(refs[1].media_type, MediaType::Video);
    }

    #[test]
    fn extract_media_refs_errors_on_no_downloadable_media() {
        let data = serde_json::json!({
            "xdt_api__v1__media__shortcode__web_info": { "items": [{}] }
        });
        assert!(extract_media_refs("abc", &data).is_err());
    }

    #[test]
    fn extract_media_refs_errors_on_unrecognized_shape() {
        let data = serde_json::json!({ "something_else": true });
        assert!(extract_media_refs("abc", &data).is_err());
    }

    #[test]
    fn classify_graphql_failure_reads_login_required_from_error_message() {
        let result = GraphQlResult {
            success: false,
            status: Some(200),
            json: Some(
                serde_json::json!({ "errors": [{ "message": "Login required to view this content" }] }),
            ),
            error: None,
        };
        assert_eq!(
            classify_graphql_failure(&result),
            GraphQlFailure::LoginRequired
        );
    }

    #[test]
    fn classify_graphql_failure_reads_rate_limited_status() {
        let result = GraphQlResult {
            success: false,
            status: Some(429),
            json: None,
            error: None,
        };
        assert_eq!(
            classify_graphql_failure(&result),
            GraphQlFailure::RateLimited
        );
    }

    #[test]
    fn build_variables_uses_discovered_providers_when_given() {
        let names = vec!["__relay_internal__pv__Arelayprovider".to_string()];
        let v = build_variables("abc", Some(&names));
        assert_eq!(
            v,
            serde_json::json!({ "shortcode": "abc", "__relay_internal__pv__Arelayprovider": false })
        );
    }

    #[test]
    fn build_variables_falls_back_when_providers_unknown() {
        let v = build_variables("abc", None);
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 1 + FALLBACK_PROVIDED_VARIABLES.len());
        assert!(FALLBACK_PROVIDED_VARIABLES
            .iter()
            .all(|n| obj.get(*n) == Some(&Value::Bool(false))));
    }

    #[test]
    fn is_stale_doc_id_failure_excludes_terminal_failures() {
        assert!(!is_stale_doc_id_failure(GraphQlFailure::RateLimited));
        assert!(!is_stale_doc_id_failure(GraphQlFailure::LoginRequired));
        assert!(!is_stale_doc_id_failure(GraphQlFailure::MediaUnavailable));
        assert!(is_stale_doc_id_failure(GraphQlFailure::InvalidDocId));
    }
}
