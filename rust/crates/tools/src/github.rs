//! `GitHub` and `GitHubWrite`: the GitHub REST API as tools, without the
//! `gh` CLI. `GitHub` only reads (search repositories, code and issues; a
//! repository's facts and README; issues, pull requests and their diffs;
//! files, commits, releases, Actions runs; any other GET endpoint) and runs
//! in read-only mode. `GitHubWrite` creates issues, comments and pull
//! requests, opens or closes them, or sends any other API request, and
//! needs full access: it acts on GitHub as the user.
//!
//! The token comes from `GITHUB_TOKEN`, then `GH_TOKEN`, then `gh auth
//! token` when the `gh` CLI is installed. Reading works without one (60
//! requests an hour, no code search); writing needs one.
//!
//! Results are short text lines rather than the API's JSON, so they fit a
//! small model's context.

use std::fmt::Write as _;
use std::time::Duration;

use reqwest::blocking::{Client, RequestBuilder};
use reqwest::Method;
use runtime::PermissionMode;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ToolSpec;

const API: &str = "https://api.github.com";
/// Longest text returned (file, README, diff, issue thread).
const MAX_TEXT: usize = 40_000;
const DEFAULT_LIMIT: u64 = 20;
/// Comments shown in an issue or PR thread (half from each end).
const MAX_COMMENTS: usize = 30;

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "GitHub",
            description: "Read GitHub through its REST API (no gh CLI needed). Actions: search_repos, search_code (needs a token), search_issues (query uses GitHub search syntax, e.g. \"repo:o/r is:open label:bug\"); repo (facts + README); issues / pulls (list; state open|closed|all); issue / pr (one, with its comments; pr also lists changed files, diff=true adds the diff); file (a file's text or a folder listing; ref = branch/tag/sha); commits (optional path, ref); releases; runs (Actions workflow runs, optional branch); user; api (GET any endpoint path, e.g. /repos/o/r/contributors). repo is \"owner/name\". Token: GITHUB_TOKEN, GH_TOKEN or `gh auth token`.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["search_repos", "search_code", "search_issues", "repo", "issues", "pulls", "issue", "pr", "file", "commits", "releases", "runs", "user", "api"] },
                    "query": { "type": "string", "description": "Search query (search_*)." },
                    "repo": { "type": "string", "description": "owner/name" },
                    "number": { "type": "integer", "minimum": 1, "description": "Issue or pull request number." },
                    "path": { "type": "string", "description": "File or folder path (file, commits), or the endpoint for api." },
                    "ref": { "type": "string", "description": "Branch, tag or commit (file, commits)." },
                    "state": { "type": "string", "enum": ["open", "closed", "all"] },
                    "branch": { "type": "string", "description": "runs: only this branch." },
                    "login": { "type": "string", "description": "user: the account (default: the token's own)." },
                    "sort": { "type": "string", "description": "Search sort, e.g. stars, updated, comments." },
                    "diff": { "type": "boolean", "description": "pr: include the diff." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "GitHubWrite",
            description: "Change things on GitHub as the user (needs a token: GITHUB_TOKEN, GH_TOKEN or `gh auth token`). Actions: create_issue (title, body, labels); comment (on issue or PR number); set_state (number, state open|closed); create_pr (title, head branch, base branch, body, draft); api (any method and endpoint with a JSON body, e.g. PATCH /repos/o/r/issues/5 {\"labels\":[\"bug\"]}). Confirm with the user before writing to repositories they did not ask about.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["create_issue", "comment", "set_state", "create_pr", "api"] },
                    "repo": { "type": "string", "description": "owner/name" },
                    "number": { "type": "integer", "minimum": 1 },
                    "title": { "type": "string" },
                    "body": { "type": "string", "description": "Markdown text (issue, comment, PR)." },
                    "labels": { "type": "array", "items": { "type": "string" } },
                    "state": { "type": "string", "enum": ["open", "closed"] },
                    "head": { "type": "string", "description": "create_pr: the branch with the changes (or owner:branch)." },
                    "base": { "type": "string", "description": "create_pr: the branch to merge into (default: the repo's default branch)." },
                    "draft": { "type": "boolean" },
                    "method": { "type": "string", "enum": ["GET", "POST", "PATCH", "PUT", "DELETE"] },
                    "path": { "type": "string", "description": "api: endpoint path, e.g. /repos/o/r/labels" },
                    "data": { "type": "object", "description": "api: JSON body." }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
    ]
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    let run = match name {
        "GitHub" => read,
        "GitHubWrite" => write,
        _ => return None,
    };
    Some(
        serde_json::from_value::<GitHubInput>(input.clone())
            .map_err(|error| error.to_string())
            .and_then(run),
    )
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct GitHubInput {
    action: String,
    query: Option<String>,
    repo: Option<String>,
    number: Option<u64>,
    path: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    state: Option<String>,
    branch: Option<String>,
    login: Option<String>,
    sort: Option<String>,
    #[serde(default)]
    diff: bool,
    limit: Option<u64>,
    title: Option<String>,
    body: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    head: Option<String>,
    base: Option<String>,
    #[serde(default)]
    draft: bool,
    method: Option<String>,
    data: Option<Value>,
}

