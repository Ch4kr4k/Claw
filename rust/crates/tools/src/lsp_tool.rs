//! The `LSP` tool: code intelligence from real language servers, like
//! Claude Code's. A server is started on first use for a file's language
//! (rust-analyzer, pyright/pylsp, typescript-language-server, gopls, clangd,
//! or what `lspServers` in the settings names) and kept for the session.
//! Files are opened in (and re-synced to) the server before each request;
//! diagnostics come from the server's `publishDiagnostics` notifications.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

/// How long one request may take (an indexing server answers slowly).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for a file's diagnostics after opening or changing it.
const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(8);

#[derive(Debug, Deserialize)]
pub(crate) struct LspInput {
    /// `goToDefinition`, `findReferences`, `hover`, `documentSymbol`,
    /// `workspaceSymbol`, `goToImplementation`, `prepareCallHierarchy`,
    /// `incomingCalls`, `outgoingCalls`, `diagnostics`.
    #[serde(alias = "action")]
    operation: String,
    #[serde(alias = "path", alias = "file_path", rename = "filePath")]
    file_path: Option<String>,
    /// 1-based, as editors show it.
    line: Option<u32>,
    /// 1-based.
    character: Option<u32>,
    query: Option<String>,
}

/// A language server command for files with these extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerSpec {
    language: String,
    command: String,
    args: Vec<String>,
}

/// File extensions, the language, and candidate commands (program and
/// arguments) in order of preference.
type BuiltinServer = (
    &'static [&'static str],
    &'static str,
    &'static [&'static [&'static str]],
);

fn builtin_servers() -> Vec<BuiltinServer> {
    // (extensions, language id, candidate commands in order of preference)
    vec![
        (&["rs"], "rust", &[&["rust-analyzer"]]),
        (
            &["py", "pyi"],
            "python",
            &[
                &["pyright-langserver", "--stdio"],
                &["basedpyright-langserver", "--stdio"],
                &["pylsp"],
            ],
        ),
        (
            &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
            "typescript",
            &[&["typescript-language-server", "--stdio"]],
        ),
        (&["go"], "go", &[&["gopls"]]),
        (
            &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "m", "mm"],
            "cpp",
            &[&["clangd"]],
        ),
        (&["java"], "java", &[&["jdtls"]]),
        (&["lua"], "lua", &[&["lua-language-server"]]),
        (&["rb"], "ruby", &[&["solargraph", "stdio"], &["ruby-lsp"]]),
        (&["zig"], "zig", &[&["zls"]]),
    ]
}

fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        && (program != "rust-analyzer" || rust_analyzer_works())
}

