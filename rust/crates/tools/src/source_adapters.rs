//! WebFetch's source adapters, after "Parkour": for sites with a clean API,
//! fetch the content from the API instead of scraping the page, and start
//! the result with a small YAML header (the "frontmatter envelope") naming
//! the source and the links worth following next, so the model heads for
//! high-signal sources instead of dead ends.
//!
//! - Wikipedia: the article as plain text (MediaWiki API).
//! - arXiv: title, authors, date, categories and abstract (arXiv API),
//!   pointing at the full-text HTML and PDF.
//! - GitHub: a repository's facts and README, a file's raw text, a folder
//!   listing, or an issue / pull request with its comments (REST API;
//!   `GITHUB_TOKEN` raises the rate limit).
//! - Semantic Scholar (and doi.org links): abstract, TL;DR, venue,
//!   citation count and open-access PDF (Graph API; the keyless pool is
//!   often rate-limited, `SEMANTIC_SCHOLAR_API_KEY` avoids that). arXiv
//!   DOIs go to arXiv.
//!
//! Anything else, or an API that fails, goes to the ordinary page fetch.

use std::fmt::Write as _;
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::Value;

/// Longest body returned; the rest is cut with a note.
const MAX_BODY: usize = 60_000;

/// What an adapter fetched.
pub(crate) struct Adapted {
    pub(crate) source: &'static str,
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) next: Vec<(String, String)>,
    pub(crate) body: String,
}

impl Adapted {
    /// The result text: the YAML envelope, then the content.
    pub(crate) fn render(&self) -> String {
        let quote = |text: &str| format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
        let mut out = String::from("---\n");
        let _ = writeln!(out, "source: {}", self.source);
        let _ = writeln!(out, "url: {}", self.url);
        let _ = writeln!(out, "title: {}", quote(&self.title));
        if !self.next.is_empty() {
            out.push_str("next:\n");
            for (label, url) in &self.next {
                let _ = writeln!(out, "  - {}: {url}", quote(label));
            }
        }
        out.push_str("---\n");
        let mut body = self.body.trim().to_string();
        if body.len() > MAX_BODY {
            let mut end = MAX_BODY;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
            body.push_str("\n\n[… cut; the full text is at the url above]");
        }
        out.push_str(&body);
        out
    }
}

/// `Some` when `url` belongs to a source with an adapter.
pub(crate) fn fetch(url: &str) -> Option<Result<Adapted, String>> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed
        .host_str()?
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    let segments: Vec<String> = parsed
        .path_segments()
        .map(|parts| parts.filter(|part| !part.is_empty()).map(decode).collect())
        .unwrap_or_default();
    if host.ends_with("wikipedia.org") && segments.first().map(String::as_str) == Some("wiki") {
        let lang = host.split('.').next().unwrap_or("en").to_string();
        let title = segments[1..].join("/");
        return (!title.is_empty()).then(|| wikipedia(&lang, &title));
    }
    if host == "arxiv.org" || host == "export.arxiv.org" {
        if let [kind, id @ ..] = segments.as_slice() {
            if matches!(kind.as_str(), "abs" | "pdf" | "html") && !id.is_empty() {
                let id = id.join("/").trim_end_matches(".pdf").to_string();
                return Some(arxiv(&id));
            }
        }
        return None;
    }
    if host == "github.com" && segments.len() >= 2 {
        return Some(github(&segments));
    }
    if host.ends_with("semanticscholar.org")
        && segments.first().map(String::as_str) == Some("paper")
    {
        let id = segments.last()?.clone();
        return Some(semantic_scholar(&id, url));
    }
    if host == "doi.org" || host == "dx.doi.org" {
        let doi = segments.join("/");
        // arXiv's own DOIs: straight to arXiv.
        if let Some(id) = doi
            .to_ascii_lowercase()
            .strip_prefix("10.48550/arxiv.")
            .map(str::to_string)
        {
            return Some(arxiv(&id));
        }
        return (!doi.is_empty()).then(|| semantic_scholar(&format!("DOI:{doi}"), url));
    }
    None
}