impl GitHubInput {
    fn repo(&self) -> Result<&str, String> {
        let repo = self
            .repo
            .as_deref()
            .map(|repo| repo.trim().trim_start_matches("https://github.com/"))
            .map(|repo| repo.trim_end_matches('/').trim_end_matches(".git"))
            .ok_or_else(|| format!("{} needs repo (owner/name)", self.action))?;
        let mut parts = repo.split('/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty() => Ok(repo),
            _ => Err(format!("repo must be owner/name, got {repo:?}")),
        }
    }

    fn number(&self) -> Result<u64, String> {
        self.number
            .ok_or_else(|| format!("{} needs number", self.action))
    }

    fn query(&self) -> Result<&str, String> {
        self.query
            .as_deref()
            .filter(|query| !query.trim().is_empty())
            .ok_or_else(|| format!("{} needs query", self.action))
    }

    fn limit(&self) -> u64 {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 100)
    }
}

/// The GitHub token, if one can be found.
fn token() -> Option<String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(var) {
            if !token.trim().is_empty() {
                return Some(token.trim().to_string());
            }
        }
    }
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !token.is_empty()).then_some(token)
}

struct Api {
    client: Client,
    token: Option<String>,
}

impl Api {
    fn new() -> Result<Self, String> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("claw-code (GitHub tool)")
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            client,
            token: token(),
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.request_accepting(method, path, "application/vnd.github+json")
    }

    fn request_accepting(&self, method: Method, path: &str, accept: &str) -> RequestBuilder {
        let url = if path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{API}/{}", path.trim_start_matches('/'))
        };
        let mut request = self
            .client
            .request(method, url)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
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
        let remaining = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body: Value = response.json().unwrap_or(Value::Null);
        let message = body["message"].as_str().unwrap_or("").to_string();
        let mut error = format!("GitHub API: HTTP {status}");
        if !message.is_empty() {
            let _ = write!(error, ": {message}");
        }
        if let Some(errors) = body["errors"].as_array() {
            for detail in errors.iter().take(5) {
                let text = detail["message"]
                    .as_str()
                    .map_or_else(|| detail.to_string(), str::to_string);
                let _ = write!(error, "; {text}");
            }
        }
        if remaining.as_deref() == Some("0") || (status.as_u16() == 401 && self.token.is_none()) {
            error.push_str(
                if self.token.is_none() {
                    " (no token: set GITHUB_TOKEN or GH_TOKEN for a higher rate limit and private repos)"
                } else {
                    " (rate limit reached)"
                },
            );
        } else if status.as_u16() == 404 && self.token.is_none() {
            error.push_str(" (private repositories need GITHUB_TOKEN or GH_TOKEN)");
        }
        Err(error)
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        self.send(self.request(Method::GET, path))?
            .json()
            .map_err(|error| error.to_string())
    }

    fn get_query(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        self.send(self.request(Method::GET, path).query(query))?
            .json()
            .map_err(|error| error.to_string())
    }

    fn get_raw(
        &self,
        path: &str,
        accept: &str,
        query: &[(&str, String)],
    ) -> Result<String, String> {
        self.send(
            self.request_accepting(Method::GET, path, accept)
                .query(query),
        )?
        .text()
        .map_err(|error| error.to_string())
    }

    fn send_json(&self, method: Method, path: &str, body: &Value) -> Result<Value, String> {
        if self.token.is_none() {
            return Err(
                "GitHubWrite needs a token: set GITHUB_TOKEN or GH_TOKEN (or log in with `gh auth login`)"
                    .to_string(),
            );
        }
        let mut request = self.request(method, path);
        if !body.is_null() {
            request = request.json(body);
        }
        let response = self.send(request)?;
        if response.status().as_u16() == 204 {
            return Ok(Value::Null);
        }
        let text = response.text().map_err(|error| error.to_string())?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
}