/// rustup installs a `rust-analyzer` shim that fails when the component is
/// missing; try it once.
fn rust_analyzer_works() -> bool {
    static WORKS: OnceLock<bool> = OnceLock::new();
    *WORKS.get_or_init(|| {
        Command::new("rust-analyzer")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// The server for `path`: `lspServers` in the settings first
/// (`{"rs": {"command": "rust-analyzer", "args": []}}`), then the built-in
/// list, using the first command found on PATH.
fn server_for(path: &Path) -> Result<ServerSpec, String> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if let Some(spec) = configured_server(&extension) {
        return Ok(spec);
    }
    for (extensions, language, candidates) in builtin_servers() {
        if !extensions.contains(&extension.as_str()) {
            continue;
        }
        for candidate in candidates {
            if on_path(candidate[0]) {
                return Ok(ServerSpec {
                    language: language.to_string(),
                    command: candidate[0].to_string(),
                    args: candidate[1..].iter().map(|a| (*a).to_string()).collect(),
                });
            }
        }
        let names: Vec<&str> = candidates.iter().map(|c| c[0]).collect();
        return Err(format!(
            "no {language} language server found: install one of {} (or set lspServers.{extension} in settings)",
            names.join(", ")
        ));
    }
    Err(format!(
        "no language server known for .{extension} files; set lspServers.{extension} = {{\"command\": …, \"args\": […]}} in settings"
    ))
}

fn configured_server(extension: &str) -> Option<ServerSpec> {
    let cwd = std::env::current_dir().ok()?;
    let config = runtime::ConfigLoader::default_for(cwd).load().ok()?;
    let entry = config
        .get("lspServers")?
        .as_object()?
        .get(extension)?
        .clone();
    let entry = entry.as_object()?;
    let command = entry.get("command")?.as_str()?.to_string();
    let args = entry
        .get("args")
        .and_then(|a| a.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(ServerSpec {
        language: extension.to_string(),
        command,
        args,
    })
}

/// Requests waiting for their response, by id.
type Pending = Mutex<HashMap<i64, Sender<Value>>>;
/// Each file's latest diagnostics and when they arrived, by URI.
type Diagnostics = Mutex<BTreeMap<String, (Instant, Vec<Value>)>>;
/// Running servers, by (command, project root).
type Servers = Mutex<HashMap<(String, PathBuf), Arc<Server>>>;

/// A running language server.
struct Server {
    child: Child,
    stdin: Mutex<ChildStdin>,
    next_id: Mutex<i64>,
    pending: Arc<Pending>,
    diagnostics: Arc<Diagnostics>,
    /// Open documents: URI → (version, text sent).
    open: Mutex<HashMap<String, (i64, String)>>,
    root: PathBuf,
    language: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn servers() -> &'static Servers {
    static SERVERS: OnceLock<Servers> = OnceLock::new();
    SERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn file_uri(path: &Path) -> String {
    let text = path.display().to_string();
    let mut uri = String::from("file://");
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(byte as char);
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

fn uri_path(uri: &str) -> String {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The project root for `path`: the nearest directory with a project
/// marker (for Rust, the outermost Cargo workspace inside the repository),
/// else the git top level, else the file's directory.
fn project_root(path: &Path) -> PathBuf {
    const MARKERS: &[&str] = &[
        "Cargo.toml",
        "go.mod",
        "package.json",
        "pyproject.toml",
        "setup.py",
        "compile_commands.json",
        "CMakeLists.txt",
        "Makefile",
    ];
    let start = path.parent().unwrap_or(path);
    let git_root = start.ancestors().find(|dir| dir.join(".git").exists());
    let inside_repo = |dir: &Path| git_root.is_none_or(|root| dir.starts_with(root));
    let nearest = start
        .ancestors()
        .take_while(|dir| inside_repo(dir))
        .find(|dir| MARKERS.iter().any(|marker| dir.join(marker).exists()));
    match nearest {
        Some(dir) if dir.join("Cargo.toml").exists() => dir
            .ancestors()
            .take_while(|dir| inside_repo(dir))
            .filter(|dir| dir.join("Cargo.toml").exists())
            .last()
            .unwrap_or(dir)
            .to_path_buf(),
        Some(dir) => dir.to_path_buf(),
        None => git_root.unwrap_or(start).to_path_buf(),
    }
}

fn start_server(spec: &ServerSpec, root: &Path) -> Result<Arc<Server>, String> {
    let mut child = Command::new(&spec.command)
        .args(&spec.args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", spec.command))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let pending: Arc<Pending> = Arc::default();
    let diagnostics: Arc<Diagnostics> = Arc::default();
    let server = Arc::new(Server {
        child,
        stdin: Mutex::new(stdin),
        next_id: Mutex::new(0),
        pending: Arc::clone(&pending),
        diagnostics: Arc::clone(&diagnostics),
        open: Mutex::new(HashMap::new()),
        root: root.to_path_buf(),
        language: spec.language.clone(),
    });
    let reader_server = Arc::downgrade(&server);
    std::thread::Builder::new()
        .name(format!("claw-lsp-{}", spec.language))
        .spawn(move || read_messages(stdout, &pending, &diagnostics, &reader_server))
        .map_err(|error| error.to_string())?;

    let root_uri = file_uri(root);
    server.request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "rootPath": root.display().to_string(),
            "workspaceFolders": [{ "uri": root_uri, "name": root.file_name().and_then(|n| n.to_str()).unwrap_or("root") }],
            "capabilities": {
                "textDocument": {
                    "synchronization": { "didSave": false, "dynamicRegistration": false },
                    "publishDiagnostics": { "relatedInformation": false },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": true },
                    "implementation": { "linkSupport": true },
                    "references": {},
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "callHierarchy": {}
                },
                "workspace": { "symbol": {}, "workspaceFolders": true, "configuration": true }
            },
            "clientInfo": { "name": "claw" }
        }),
        Duration::from_secs(60),
    )?;
    server.notify("initialized", json!({}))?;
    Ok(server)
}

fn read_messages(
    stdout: impl Read,
    pending: &Pending,
    diagnostics: &Diagnostics,
    server: &std::sync::Weak<Server>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut length = None;
        loop {
            let mut header = String::new();
            match reader.read_line(&mut header) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = length else { continue };
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        let method = message.get("method").and_then(Value::as_str);
        match (message.get("id"), method) {
            // A response to one of our requests.
            (Some(id), None) => {
                if let Some(sender) = id.as_i64().and_then(|id| {
                    pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id)
                }) {
                    let _ = sender.send(message);
                }
            }
            // A request from the server: answer what must be answered.
            (Some(id), Some(method)) => {
                let result = match method {
                    "workspace/configuration" => {
                        let count = message
                            .pointer("/params/items")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; count])
                    }
                    _ => Value::Null,
                };
                if let Some(server) = server.upgrade() {
                    let _ = server.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                }
            }
            (None, Some("textDocument/publishDiagnostics")) => {
                let uri = message
                    .pointer("/params/uri")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let list = message
                    .pointer("/params/diagnostics")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                diagnostics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(uri.to_string(), (Instant::now(), list));
            }
            _ => {}
        }
    }
}

impl Server {
    fn send(&self, message: &Value) -> Result<(), String> {
        let body = message.to_string();
        let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len())
            .and_then(|()| stdin.flush())
            .map_err(|error| format!("the {} language server stopped: {error}", self.language))
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = {
            let mut next = self.next_id.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            *next
        };
        let (sender, receiver): (Sender<Value>, Receiver<Value>) = channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sender);
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let response = receiver.recv_timeout(timeout).map_err(|_| {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            format!(
                "the {} language server did not answer {method} within {}s (it may still be indexing; try again)",
                self.language,
                timeout.as_secs()
            )
        })?;
        if let Some(error) = response.get("error") {
            return Err(format!(
                "{method} failed: {}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
            ));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Opens `path` in the server, or sends its new text when it changed.
    /// Returns true when the server was told something new.
    fn sync(&self, path: &Path) -> Result<bool, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let uri = file_uri(path);
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        match open.get_mut(&uri) {
            Some((_, sent)) if *sent == text => Ok(false),
            Some((version, sent)) => {
                *version += 1;
                *sent = text.clone();
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": *version },
                        "contentChanges": [{ "text": text }]
                    }),
                )?;
                Ok(true)
            }
            None => {
                open.insert(uri.clone(), (1, text.clone()));
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id(path, &self.language),
                            "version": 1,
                            "text": text
                        }
                    }),
                )?;
                Ok(true)
            }
        }
    }
}

