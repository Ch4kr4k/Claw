//! Rediscovers Instagram's current GraphQL persisted-query `doc_id` when
//! `cache::read_operation_cache` comes up empty or stale: scans the post
//! page's own JS bundle URLs for the one that registers the target
//! operation's Relay module, then pulls the numeric id out of its
//! `__d(...)` registration. This mirrors how Instagram's own web client
//! resolves the same id client-side — nothing here is a documented API,
//! it's pattern-matching against the same minified JS a real browser
//! would also have to parse to make the same GraphQL call.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use reqwest::Url;

use super::net::guarded_client;
use super::session::Session;
use super::types::{GraphQlOperation, MAX_PROVIDED_VARIABLES};

struct Candidate {
    url: String,
    content: String,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn looks_like_javascript(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.ends_with(".js")
}

/// Order-preserving (`Vec` + a `HashSet` used only for membership testing,
/// same pattern `normalize_and_deduplicate_urls` below already uses) —
/// deliberately **not** collected through a bare `HashSet<String>` the way
/// an earlier version of this function did. `discover_graphql_operation`
/// truncates this list to `max_bundle_downloads` before searching it (a
/// real post page can embed 400+ bundle URLs, far more than are worth
/// downloading for one lookup) — a `HashSet`'s iteration order depends on
/// Rust's per-process-randomized default hasher, so that truncation was
/// silently keeping a different random ~120-of-477 bundles on every
/// process run. With the actual target operation living in only a
/// handful of those hundreds of bundles, that made discovery intermittent
/// by pure luck of the random seed — confirmed live: `PolarisPostRootQuery`
/// really was present in exactly 4 of 477 bundles on a real post page, and
/// a randomly-ordered 120-of-477 sample misses all four roughly 30% of the
/// time. Preserving the page's own script order instead makes truncation
/// deterministic and reproducible run to run (still no guarantee the
/// target bundle is early, but at least a failure is now a real "not
/// found in the first N" signal, not a coin flip).
pub fn extract_javascript_bundle_urls(html: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut results: Vec<String> = Vec::new();

    let script_src_re = Regex::new(r#"<script\b[^>]*\bsrc="([^"]+)"[^>]*>"#).unwrap();
    for caps in script_src_re.captures_iter(html) {
        let src = &caps[1];
        if src.starts_with("data:") {
            continue;
        }
        if seen.insert(src.to_string()) {
            results.push(src.to_string());
        }
    }

    // Instagram's SSR HTML also embeds bundle URLs inside JSON blobs with
    // every `/` escaped as `\/` — normalize those escapes away first so a
    // URL with several path segments (the common case for a real CDN
    // bundle path) is captured in full, rather than stopping at whichever
    // escaped slash happens to fall inside the path rather than between
    // scheme/host components.
    let normalized_html = html.replace("\\/", "/");
    let embedded_re = Regex::new(r#"https://static\.cdninstagram\.com/[^\s"'<>\\]+\.js"#).unwrap();
    for m in embedded_re.find_iter(&normalized_html) {
        let url = m.as_str().to_string();
        if seen.insert(url.clone()) {
            results.push(url);
        }
    }

    results
}

pub fn normalize_and_deduplicate_urls(urls: Vec<String>, base_url: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();

    for raw in urls {
        let mut url = raw.replace("\\/", "/");

        if url.starts_with("//") {
            url = format!("https:{url}");
        } else if url.starts_with('/') {
            url = format!("{base_url}{url}");
        } else if !url.starts_with("https://") && !url.starts_with("http://") {
            continue;
        }

        if !looks_like_javascript(&url) {
            continue;
        }

        if seen.insert(url.clone()) {
            normalized.push(url);
        }
    }

    normalized
}

async fn download_javascript(session: &Session, url: &str) -> Result<String, String> {
    let parsed = Url::parse(url).map_err(|_| "invalid bundle URL".to_string())?;
    let client = guarded_client(&parsed, session.jar.clone()).await?;
    let response = client
        .get(parsed)
        .send()
        .await
        .map_err(|_| "failed to fetch JS bundle".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "bad status {} downloading bundle",
            response.status()
        ));
    }
    response
        .text()
        .await
        .map_err(|_| "failed to read JS bundle".to_string())
}

fn isolate_javascript_module(javascript: &str, module_name: &str) -> Option<String> {
    let marker = format!("__d(\"{module_name}\"");
    let start = javascript.find(&marker)?;

    let next_module_re = Regex::new(r#"__d\(""#).unwrap();
    let end = next_module_re
        .find_iter(&javascript[start + marker.len()..])
        .next()
        .map(|m| start + marker.len() + m.start())
        .unwrap_or(javascript.len());

    Some(javascript[start..end].to_string())
}

fn extract_operation_id(javascript: &str, relay_module_name: &str) -> Option<String> {
    let module_position = javascript.find(relay_module_name)?;

    // The relay operation module is registered as:
    //   __d("Foo_instagramRelayOperation",[],(function(t,n,r,o,a,i){
    //       a.exports="28276397082003286"
    //   }),null);
    // so the id lives a few dozen characters after the module name.
    let exports_re = Regex::new(r#"\.exports\s*=\s*"(\d+)""#).unwrap();

    let close_window_end = (module_position + relay_module_name.len() + 300).min(javascript.len());
    if let Some(caps) = exports_re.captures(&javascript[module_position..close_window_end]) {
        return Some(caps[1].to_string());
    }

    let window_start = module_position.saturating_sub(5000);
    let window_end = (module_position + relay_module_name.len() + 5000).min(javascript.len());
    let nearby_text = &javascript[window_start..window_end];

    if let Some(caps) = exports_re.captures(nearby_text) {
        return Some(caps[1].to_string());
    }

    let id_re = Regex::new(r#"(?:"id"|id)\s*:\s*"(\d+)""#).unwrap();
    if let Some(caps) = id_re.captures(nearby_text) {
        return Some(caps[1].to_string());
    }

    // Minification changed formatting badly enough that the surrounding-window
    // search missed it — fall back to isolating just this module's source.
    let module_text = isolate_javascript_module(javascript, relay_module_name)?;
    exports_re
        .captures(&module_text)
        .or_else(|| id_re.captures(&module_text))
        .map(|caps| caps[1].to_string())
}

/// Reads the operation's Relay `providedVariables` key names out of its
/// compiled query module, which Instagram ships as:
///   params:{id:n("Foo_instagramRelayOperation"),metadata:{...},
///     name:"Foo",operationKind:"query",text:null,
///     providedVariables:{__relay_internal__pv__Barrelayprovider:n("..."),...}}
/// The persisted query refuses ("execution error") any request that omits
/// one of these, so sending the live list is what keeps `/igd` working
/// across a web build that adds a provider without changing the `doc_id`.
/// Only names matching `__relay_internal__pv__[A-Za-z0-9_]+` are accepted
/// (they're serialized as JSON object keys via `serde_json`, so this is a
/// sanity filter, not an injection boundary), capped at
/// `MAX_PROVIDED_VARIABLES`. `None` if the block isn't in this bundle.
fn extract_provided_variables(javascript: &str, operation_name: &str) -> Option<Vec<String>> {
    let block_re = Regex::new(&format!(
        r#"name:"{}",operationKind:"query"[^{{}}]*providedVariables:\{{([^}}]*)\}}"#,
        regex::escape(operation_name)
    ))
    .ok()?;
    let body = block_re.captures(javascript)?.get(1)?.as_str();

    let key_re = Regex::new(r#"(__relay_internal__pv__[A-Za-z0-9_]+)\s*:"#).unwrap();
    let mut names: Vec<String> = Vec::new();
    for caps in key_re.captures_iter(body) {
        let name = caps[1].to_string();
        if !names.contains(&name) {
            names.push(name);
        }
        if names.len() >= MAX_PROVIDED_VARIABLES {
            break;
        }
    }
    Some(names)
}

/// The compiled query module (with `providedVariables`) and the
/// `_instagramRelayOperation` module (with the `doc_id`) aren't guaranteed
/// to share a bundle, so this searches every bundle already downloaded.
fn find_provided_variables<'a>(
    bundles: impl Iterator<Item = &'a str>,
    operation_name: &str,
) -> Option<Vec<String>> {
    bundles
        .filter(|js| js.contains(operation_name))
        .find_map(|js| extract_provided_variables(js, operation_name))
}

pub async fn discover_graphql_operation(
    session: &Session,
    html: &str,
    target_operation_name: &str,
    max_bundle_downloads: usize,
) -> Result<GraphQlOperation, String> {
    let script_urls = extract_javascript_bundle_urls(html);
    if script_urls.is_empty() {
        return Err("no JS bundles found on the post page".to_string());
    }

    let script_urls = normalize_and_deduplicate_urls(script_urls, super::session::IG_BASE_URL);
    let script_urls: Vec<String> = script_urls.into_iter().take(max_bundle_downloads).collect();

    // PHASE 1 — find bundles that mention the query at all.
    let mut downloaded: HashMap<String, String> = HashMap::new();
    let mut candidate_bundles: Vec<Candidate> = Vec::new();

    for script_url in &script_urls {
        let js = match download_javascript(session, script_url).await {
            Ok(js) => js,
            Err(_) => continue,
        };
        let mentions = js.contains(target_operation_name);
        downloaded.insert(script_url.clone(), js.clone());
        if mentions {
            candidate_bundles.push(Candidate {
                url: script_url.clone(),
                content: js,
            });
        }
    }

    if candidate_bundles.is_empty() {
        return Err(format!(
            "{target_operation_name} not found in any JS bundle"
        ));
    }

    // PHASE 2 — look for the relay operation module in a bundle that
    // already mentioned the query.
    let relay_module_name = format!("{target_operation_name}_instagramRelayOperation");

    for candidate in &candidate_bundles {
        if candidate.content.contains(&relay_module_name) {
            if let Some(doc_id) = extract_operation_id(&candidate.content, &relay_module_name) {
                return Ok(GraphQlOperation {
                    operation_name: target_operation_name.to_string(),
                    doc_id,
                    provided_variables: find_provided_variables(
                        candidate_bundles.iter().map(|c| c.content.as_str()),
                        target_operation_name,
                    ),
                    discovered_from_bundle: candidate.url.clone(),
                    discovered_at: now_unix(),
                });
            }
        }
    }

    // PHASE 3 — the operation module may live in a different bundle than
    // the one that merely referenced it.
    for script_url in &script_urls {
        let js = if let Some(cached) = downloaded.get(script_url) {
            cached.clone()
        } else {
            match download_javascript(session, script_url).await {
                Ok(js) => {
                    downloaded.insert(script_url.clone(), js.clone());
                    js
                }
                Err(_) => continue,
            }
        };

        if js.contains(&relay_module_name) {
            if let Some(doc_id) = extract_operation_id(&js, &relay_module_name) {
                return Ok(GraphQlOperation {
                    operation_name: target_operation_name.to_string(),
                    doc_id,
                    provided_variables: find_provided_variables(
                        downloaded.values().map(String::as_str),
                        target_operation_name,
                    ),
                    discovered_from_bundle: script_url.clone(),
                    discovered_at: now_unix(),
                });
            }
        }
    }

    Err("relay operation module found but its doc_id could not be extracted".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_script_src_urls() {
        let html = r#"<script src="https://static.cdninstagram.com/rsrc.php/a.js"></script>"#;
        let urls = extract_javascript_bundle_urls(html);
        assert_eq!(
            urls,
            vec!["https://static.cdninstagram.com/rsrc.php/a.js".to_string()]
        );
    }

    #[test]
    fn extracts_embedded_escaped_urls() {
        let html = r#"{"url":"https:\/\/static.cdninstagram.com\/rsrc.php\/b.js"}"#;
        let urls = extract_javascript_bundle_urls(html);
        assert_eq!(
            urls,
            vec!["https://static.cdninstagram.com/rsrc.php/b.js".to_string()]
        );
    }

    #[test]
    fn ignores_data_uri_script_tags() {
        let html = r#"<script src="data:text/javascript;base64,abc"></script>"#;
        assert!(extract_javascript_bundle_urls(html).is_empty());
    }

    #[test]
    fn normalize_resolves_protocol_relative_and_root_relative_urls() {
        let urls = vec![
            "//static.cdninstagram.com/a.js".to_string(),
            "/static/b.js".to_string(),
        ];
        let normalized = normalize_and_deduplicate_urls(urls, "https://www.instagram.com");
        assert_eq!(
            normalized,
            vec![
                "https://static.cdninstagram.com/a.js".to_string(),
                "https://www.instagram.com/static/b.js".to_string()
            ]
        );
    }

    #[test]
    fn normalize_drops_non_javascript_and_non_http_urls() {
        let urls = vec![
            "https://static.cdninstagram.com/a.css".to_string(),
            "ftp://example.com/a.js".to_string(),
        ];
        assert!(normalize_and_deduplicate_urls(urls, "https://www.instagram.com").is_empty());
    }

    #[test]
    fn extract_operation_id_finds_the_id_right_after_the_module_name() {
        let js = r#"__d("PolarisPostRootQuery_instagramRelayOperation",[],(function(t,n,r,o,a,i){a.exports="28276397082003286"}),null);"#;
        assert_eq!(
            extract_operation_id(js, "PolarisPostRootQuery_instagramRelayOperation"),
            Some("28276397082003286".to_string())
        );
    }

    #[test]
    fn extract_provided_variables_reads_every_provider_name() {
        let js = r#"params:{id:n("PolarisPostRootQuery_instagramRelayOperation"),metadata:{is_distillery:!0,root_field_name:["x"]},name:"PolarisPostRootQuery",operationKind:"query",text:null,providedVariables:{__relay_internal__pv__AEnabledrelayprovider:n("A.relayprovider"),__relay_internal__pv__BEnabledrelayprovider:n("B.relayprovider")}}}"#;
        assert_eq!(
            extract_provided_variables(js, "PolarisPostRootQuery"),
            Some(vec![
                "__relay_internal__pv__AEnabledrelayprovider".to_string(),
                "__relay_internal__pv__BEnabledrelayprovider".to_string(),
            ])
        );
    }

    #[test]
    fn extract_provided_variables_ignores_a_different_operation() {
        let js = r#"name:"OtherQuery",operationKind:"query",text:null,providedVariables:{__relay_internal__pv__Xrelayprovider:n("X")}"#;
        assert_eq!(extract_provided_variables(js, "PolarisPostRootQuery"), None);
    }

    #[test]
    fn extract_provided_variables_distinguishes_empty_from_absent() {
        let js =
            r#"name:"PolarisPostRootQuery",operationKind:"query",text:null,providedVariables:{}"#;
        assert_eq!(
            extract_provided_variables(js, "PolarisPostRootQuery"),
            Some(vec![])
        );
    }

    #[test]
    fn extract_operation_id_returns_none_when_absent() {
        assert_eq!(
            extract_operation_id(
                "no such module here",
                "PolarisPostRootQuery_instagramRelayOperation"
            ),
            None
        );
    }
}
