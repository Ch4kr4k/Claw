//! `HuggingFace`: the Hugging Face Hub API as a read-only tool. It searches
//! models, datasets and Spaces (or lists what is trending); shows a repo's
//! facts (task, library, license, downloads, parameter count, GGUF
//! context, gated) with its card; lists a repo's files with sizes; reads a
//! text file (config.json, README.md, tokenizer_config.json …); lists
//! today's papers or searches papers; and says who the token belongs to.
//! File results carry the direct download URL for the `Download` tool.
//!
//! The token (optional, needed for gated and private repos) comes from
//! `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN`, or the file `huggingface-cli
//! login` writes (`$HF_HOME/token`, default `~/.cache/huggingface/token`).

use std::fmt::Write as _;
use std::time::Duration;

use reqwest::blocking::{Client, RequestBuilder};
use runtime::PermissionMode;
use serde::Deserialize;
use serde_json::{json, Value};

use super::github::cut;
use super::ToolSpec;

const HUB: &str = "https://huggingface.co";
const MAX_TEXT: usize = 40_000;
/// Text files larger than this are not read whole; download them instead.
const MAX_READ_BYTES: u64 = 2_000_000;
const DEFAULT_LIMIT: u64 = 20;

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "HuggingFace",
        description: "Read the Hugging Face Hub. Actions: search (query, optional author, filter tag e.g. \"gguf\" or \"text-generation\", sort downloads|likes|trending|updated); trending; info (a repo's task, library, license, downloads, parameters, GGUF context, gated, file count, and its card); files (the repo's files with sizes and download URLs; path narrows to a folder); read (a text file such as config.json or README.md); papers (today's papers, or query to search papers); whoami. kind = model (default) | dataset | space. repo is \"owner/name\". To fetch weights, pass a file's download URL to the Download tool. Token for gated/private repos: HF_TOKEN or `huggingface-cli login`.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["search", "trending", "info", "files", "read", "papers", "whoami"] },
                "kind": { "type": "string", "enum": ["model", "dataset", "space"] },
                "query": { "type": "string" },
                "author": { "type": "string", "description": "search: only this user or organization." },
                "filter": { "type": "string", "description": "search: a tag, e.g. gguf, text-generation, license:mit." },
                "sort": { "type": "string", "enum": ["downloads", "likes", "trending", "updated"] },
                "repo": { "type": "string", "description": "owner/name" },
                "path": { "type": "string", "description": "files: a folder; read: the file." },
                "revision": { "type": "string", "description": "Branch, tag or commit (default main)." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::ReadOnly,
    }]
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    (name == "HuggingFace").then(|| {
        serde_json::from_value::<HfInput>(input.clone())
            .map_err(|error| error.to_string())
            .and_then(huggingface)
    })
}

#[derive(Deserialize, Default)]
struct HfInput {
    action: String,
    kind: Option<String>,
    query: Option<String>,
    author: Option<String>,
    filter: Option<String>,
    sort: Option<String>,
    repo: Option<String>,
    path: Option<String>,
    revision: Option<String>,
    limit: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Model,
    Dataset,
    Space,
}

impl Kind {
    fn parse(kind: Option<&str>) -> Result<Self, String> {
        match kind.unwrap_or("model").trim_end_matches('s') {
            "model" => Ok(Self::Model),
            "dataset" => Ok(Self::Dataset),
            "space" => Ok(Self::Space),
            other => Err(format!(
                "kind must be model, dataset or space, got {other:?}"
            )),
        }
    }

    /// The API collection: `models`, `datasets`, `spaces`.
    fn api(self) -> &'static str {
        match self {
            Self::Model => "models",
            Self::Dataset => "datasets",
            Self::Space => "spaces",
        }
    }

    /// The prefix of web and file URLs: models have none.
    fn url_prefix(self) -> &'static str {
        match self {
            Self::Model => "",
            Self::Dataset => "datasets/",
            Self::Space => "spaces/",
        }
    }
}

impl HfInput {
    fn repo(&self) -> Result<String, String> {
        let repo = self
            .repo
            .as_deref()
            .map(str::trim)
            .filter(|repo| !repo.is_empty())
            .ok_or_else(|| format!("{} needs repo (owner/name)", self.action))?;
        let repo = repo.trim_start_matches(HUB).trim_matches('/');
        let repo = repo
            .trim_start_matches("datasets/")
            .trim_start_matches("spaces/");
        if repo.contains("..") || repo.split('/').count() > 2 {
            return Err(format!("repo must be owner/name, got {repo:?}"));
        }
        Ok(repo.to_string())
    }