fn language_id(path: &Path, language: &str) -> String {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match extension {
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "m" => "objective-c",
        "mm" => "objective-cpp",
        "py" | "pyi" => "python",
        _ => language,
    }
    .to_string()
}

fn server_for_file(path: &Path) -> Result<Arc<Server>, String> {
    let spec = server_for(path)?;
    let root = project_root(path);
    let key = (spec.command.clone(), root.clone());
    let mut servers = servers().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(server) = servers.get(&key) {
        if server_alive(server) {
            return Ok(Arc::clone(server));
        }
        servers.remove(&key);
    }
    let server = start_server(&spec, &root)?;
    servers.insert(key, Arc::clone(&server));
    Ok(server)
}

fn server_alive(server: &Arc<Server>) -> bool {
    // A dead server's stdin write fails; checking the child needs &mut.
    server
        .send(&json!({ "jsonrpc": "2.0", "method": "$/claw/ping", "params": {} }))
        .is_ok()
}

/// Runs the `LSP` tool.
pub(crate) fn run(input: LspInput) -> Result<Value, String> {
    let operation = input.operation.trim().to_string();
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let path = input.file_path.as_deref().map(|file| {
        let path = runtime::expand_home(file);
        if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    });
    let path =
        path.ok_or("filePath is required (workspaceSymbol uses it to pick the language server)")?;
    let path = path
        .canonicalize()
        .map_err(|_| format!("no such file: {}", path.display()))?;
    let server = server_for_file(&path)?;
    let changed = server.sync(&path)?;
    let uri = file_uri(&path);
    // 1-based positions in, 0-based to the server.
    let position = || -> Result<Value, String> {
        let line = input.line.ok_or("line is required for this operation")?;
        let character = input.character.unwrap_or(1);
        Ok(json!({ "line": line.saturating_sub(1), "character": character.saturating_sub(1) }))
    };
    let text_position = || -> Result<Value, String> {
        Ok(json!({ "textDocument": { "uri": uri }, "position": position()? }))
    };
    let result = match operation.as_str() {
        "goToDefinition" | "definition" => locations(&server.request("textDocument/definition", text_position()?, REQUEST_TIMEOUT)?),
        "goToImplementation" | "implementation" => locations(&server.request("textDocument/implementation", text_position()?, REQUEST_TIMEOUT)?),
        "findReferences" | "references" => {
            let mut params = text_position()?;
            params["context"] = json!({ "includeDeclaration": true });
            locations(&server.request("textDocument/references", params, REQUEST_TIMEOUT)?)
        }
        "hover" => hover(&server.request("textDocument/hover", text_position()?, REQUEST_TIMEOUT)?),
        "documentSymbol" | "symbols" => symbols(
            &server.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": uri } }), REQUEST_TIMEOUT)?,
            Some(&path),
        ),
        "workspaceSymbol" => {
            let query = input.query.clone().unwrap_or_default();
            symbols(&server.request("workspace/symbol", json!({ "query": query }), REQUEST_TIMEOUT)?, None)
        }
        "prepareCallHierarchy" => call_items(&server.request("textDocument/prepareCallHierarchy", text_position()?, REQUEST_TIMEOUT)?),
        "incomingCalls" | "outgoingCalls" => {
            let items = server.request("textDocument/prepareCallHierarchy", text_position()?, REQUEST_TIMEOUT)?;
            let Some(item) = items.as_array().and_then(|items| items.first()).cloned() else {
                return Ok(json!({ "operation": operation, "result": "no callable symbol at that position" }));
            };
            let method = if operation == "incomingCalls" { "callHierarchy/incomingCalls" } else { "callHierarchy/outgoingCalls" };
            let calls = server.request(method, json!({ "item": item }), REQUEST_TIMEOUT)?;
            let key = if operation == "incomingCalls" { "from" } else { "to" };
            let items: Vec<Value> = calls
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|call| call.get(key).cloned())
                .collect();
            call_items(&Value::Array(items))
        }
        "diagnostics" => {
            let since = Instant::now();
            let deadline = since + DIAGNOSTICS_WAIT;
            loop {
                let found = server
                    .diagnostics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&uri)
                    .cloned();
                match found {
                    // Fresh after this sync, or nothing changed since the last report.
                    Some((at, list)) if !changed || at >= since => break diagnostics(&list),
                    _ if Instant::now() >= deadline => break json!({ "diagnostics": [], "note": "the server reported no diagnostics yet (it may still be indexing)" }),
                    _ => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        }
        other => {
            return Err(format!(
                "unknown LSP operation `{other}`; use goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol, goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls or diagnostics"
            ))
        }
    };
    Ok(json!({
        "operation": operation,
        "file": path.display().to_string(),
        "server": format!("{} ({})", server.language, server.root.display()),
        "result": result,
    }))
}