fn read(input: GitHubInput) -> Result<String, String> {
    let api = Api::new()?;
    let limit = input.limit().to_string();
    match input.action.as_str() {
        "search_repos" => {
            let mut query = vec![("q", input.query()?.to_string()), ("per_page", limit)];
            if let Some(sort) = &input.sort {
                query.push(("sort", sort.clone()));
            }
            let found = api.get_query("search/repositories", &query)?;
            Ok(list_with_total(&found, format_repo_line))
        }
        "search_code" => {
            if api.token.is_none() {
                return Err(
                    "GitHub code search needs a token: set GITHUB_TOKEN or GH_TOKEN".into(),
                );
            }
            let found = api.get_query(
                "search/code",
                &[("q", input.query()?.to_string()), ("per_page", limit)],
            )?;
            Ok(list_with_total(&found, |item| {
                format!(
                    "{} {}  {}",
                    str_of(&item["repository"]["full_name"]),
                    str_of(&item["path"]),
                    str_of(&item["html_url"])
                )
            }))
        }
        "search_issues" => {
            let mut query = vec![("q", input.query()?.to_string()), ("per_page", limit)];
            if let Some(sort) = &input.sort {
                query.push(("sort", sort.clone()));
            }
            let found = api.get_query("search/issues", &query)?;
            Ok(list_with_total(&found, |item| {
                let repo = str_of(&item["repository_url"])
                    .trim_start_matches("https://api.github.com/repos/");
                format!("{repo} {}", format_issue_line(item))
            }))
        }
        "repo" => repo_summary(&api, input.repo()?),
        "issues" | "pulls" => {
            let repo = input.repo()?;
            let kind = if input.action == "issues" {
                "issues"
            } else {
                "pulls"
            };
            let state = input.state.clone().unwrap_or_else(|| "open".into());
            // The issues endpoint also returns pull requests, which can
            // fill a whole page; ask for a full page and keep `limit`.
            let items = api.get_query(
                &format!("repos/{repo}/{kind}"),
                &[("state", state.clone()), ("per_page", "100".to_string())],
            )?;
            let items = items.as_array().cloned().unwrap_or_default();
            let lines: Vec<String> = items
                .iter()
                .filter(|item| kind == "pulls" || item.get("pull_request").is_none())
                .take(usize::try_from(input.limit()).unwrap_or(usize::MAX))
                .map(format_issue_line)
                .collect();
            if lines.is_empty() {
                return Ok(format!("No {state} {kind} in {repo}."));
            }
            Ok(lines.join("\n"))
        }
        "issue" | "pr" => thread(&api, &input),
        "file" => file(&api, &input),
        "commits" => {
            let repo = input.repo()?;
            let mut query = vec![("per_page", limit)];
            if let Some(path) = &input.path {
                query.push(("path", path.clone()));
            }
            if let Some(reference) = &input.reference {
                query.push(("sha", reference.clone()));
            }
            let commits = api.get_query(&format!("repos/{repo}/commits"), &query)?;
            Ok(lines_of(&commits, |commit| {
                let message = str_of(&commit["commit"]["message"]);
                format!(
                    "{} {} {}  {}",
                    str_of(&commit["sha"]).chars().take(10).collect::<String>(),
                    date(str_of(&commit["commit"]["author"]["date"])),
                    str_of(&commit["commit"]["author"]["name"]),
                    message.lines().next().unwrap_or("")
                )
            }))
        }
        "releases" => {
            let repo = input.repo()?;
            let releases =
                api.get_query(&format!("repos/{repo}/releases"), &[("per_page", limit)])?;
            Ok(lines_of(&releases, |release| {
                let assets = release["assets"].as_array().map_or(0, Vec::len);
                format!(
                    "{} {}{}{}  {} asset(s)  {}",
                    str_of(&release["tag_name"]),
                    date(str_of(&release["published_at"])),
                    if release["prerelease"].as_bool() == Some(true) {
                        " pre-release"
                    } else {
                        ""
                    },
                    match str_of(&release["name"]) {
                        name if name.is_empty() || name == str_of(&release["tag_name"]) => {
                            String::new()
                        }
                        name => format!("  {name}"),
                    },
                    assets,
                    str_of(&release["html_url"])
                )
            }))
        }
        "runs" => {
            let repo = input.repo()?;
            let mut query = vec![("per_page", limit)];
            if let Some(branch) = &input.branch {
                query.push(("branch", branch.clone()));
            }
            let runs = api.get_query(&format!("repos/{repo}/actions/runs"), &query)?;
            Ok(lines_of(&runs["workflow_runs"], |run| {
                let outcome = match str_of(&run["conclusion"]) {
                    "" => str_of(&run["status"]),
                    conclusion => conclusion,
                };
                format!(
                    "{outcome} {} {} ({}) {}  {}",
                    str_of(&run["name"]),
                    str_of(&run["head_branch"]),
                    str_of(&run["event"]),
                    date(str_of(&run["created_at"])),
                    str_of(&run["html_url"])
                )
            }))
        }
        "user" => {
            let user = match &input.login {
                Some(login) => api.get(&format!("users/{login}"))?,
                None if api.token.is_some() => api.get("user")?,
                None => return Err("user needs login (or a token for your own account)".into()),
            };
            let mut out = format!(
                "{} ({}){}\n",
                str_of(&user["login"]),
                str_of(&user["type"]),
                match str_of(&user["name"]) {
                    "" => String::new(),
                    name => format!(" {name}"),
                }
            );
            for (label, key) in [
                ("bio", "bio"),
                ("company", "company"),
                ("location", "location"),
                ("blog", "blog"),
            ] {
                if let Some(text) = user[key].as_str().filter(|text| !text.is_empty()) {
                    let _ = writeln!(out, "{label}: {text}");
                }
            }
            let _ = writeln!(
                out,
                "public repos: {}, followers: {}, joined {}\n{}",
                user["public_repos"],
                user["followers"],
                date(str_of(&user["created_at"])),
                str_of(&user["html_url"])
            );
            Ok(out)
        }
        "api" => {
            let path = input.path.as_deref().ok_or("api needs path")?;
            let value = api.get(path)?;
            Ok(cut(
                serde_json::to_string_pretty(&value).unwrap_or_default(),
                MAX_TEXT,
            ))
        }
        other => Err(format!(
            "unknown GitHub action {other:?}; writing (issues, comments, PRs) is GitHubWrite"
        )),
    }
}

