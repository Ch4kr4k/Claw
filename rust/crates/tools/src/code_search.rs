//! `CodeSearch`: find code by meaning-ish queries, like opencode's and
//! Hermes' codebase search. Three sources, ranked together:
//! - a symbol index (functions, types, classes, modules, macros) built by
//!   scanning definitions in common languages; cached per repository under
//!   `~/.claw/index/` and refreshed only for files whose size or mtime
//!   changed, so later searches are fast;
//! - ranked text search: ripgrep for every query term, scored by how many
//!   different terms meet in the same stretch of a file;
//! - file paths that match the terms.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use runtime::PermissionMode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{to_pretty_json, ToolSpec};

/// Files larger than this are not indexed.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// At most this many files are indexed.
const MAX_FILES: usize = 60_000;
/// Lines that count as one stretch of a file for text ranking.
const WINDOW: usize = 30;
/// Bump when the index format or symbol rules change.
const INDEX_VERSION: u32 = 1;

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "CodeSearch",
        description: "Search the codebase by what you are looking for (\"where are tool permissions checked\", \"session save\", \"parse_args\"), not just exact text. Combines an indexed symbol table (functions, structs/classes, traits/interfaces, modules, macros across Rust, Python, JS/TS, Go, C/C++, Java/Kotlin, Ruby, Lua, shell), ranked text search (ripgrep, scored by how many query terms meet in the same part of a file) and file paths. The index is cached per repository and updated incrementally. kind: \"auto\" (default, all three), \"symbols\", \"text\" or \"files\"; \"callers\" (where a function is called from, by enclosing function) and \"callees\" (the known functions a definition calls) take a symbol name as the query. Use grep_search for one exact pattern and read_file to open the results.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "path": { "type": "string", "description": "Repository or directory to search (default: current directory)." },
                "kind": { "type": "string", "enum": ["auto", "symbols", "text", "files", "callers", "callees"] },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                "reindex": { "type": "boolean", "description": "Rebuild the symbol index from scratch." }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::ReadOnly,
    }]
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    (name == "CodeSearch").then(|| {
        serde_json::from_value::<SearchInput>(input.clone())
            .map_err(|error| error.to_string())
            .and_then(|input| search(&input))
    })
}

#[derive(Deserialize)]
struct SearchInput {
    query: String,
    path: Option<String>,
    kind: Option<String>,
    limit: Option<usize>,
    reindex: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Symbol {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) line: usize,
    pub(crate) text: String,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct FileEntry {
    size: u64,
    mtime: u64,
    pub(crate) symbols: Vec<Symbol>,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Index {
    version: u32,
    root: String,
    pub(crate) files: BTreeMap<String, FileEntry>,
}

fn search(input: &SearchInput) -> Result<String, String> {
    let query = input.query.trim();
    if query.is_empty() {
        return Err("query must not be empty".to_string());
    }
    let root = std::fs::canonicalize(input.path.as_deref().unwrap_or("."))
        .map_err(|error| format!("cannot open the search path: {error}"))?;
    let kind = input.kind.as_deref().unwrap_or("auto");
    let limit = input.limit.unwrap_or(20);
    if matches!(kind, "callers" | "callees") {
        let files = list_files(&root);
        let (index, _) = load_index(&root, &files, input.reindex.unwrap_or(false));
        let found = if kind == "callers" {
            callers(&root, &index, query, limit)
        } else {
            callees(&root, &index, query, limit)
        };
        return to_pretty_json(
            json!({ "root": root.display().to_string(), "symbol": query, kind: found }),
        );
    }
    let terms = terms(query);
    let files = list_files(&root);
    let mut out = serde_json::Map::new();
    out.insert("root".into(), json!(root.display().to_string()));
    out.insert("query".into(), json!(query));
    if matches!(kind, "auto" | "symbols") {
        let (index, updated) = load_index(&root, &files, input.reindex.unwrap_or(false));
        out.insert("indexed_files".into(), json!(index.files.len()));
        out.insert("reindexed_files".into(), json!(updated));
        out.insert(
            "symbols".into(),
            json!(rank_symbols(&index, query, &terms, limit)),
        );
    }
    if matches!(kind, "auto" | "text") {
        out.insert(
            "text".into(),
            json!(rank_text(&root, &files, &terms, limit)),
        );
    }
    if matches!(kind, "auto" | "files") {
        out.insert("files".into(), json!(rank_paths(&files, &terms, limit)));
    }
    to_pretty_json(Value::Object(out))
}

/// Lowercase query terms, with `snake_case` and `camelCase` split too.
fn terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if word.len() < 2 || STOP_WORDS.contains(&word.to_lowercase().as_str()) {
            continue;
        }
        terms.push(word.to_lowercase());
        for part in split_identifier(word) {
            if part.len() >= 3 && !terms.contains(&part) {
                terms.push(part);
            }
        }
    }
    terms.dedup();
    terms
}