/// `path:line:col` (1-based, relative to the cwd when inside it) and the
/// source line there.
fn location_text(uri: &str, range: &Value) -> String {
    let path = uri_path(uri);
    let line = range
        .pointer("/start/line")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let character = range
        .pointer("/start/character")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let shown = std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            Path::new(&path)
                .strip_prefix(&cwd)
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| path.clone());
    let source = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| {
            text.lines()
                .nth(usize::try_from(line).unwrap_or(0))
                .map(|l| l.trim().to_string())
        })
        .unwrap_or_default();
    format!("{shown}:{}:{}  {source}", line + 1, character + 1)
}

fn locations(result: &Value) -> Value {
    let items: Vec<&Value> = match result {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        single => vec![single],
    };
    let rows: Vec<String> = items
        .into_iter()
        .filter_map(|item| {
            // Location or LocationLink.
            let uri = item
                .get("uri")
                .or_else(|| item.get("targetUri"))?
                .as_str()?;
            let range = item
                .get("range")
                .or_else(|| item.get("targetSelectionRange"))
                .or_else(|| item.get("targetRange"))?;
            Some(location_text(uri, range))
        })
        .collect();
    if rows.is_empty() {
        json!("no results")
    } else {
        json!(rows)
    }
}

fn hover(result: &Value) -> Value {
    let contents = result.get("contents").cloned().unwrap_or(Value::Null);
    let text = match &contents {
        Value::String(text) => text.clone(),
        Value::Object(object) => object
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part {
                Value::String(text) => text.clone(),
                other => other
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    };
    if text.trim().is_empty() {
        json!("no hover information at that position")
    } else {
        json!(text)
    }
}

fn symbol_kind(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type parameter",
        _ => "symbol",
    }
}