    fn kind(&self) -> Result<Kind, String> {
        // A pasted URL says what it is.
        let repo = self.repo.as_deref().unwrap_or("");
        let from_url = repo.trim_start_matches(HUB).trim_start_matches('/');
        if self.kind.is_none() {
            if from_url.starts_with("datasets/") {
                return Ok(Kind::Dataset);
            }
            if from_url.starts_with("spaces/") {
                return Ok(Kind::Space);
            }
        }
        Kind::parse(self.kind.as_deref())
    }

    fn revision(&self) -> &str {
        self.revision.as_deref().unwrap_or("main")
    }
}

fn token() -> Option<String> {
    for var in ["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"] {
        if let Ok(token) = std::env::var(var) {
            if !token.trim().is_empty() {
                return Some(token.trim().to_string());
            }
        }
    }
    let home = std::env::var("HF_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|home| std::path::Path::new(&home).join(".cache/huggingface"))
        });
    let token = std::fs::read_to_string(home.ok()?.join("token")).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

struct Hub {
    client: Client,
    token: Option<String>,
}

impl Hub {
    fn new() -> Result<Self, String> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("claw-code (HuggingFace tool)")
                .build()
                .map_err(|error| error.to_string())?,
            token: token(),
        })
    }

    fn request(&self, url: &str) -> RequestBuilder {
        let mut request = self.client.get(url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        request
    }

    fn send(&self, request: RequestBuilder) -> Result<reqwest::blocking::Response, String> {
        let response = request.send().map_err(|error| error.to_string())?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| body.chars().take(300).collect());
        let mut error = format!("Hugging Face: HTTP {status}");
        if !message.trim().is_empty() {
            let _ = write!(error, ": {}", message.trim());
        }
        if matches!(status.as_u16(), 401 | 403) {
            error.push_str(if self.token.is_none() {
                " (gated or private: set HF_TOKEN, or run `huggingface-cli login`, after accepting the repo's terms on its page)"
            } else {
                " (the token may lack access: accept the repo's terms on its page)"
            });
        }
        Err(error)
    }

    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        self.send(self.request(&format!("{HUB}/api/{path}")).query(query))?
            .json()
            .map_err(|error| error.to_string())
    }
}

fn huggingface(input: HfInput) -> Result<String, String> {
    let hub = Hub::new()?;
    let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 100);
    match input.action.as_str() {
        "search" | "trending" => {
            let kind = input.kind()?;
            let sort = if input.action == "trending" {
                "trending"
            } else {
                input.sort.as_deref().unwrap_or("downloads")
            };
            let sort = match sort {
                "trending" => "trendingScore",
                "updated" => "lastModified",
                "likes" => "likes",
                _ => "downloads",
            };
            let mut query = vec![
                ("sort", sort.to_string()),
                ("direction", "-1".to_string()),
                ("limit", limit.to_string()),
            ];
            if let Some(text) = input
                .query
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                query.push(("search", text.trim().to_string()));
            }
            if let Some(author) = &input.author {
                query.push(("author", author.clone()));
            }
            if let Some(filter) = &input.filter {
                query.push(("filter", filter.clone()));
            }
            let found = hub.get(kind.api(), &query)?;
            let items = found.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return Ok("No results.".to_string());
            }
            Ok(items
                .iter()
                .map(|item| search_line(kind, item))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "info" => info(&hub, &input),
        "files" => files(&hub, &input),
        "read" => read(&hub, &input),
        "papers" => {
            let papers = match input
                .query
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                Some(text) => hub.get("papers/search", &[("q", text.trim().to_string())])?,
                None => hub.get("daily_papers", &[("limit", limit.to_string())])?,
            };
            let lines: Vec<String> = papers
                .as_array()
                .into_iter()
                .flatten()
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .map(paper_line)
                .collect();
            if lines.is_empty() {
                return Ok("No papers.".to_string());
            }
            Ok(lines.join("\n\n"))
        }
        "whoami" => {
            if hub.token.is_none() {
                return Err(
                    "No Hugging Face token: set HF_TOKEN or run `huggingface-cli login`".into(),
                );
            }
            let me = hub.get("whoami-v2", &[])?;
            let orgs: Vec<&str> = me["orgs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|org| org["name"].as_str())
                .collect();
            Ok(format!(
                "{}{} | token role: {}{}",
                str_of(&me["name"]),
                match str_of(&me["fullname"]) {
                    "" => String::new(),
                    name => format!(" ({name})"),
                },
                me["auth"]["accessToken"]["role"].as_str().unwrap_or("?"),
                if orgs.is_empty() {
                    String::new()
                } else {
                    format!(" | orgs: {}", orgs.join(", "))
                }
            ))
        }
        other => Err(format!("unknown HuggingFace action {other:?}")),
    }
}