fn write(input: GitHubInput) -> Result<String, String> {
    let api = Api::new()?;
    match input.action.as_str() {
        "create_issue" => {
            let repo = input.repo()?;
            let title = input.title.as_deref().ok_or("create_issue needs title")?;
            let mut body =
                json!({ "title": title, "body": input.body.clone().unwrap_or_default() });
            if !input.labels.is_empty() {
                body["labels"] = json!(input.labels);
            }
            let issue = api.send_json(Method::POST, &format!("repos/{repo}/issues"), &body)?;
            Ok(format!(
                "Created issue #{} {}",
                issue["number"],
                str_of(&issue["html_url"])
            ))
        }
        "comment" => {
            let repo = input.repo()?;
            let number = input.number()?;
            let text = input.body.as_deref().ok_or("comment needs body")?;
            let comment = api.send_json(
                Method::POST,
                &format!("repos/{repo}/issues/{number}/comments"),
                &json!({ "body": text }),
            )?;
            Ok(format!("Commented: {}", str_of(&comment["html_url"])))
        }
        "set_state" => {
            let repo = input.repo()?;
            let number = input.number()?;
            let state = input.state.as_deref().ok_or("set_state needs state")?;
            if state != "open" && state != "closed" {
                return Err("state must be open or closed".into());
            }
            let issue = api.send_json(
                Method::PATCH,
                &format!("repos/{repo}/issues/{number}"),
                &json!({ "state": state }),
            )?;
            Ok(format!(
                "#{number} is now {}: {}",
                str_of(&issue["state"]),
                str_of(&issue["html_url"])
            ))
        }
        "create_pr" => {
            let repo = input.repo()?;
            let title = input.title.as_deref().ok_or("create_pr needs title")?;
            let head = input
                .head
                .as_deref()
                .ok_or("create_pr needs head (the branch with the changes)")?;
            let base = match &input.base {
                Some(base) => base.clone(),
                None => str_of(&api.get(&format!("repos/{repo}"))?["default_branch"]).to_string(),
            };
            let pr = api.send_json(
                Method::POST,
                &format!("repos/{repo}/pulls"),
                &json!({
                    "title": title,
                    "head": head,
                    "base": base,
                    "body": input.body.clone().unwrap_or_default(),
                    "draft": input.draft,
                }),
            )?;
            Ok(format!(
                "Opened PR #{} ({head} → {base}) {}",
                pr["number"],
                str_of(&pr["html_url"])
            ))
        }
        "api" => {
            let path = input.path.as_deref().ok_or("api needs path")?;
            let method = input
                .method
                .as_deref()
                .unwrap_or("GET")
                .to_ascii_uppercase();
            let method =
                Method::from_bytes(method.as_bytes()).map_err(|error| error.to_string())?;
            let body = input.data.clone().unwrap_or(Value::Null);
            let value = api.send_json(method.clone(), path, &body)?;
            if value.is_null() {
                return Ok(format!("{method} {path}: done (no content)"));
            }
            Ok(cut(
                serde_json::to_string_pretty(&value).unwrap_or_default(),
                MAX_TEXT,
            ))
        }
        other => Err(format!(
            "unknown GitHubWrite action {other:?}; reading is the GitHub tool"
        )),
    }
}

