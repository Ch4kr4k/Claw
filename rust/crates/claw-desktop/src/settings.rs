//! What the Settings screen reads and changes: skills (folders with a
//! `SKILL.md`), MCP servers and turned-off tools in claw's own
//! `~/.claw/settings.json`, so the terminal claw sees the same changes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Map, Value};

use crate::sessions::config_home;

// ------------------------------------------------------------------ skills

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

/// `~/.claw/skills`, where installed skills go.
pub fn skills_dir() -> PathBuf {
    config_home().join("skills")
}

/// Installed skills: `~/.claw/skills` and `~/.claude/skills`.
pub fn list_skills() -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut roots = vec![skills_dir()];
    roots.push(crate::sessions::home_dir().join(".claude").join("skills"));
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let file = dir.join("SKILL.md");
            if !file.is_file() {
                continue;
            }
            let text = fs::read_to_string(&file).unwrap_or_default();
            let name = frontmatter(&text, "name").unwrap_or_else(|| {
                dir.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            let description = frontmatter(&text, "description").unwrap_or_default();
            skills.push(Skill {
                name,
                description,
                dir,
            });
        }
    }
    skills.sort_by_key(|skill| skill.name.to_lowercase());
    skills
}

fn frontmatter(text: &str, key: &str) -> Option<String> {
    let body = text.strip_prefix("---")?;
    let end = body.find("\n---")?;
    body[..end].lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| {
            value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string()
        })
    })
}

/// Installs skills from a folder, a `SKILL.md`/`.md` file, a `.zip`/`.skill`
/// archive or a git URL. Returns the names installed.
pub fn install_skill(source: &str) -> Result<Vec<String>, String> {
    let source = source.trim();
    let target = skills_dir();
    fs::create_dir_all(&target).map_err(|error| error.to_string())?;
    let is_url = source.starts_with("https://")
        || source.starts_with("http://")
        || source.starts_with("git@");
    if is_url {
        let temp = temp_dir("skill-clone");
        let status = Command::new("git")
            .args(["clone", "--depth", "1", "--quiet", source])
            .arg(&temp)
            .status()
            .map_err(|error| format!("could not run git: {error}"))?;
        if !status.success() {
            let _ = fs::remove_dir_all(&temp);
            return Err(format!("git clone {source} failed"));
        }
        let result = install_found(&temp, &target);
        let _ = fs::remove_dir_all(&temp);
        return result;
    }
    let path = crate::expand_path(source);
    if path.is_dir() {
        return install_found(&path, &target);
    }
    if !path.is_file() {
        return Err(format!("{} does not exist", path.display()));
    }
    let lower = path.to_string_lossy().to_lowercase();
    if lower.ends_with(".zip") || lower.ends_with(".skill") {
        let temp = temp_dir("skill-unzip");
        let status = Command::new("unzip")
            .arg("-q")
            .arg(&path)
            .arg("-d")
            .arg(&temp)
            .status()
            .map_err(|error| format!("could not run unzip: {error}"))?;
        if !status.success() {
            let _ = fs::remove_dir_all(&temp);
            return Err("unzip failed".to_string());
        }
        let result = install_found(&temp, &target);
        let _ = fs::remove_dir_all(&temp);
        return result;
    }
    if lower.ends_with(".md") {
        let text = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let name = frontmatter(&text, "name")
            .or_else(|| {
                let stem = path.file_stem()?.to_string_lossy().into_owned();
                // A file called SKILL.md is named after its folder.
                if stem.eq_ignore_ascii_case("skill") {
                    path.parent()?
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                } else {
                    Some(stem)
                }
            })
            .ok_or("could not name the skill")?;
        let name = safe_name(&name);
        if path
            .file_name()
            .is_some_and(|file| file.eq_ignore_ascii_case("SKILL.md"))
        {
            if let Some(dir) = path.parent() {
                copy_dir(dir, &target.join(&name)).map_err(|error| error.to_string())?;
                return Ok(vec![name]);
            }
        }
        let dir = target.join(&name);
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        fs::write(dir.join("SKILL.md"), text).map_err(|error| error.to_string())?;
        return Ok(vec![name]);
    }
    Err("choose a skill folder, a SKILL.md, a .zip/.skill file or a git URL".to_string())
}