const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "where", "what", "how", "is", "are", "of", "in", "to", "a", "an", "do",
    "does", "find", "code", "that", "this", "with", "from", "which", "when", "get",
];

pub(crate) fn split_identifier(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for ch in word.chars() {
        if ch == '_' || ch == '-' {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current).to_lowercase());
            }
            previous_lower = false;
            continue;
        }
        if ch.is_uppercase() && previous_lower && !current.is_empty() {
            parts.push(std::mem::take(&mut current).to_lowercase());
        }
        previous_lower = ch.is_lowercase() || ch.is_ascii_digit();
        current.push(ch);
    }
    if !current.is_empty() {
        parts.push(current.to_lowercase());
    }
    parts
}

/// Files to search: git's view (tracked and untracked, minus ignored) in a
/// repository, otherwise a walk that skips build and dependency folders.
pub(crate) fn list_files(root: &Path) -> Vec<String> {
    let git = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-co", "--exclude-standard", "-z"])
        .output();
    if let Ok(output) = git {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout)
                .split('\0')
                .filter(|path| !path.is_empty())
                .take(MAX_FILES)
                .map(str::to_string)
                .collect();
        }
    }
    let mut files = Vec::new();
    walk(root, root, &mut files);
    files
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<String>) {
    const SKIP: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        "build",
        "dist",
        ".venv",
        "venv",
        "__pycache__",
        ".cache",
        ".claw",
        ".idea",
        ".vscode",
    ];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_FILES {
            return;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !SKIP.contains(&name.as_str()) && !name.starts_with('.') {
                walk(root, &path, files);
            }
        } else if kind.is_file() {
            if let Ok(relative) = path.strip_prefix(root) {
                files.push(relative.to_string_lossy().into_owned());
            }
        }
    }
}

fn index_path(root: &Path) -> PathBuf {
    let home = std::env::var_os("CLAW_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claw")))
        .unwrap_or_else(|| PathBuf::from(".claw"));
    // FNV-1a of the root path names the cache file.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    home.join("index").join(format!("{hash:016x}.json"))
}

/// Loads the cached index and brings it up to date with `files`. Returns it
/// and how many files were (re)scanned.
pub(crate) fn load_index(root: &Path, files: &[String], rebuild: bool) -> (Index, usize) {
    let path = index_path(root);
    let mut index: Index = if rebuild {
        Index::default()
    } else {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .filter(|index: &Index| index.version == INDEX_VERSION)
            .unwrap_or_default()
    };
    index.version = INDEX_VERSION;
    index.root = root.display().to_string();
    let mut updated = 0;
    let mut fresh = BTreeMap::new();
    for file in files {
        let Some(language) = language(file) else {
            continue;
        };
        let full = root.join(file);
        let Ok(metadata) = std::fs::metadata(&full) else {
            continue;
        };
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_secs());
        if let Some(entry) = index.files.remove(file) {
            if entry.size == metadata.len() && entry.mtime == mtime {
                fresh.insert(file.clone(), entry);
                continue;
            }
        }
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        updated += 1;
        fresh.insert(
            file.clone(),
            FileEntry {
                size: metadata.len(),
                mtime,
                symbols: extract_symbols(&text, language),
            },
        );
    }
    index.files = fresh;
    if updated > 0 || rebuild {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string(&index) {
            let _ = std::fs::write(&path, text);
        }
    }
    (index, updated)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    Rust,
    Python,
    Script,
    Go,
    C,
    Jvm,
    Ruby,
    Lua,
    Shell,
}