fn repo_summary(api: &Api, repo: &str) -> Result<String, String> {
    let info = api.get(&format!("repos/{repo}"))?;
    let mut out = format!("{}\n", str_of(&info["full_name"]));
    if let Some(description) = info["description"].as_str() {
        let _ = writeln!(out, "{description}");
    }
    let _ = writeln!(
        out,
        "★{} forks {} open issues {} | {} | license {} | default branch {}",
        info["stargazers_count"],
        info["forks_count"],
        info["open_issues_count"],
        info["language"].as_str().unwrap_or("-"),
        info["license"]["spdx_id"].as_str().unwrap_or("-"),
        str_of(&info["default_branch"])
    );
    let _ = writeln!(
        out,
        "created {}, last push {}{}{}",
        date(str_of(&info["created_at"])),
        date(str_of(&info["pushed_at"])),
        if info["archived"].as_bool() == Some(true) {
            ", ARCHIVED"
        } else {
            ""
        },
        if info["fork"].as_bool() == Some(true) {
            format!(", fork of {}", str_of(&info["parent"]["full_name"]))
        } else {
            String::new()
        }
    );
    if let Some(topics) = info["topics"]
        .as_array()
        .filter(|topics| !topics.is_empty())
    {
        let topics: Vec<&str> = topics.iter().filter_map(Value::as_str).collect();
        let _ = writeln!(out, "topics: {}", topics.join(", "));
    }
    if let Some(homepage) = info["homepage"].as_str().filter(|page| !page.is_empty()) {
        let _ = writeln!(out, "homepage: {homepage}");
    }
    let _ = writeln!(out, "{}", str_of(&info["html_url"]));
    if let Ok(readme) = api.get_raw(
        &format!("repos/{repo}/readme"),
        "application/vnd.github.raw",
        &[],
    ) {
        let _ = write!(out, "\n--- README ---\n{}", cut(readme, MAX_TEXT / 2));
    }
    Ok(out)
}

