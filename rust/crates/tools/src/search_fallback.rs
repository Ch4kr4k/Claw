//! Search engines for `WebSearch` (and `GoogleDork`) besides DuckDuckGo's
//! HTML endpoint, which answers scripted requests from some addresses with a
//! bot check that holds no results:
//! - a local SearXNG (`scripts/install-searxng.sh`), asked first: it merges
//!   Google, DuckDuckGo, Brave, Bing, Wikipedia and more, so one engine
//!   blocking us does not empty the results;
//! - after DuckDuckGo, Brave Search, then Bing, read from their result pages.
//!
//! After a DuckDuckGo bot check, DuckDuckGo is skipped for
//! [`DUCKDUCKGO_BACKOFF`] rather than asked again on every search, which
//! would keep the block alive.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use serde_json::Value;

use super::{decode_html_entities, SearchHit};

/// What claw's searches identify as: a bot, honestly, in the usual format.
pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (compatible; claw-code/0.1; +web search tool)";
/// Brave and Bing only serve result pages to browsers.
const BROWSER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
/// How long DuckDuckGo is left alone after its bot check.
const DUCKDUCKGO_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// Where SearXNG listens unless `CLAW_SEARXNG_URL` says otherwise.
const DEFAULT_SEARXNG: &str = "http://127.0.0.1:8888";

static DUCKDUCKGO_BLOCKED_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

pub(crate) fn duckduckgo_blocked() -> bool {
    DUCKDUCKGO_BLOCKED_UNTIL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some_and(|until| Instant::now() < until)
}

pub(crate) fn block_duckduckgo() {
    *DUCKDUCKGO_BLOCKED_UNTIL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(Instant::now() + DUCKDUCKGO_BACKOFF);
}

/// Results from the local SearXNG (`CLAW_SEARXNG_URL`, default
/// 127.0.0.1:8888; `off` skips it), named after the engines that answered.
/// `None` when it is not running or found nothing.
pub(crate) fn searxng(query: &str) -> Option<(String, Vec<SearchHit>)> {
    let base = std::env::var("CLAW_SEARXNG_URL").unwrap_or_else(|_| DEFAULT_SEARXNG.to_string());
    if base.trim().is_empty() || base.eq_ignore_ascii_case("off") {
        return None;
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_secs(20))
        .user_agent(USER_AGENT)
        .build()
        .ok()?;
    let url = reqwest::Url::parse_with_params(
        &format!("{}/search", base.trim_end_matches('/')),
        &[("q", query), ("format", "json")],
    )
    .ok()?;
    let body: Value = client.get(url).send().ok()?.json().ok()?;
    let mut engines: Vec<String> = Vec::new();
    let hits: Vec<SearchHit> = body
        .get("results")?
        .as_array()?
        .iter()
        .filter_map(|result| {
            let url = result.get("url")?.as_str()?.to_string();
            for engine in result
                .get("engines")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !engines.iter().any(|known| known == engine) {
                    engines.push(engine.to_string());
                }
            }
            Some(SearchHit {
                title: result
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|title| !title.is_empty())
                    .unwrap_or(&url)
                    .to_string(),
                snippet: result
                    .get("content")
                    .and_then(Value::as_str)
                    .and_then(|text| {
                        snippet(text.split_whitespace().collect::<Vec<_>>().join(" "))
                    }),
                url,
            })
        })
        .collect();
    if hits.is_empty() {
        return None;
    }
    Some((format!("SearXNG: {}", engines.join(", ")), hits))
}

/// Results from the first fallback engine that has any, with its name.
pub(crate) fn search(query: &str) -> Option<(&'static str, Vec<SearchHit>)> {
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(BROWSER_AGENT)
        .build()
        .ok()?;
    let fetch = |base: &str| -> Option<String> {
        let url = reqwest::Url::parse_with_params(base, &[("q", query)]).ok()?;
        let response = client.get(url).send().ok()?;
        response
            .status()
            .is_success()
            .then(|| response.text().ok())?
    };
    if let Some(hits) = fetch("https://search.brave.com/search")
        .map(|html| brave_hits(&html))
        .filter(|hits| !hits.is_empty())
    {
        return Some(("Brave Search", hits));
    }
    fetch("https://www.bing.com/search")
        .map(|html| bing_hits(&html))
        .filter(|hits| !hits.is_empty())
        .map(|hits| ("Bing", hits))
}