pub(crate) fn language(path: &str) -> Option<Language> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => Language::Rust,
        "py" | "pyi" => Language::Python,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Language::Script,
        "go" => Language::Go,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "cu" | "cuh" | "m" | "mm" => Language::C,
        "java" | "kt" | "kts" | "scala" | "cs" | "swift" | "dart" => Language::Jvm,
        "rb" => Language::Ruby,
        "lua" => Language::Lua,
        "sh" | "bash" | "zsh" => Language::Shell,
        _ => return None,
    })
}

/// The identifier at the start of `text` (letters, digits, `_`, `$`).
fn ident(text: &str) -> Option<String> {
    let name: String = text
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    (!name.is_empty() && !name.chars().next().is_some_and(|c| c.is_ascii_digit())).then_some(name)
}

/// Strips leading modifiers such as `pub`, `export`, `async`, `static`.
fn strip_modifiers<'a>(mut line: &'a str, modifiers: &[&str]) -> &'a str {
    loop {
        let before = line;
        for modifier in modifiers {
            if let Some(rest) = line.strip_prefix(modifier) {
                line = rest.trim_start();
            }
        }
        if line == before {
            return line;
        }
    }
}

pub(crate) fn extract_symbols(text: &str, language: Language) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (number, raw) in lines.iter().enumerate() {
        let line = raw.trim_start();
        let indented = raw.len() != line.len();
        let found = match language {
            Language::Rust => rust_symbol(line),
            Language::Python => keyword_symbol(
                strip_modifiers(line, &["async "]),
                &[("def ", "function"), ("class ", "class")],
            ),
            Language::Script => script_symbol(line),
            Language::Go => go_symbol(line),
            Language::C => c_symbol(line, indented, lines.get(number + 1).copied()),
            Language::Jvm => keyword_symbol(
                strip_modifiers(
                    line,
                    &[
                        "public ",
                        "private ",
                        "protected ",
                        "internal ",
                        "static ",
                        "final ",
                        "abstract ",
                        "open ",
                        "data ",
                        "sealed ",
                        "override ",
                        "suspend ",
                        "inline ",
                        "partial ",
                    ],
                ),
                &[
                    ("class ", "class"),
                    ("interface ", "interface"),
                    ("enum ", "enum"),
                    ("object ", "object"),
                    ("record ", "class"),
                    ("struct ", "struct"),
                    ("fun ", "function"),
                    ("func ", "function"),
                ],
            ),
            Language::Ruby => keyword_symbol(
                line,
                &[
                    ("def ", "function"),
                    ("class ", "class"),
                    ("module ", "module"),
                ],
            ),
            Language::Lua => keyword_symbol(
                strip_modifiers(line, &["local "]),
                &[("function ", "function")],
            ),
            Language::Shell => shell_symbol(line),
        };
        if let Some((name, kind)) = found {
            symbols.push(Symbol {
                name,
                kind: kind.to_string(),
                line: number + 1,
                text: raw.trim().chars().take(160).collect(),
            });
        }
    }
    symbols
}