fn thread(api: &Api, input: &GitHubInput) -> Result<String, String> {
    let repo = input.repo()?;
    let number = input.number()?;
    let issue = api.get(&format!("repos/{repo}/issues/{number}"))?;
    let is_pr = issue.get("pull_request").is_some();
    let mut out = format!(
        "{} #{number} [{}] {}\nby {} on {}",
        if is_pr { "PR" } else { "Issue" },
        str_of(&issue["state"]),
        str_of(&issue["title"]),
        str_of(&issue["user"]["login"]),
        date(str_of(&issue["created_at"]))
    );
    let labels = names(&issue["labels"], "name");
    if !labels.is_empty() {
        let _ = write!(out, " | labels: {labels}");
    }
    let assignees = names(&issue["assignees"], "login");
    if !assignees.is_empty() {
        let _ = write!(out, " | assigned: {assignees}");
    }
    let _ = writeln!(out, "\n{}", str_of(&issue["html_url"]));
    if is_pr {
        let pr = api.get(&format!("repos/{repo}/pulls/{number}"))?;
        let _ = writeln!(
            out,
            "{} → {} | {} commit(s), +{} −{} in {} file(s){}{}",
            str_of(&pr["head"]["label"]),
            str_of(&pr["base"]["ref"]),
            pr["commits"],
            pr["additions"],
            pr["deletions"],
            pr["changed_files"],
            if pr["merged"].as_bool() == Some(true) {
                " | merged"
            } else {
                ""
            },
            if pr["draft"].as_bool() == Some(true) {
                " | draft"
            } else {
                ""
            }
        );
    }
    let _ = writeln!(
        out,
        "\n{}",
        issue["body"].as_str().unwrap_or("(no description)")
    );
    let comments = api.get_query(
        &format!("repos/{repo}/issues/{number}/comments"),
        &[("per_page", "100".to_string())],
    )?;
    let comments = comments.as_array().cloned().unwrap_or_default();
    // Long threads: the first comments and the latest ones.
    let skipped = comments.len().saturating_sub(MAX_COMMENTS);
    for (index, comment) in comments.iter().enumerate() {
        if skipped > 0 && index == MAX_COMMENTS / 2 {
            let _ = writeln!(out, "\n[… {skipped} comment(s) skipped …]");
        }
        if skipped > 0 && index >= MAX_COMMENTS / 2 && index < MAX_COMMENTS / 2 + skipped {
            continue;
        }
        let _ = writeln!(
            out,
            "\n--- {} on {} ---\n{}",
            str_of(&comment["user"]["login"]),
            date(str_of(&comment["created_at"])),
            str_of(&comment["body"])
        );
    }
    if is_pr {
        let files = api.get_query(
            &format!("repos/{repo}/pulls/{number}/files"),
            &[("per_page", "100".to_string())],
        )?;
        out.push_str("\n--- files ---\n");
        for file in files.as_array().into_iter().flatten() {
            let _ = writeln!(
                out,
                "{} +{} −{} {}",
                str_of(&file["status"]),
                file["additions"],
                file["deletions"],
                str_of(&file["filename"])
            );
        }
        if input.diff {
            let diff = api.get_raw(
                &format!("repos/{repo}/pulls/{number}"),
                "application/vnd.github.diff",
                &[],
            )?;
            let _ = write!(out, "\n--- diff ---\n{diff}");
        }
    }
    Ok(cut(out, MAX_TEXT))
}