/// Copies every folder under `root` that holds a `SKILL.md` into `target`.
fn install_found(root: &Path, target: &Path) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    find_skill_dirs(root, 0, &mut found);
    if found.is_empty() {
        return Err("no SKILL.md found there".to_string());
    }
    let mut names = Vec::new();
    for dir in found {
        let text = fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
        let name = frontmatter(&text, "name")
            .or_else(|| {
                dir.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "skill".to_string());
        let name = safe_name(&name);
        let destination = target.join(&name);
        if destination == dir {
            continue;
        }
        if destination.exists() {
            fs::remove_dir_all(&destination).map_err(|error| error.to_string())?;
        }
        copy_dir(&dir, &destination).map_err(|error| error.to_string())?;
        names.push(name);
    }
    Ok(names)
}

fn find_skill_dirs(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if dir.join("SKILL.md").is_file() {
        found.push(dir.to_path_buf());
        return;
    }
    if depth >= 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if path.is_dir() && !hidden {
            find_skill_dirs(&path, depth + 1, found);
        }
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            copy_dir(&path, &to.join(&name))?;
        } else {
            fs::copy(&path, to.join(&name))?;
        }
    }
    Ok(())
}

fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "skill".to_string()
    } else {
        cleaned
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "claw-desktop-{prefix}-{}-{}",
        std::process::id(),
        crate::sessions::now_ms()
    ))
}

pub fn delete_skill(dir: &Path) -> std::io::Result<()> {
    fs::remove_dir_all(dir)
}

// ------------------------------------------------- claw's settings.json

fn settings_path() -> PathBuf {
    config_home().join("settings.json")
}

fn read_settings() -> Map<String, Value> {
    fs::read_to_string(settings_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

fn write_settings(settings: &Map<String, Value>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;
    let path = settings_path();
    // Keep the previous file, in case an edit goes wrong.
    if path.exists() {
        let _ = fs::copy(&path, path.with_extension("json.bak"));
    }
    fs::write(&path, text + "\n").map_err(|error| error.to_string())
}

// ------------------------------------------------------------- MCP servers

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    /// `command args…` for a local server, or the URL for a remote one.
    pub summary: String,
    pub remote: bool,
}

pub fn list_mcp() -> Vec<McpServer> {
    let settings = read_settings();
    let Some(Value::Object(servers)) = settings.get("mcpServers") else {
        return Vec::new();
    };
    servers
        .iter()
        .map(|(name, config)| {
            let url = config.get("url").and_then(Value::as_str);
            let summary = match url {
                Some(url) => url.to_string(),
                None => {
                    let mut parts = vec![config
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()];
                    if let Some(args) = config.get("args").and_then(Value::as_array) {
                        parts.extend(args.iter().filter_map(Value::as_str).map(str::to_string));
                    }
                    parts.join(" ")
                }
            };
            McpServer {
                name: name.clone(),
                summary,
                remote: url.is_some(),
            }
        })
        .collect()
}

/// Adds (or replaces) a server. `target` is a command line for a local
/// server, or an `http(s)://` URL for a remote one. `env` holds `KEY=value`
/// lines (headers for a remote server: `Name: value`).
pub fn add_mcp(name: &str, target: &str, env: &str) -> Result<(), String> {
    let name = name.trim();
    let target = target.trim();
    if name.is_empty() || target.is_empty() {
        return Err("give the server a name and a command or URL".to_string());
    }
    let config = if target.starts_with("http://") || target.starts_with("https://") {
        let mut headers = Map::new();
        for line in env.lines().filter(|line| !line.trim().is_empty()) {
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| format!("header lines look like `Name: value`, got `{line}`"))?;
            headers.insert(key.trim().to_string(), json!(value.trim()));
        }
        let mut config = json!({"type": "http", "url": target});
        if !headers.is_empty() {
            config["headers"] = Value::Object(headers);
        }
        config
    } else {
        let words = split_command(target);
        let (command, args) = words.split_first().ok_or("the command is empty")?;
        let mut vars = Map::new();
        for line in env.lines().filter(|line| !line.trim().is_empty()) {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("environment lines look like `KEY=value`, got `{line}`"))?;
            vars.insert(key.trim().to_string(), json!(value.trim()));
        }
        let mut config = json!({"command": command, "args": args});
        if !vars.is_empty() {
            config["env"] = Value::Object(vars);
        }
        config
    };
    let mut settings = read_settings();
    let servers = settings
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(servers) = servers else {
        return Err("mcpServers in settings.json is not an object".to_string());
    };
    servers.insert(name.to_string(), config);
    write_settings(&settings)
}