fn keyword_symbol(line: &str, keywords: &[(&str, &'static str)]) -> Option<(String, &'static str)> {
    keywords.iter().find_map(|(keyword, kind)| {
        line.strip_prefix(keyword)
            .and_then(ident)
            .map(|name| (name, *kind))
    })
}

fn rust_symbol(line: &str) -> Option<(String, &'static str)> {
    let line = strip_modifiers(
        line,
        &[
            "pub(crate) ",
            "pub(super) ",
            "pub(self) ",
            "pub ",
            "async ",
            "unsafe ",
            "extern \"C\" ",
            "default ",
        ],
    );
    if let Some(rest) = line.strip_prefix("impl") {
        // `impl<T> Trait for Type` or `impl Type`: index the type.
        let rest = rest.trim_start();
        let rest = if rest.starts_with('<') {
            rest.split_once('>').map_or(rest, |(_, after)| after)
        } else {
            rest
        };
        let target = rest.split(" for ").last().unwrap_or(rest);
        return ident(target).map(|name| (name, "impl"));
    }
    if let Some(rest) = line.strip_prefix("macro_rules!") {
        return ident(rest).map(|name| (name, "macro"));
    }
    let found = keyword_symbol(
        line,
        &[
            ("fn ", "function"),
            ("struct ", "struct"),
            ("enum ", "enum"),
            ("trait ", "trait"),
            ("type ", "type"),
            ("mod ", "module"),
            ("union ", "struct"),
        ],
    );
    if found.is_some() {
        return found;
    }
    for keyword in ["const ", "static "] {
        if let Some(rest) = line.strip_prefix(keyword) {
            let rest = rest.strip_prefix("mut ").unwrap_or(rest);
            if let Some(name) = ident(rest) {
                if rest[name.len()..].trim_start().starts_with(':') {
                    return Some((name, "constant"));
                }
            }
        }
    }
    None
}

fn script_symbol(line: &str) -> Option<(String, &'static str)> {
    let line = strip_modifiers(
        line,
        &[
            "export default ",
            "export ",
            "declare ",
            "async ",
            "abstract ",
        ],
    );
    let found = keyword_symbol(
        line,
        &[
            ("function* ", "function"),
            ("function ", "function"),
            ("class ", "class"),
            ("interface ", "interface"),
            ("enum ", "enum"),
            ("namespace ", "module"),
        ],
    );
    if found.is_some() {
        return found;
    }
    if let Some(rest) = line.strip_prefix("type ") {
        let name = ident(rest)?;
        return rest.contains('=').then_some((name, "type"));
    }
    for keyword in ["const ", "let ", "var "] {
        if let Some(rest) = line.strip_prefix(keyword) {
            let name = ident(rest)?;
            let value = rest.split_once('=')?.1.trim_start();
            if value.starts_with("(")
                || value.starts_with("async")
                || value.starts_with("function")
                || value.contains("=>")
            {
                return Some((name, "function"));
            }
            if value.starts_with("class") {
                return Some((name, "class"));
            }
        }
    }
    None
}

fn go_symbol(line: &str) -> Option<(String, &'static str)> {
    if let Some(rest) = line.strip_prefix("func ") {
        // Methods: `func (r *T) Name(`.
        let rest = if rest.starts_with('(') {
            rest.split_once(')').map_or(rest, |(_, after)| after)
        } else {
            rest
        };
        return ident(rest).map(|name| (name, "function"));
    }
    keyword_symbol(line, &[("type ", "type")])
}

fn c_symbol(line: &str, indented: bool, next: Option<&str>) -> Option<(String, &'static str)> {
    if let Some(rest) = line.strip_prefix("#define ") {
        return ident(rest).map(|name| (name, "macro"));
    }
    let stripped = strip_modifiers(line, &["typedef ", "template<typename T> ", "export "]);
    for (keyword, kind) in [
        ("struct ", "struct"),
        ("class ", "class"),
        ("enum class ", "enum"),
        ("enum ", "enum"),
        ("union ", "struct"),
        ("namespace ", "module"),
    ] {
        if let Some(rest) = stripped.strip_prefix(keyword) {
            let name = ident(rest)?;
            let after = rest[name.len()..].trim_start();
            // A definition, not `struct foo *bar;`.
            if after.is_empty() || after.starts_with('{') || after.starts_with(':') {
                return Some((name, kind));
            }
            return None;
        }
    }
    // A function definition at the top level: `type name(args)` followed by
    // `{` on this line or the next.
    if indented || line.starts_with('#') || line.starts_with("//") || line.starts_with('*') {
        return None;
    }
    let open = line.find('(')?;
    let head = line[..open].trim_end();
    let first = head.split_whitespace().next()?;
    if [
        "if", "for", "while", "switch", "return", "else", "do", "case", "sizeof", "using",
    ]
    .contains(&first)
        || line.ends_with(';')
        || !head.contains(|c: char| c.is_whitespace() || c == '*' || c == '&')
    {
        return None;
    }
    let opens_body = line.trim_end().ends_with('{')
        || line.trim_end().ends_with(')')
            && next.is_some_and(|next| next.trim_start().starts_with('{'))
        || line.trim_end().ends_with(',');
    if !opens_body {
        return None;
    }
    let name: String = head
        .rsplit(|c: char| c.is_whitespace() || c == '*' || c == '&')
        .next()?
        .to_string();
    let name = name.rsplit("::").next().unwrap_or(&name).to_string();
    ident(&name)
        .filter(|ident| *ident == name)
        .map(|name| (name, "function"))
}

fn shell_symbol(line: &str) -> Option<(String, &'static str)> {
    if let Some(rest) = line.strip_prefix("function ") {
        return ident(rest).map(|name| (name, "function"));
    }
    let name = ident(line)?;
    let after = line[name.len()..].trim_start();
    after.starts_with("()").then_some((name, "function"))
}

fn rank_symbols(index: &Index, query: &str, terms: &[String], limit: usize) -> Vec<Value> {
    let whole = query.trim().to_lowercase();
    let mut scored: Vec<(i64, &str, &Symbol)> = Vec::new();
    for (file, entry) in &index.files {
        let file_lower = file.to_lowercase();
        for symbol in &entry.symbols {
            let name = symbol.name.to_lowercase();
            let mut score = 0_i64;
            if name == whole {
                score += 100;
            } else if !whole.contains(' ') && name.contains(&whole) {
                score += 50;
            }
            let parts = split_identifier(&symbol.name);
            for term in terms {
                if name == *term {
                    score += 40;
                } else if parts.iter().any(|part| part == term) {
                    score += 18;
                } else if name.contains(term.as_str()) {
                    score += 10;
                }
                if file_lower.contains(term.as_str()) {
                    score += 3;
                }
            }
            if score == 0 {
                continue;
            }
            score += match symbol.kind.as_str() {
                "struct" | "class" | "trait" | "interface" | "enum" => 4,
                "function" => 3,
                "impl" => 1,
                _ => 0,
            };
            scored.push((score, file, symbol));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(score, file, symbol)| {
            json!({
                "name": symbol.name,
                "kind": symbol.kind,
                "location": format!("{file}:{}", symbol.line),
                "line": symbol.text,
                "score": score,
            })
        })
        .collect()
}

/// Text hits per (file, window of lines): which terms matched and where.
type Windows = HashMap<(String, usize), Vec<(usize, usize, String)>>;

fn rank_text(root: &Path, files: &[String], terms: &[String], limit: usize) -> Vec<Value> {
    if terms.is_empty() {
        return Vec::new();
    }
    let mut windows: Windows = HashMap::new();
    if !ripgrep(root, terms, &mut windows) {
        scan(root, files, terms, &mut windows);
    }
    type Ranked = (f64, (String, usize), Vec<(usize, usize, String)>);
    let mut ranked: Vec<Ranked> = windows
        .into_iter()
        .map(|(key, hits)| {
            let mut distinct: Vec<usize> = hits.iter().map(|(term, _, _)| *term).collect();
            distinct.sort_unstable();
            distinct.dedup();
            #[allow(clippy::cast_precision_loss)]
            let score = (distinct.len() * distinct.len()) as f64 * 10.0
                + (hits.len().min(20) as f64)
                + if terms
                    .iter()
                    .any(|term| key.0.to_lowercase().contains(term.as_str()))
                {
                    5.0
                } else {
                    0.0
                };
            (score, key, hits)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked
        .into_iter()
        .take(limit)
        .map(|(score, (file, _), mut hits)| {
            hits.sort_by_key(|(_, line, _)| *line);
            hits.dedup_by_key(|(_, line, _)| *line);
            let matched: Vec<&str> = {
                let mut ids: Vec<usize> = hits.iter().map(|(term, _, _)| *term).collect();
                ids.sort_unstable();
                ids.dedup();
                ids.into_iter().map(|id| terms[id].as_str()).collect()
            };
            json!({
                "location": format!("{file}:{}", hits[0].1),
                "matched_terms": matched,
                "lines": hits.iter().take(4).map(|(_, line, text)| format!("{line}: {text}")).collect::<Vec<_>>(),
                "score": score,
            })
        })
        .collect()
}

/// Fills `windows` from ripgrep; false when ripgrep is not available.
fn ripgrep(root: &Path, terms: &[String], windows: &mut Windows) -> bool {
    for (id, term) in terms.iter().enumerate() {
        let output = Command::new("rg")
            .args([
                "--no-heading",
                "-n",
                "-i",
                "-F",
                "--max-count",
                "50",
                "--max-columns",
                "240",
                "--max-columns-preview",
                "--max-filesize",
                "1M",
                "-e",
                term,
            ])
            .current_dir(root)
            .output();
        let Ok(output) = output else { return false };
        for line in String::from_utf8_lossy(&output.stdout).lines().take(3000) {
            let mut parts = line.splitn(3, ':');
            let (Some(file), Some(number), Some(text)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Ok(number) = number.parse::<usize>() else {
                continue;
            };
            windows
                .entry((file.to_string(), number / WINDOW))
                .or_default()
                .push((id, number, text.trim().chars().take(200).collect()));
        }
    }
    true
}

fn scan(root: &Path, files: &[String], terms: &[String], windows: &mut Windows) {
    for file in files {
        let full = root.join(file);
        if std::fs::metadata(&full).map_or(true, |meta| meta.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            let lower = line.to_lowercase();
            for (id, term) in terms.iter().enumerate() {
                if lower.contains(term.as_str()) {
                    windows
                        .entry((file.clone(), (index + 1) / WINDOW))
                        .or_default()
                        .push((id, index + 1, line.trim().chars().take(200).collect()));
                }
            }
        }
    }
}

// ───────────────────────────── call graph ─────────────────────────────

const NOT_CALLS: &[&str] = &[
    "if", "for", "while", "match", "return", "switch", "catch", "sizeof", "fn", "def", "function",
    "loop", "else", "new", "assert", "print", "println", "format", "vec", "Some", "Ok", "Err",
    "Box", "await", "typeof", "defined",
];

/// The definition a line of `file` belongs to: the last indexed symbol at
/// or above it.
fn enclosing<'a>(index: &'a Index, file: &str, line: usize) -> Option<&'a Symbol> {
    index
        .files
        .get(file)?
        .symbols
        .iter()
        .filter(|symbol| symbol.line <= line && symbol.kind != "impl")
        .max_by_key(|symbol| symbol.line)
}

/// Where `name` is called: each call site with the function it is in.
fn callers(root: &Path, index: &Index, name: &str, limit: usize) -> Vec<Value> {
    let Ok(output) = Command::new("rg")
        .args([
            "--no-heading",
            "-n",
            "-w",
            "-F",
            "--max-columns",
            "300",
            "-e",
            name,
        ])
        .current_dir(root)
        .output()
    else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(file), Some(number), Some(text)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(number) = number.parse::<usize>() else {
            continue;
        };
        // A call: the name followed by `(` (or a Rust macro / turbofish).
        let is_call = text.match_indices(name).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + name.len()..].trim_start();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                && (after.starts_with('(') || after.starts_with("!(") || after.starts_with("::<"))
        });
        if !is_call {
            continue;
        }
        let caller = enclosing(index, file, number);
        // The definition itself is not a call.
        if caller.is_some_and(|symbol| symbol.line == number && symbol.name == name) {
            continue;
        }
        rows.push(json!({
            "caller": caller.map(|symbol| symbol.name.clone()),
            "location": format!("{file}:{number}"),
            "line": text.trim().chars().take(160).collect::<String>(),
        }));
        if rows.len() >= limit.max(1) * 3 {
            break;
        }
    }
    rows
}