fn file(api: &Api, input: &GitHubInput) -> Result<String, String> {
    let repo = input.repo()?;
    let path = input.path.as_deref().unwrap_or("").trim_matches('/');
    let query: Vec<(&str, String)> = input
        .reference
        .iter()
        .map(|reference| ("ref", reference.clone()))
        .collect();
    let endpoint = format!("repos/{repo}/contents/{path}");
    let listing = api.get_query(&endpoint, &query)?;
    if let Some(entries) = listing.as_array() {
        let mut lines: Vec<String> = entries
            .iter()
            .map(|entry| match str_of(&entry["type"]) {
                "dir" => format!("{}/", str_of(&entry["path"])),
                _ => format!("{}  ({} bytes)", str_of(&entry["path"]), entry["size"]),
            })
            .collect();
        lines.sort();
        return Ok(format!("{repo}:{}/\n{}", path, lines.join("\n")));
    }
    if str_of(&listing["type"]) != "file" {
        return Err(format!(
            "{path} is a {}, not a file",
            str_of(&listing["type"])
        ));
    }
    let text = api.get_raw(&endpoint, "application/vnd.github.raw", &query)?;
    if text.contains('\0') {
        return Err(format!(
            "{path} is binary ({} bytes); download it from {}",
            listing["size"],
            str_of(&listing["download_url"])
        ));
    }
    Ok(cut(text, MAX_TEXT))
}

fn format_repo_line(item: &Value) -> String {
    format!(
        "{} ★{} {}{}  {}",
        str_of(&item["full_name"]),
        item["stargazers_count"],
        item["language"].as_str().unwrap_or("-"),
        if item["archived"].as_bool() == Some(true) {
            " (archived)"
        } else {
            ""
        },
        item["description"].as_str().unwrap_or("")
    )
}

fn format_issue_line(item: &Value) -> String {
    let kind = if item.get("pull_request").is_some() || item.get("head").is_some() {
        "PR "
    } else {
        ""
    };
    let labels = names(&item["labels"], "name");
    format!(
        "{kind}#{} [{}] {}{} — {}, {} comment(s), updated {}",
        item["number"],
        str_of(&item["state"]),
        str_of(&item["title"]),
        if labels.is_empty() {
            String::new()
        } else {
            format!(" [{labels}]")
        },
        str_of(&item["user"]["login"]),
        item["comments"].as_u64().unwrap_or(0),
        date(str_of(&item["updated_at"]))
    )
}

fn list_with_total(found: &Value, line: impl Fn(&Value) -> String) -> String {
    let items = found["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return "No results.".to_string();
    }
    let mut out = format!(
        "{} result(s), showing {}:\n",
        found["total_count"],
        items.len()
    );
    for item in &items {
        out.push_str(&line(item));
        out.push('\n');
    }
    out
}

fn lines_of(items: &Value, line: impl Fn(&Value) -> String) -> String {
    let lines: Vec<String> = items.as_array().into_iter().flatten().map(line).collect();
    if lines.is_empty() {
        "None.".to_string()
    } else {
        lines.join("\n")
    }
}