pub fn remove_mcp(name: &str) -> Result<(), String> {
    let mut settings = read_settings();
    if let Some(Value::Object(servers)) = settings.get_mut("mcpServers") {
        servers.remove(name);
        if servers.is_empty() {
            settings.remove("mcpServers");
        }
    }
    write_settings(&settings)
}

/// Splits a command line at spaces, keeping quoted parts together.
fn split_command(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

// ------------------------------------------------------------------- tools

/// The tools worth switching off, with what they do.
/// Short descriptions for the common tools; the rest use the first sentence
/// of the tool's own description.
const TOOL_BLURBS: &[(&str, &str)] = &[
    ("bash", "Run shell commands"),
    ("read_file", "Read files"),
    ("write_file", "Create and overwrite files"),
    ("edit_file", "Edit files in place"),
    ("glob_search", "Find files by name"),
    ("grep_search", "Search file contents"),
    ("WebSearch", "Search the web"),
    ("WebFetch", "Read web pages and find images on them"),
    ("Download", "Save files from the web into ~/Downloads"),
    (
        "X11",
        "Control the desktop: windows, typing, clicks, screenshots",
    ),
    ("Agent", "Start sub-agents for side tasks"),
    ("TodoWrite", "Keep a task list"),
    ("Skill", "Use installed skills"),
    ("NotebookEdit", "Edit Jupyter notebooks"),
    ("REPL", "Run code in a Python/Node REPL"),
    ("LSP", "Ask language servers (go to definition, …)"),
    ("CronCreate", "Schedule prompts inside a session"),
    ("write_docx", "Write Word documents"),
    ("write_pptx", "Write PowerPoint decks"),
    ("write_xlsx", "Write Excel sheets"),
];

/// Every tool claw can give the model (the same registry the agent uses),
/// with a one-line description, curated ones first.
pub fn tools() -> Vec<(String, String)> {
    let specs: Vec<_> = tools::mvp_tool_specs()
        .into_iter()
        .filter(|spec| !spec.name.starts_with("Testing"))
        .collect();
    let mut list: Vec<(String, String)> = TOOL_BLURBS
        .iter()
        .filter(|(name, _)| specs.iter().any(|spec| spec.name == *name))
        .map(|(name, blurb)| ((*name).to_string(), (*blurb).to_string()))
        .collect();
    for spec in specs {
        if list.iter().any(|(name, _)| name == spec.name) {
            continue;
        }
        let first = spec
            .description
            .split_inclusive(". ")
            .next()
            .unwrap_or(spec.description)
            .trim()
            .trim_end_matches('.');
        list.push((spec.name.to_string(), first.to_string()));
    }
    list
}

/// Tools turned off with `permissions.deniedTools`.
pub fn denied_tools() -> Vec<String> {
    read_settings()
        .get("permissions")
        .and_then(|permissions| permissions.get("deniedTools"))
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn set_tool_enabled(tool: &str, enabled: bool) -> Result<(), String> {
    let mut settings = read_settings();
    let permissions = settings
        .entry("permissions")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(permissions) = permissions else {
        return Err("permissions in settings.json is not an object".to_string());
    };
    let mut denied: Vec<String> = permissions
        .get("deniedTools")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    denied.retain(|name| name != tool);
    if !enabled {
        denied.push(tool.to_string());
    }
    if denied.is_empty() {
        permissions.remove("deniedTools");
    } else {
        permissions.insert("deniedTools".to_string(), json!(denied));
    }
    write_settings(&settings)
}

#[cfg(test)]
mod tests {
    use super::{frontmatter, safe_name, split_command};

    #[test]
    fn reads_skill_frontmatter() {
        let text = "---\nname: pdf-tools\ndescription: \"Work with PDFs\"\n---\n# Body\n";
        assert_eq!(frontmatter(text, "name").as_deref(), Some("pdf-tools"));
        assert_eq!(
            frontmatter(text, "description").as_deref(),
            Some("Work with PDFs")
        );
        assert_eq!(frontmatter("# no frontmatter", "name"), None);
    }

    #[test]
    fn splits_commands_with_quotes() {
        assert_eq!(
            split_command(r#"npx -y "@scope/server name" --dir '/a b'"#),
            vec!["npx", "-y", "@scope/server name", "--dir", "/a b"]
        );
        assert_eq!(safe_name("My Skill!"), "my-skill");
    }
}