fn symbols(result: &Value, file: Option<&Path>) -> Value {
    fn walk(items: &[Value], depth: usize, file: Option<&Path>, out: &mut Vec<String>) {
        for item in items {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
            let kind = symbol_kind(item.get("kind").and_then(Value::as_u64).unwrap_or(0));
            let indent = "  ".repeat(depth);
            if let (Some(uri), Some(range)) = (
                item.pointer("/location/uri").and_then(Value::as_str),
                item.pointer("/location/range"),
            ) {
                out.push(format!(
                    "{indent}{kind} {name}  {}",
                    location_text(uri, range)
                ));
            } else {
                let line = item
                    .pointer("/selectionRange/start/line")
                    .or_else(|| item.pointer("/range/start/line"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let place = file.map_or(String::new(), |_| format!("  line {}", line + 1));
                out.push(format!("{indent}{kind} {name}{place}"));
            }
            if let Some(children) = item.get("children").and_then(Value::as_array) {
                walk(children, depth + 1, file, out);
            }
        }
        if out.len() > 500 {
            out.truncate(500);
        }
    }
    let mut out = Vec::new();
    walk(
        result.as_array().map_or(&[][..], Vec::as_slice),
        0,
        file,
        &mut out,
    );
    if out.is_empty() {
        json!("no symbols")
    } else {
        json!(out)
    }
}

fn call_items(result: &Value) -> Value {
    let rows: Vec<String> = result
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let name = item.get("name").and_then(Value::as_str)?;
            let uri = item.get("uri").and_then(Value::as_str)?;
            let range = item.get("selectionRange").or_else(|| item.get("range"))?;
            Some(format!(
                "{} {name}  {}",
                symbol_kind(item.get("kind").and_then(Value::as_u64).unwrap_or(0)),
                location_text(uri, range)
            ))
        })
        .collect();
    if rows.is_empty() {
        json!("no results")
    } else {
        json!(rows)
    }
}

fn diagnostics(list: &[Value]) -> Value {
    let rows: Vec<String> = list
        .iter()
        .map(|diagnostic| {
            let severity = match diagnostic.get("severity").and_then(Value::as_u64) {
                Some(1) => "error",
                Some(2) => "warning",
                Some(3) => "info",
                Some(4) => "hint",
                _ => "issue",
            };
            let line = diagnostic
                .pointer("/range/start/line")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            let character = diagnostic
                .pointer("/range/start/character")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            let message = diagnostic
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .replace('\n', " ");
            let source = diagnostic
                .get("source")
                .and_then(Value::as_str)
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            format!("{line}:{character} {severity}: {message}{source}")
        })
        .collect();
    json!({ "count": rows.len(), "diagnostics": rows })
}

#[cfg(test)]
mod tests {
    use super::{file_uri, project_root, run, uri_path, LspInput};
    use std::path::Path;

    #[test]
    fn uris_round_trip_with_odd_characters() {
        let path = Path::new("/tmp/a dir/ü.rs");
        assert_eq!(file_uri(path), "file:///tmp/a%20dir/%C3%BC.rs");
        assert_eq!(uri_path(&file_uri(path)), "/tmp/a dir/ü.rs");
    }

    #[test]
    fn project_root_finds_the_marker() {
        let dir = std::env::temp_dir().join(format!("claw-lsp-root-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("go.mod"), "module x\n").unwrap();
        assert_eq!(project_root(&dir.join("src/main.go")), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs only where clangd is installed.
    #[test]
    fn clangd_answers_symbols_definition_and_diagnostics() {
        if !super::on_path("clangd") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("claw-lsp-c-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.c");
        std::fs::write(
            &file,
            "int add(int a, int b) { return a + b; }\nint main(void) { return add(1, 2); }\n",
        )
        .unwrap();
        let input = |operation: &str, line: Option<u32>, character: Option<u32>| LspInput {
            operation: operation.to_string(),
            file_path: Some(file.display().to_string()),
            line,
            character,
            query: None,
        };
        let symbols = run(input("documentSymbol", None, None)).unwrap();
        assert!(
            symbols["result"].to_string().contains("function add"),
            "{symbols}"
        );
        let definition = run(input("goToDefinition", Some(2), Some(25))).unwrap();
        assert!(
            definition["result"].to_string().contains("main.c:1:5"),
            "{definition}"
        );
        // An edit is synced to the server before the next request.
        std::fs::write(
            &file,
            "int add(int a, int b) { return a + b; }\nint main(void) { return add(1, 2) + missing; }\n",
        )
        .unwrap();
        let diagnostics = run(input("diagnostics", None, None)).unwrap();
        assert!(
            diagnostics["result"].to_string().contains("missing"),
            "{diagnostics}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