/// What the definition(s) of `name` call, among functions the index knows.
fn callees(root: &Path, index: &Index, name: &str, limit: usize) -> Vec<Value> {
    let mut known: BTreeMap<&str, String> = BTreeMap::new();
    for (file, entry) in &index.files {
        for symbol in &entry.symbols {
            if symbol.kind == "function" {
                known
                    .entry(symbol.name.as_str())
                    .or_insert_with(|| format!("{file}:{}", symbol.line));
            }
        }
    }
    let mut rows = Vec::new();
    for (file, entry) in &index.files {
        for symbol in entry.symbols.iter().filter(|symbol| symbol.name == name) {
            let Ok(text) = std::fs::read_to_string(root.join(file)) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            let end = symbol_end(&lines, &entry.symbols, symbol.line);
            let body = lines[symbol.line.min(lines.len())..end].join("\n");
            let mut called: Vec<String> = Vec::new();
            let bytes: Vec<char> = body.chars().collect();
            let mut word = String::new();
            for (i, ch) in bytes.iter().enumerate() {
                if ch.is_alphanumeric() || *ch == '_' {
                    word.push(*ch);
                    continue;
                }
                if !word.is_empty() {
                    let rest: String = bytes[i..].iter().take(3).collect();
                    let rest = rest.trim_start();
                    if (rest.starts_with('(') || rest.starts_with("!("))
                        && word != name
                        && !NOT_CALLS.contains(&word.as_str())
                        && known.contains_key(word.as_str())
                        && !called.contains(&word)
                    {
                        called.push(word.clone());
                    }
                }
                word.clear();
            }
            rows.push(json!({
                "definition": format!("{file}:{}", symbol.line),
                "calls": called
                    .iter()
                    .take(limit.max(1) * 3)
                    .map(|callee| json!({"name": callee, "defined_at": known.get(callee.as_str())}))
                    .collect::<Vec<_>>(),
            }));
        }
    }
    rows
}