fn decode(part: &str) -> String {
    let bytes = part.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&part[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("claw-code/0.1 (WebFetch source adapter)")
        .build()
        .map_err(|error| error.to_string())
}

fn get_json(client: &Client, url: &str, github: bool) -> Result<Value, String> {
    let mut request = client.get(url);
    if github {
        request = request.header("Accept", "application/vnd.github+json");
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            request = request.bearer_auth(token);
        }
    }
    // The keyless Semantic Scholar pool is often rate-limited.
    if url.contains("api.semanticscholar.org") {
        if let Ok(key) = std::env::var("SEMANTIC_SCHOLAR_API_KEY") {
            request = request.header("x-api-key", key);
        }
    }
    let response = request.send().map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} from {url}"));
    }
    response.json().map_err(|error| error.to_string())
}

fn get_text(client: &Client, url: &str) -> Result<String, String> {
    let mut request = client.get(url);
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if url.contains("github") {
            request = request.bearer_auth(token);
        }
    }
    let response = request.send().map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} from {url}"));
    }
    response.text().map_err(|error| error.to_string())
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn wikipedia(lang: &str, title: &str) -> Result<Adapted, String> {
    let client = client()?;
    let api = reqwest::Url::parse_with_params(
        &format!("https://{lang}.wikipedia.org/w/api.php"),
        &[
            ("action", "query"),
            ("format", "json"),
            ("prop", "extracts|info"),
            ("explaintext", "1"),
            ("inprop", "url"),
            ("redirects", "1"),
            ("titles", title),
        ],
    )
    .map_err(|error| error.to_string())?;
    let body = get_json(&client, api.as_str(), false)?;
    let page = body
        .pointer("/query/pages")
        .and_then(Value::as_object)
        .and_then(|pages| pages.values().next())
        .ok_or("no page in the Wikipedia reply")?;
    if page.get("missing").is_some() {
        return Err(format!("no Wikipedia article named {title}"));
    }
    let extract = text(page, "extract");
    if extract.trim().is_empty() {
        return Err("empty Wikipedia extract".to_string());
    }
    let title = text(page, "title");
    let url = text(page, "fullurl");
    Ok(Adapted {
        source: "wikipedia (MediaWiki API, plain text)",
        next: vec![(
            "Wikipedia search on this topic".to_string(),
            format!(
                "https://{lang}.wikipedia.org/w/index.php?search={}",
                title.replace(' ', "+")
            ),
        )],
        url,
        title,
        body: extract,
    })
}

/// The text inside the first `<tag ...>…</tag>` of `xml`.
fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = xml.find(&format!("<{tag}"))?;
    let start = open + xml[open..].find('>')? + 1;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(&xml[start..end])
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn arxiv(id: &str) -> Result<Adapted, String> {
    let client = client()?;
    let xml = get_text(
        &client,
        &format!("https://export.arxiv.org/api/query?id_list={id}"),
    )?;
    let entry = xml_tag(&xml, "entry").ok_or("no entry in the arXiv reply")?;
    let title = squash(xml_tag(entry, "title").unwrap_or_default());
    if title.is_empty() || title.eq_ignore_ascii_case("error") {
        return Err(format!("arXiv has no paper {id}"));
    }
    let summary = squash(xml_tag(entry, "summary").unwrap_or_default());
    let published = xml_tag(entry, "published").unwrap_or_default().to_string();
    let authors: Vec<String> = entry
        .split("<author>")
        .skip(1)
        .filter_map(|author| xml_tag(author, "name").map(squash))
        .collect();
    let categories: Vec<String> = entry
        .split("<category ")
        .skip(1)
        .filter_map(|category| {
            let start = category.find("term=\"")? + 6;
            let end = start + category[start..].find('"')?;
            Some(category[start..end].to_string())
        })
        .collect();
    let mut body = String::new();
    let _ = writeln!(body, "# {title}\n");
    let _ = writeln!(body, "Authors: {}", authors.join(", "));
    let _ = writeln!(
        body,
        "Published: {}",
        published.get(..10).unwrap_or(&published)
    );
    if !categories.is_empty() {
        let _ = writeln!(body, "Categories: {}", categories.join(", "));
    }
    let _ = writeln!(body, "\n## Abstract\n\n{summary}");
    Ok(Adapted {
        source: "arxiv (export API)",
        url: format!("https://arxiv.org/abs/{id}"),
        title,
        next: vec![
            (
                "full text (HTML)".to_string(),
                format!("https://arxiv.org/html/{id}"),
            ),
            ("PDF".to_string(), format!("https://arxiv.org/pdf/{id}")),
            (
                "citations and related work (Semantic Scholar)".to_string(),
                format!("https://www.semanticscholar.org/arxiv/{id}"),
            ),
        ],
        body,
    })
}