fn names(items: &Value, key: &str) -> String {
    items
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item[key].as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn str_of(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

/// `2024-05-01T12:00:00Z` → `2024-05-01`.
fn date(timestamp: &str) -> &str {
    timestamp.get(..10).unwrap_or(timestamp)
}

pub(crate) fn cut(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n\n[… cut]");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: Value) -> GitHubInput {
        serde_json::from_value(value).expect("input")
    }

    #[test]
    fn repo_accepts_owner_name_and_urls() {
        assert_eq!(
            input(json!({"action":"repo","repo":"a/b"})).repo(),
            Ok("a/b")
        );
        assert_eq!(
            input(json!({"action":"repo","repo":"https://github.com/a/b.git"})).repo(),
            Ok("a/b")
        );
        assert!(input(json!({"action":"repo","repo":"a"})).repo().is_err());
        assert!(input(json!({"action":"repo","repo":"a/b/c"}))
            .repo()
            .is_err());
        assert!(input(json!({"action":"repo"})).repo().is_err());
    }

    #[test]
    fn lines_are_compact() {
        let issue = json!({
            "number": 7, "state": "open", "title": "Crash", "comments": 2,
            "user": {"login": "x"}, "updated_at": "2026-01-02T03:04:05Z",
            "labels": [{"name": "bug"}], "pull_request": {}
        });
        assert_eq!(
            format_issue_line(&issue),
            "PR #7 [open] Crash [bug] — x, 2 comment(s), updated 2026-01-02"
        );
        let repo = json!({"full_name": "a/b", "stargazers_count": 5, "language": "Rust", "description": "d"});
        assert_eq!(format_repo_line(&repo), "a/b ★5 Rust  d");
        assert_eq!(cut("héllo".to_string(), 2), "h\n\n[… cut]");
    }

    #[test]
    fn unknown_actions_point_at_the_other_tool() {
        let error = read(input(json!({"action":"create_issue"}))).unwrap_err();
        assert!(error.contains("GitHubWrite"), "{error}");
        let error = write(input(json!({"action":"search_repos"}))).unwrap_err();
        assert!(error.contains("GitHub tool"), "{error}");
    }
}

/// Live calls against api.github.com and huggingface.co; run with
/// `cargo test -p tools --lib live_hub -- --ignored --nocapture`.
#[cfg(test)]
mod live_hub {
    use serde_json::json;

    fn run(name: &str, input: serde_json::Value) {
        let result = crate::execute_tool(name, &input);
        let text = match &result {
            Ok(text) | Err(text) => text,
        };
        println!(
            "=== {name} {input}\n{}\n",
            text.chars().take(1500).collect::<String>()
        );
        assert!(result.is_ok(), "{name} {input}: {text}");
    }

    #[test]
    #[ignore = "network"]
    fn live_hub() {
        run(
            "GitHub",
            json!({"action": "search_repos", "query": "llama.cpp", "limit": 3}),
        );
        run(
            "GitHub",
            json!({"action": "repo", "repo": "ggml-org/llama.cpp"}),
        );
        run(
            "GitHub",
            json!({"action": "issues", "repo": "ggml-org/llama.cpp", "limit": 3}),
        );
        run(
            "GitHub",
            json!({"action": "pulls", "repo": "ggml-org/llama.cpp", "limit": 2}),
        );
        run(
            "GitHub",
            json!({"action": "file", "repo": "ggml-org/llama.cpp", "path": "docs"}),
        );
        run(
            "GitHub",
            json!({"action": "file", "repo": "ggml-org/llama.cpp", "path": "CMakeLists.txt"}),
        );
        run(
            "GitHub",
            json!({"action": "commits", "repo": "ggml-org/llama.cpp", "limit": 3}),
        );
        run(
            "GitHub",
            json!({"action": "releases", "repo": "ggml-org/llama.cpp", "limit": 2}),
        );
        run(
            "GitHub",
            json!({"action": "runs", "repo": "ggml-org/llama.cpp", "limit": 2}),
        );
        run(
            "GitHub",
            json!({"action": "search_issues", "query": "repo:ggml-org/llama.cpp is:pr is:merged", "limit": 2}),
        );
        run(
            "GitHub",
            json!({"action": "pr", "repo": "rust-lang/rust", "number": 150000, "diff": true}),
        );
        run("GitHub", json!({"action": "user", "login": "torvalds"}));
        run(
            "GitHub",
            json!({"action": "api", "path": "/repos/ggml-org/llama.cpp/languages"}),
        );
        run(
            "HuggingFace",
            json!({"action": "search", "query": "qwen3", "filter": "gguf", "limit": 3}),
        );
        run(
            "HuggingFace",
            json!({"action": "trending", "kind": "dataset", "limit": 3}),
        );
        run(
            "HuggingFace",
            json!({"action": "trending", "kind": "space", "limit": 3}),
        );
        run(
            "HuggingFace",
            json!({"action": "info", "repo": "Qwen/Qwen3-0.6B"}),
        );
        run(
            "HuggingFace",
            json!({"action": "info", "repo": "unsloth/Qwen3-0.6B-GGUF"}),
        );
        run(
            "HuggingFace",
            json!({"action": "files", "repo": "unsloth/Qwen3-0.6B-GGUF", "limit": 5}),
        );
        run(
            "HuggingFace",
            json!({"action": "read", "repo": "Qwen/Qwen3-0.6B", "path": "config.json"}),
        );
        run("HuggingFace", json!({"action": "papers", "limit": 2}));
        run(
            "HuggingFace",
            json!({"action": "papers", "query": "speculative decoding", "limit": 2}),
        );
    }
}