/// The line after a definition's body: the next indexed symbol at the same
/// or an outer indent (or 400 lines on, or the end of the file).
fn symbol_end(lines: &[&str], symbols: &[Symbol], line: usize) -> usize {
    let indent = |text: &str| text.len() - text.trim_start().len();
    let start = line.saturating_sub(1);
    let own = lines.get(start).map_or(0, |text| indent(text));
    symbols
        .iter()
        .map(|symbol| symbol.line.saturating_sub(1))
        .filter(|&other| other > start && lines.get(other).is_some_and(|text| indent(text) <= own))
        .min()
        .unwrap_or(lines.len())
        .min(start + 400)
        .min(lines.len())
}

/// A fingerprint of a memory anchor's code: the whole file, or the body of
/// `symbol` in it. `None` when the file or symbol is gone.
#[must_use]
pub fn anchor_hash(root: &Path, path: &str, symbol: Option<&str>) -> Option<String> {
    let text = std::fs::read_to_string(root.join(path)).ok()?;
    let fnv = |text: &str| {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in text.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{:012x}", hash & 0xffff_ffff_ffff)
    };
    let Some(symbol) = symbol else {
        return Some(fnv(&text));
    };
    let symbols = extract_symbols(&text, language(path)?);
    let found = symbols.iter().find(|candidate| candidate.name == symbol)?;
    let lines: Vec<&str> = text.lines().collect();
    let end = symbol_end(&lines, &symbols, found.line);
    let body: Vec<&str> = lines[found.line.saturating_sub(1)..end]
        .iter()
        .map(|line| line.trim_end())
        .collect();
    Some(fnv(&body.join("\n")))
}