fn github(segments: &[String]) -> Result<Adapted, String> {
    let client = client()?;
    let (owner, repo) = (&segments[0], segments[1].trim_end_matches(".git"));
    let api = format!("https://api.github.com/repos/{owner}/{repo}");
    let page = format!("https://github.com/{}", segments.join("/"));
    match segments.get(2).map(String::as_str) {
        // A file: its raw text.
        Some("blob") if segments.len() >= 5 => {
            let reference = &segments[3];
            let path = segments[4..].join("/");
            let raw = get_text(
                &client,
                &format!("https://raw.githubusercontent.com/{owner}/{repo}/{reference}/{path}"),
            )?;
            Ok(Adapted {
                source: "github (raw file)",
                url: page,
                title: format!("{owner}/{repo}: {path} @ {reference}"),
                next: vec![(
                    "folder".to_string(),
                    format!(
                        "https://github.com/{owner}/{repo}/tree/{reference}/{}",
                        path.rsplit_once('/').map_or("", |(dir, _)| dir)
                    ),
                )],
                body: raw,
            })
        }
        // A folder: its listing.
        Some("tree") if segments.len() >= 4 => {
            let reference = &segments[3];
            let path = segments[4..].join("/");
            let listing = get_json(
                &client,
                &format!("{api}/contents/{path}?ref={reference}"),
                true,
            )?;
            let mut body = String::new();
            for item in listing.as_array().into_iter().flatten() {
                let kind = if text(item, "type") == "dir" {
                    "dir "
                } else {
                    "file"
                };
                let _ = writeln!(body, "{kind}  {}", text(item, "path"));
            }
            Ok(Adapted {
                source: "github (contents API)",
                url: page,
                title: format!("{owner}/{repo}/{path} @ {reference}"),
                next: Vec::new(),
                body,
            })
        }
        // An issue or pull request with its comments.
        Some(kind @ ("issues" | "pull")) if segments.len() >= 4 => {
            let number = &segments[3];
            let issue = get_json(&client, &format!("{api}/issues/{number}"), true)?;
            let comments = get_json(
                &client,
                &format!("{api}/issues/{number}/comments?per_page=30"),
                true,
            )
            .unwrap_or(Value::Null);
            let mut body = String::new();
            let _ = writeln!(
                body,
                "State: {} · by {} · {} comments\n",
                text(&issue, "state"),
                issue
                    .pointer("/user/login")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                issue.get("comments").and_then(Value::as_u64).unwrap_or(0)
            );
            body.push_str(&text(&issue, "body"));
            for comment in comments.as_array().into_iter().flatten() {
                let _ = write!(
                    body,
                    "\n\n--- {} ({})\n{}",
                    comment
                        .pointer("/user/login")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                    text(comment, "created_at").get(..10).unwrap_or(""),
                    text(comment, "body")
                );
            }
            let mut next = Vec::new();
            if kind == "pull" {
                next.push((
                    "the diff".to_string(),
                    format!("https://patch-diff.githubusercontent.com/raw/{owner}/{repo}/pull/{number}.diff"),
                ));
            }
            Ok(Adapted {
                source: "github (issues API)",
                url: page,
                title: format!("#{number}: {}", text(&issue, "title")),
                next,
                body,
            })
        }
        // The repository: its facts and README.
        _ => {
            let info = get_json(&client, &api, true)?;
            let readme = get_text(
                &client,
                &format!(
                    "https://raw.githubusercontent.com/{owner}/{repo}/{}/README.md",
                    text(&info, "default_branch")
                ),
            )
            .unwrap_or_default();
            let mut body = String::new();
            let _ = writeln!(body, "{}\n", text(&info, "description"));
            let _ = writeln!(
                body,
                "Stars: {} · Forks: {} · Language: {} · License: {} · Updated: {} · Default branch: {}",
                info.get("stargazers_count").and_then(Value::as_u64).unwrap_or(0),
                info.get("forks_count").and_then(Value::as_u64).unwrap_or(0),
                text(&info, "language"),
                info.pointer("/license/spdx_id").and_then(Value::as_str).unwrap_or("none"),
                text(&info, "pushed_at").get(..10).unwrap_or(""),
                text(&info, "default_branch"),
            );
            if !readme.is_empty() {
                let _ = write!(body, "\n## README\n\n{readme}");
            }
            let branch = text(&info, "default_branch");
            Ok(Adapted {
                source: "github (repos API + README)",
                url: text(&info, "html_url"),
                title: text(&info, "full_name"),
                next: vec![
                    (
                        "file tree".to_string(),
                        format!("https://github.com/{owner}/{repo}/tree/{branch}"),
                    ),
                    (
                        "open issues".to_string(),
                        format!("https://github.com/{owner}/{repo}/issues"),
                    ),
                ],
                body,
            })
        }
    }
}