fn search_line(kind: Kind, item: &Value) -> String {
    let id = str_of(&item["id"]);
    let mut line = format!("{id}  ♥{}", item["likes"].as_u64().unwrap_or(0));
    if let Some(downloads) = item["downloads"].as_u64() {
        let _ = write!(line, " ↓{}", human_count(downloads));
    }
    match kind {
        Kind::Model => {
            if let Some(task) = item["pipeline_tag"].as_str() {
                let _ = write!(line, "  {task}");
            }
            let tags = tags(item);
            for marker in ["gguf", "mlx", "onnx", "gptq", "awq"] {
                if tags.contains(&marker) {
                    let _ = write!(line, " [{marker}]");
                }
            }
        }
        Kind::Space => {
            if let Some(sdk) = item["sdk"].as_str() {
                let _ = write!(line, "  {sdk}");
            }
        }
        Kind::Dataset => {}
    }
    if let Some(updated) = item["lastModified"].as_str().or(item["createdAt"].as_str()) {
        let _ = write!(line, "  {}", date(updated));
    }
    line
}

fn info(hub: &Hub, input: &HfInput) -> Result<String, String> {
    let kind = input.kind()?;
    let repo = input.repo()?;
    let info = hub.get(&format!("{}/{repo}", kind.api()), &[])?;
    let card = &info["cardData"];
    let mut out = format!("{}{repo}\n", kind.url_prefix());
    let mut facts = Vec::new();
    if let Some(task) = info["pipeline_tag"].as_str() {
        facts.push(task.to_string());
    }
    if let Some(library) = info["library_name"].as_str() {
        facts.push(format!("library {library}"));
    }
    if let Some(sdk) = info["sdk"].as_str() {
        facts.push(format!("sdk {sdk}"));
    }
    let license = card["license"]
        .as_str()
        .map(str::to_string)
        .or_else(|| tag_value(&info, "license:"));
    if let Some(license) = license {
        facts.push(format!("license {license}"));
    }
    facts.push(format!("♥{}", info["likes"].as_u64().unwrap_or(0)));
    if let Some(downloads) = info["downloads"].as_u64() {
        facts.push(format!("↓{} (30 days)", human_count(downloads)));
    }
    let _ = writeln!(out, "{}", facts.join(" | "));
    if let Some(total) = info["safetensors"]["total"].as_u64() {
        let dtypes: Vec<String> = info["safetensors"]["parameters"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(dtype, _)| dtype.clone())
            .collect();
        let _ = writeln!(
            out,
            "parameters: {} ({})",
            human_count(total),
            dtypes.join(", ")
        );
    }
    let gguf = &info["gguf"];
    if gguf.is_object() {
        let mut parts = Vec::new();
        if let Some(total) = gguf["total"].as_u64() {
            parts.push(format!("{} parameters", human_count(total)));
        }
        if let Some(architecture) = gguf["architecture"].as_str() {
            parts.push(format!("architecture {architecture}"));
        }
        if let Some(context) = gguf["context_length"].as_u64() {
            parts.push(format!("context {context}"));
        }
        let _ = writeln!(out, "gguf: {}", parts.join(", "));
    }
    if let Some(base) = card["base_model"].as_str().map(str::to_string).or_else(|| {
        card["base_model"].as_array().map(|bases| {
            bases
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
    }) {
        if !base.is_empty() {
            let _ = writeln!(out, "base model: {base}");
        }
    }
    match &info["gated"] {
        Value::Bool(false) | Value::Null => {}
        gated => {
            let _ = writeln!(
                out,
                "gated ({}): accept the terms on the repo page; downloads need HF_TOKEN",
                gated.as_str().unwrap_or("yes")
            );
        }
    }
    if info["private"].as_bool() == Some(true) {
        out.push_str("private\n");
    }
    if let Some(siblings) = info["siblings"].as_array() {
        let _ = writeln!(out, "{} file(s) (action files lists them)", siblings.len());
    }
    let interesting: Vec<&str> = tags(&info)
        .into_iter()
        .filter(|tag| !tag.contains(':') && !matches!(*tag, "region" | "endpoints_compatible"))
        .take(15)
        .collect();
    if !interesting.is_empty() {
        let _ = writeln!(out, "tags: {}", interesting.join(", "));
    }
    if let Some(updated) = info["lastModified"].as_str() {
        let _ = writeln!(out, "updated {}", date(updated));
    }
    let _ = writeln!(out, "{HUB}/{}{repo}", kind.url_prefix());
    let readme = resolve_url(kind, &repo, input.revision(), "README.md");
    if let Ok(response) = hub.send(hub.request(&readme)) {
        if let Ok(text) = response.text() {
            let _ = write!(
                out,
                "\n--- card ---\n{}",
                cut(strip_front_matter(&text).to_string(), MAX_TEXT / 2)
            );
        }
    }
    Ok(out)
}

fn files(hub: &Hub, input: &HfInput) -> Result<String, String> {
    let kind = input.kind()?;
    let repo = input.repo()?;
    let folder = input.path.as_deref().unwrap_or("").trim_matches('/');
    let revision = input.revision();
    let path = if folder.is_empty() {
        format!("{}/{repo}/tree/{revision}", kind.api())
    } else {
        format!("{}/{repo}/tree/{revision}/{folder}", kind.api())
    };
    let entries = hub.get(&path, &[("recursive", "true".to_string())])?;
    let mut total = 0_u64;
    let mut lines = Vec::new();
    for entry in entries.as_array().into_iter().flatten() {
        if str_of(&entry["type"]) != "file" {
            continue;
        }
        let file = str_of(&entry["path"]);
        let size = entry["lfs"]["size"]
            .as_u64()
            .or_else(|| entry["size"].as_u64())
            .unwrap_or(0);
        total += size;
        let mut line = format!("{file}  {}", human_bytes(size));
        // Weights are what people download; give their URL.
        if size > 10_000_000 {
            let _ = write!(line, "  {}", resolve_url(kind, &repo, revision, file));
        }
        lines.push(line);
    }
    if lines.is_empty() {
        return Ok(format!("No files under {folder:?} in {repo}."));
    }
    Ok(cut(
        format!(
            "{} file(s), {} in all. Download URL of any file: {HUB}/{}{repo}/resolve/{revision}/<path>\n{}",
            lines.len(),
            human_bytes(total),
            kind.url_prefix(),
            lines.join("\n")
        ),
        MAX_TEXT,
    ))
}

fn read(hub: &Hub, input: &HfInput) -> Result<String, String> {
    let kind = input.kind()?;
    let repo = input.repo()?;
    let path = input
        .path
        .as_deref()
        .map(|path| path.trim_matches('/'))
        .filter(|path| !path.is_empty())
        .ok_or("read needs path (e.g. config.json)")?;
    let url = resolve_url(kind, &repo, input.revision(), path);
    let response = hub.send(hub.request(&url))?;
    if let Some(length) = response.content_length() {
        if length > MAX_READ_BYTES {
            return Err(format!(
                "{path} is {}; too big to read here, use the Download tool with {url}",
                human_bytes(length)
            ));
        }
    }
    let bytes = response.bytes().map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_READ_BYTES || bytes.contains(&0) {
        return Err(format!(
            "{path} is binary or too big ({}); use the Download tool with {url}",
            human_bytes(bytes.len() as u64)
        ));
    }
    Ok(cut(String::from_utf8_lossy(&bytes).into_owned(), MAX_TEXT))
}

fn paper_line(item: &Value) -> String {
    let paper = if item["paper"].is_object() {
        &item["paper"]
    } else {
        item
    };
    let id = str_of(&paper["id"]);
    let authors: Vec<&str> = paper["authors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|author| author["name"].as_str())
        .take(4)
        .collect();
    let summary: String = str_of(&paper["summary"])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let summary = match summary.char_indices().nth(400) {
        Some((end, _)) => format!("{}…", &summary[..end]),
        None => summary,
    };
    let title = match str_of(&paper["title"]) {
        "" => str_of(&item["title"]),
        title => title,
    };
    format!(
        "{title}\n  arXiv {id} | ▲{} | {} | {}\n  {HUB}/papers/{id}\n  {summary}",
        paper["upvotes"].as_u64().unwrap_or(0),
        date(str_of(&paper["publishedAt"])),
        authors.join(", "),
    )
}

fn resolve_url(kind: Kind, repo: &str, revision: &str, path: &str) -> String {
    let path: Vec<String> = path.split('/').map(encode_segment).collect();
    format!(
        "{HUB}/{}{repo}/resolve/{}/{}",
        kind.url_prefix(),
        encode_segment(revision),
        path.join("/")
    )
}

fn encode_segment(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// The card without its YAML header (the facts above already cover it).
fn strip_front_matter(text: &str) -> &str {
    text.strip_prefix("---")
        .and_then(|rest| rest.find("\n---").map(|end| &rest[end + 4..]))
        .unwrap_or(text)
        .trim_start()
}

fn tags(item: &Value) -> Vec<&str> {
    item["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn tag_value(item: &Value, prefix: &str) -> Option<String> {
    tags(item)
        .into_iter()
        .find_map(|tag| tag.strip_prefix(prefix).map(str::to_string))
}

fn str_of(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn date(timestamp: &str) -> &str {
    timestamp.get(..10).unwrap_or(timestamp)
}

#[allow(clippy::cast_precision_loss)]
fn human_count(count: u64) -> String {
    match count {
        0..=9_999 => count.to_string(),
        10_000..=999_999 => format!("{:.1}K", count as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", count as f64 / 1e6),
        _ => format!("{:.1}B", count as f64 / 1e9),
    }
}

#[allow(clippy::cast_precision_loss)]
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: Value) -> HfInput {
        serde_json::from_value(value).expect("input")
    }

    #[test]
    fn repo_and_kind_come_from_ids_or_urls() {
        let plain = input(json!({"action": "info", "repo": "Qwen/Qwen3-0.6B"}));
        assert_eq!(plain.repo().as_deref(), Ok("Qwen/Qwen3-0.6B"));
        assert_eq!(plain.kind(), Ok(Kind::Model));
        let url = input(json!({"action": "info", "repo": "https://huggingface.co/datasets/a/b/"}));
        assert_eq!(url.repo().as_deref(), Ok("a/b"));
        assert_eq!(url.kind(), Ok(Kind::Dataset));
        let space = input(json!({"action": "info", "repo": "a/b", "kind": "spaces"}));
        assert_eq!(space.kind(), Ok(Kind::Space));
        assert!(input(json!({"action": "info", "repo": "a/b/c"}))
            .repo()
            .is_err());
        assert!(input(json!({"action": "info", "kind": "x"}))
            .kind()
            .is_err());
    }

    #[test]
    fn urls_and_sizes_format() {
        assert_eq!(
            resolve_url(Kind::Dataset, "a/b", "main", "data/train 1.parquet"),
            "https://huggingface.co/datasets/a/b/resolve/main/data/train%201.parquet"
        );
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
        assert_eq!(human_count(751_632_384), "751.6M");
        assert_eq!(
            strip_front_matter("---\nlicense: mit\n---\n# Title"),
            "# Title"
        );
        assert_eq!(strip_front_matter("# Title"), "# Title");
    }

    #[test]
    fn search_lines_and_papers_are_compact() {
        let model = json!({"id": "a/b", "likes": 3, "downloads": 12345, "pipeline_tag": "text-generation", "tags": ["gguf"], "createdAt": "2026-10-25T05:43:34.000Z"});
        assert_eq!(
            search_line(Kind::Model, &model),
            "a/b  ♥3 ↓12.3K  text-generation [gguf]  2026-10-25"
        );
        let paper = json!({"paper": {"id": "2211.17192", "title": "Fast", "upvotes": 9, "publishedAt": "2022-11-30T17:33:28.000Z", "authors": [{"name": "Y"}], "summary": "A\n  b."}});
        let line = paper_line(&paper);
        assert!(
            line.starts_with("Fast\n  arXiv 2211.17192 | ▲9 | 2022-11-30 | Y"),
            "{line}"
        );
        assert!(line.ends_with("A b."), "{line}");
    }
}