fn rank_paths(files: &[String], terms: &[String], limit: usize) -> Vec<String> {
    let mut scored: Vec<(usize, &String)> = files
        .iter()
        .filter_map(|file| {
            let lower = file.to_lowercase();
            let score = terms
                .iter()
                .filter(|term| lower.contains(term.as_str()))
                .count();
            (score > 0).then_some((score, file))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, file)| file.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{extract_symbols, terms, Language};

    #[test]
    fn splits_query_terms() {
        assert_eq!(
            terms("where is parseArgs and session_save"),
            vec![
                "parseargs",
                "parse",
                "args",
                "session_save",
                "session",
                "save"
            ]
        );
    }

    #[test]
    fn finds_definitions_in_several_languages() {
        let names = |text: &str, language| {
            extract_symbols(text, language)
                .into_iter()
                .map(|symbol| format!("{}:{}", symbol.kind, symbol.name))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(
                "pub(crate) async fn run_turn() {}\nimpl<T> Drop for Guard<T> {}\npub struct Session;\nconst MAX: usize = 1;",
                Language::Rust
            ),
            ["function:run_turn", "impl:Guard", "struct:Session", "constant:MAX"]
        );
        assert_eq!(
            names(
                "class Foo:\n    async def bar(self): pass",
                Language::Python
            ),
            ["class:Foo", "function:bar"]
        );
        assert_eq!(
            names(
                "export const handler = async (req) => {}\nexport interface Props {}\ntype Id = string",
                Language::Script
            ),
            ["function:handler", "interface:Props", "type:Id"]
        );
        assert_eq!(
            names(
                "static int llama_decode_impl(struct llama_context * ctx,\n                  int n) {\nstruct llama_model {\n#define GGML_MAX 4",
                Language::C
            ),
            ["function:llama_decode_impl", "struct:llama_model", "macro:GGML_MAX"]
        );
        assert_eq!(
            names(
                "func (s *Server) Serve(l net.Listener) error {\ntype Server struct {",
                Language::Go
            ),
            ["function:Serve", "type:Server"]
        );
    }

    #[test]
    fn call_graph_and_anchor_fingerprints() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = super::list_files(root);
        let (index, _) = super::load_index(root, &files, false);
        // `strip_ansi` is called by pty_tool's respond().
        let callers = super::callers(root, &index, "strip_ansi", 10);
        assert!(
            callers.iter().any(|row| row["caller"] == "respond"),
            "{callers:?}"
        );
        let callees = super::callees(root, &index, "respond", 20);
        let names: Vec<&str> = callees
            .iter()
            .flat_map(|row| row["calls"].as_array().unwrap().iter())
            .filter_map(|call| call["name"].as_str())
            .collect();
        assert!(
            names.contains(&"strip_ansi") && names.contains(&"sessions"),
            "{names:?}"
        );
        let a = super::anchor_hash(root, "src/pty_tool.rs", Some("strip_ansi")).expect("hash");
        assert_eq!(a.len(), 12);
        assert_ne!(
            Some(a),
            super::anchor_hash(root, "src/pty_tool.rs", Some("key_bytes"))
        );
        assert!(super::anchor_hash(root, "src/pty_tool.rs", Some("no_such_fn")).is_none());
        assert!(super::anchor_hash(root, "src/missing.rs", None).is_none());
    }
}