fn semantic_scholar(id: &str, original: &str) -> Result<Adapted, String> {
    let client = client()?;
    let fields = "title,abstract,year,venue,authors,citationCount,referenceCount,tldr,externalIds,openAccessPdf,url";
    let paper = get_json(
        &client,
        &format!("https://api.semanticscholar.org/graph/v1/paper/{id}?fields={fields}"),
        false,
    )?;
    let title = text(&paper, "title");
    if title.is_empty() {
        return Err("Semantic Scholar has no such paper".to_string());
    }
    let authors: Vec<String> = paper
        .get("authors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|author| text(author, "name"))
        .collect();
    let mut body = String::new();
    let _ = writeln!(body, "# {title}\n");
    let _ = writeln!(body, "Authors: {}", authors.join(", "));
    let _ = writeln!(
        body,
        "Year: {} · Venue: {} · Citations: {} · References: {}",
        paper
            .get("year")
            .and_then(Value::as_u64)
            .map_or_else(String::new, |y| y.to_string()),
        text(&paper, "venue"),
        paper
            .get("citationCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        paper
            .get("referenceCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    if let Some(tldr) = paper.pointer("/tldr/text").and_then(Value::as_str) {
        let _ = writeln!(body, "\nTL;DR: {tldr}");
    }
    let abstract_text = text(&paper, "abstract");
    if !abstract_text.is_empty() {
        let _ = writeln!(body, "\n## Abstract\n\n{abstract_text}");
    }
    let mut next = Vec::new();
    if let Some(pdf) = paper.pointer("/openAccessPdf/url").and_then(Value::as_str) {
        if !pdf.is_empty() {
            next.push(("open-access PDF".to_string(), pdf.to_string()));
        }
    }
    if let Some(arxiv) = paper.pointer("/externalIds/ArXiv").and_then(Value::as_str) {
        next.push((
            "arXiv full text (HTML)".to_string(),
            format!("https://arxiv.org/html/{arxiv}"),
        ));
    }
    let url = text(&paper, "url");
    Ok(Adapted {
        source: "semantic scholar (Graph API)",
        url: if url.is_empty() {
            original.to_string()
        } else {
            url
        },
        title,
        next,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::{decode, xml_tag, Adapted};

    #[test]
    fn renders_the_frontmatter_envelope() {
        let adapted = Adapted {
            source: "test",
            url: "https://example.org".to_string(),
            title: "A \"quoted\" title".to_string(),
            next: vec![("PDF".to_string(), "https://example.org/a.pdf".to_string())],
            body: "Body text".to_string(),
        };
        let text = adapted.render();
        assert!(
            text.starts_with("---\nsource: test\nurl: https://example.org\n"),
            "{text}"
        );
        assert!(text.contains("title: \"A \\\"quoted\\\" title\""), "{text}");
        assert!(
            text.contains("next:\n  - \"PDF\": https://example.org/a.pdf\n---\nBody text"),
            "{text}"
        );
    }

    #[test]
    fn reads_atom_tags_and_url_escapes() {
        let xml = "<feed><entry><title>Hello\n  world</title><summary>S</summary></entry></feed>";
        assert_eq!(xml_tag(xml, "title"), Some("Hello\n  world"));
        assert_eq!(
            decode("Rust_%28programming_language%29"),
            "Rust_(programming_language)"
        );
    }

    #[test]
    fn only_known_sources_are_adapted() {
        assert!(super::fetch("https://example.com/page").is_none());
        assert!(super::fetch("https://arxiv.org/list/cs.AI/recent").is_none());
    }
}