/// The text of an HTML fragment: tags removed, entities decoded, spaces
/// collapsed.
fn text_of(fragment: &str) -> String {
    let mut text = String::with_capacity(fragment.len());
    let mut in_tag = false;
    for ch in fragment.chars() {
        match ch {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    decode_html_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The value of `attribute="…"` after `from` in `html`.
fn attribute<'a>(html: &'a str, attribute: &str) -> Option<&'a str> {
    let marker = format!("{attribute}=\"");
    let start = html.find(&marker)? + marker.len();
    let end = html[start..].find('"')? + start;
    Some(&html[start..end])
}

fn snippet(text: String) -> Option<String> {
    let text: String = text.chars().take(300).collect();
    (!text.is_empty()).then_some(text)
}

fn brave_hits(html: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for block in html.split("data-type=\"web\"").skip(1) {
        let block = block.split("data-type=\"").next().unwrap_or(block);
        let Some(url) = attribute(block, "href") else {
            continue;
        };
        if !url.starts_with("http") || url.contains("search.brave.com") {
            continue;
        }
        let title = block
            .find("search-snippet-title")
            .and_then(|at| attribute(&block[at..], "title"))
            .map(decode_html_entities)
            .unwrap_or_else(|| url.to_string());
        let body = block.find("</a>").map_or("", |at| &block[at..]);
        // The last result runs into the page footer.
        let body = body.split("Find elsewhere").next().unwrap_or(body);
        hits.push(SearchHit {
            title,
            url: decode_html_entities(url),
            snippet: snippet(text_of(body)),
        });
    }
    hits
}

fn bing_hits(html: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for block in html.split("<li class=\"b_algo\"").skip(1) {
        let block = block.split("</li>").next().unwrap_or(block);
        let Some(heading) = block.find("<h2") else {
            continue;
        };
        let heading = &block[heading..];
        let Some(href) = attribute(heading, "href") else {
            continue;
        };
        let url = unwrap_bing_link(&decode_html_entities(href));
        if !url.starts_with("http") {
            continue;
        }
        let title_end = heading.find("</a>").unwrap_or(heading.len());
        let title_start = heading[..title_end].rfind('>').map_or(0, |at| at + 1);
        let anchor = heading.find("<a").map_or(title_start, |at| at);
        let title = text_of(&heading[anchor..title_end]);
        let body = block
            .find("<p")
            .map(|at| &block[at..])
            .map(|rest| &rest[..rest.find("</p>").unwrap_or(rest.len())])
            .unwrap_or("");
        hits.push(SearchHit {
            title: if title.is_empty() { url.clone() } else { title },
            url,
            snippet: snippet(text_of(body)),
        });
    }
    hits
}

/// Bing wraps results in `bing.com/ck/a?…&u=a1<base64url of the target>`.
fn unwrap_bing_link(url: &str) -> String {
    if !url.contains("bing.com/ck/a") {
        return url.to_string();
    }
    url.split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("u=a1"))
        .and_then(super::media::base64_decode)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| url.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_brave_results() {
        let html = r#"<div data-type="web" x><a href="https://example.com/a" class="l1"><div class="title search-snippet-title" title="Example &amp; A">Example</div></a><p>First <b>result</b> text</p></div><div data-type="web"><a href="https://search.brave.com/x">skip</a></div>"#;
        let hits = super::brave_hits(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Example & A");
        assert_eq!(hits[0].url, "https://example.com/a");
        assert_eq!(hits[0].snippet.as_deref(), Some("First result text"));
    }

    #[test]
    fn reads_bing_results_and_unwraps_links() {
        let html = r#"<li class="b_algo"><h2><a href="https://www.bing.com/ck/a?!&amp;&amp;p=1&amp;u=a1aHR0cHM6Ly9leGFtcGxlLm9yZy9i&amp;ntb=1">Example <strong>B</strong></a></h2><p class="b_lineclamp2">Snippet B</p></li>"#;
        let hits = super::bing_hits(html);
        assert_eq!(hits.len(), 1, "{:?}", hits.len());
        assert_eq!(hits[0].url, "https://example.org/b");
        assert_eq!(hits[0].title, "Example B");
        assert_eq!(hits[0].snippet.as_deref(), Some("Snippet B"));
    }
}
