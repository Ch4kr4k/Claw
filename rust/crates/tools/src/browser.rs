//! The `Browser` tool: drives a Chrome window through the DevTools protocol
//! (CDP): open pages, list a page's links, buttons and fields, click and type
//! into them by number, read the page's text, run JavaScript, and take
//! screenshots (shown to the model).
//!
//! It works on elements, not pixels, so it is reliable and cheap next to
//! X11 clicking. Chrome refuses remote control of the everyday profile, so
//! claw runs its own Chrome with the profile `~/.claw/browser` (logins made
//! there are kept). A Chrome already listening on the port is reused.
//! `CLAW_BROWSER_PORT` (default 9222), `CLAW_BROWSER` (the program) and
//! `CLAW_BROWSER_PROFILE` (the profile folder) change the defaults.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

/// How long a page may take to load.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for one protocol reply.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Page text returned by `read`, unless `max_chars` says otherwise.
const DEFAULT_TEXT: usize = 8000;

/// The tab the last action used.
static CURRENT_TAB: Mutex<Option<String>> = Mutex::new(None);

#[derive(Debug, Deserialize)]
pub(crate) struct BrowserInput {
    action: String,
    url: Option<String>,
    /// A number from `elements`.
    #[serde(alias = "element")]
    id: Option<u64>,
    /// A CSS selector, instead of `id`.
    selector: Option<String>,
    text: Option<String>,
    /// `key`: Enter, Tab, Escape, ArrowDown, ctrl+a, …
    keys: Option<String>,
    /// `type`: press Enter afterwards.
    submit: Option<bool>,
    /// `type`: replace the field's content (default true).
    clear: Option<bool>,
    /// `eval`: JavaScript.
    script: Option<String>,
    /// `scroll`: up or down (default down).
    direction: Option<String>,
    /// `tabs` id for `switch_tab` / `close_tab`.
    tab: Option<String>,
    /// `screenshot`: the whole page, not just the window.
    full_page: Option<bool>,
    /// `read`: most characters returned.
    max_chars: Option<usize>,
    /// `start`: no window.
    headless: Option<bool>,
    /// Seconds for `wait` (default 15).
    timeout: Option<u64>,
}

pub(crate) fn run(input: BrowserInput) -> Result<Value, String> {
    let port = port();
    match input.action.as_str() {
        "start" => {
            ensure_browser(port, input.headless.unwrap_or(false))?;
            Ok(json!({ "running": true, "port": port, "tabs": tab_list(port)? }))
        }
        "tabs" => {
            ensure_browser(port, false)?;
            Ok(json!({ "tabs": tab_list(port)? }))
        }
        "open" => {
            let url = normalize_url(input.url.as_deref().ok_or("open needs url")?);
            ensure_browser(port, input.headless.unwrap_or(false))?;
            let tab = current_tab(port)?;
            let page = Page::connect(port, &tab)?;
            page.call("Page.navigate", json!({ "url": url }))?;
            page.wait_loaded()?;
            page.summary()
        }
        "new_tab" => {
            ensure_browser(port, input.headless.unwrap_or(false))?;
            let url = normalize_url(input.url.as_deref().unwrap_or("about:blank"));
            let created = http(port, "PUT", &format!("/json/new?{url}"))?;
            let tab = created["id"]
                .as_str()
                .ok_or("Chrome made no tab")?
                .to_string();
            set_current(&tab);
            let page = Page::connect(port, &tab)?;
            page.wait_loaded()?;
            page.summary()
        }
        "switch_tab" => {
            let tab = input
                .tab
                .as_deref()
                .ok_or("switch_tab needs tab (an id from `tabs`)")?;
            http(port, "GET", &format!("/json/activate/{tab}"))?;
            set_current(tab);
            Page::connect(port, tab)?.summary()
        }
        "close_tab" => {
            let tab = match input.tab.clone() {
                Some(tab) => tab,
                None => current_tab(port)?,
            };
            http(port, "GET", &format!("/json/close/{tab}"))?;
            *CURRENT_TAB.lock().unwrap_or_else(PoisonError::into_inner) = None;
            Ok(json!({ "closed": tab, "tabs": tab_list(port)? }))
        }
        other => {
            ensure_browser(port, false)?;
            let page = Page::connect(port, &current_tab(port)?)?;
            page_action(&page, other, &input)
        }
    }
}

fn page_action(page: &Page, action: &str, input: &BrowserInput) -> Result<Value, String> {
    match action {
        "read" => {
            let max = input.max_chars.unwrap_or(DEFAULT_TEXT).clamp(200, 100_000);
            let target = match input.selector.as_deref() {
                Some(selector) => format!("document.querySelector({})", js_string(selector)),
                None => "document.body".to_string(),
            };
            let text = page.eval(&format!(
                "(() => {{ const el = {target}; return el ? el.innerText : null; }})()"
            ))?;
            let text = text.as_str().ok_or("no such element")?;
            let total = text.chars().count();
            let mut summary = page.summary()?;
            summary["text"] = json!(text.chars().take(max).collect::<String>());
            if total > max {
                summary["truncated"] = json!(format!("{max} of {total} characters; use max_chars or selector for more"));
            }
            Ok(summary)
        }
        "elements" => {
            let elements = page.eval(ELEMENTS_JS)?;
            let mut summary = page.summary()?;
            summary["elements"] = elements;
            summary["note"] = json!("click/type with `id`; numbers change when the page changes, so list again after it does");
            Ok(summary)
        }
        "click" => {
            let (x, y) = page.element_center(input)?;
            for (kind, count) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 1)] {
                page.call(
                    "Input.dispatchMouseEvent",
                    json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": count }),
                )?;
            }
            sleep(Duration::from_millis(300));
            page.wait_loaded()?;
            page.summary()
        }
        "type" => {
            let text = input.text.as_deref().ok_or("type needs text")?;
            if input.id.is_some() || input.selector.is_some() {
                let target = element_js(input)?;
                let clear = input.clear.unwrap_or(true);
                page.eval(&format!(
                    "(() => {{ const el = {target}; if (!el) throw new Error('no such element'); \
                     el.scrollIntoView({{block: 'center'}}); el.focus(); \
                     if ({clear}) {{ if ('value' in el) {{ el.value = ''; el.dispatchEvent(new Event('input', {{bubbles: true}})); }} \
                     else if (el.isContentEditable) {{ el.textContent = ''; }} }} return true; }})()"
                ))?;
            }
            page.call("Input.insertText", json!({ "text": text }))?;
            if input.submit.unwrap_or(false) {
                page.press("Enter")?;
                sleep(Duration::from_millis(300));
                page.wait_loaded()?;
            }
            Ok(json!({ "typed": text.chars().count(), "submitted": input.submit.unwrap_or(false) }))
        }
        "key" => {
            let keys = input.keys.as_deref().or(input.text.as_deref()).ok_or("key needs keys")?;
            for key in keys.split_whitespace() {
                page.press(key)?;
            }
            sleep(Duration::from_millis(200));
            Ok(json!({ "pressed": keys }))
        }
        "scroll" => {
            let sign = if input.direction.as_deref() == Some("up") { "-" } else { "" };
            let position = page.eval(&format!(
                "(() => {{ window.scrollBy(0, {sign}window.innerHeight * 0.8); \
                 return {{ y: Math.round(window.scrollY), height: document.documentElement.scrollHeight }}; }})()"
            ))?;
            Ok(json!({ "scroll": position }))
        }
        "back" | "forward" | "reload" => {
            let script = match action {
                "back" => "history.back()",
                "forward" => "history.forward()",
                _ => "location.reload()",
            };
            page.eval(script)?;
            sleep(Duration::from_millis(500));
            page.wait_loaded()?;
            page.summary()
        }
        "wait" => {
            let seconds = input.timeout.unwrap_or(15).clamp(1, 120);
            let condition = match (&input.selector, &input.text) {
                (Some(selector), _) => format!("!!document.querySelector({})", js_string(selector)),
                (None, Some(text)) => format!("document.body && document.body.innerText.includes({})", js_string(text)),
                _ => return Err("wait needs selector or text".to_string()),
            };
            let deadline = Instant::now() + Duration::from_secs(seconds);
            while Instant::now() < deadline {
                if page.eval(&condition)? == json!(true) {
                    return page.summary();
                }
                sleep(Duration::from_millis(300));
            }
            Err(format!("not found after {seconds}s"))
        }
        "eval" => {
            let script = input.script.as_deref().ok_or("eval needs script")?;
            Ok(json!({ "value": page.eval(script)? }))
        }
        "screenshot" => {
            let mut params = json!({ "format": "png" });
            if input.full_page.unwrap_or(false) {
                let metrics = page.call("Page.getLayoutMetrics", json!({}))?;
                let size = &metrics["cssContentSize"];
                params["captureBeyondViewport"] = json!(true);
                params["clip"] = json!({
                    "x": 0, "y": 0, "scale": 1,
                    "width": size["width"].as_f64().unwrap_or(1280.0),
                    "height": size["height"].as_f64().unwrap_or(800.0).min(12000.0),
                });
            }
            let shot = page.call("Page.captureScreenshot", params)?;
            let png = base64_decode(shot["data"].as_str().ok_or("no screenshot data")?)?;
            let dir = std::env::temp_dir().join("claw-browser");
            std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|time| time.as_millis())
                .unwrap_or_default();
            let path: PathBuf = dir.join(format!("page-{stamp}.png"));
            std::fs::write(&path, png).map_err(|error| error.to_string())?;
            let mut summary = page.summary()?;
            summary["path"] = json!(path.display().to_string());
            Ok(summary)
        }
        other => Err(format!(
            "unknown action `{other}`; use start, tabs, open, new_tab, switch_tab, close_tab, read, elements, click, type, key, scroll, back, forward, reload, wait, eval or screenshot"
        )),
    }
}

/// Visible links, buttons and fields, numbered with `data-claw-id`.
const ELEMENTS_JS: &str = r#"(() => {
  const selector = 'a[href], button, input:not([type=hidden]), textarea, select, summary, [role=button], [role=link], [role=tab], [role=menuitem], [role=checkbox], [role=option], [contenteditable=""], [contenteditable=true], [onclick]';
  const out = [];
  let n = 0;
  for (const el of document.querySelectorAll(selector)) {
    const r = el.getBoundingClientRect();
    const style = getComputedStyle(el);
    if (r.width < 2 || r.height < 2 || style.visibility === 'hidden' || style.display === 'none') continue;
    n += 1;
    el.setAttribute('data-claw-id', String(n));
    if (out.length >= 200) continue;
    const label = (el.innerText || el.value || el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('alt') || el.name || '').trim().replace(/\s+/g, ' ');
    const item = { id: n, tag: el.tagName.toLowerCase(), text: label.slice(0, 80) };
    if (el.type && el.tagName !== 'BUTTON') item.type = el.type;
    if (el.getAttribute('role')) item.role = el.getAttribute('role');
    if (el.href) item.href = String(el.href).slice(0, 120);
    if (r.bottom < 0 || r.top > innerHeight) item.offscreen = true;
    out.push(item);
  }
  return { count: n, shown: out.length, elements: out };
})()"#;

fn element_js(input: &BrowserInput) -> Result<String, String> {
    match (input.id, input.selector.as_deref()) {
        (Some(id), _) => Ok(format!("document.querySelector('[data-claw-id=\"{id}\"]')")),
        (None, Some(selector)) => Ok(format!("document.querySelector({})", js_string(selector))),
        _ => Err("needs `id` (from elements) or `selector`".to_string()),
    }
}

fn js_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

fn normalize_url(url: &str) -> String {
    let url = url.trim();
    if url.contains("://") || url.starts_with("about:") || url.starts_with("data:") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("file://{url}")
    } else {
        format!("https://{url}")
    }
}

// ------------------------------------------------------------- the browser

fn port() -> u16 {
    std::env::var("CLAW_BROWSER_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(9222)
}

/// Starts claw's Chrome unless one answers on `port`.
fn ensure_browser(port: u16, headless: bool) -> Result<(), String> {
    if http(port, "GET", "/json/version").is_ok() {
        return Ok(());
    }
    let program = std::env::var("CLAW_BROWSER").ok().or_else(|| {
        [
            "google-chrome-stable",
            "google-chrome",
            "chromium",
            "chromium-browser",
            "chrome",
        ]
        .into_iter()
        .find(|name| {
            std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
            })
        })
        .map(str::to_string)
    });
    let program = program.ok_or("no Chrome or Chromium found (set CLAW_BROWSER)")?;
    let profile = match std::env::var_os("CLAW_BROWSER_PROFILE") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set")?
            .join(".claw")
            .join("browser"),
    };
    let mut args = vec![
        format!("--remote-debugging-port={port}"),
        "--remote-allow-origins=*".to_string(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--force-renderer-accessibility".to_string(),
    ];
    if headless {
        args.push("--headless=new".to_string());
    }
    args.push("about:blank".to_string());
    Command::new("setsid")
        .arg("-f")
        .arg(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("cannot start {program}: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if http(port, "GET", "/json/version").is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(250));
    }
    Err(format!("{program} did not open its debugging port {port}"))
}

/// A request to Chrome's HTTP endpoint on localhost.
fn http(port: u16, method: &str, path: &str) -> Result<Value, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| error.to_string())?;
    // Chrome keeps the connection open, so read by Content-Length.
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader
        .read_line(&mut status)
        .map_err(|error| error.to_string())?;
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    let body = String::from_utf8_lossy(&body);
    if !status.starts_with("HTTP/1.1 200") {
        return Err(format!("Chrome: {}", status.trim()));
    }
    Ok(serde_json::from_str(&body).unwrap_or_else(|_| json!(body.trim())))
}

fn tab_list(port: u16) -> Result<Vec<Value>, String> {
    let current = CURRENT_TAB
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    Ok(http(port, "GET", "/json/list")?
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|target| target["type"] == "page")
        .map(|target| {
            json!({
                "tab": target["id"],
                "title": target["title"],
                "url": target["url"],
                "current": current.as_deref() == target["id"].as_str(),
            })
        })
        .collect())
}

fn set_current(tab: &str) {
    *CURRENT_TAB.lock().unwrap_or_else(PoisonError::into_inner) = Some(tab.to_string());
}

/// The tab to act on: the last one used while it exists, else the first.
fn current_tab(port: u16) -> Result<String, String> {
    let tabs = tab_list(port)?;
    let known = CURRENT_TAB
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if let Some(tab) = known.filter(|tab| tabs.iter().any(|t| t["tab"] == json!(tab))) {
        return Ok(tab);
    }
    let tab = match tabs.first().and_then(|t| t["tab"].as_str()) {
        Some(tab) => tab.to_string(),
        None => {
            let created = http(port, "PUT", "/json/new?about:blank")?;
            created["id"]
                .as_str()
                .ok_or("Chrome made no tab")?
                .to_string()
        }
    };
    set_current(&tab);
    Ok(tab)
}

// ------------------------------------------------------------ one page

struct Page {
    socket: Mutex<WebSocket>,
    next_id: Mutex<u64>,
}

impl Page {
    fn connect(port: u16, tab: &str) -> Result<Self, String> {
        let socket = WebSocket::connect(port, &format!("/devtools/page/{tab}"))?;
        Ok(Self {
            socket: Mutex::new(socket),
            next_id: Mutex::new(0),
        })
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = {
            let mut next = self.next_id.lock().unwrap_or_else(PoisonError::into_inner);
            *next += 1;
            *next
        };
        let mut socket = self.socket.lock().unwrap_or_else(PoisonError::into_inner);
        socket.send(&json!({ "id": id, "method": method, "params": params }).to_string())?;
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                return Err(format!("{method}: no reply from Chrome"));
            }
            let message: Value = serde_json::from_str(&socket.receive()?)
                .map_err(|error| format!("bad message from Chrome: {error}"))?;
            if message["id"] != json!(id) {
                continue; // an event
            }
            if let Some(error) = message.get("error") {
                return Err(format!(
                    "{method}: {}",
                    error["message"].as_str().unwrap_or("failed")
                ));
            }
            return Ok(message["result"].clone());
        }
    }

    fn eval(&self, expression: &str) -> Result<Value, String> {
        let result = self.call(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
        )?;
        if let Some(exception) = result.get("exceptionDetails") {
            let text = exception["exception"]["description"]
                .as_str()
                .or_else(|| exception["text"].as_str())
                .unwrap_or("script error");
            return Err(text.lines().next().unwrap_or(text).to_string());
        }
        Ok(result["result"]["value"].clone())
    }

    fn wait_loaded(&self) -> Result<(), String> {
        let deadline = Instant::now() + LOAD_TIMEOUT;
        while Instant::now() < deadline {
            if self.eval("document.readyState").ok() == Some(json!("complete")) {
                return Ok(());
            }
            sleep(Duration::from_millis(200));
        }
        Ok(()) // still loading; what is there can be used
    }

    fn summary(&self) -> Result<Value, String> {
        let info = self.eval("({ title: document.title, url: location.href })")?;
        Ok(json!({ "title": info["title"], "url": info["url"] }))
    }

    /// The viewport point at the middle of the element, scrolled into view.
    fn element_center(&self, input: &BrowserInput) -> Result<(f64, f64), String> {
        let target = element_js(input)?;
        let rect = self.eval(&format!(
            "(() => {{ const el = {target}; if (!el) throw new Error('no such element; list elements again'); \
             el.scrollIntoView({{block: 'center', inline: 'center'}}); const r = el.getBoundingClientRect(); \
             return {{ x: r.left + r.width / 2, y: r.top + r.height / 2 }}; }})()"
        ))?;
        Ok((
            rect["x"].as_f64().ok_or("element has no position")?,
            rect["y"].as_f64().ok_or("element has no position")?,
        ))
    }

    /// Presses a key such as `Enter` or `ctrl+a`.
    fn press(&self, spec: &str) -> Result<(), String> {
        let mut modifiers = 0;
        let mut key = spec;
        for part in spec.split('+') {
            match part.to_ascii_lowercase().as_str() {
                "alt" => modifiers |= 1,
                "ctrl" | "control" => modifiers |= 2,
                "meta" | "super" | "cmd" => modifiers |= 4,
                "shift" => modifiers |= 8,
                _ => key = part,
            }
        }
        let (name, code, text) = key_info(key);
        let mut down = json!({
            "type": "keyDown", "key": name, "code": code, "modifiers": modifiers,
            "windowsVirtualKeyCode": virtual_key(&name),
        });
        if modifiers & 6 == 0 {
            if let Some(text) = text {
                down["text"] = json!(text);
            }
        }
        self.call("Input.dispatchKeyEvent", down)?;
        self.call(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": name, "code": code, "modifiers": modifiers,
                    "windowsVirtualKeyCode": virtual_key(&name) }),
        )?;
        Ok(())
    }
}

/// DOM key name, code and typed text for a key spec.
fn key_info(key: &str) -> (String, String, Option<String>) {
    let named = |name: &str, text: Option<&str>| {
        (name.to_string(), name.to_string(), text.map(str::to_string))
    };
    match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => named("Enter", Some("\r")),
        "tab" => named("Tab", None),
        "escape" | "esc" => named("Escape", None),
        "backspace" => named("Backspace", None),
        "delete" | "del" => named("Delete", None),
        "space" => (" ".to_string(), "Space".to_string(), Some(" ".to_string())),
        "up" | "arrowup" => named("ArrowUp", None),
        "down" | "arrowdown" => named("ArrowDown", None),
        "left" | "arrowleft" => named("ArrowLeft", None),
        "right" | "arrowright" => named("ArrowRight", None),
        "home" => named("Home", None),
        "end" => named("End", None),
        "pageup" => named("PageUp", None),
        "pagedown" => named("PageDown", None),
        _ => {
            let ch = key.chars().next().unwrap_or(' ');
            let code = if ch.is_ascii_alphabetic() {
                format!("Key{}", ch.to_ascii_uppercase())
            } else if ch.is_ascii_digit() {
                format!("Digit{ch}")
            } else {
                String::new()
            };
            (key.to_string(), code, Some(key.to_string()))
        }
    }
}

fn virtual_key(name: &str) -> u32 {
    match name {
        "Enter" => 13,
        "Tab" => 9,
        "Escape" => 27,
        "Backspace" => 8,
        "Delete" => 46,
        " " => 32,
        "ArrowLeft" => 37,
        "ArrowUp" => 38,
        "ArrowRight" => 39,
        "ArrowDown" => 40,
        "Home" => 36,
        "End" => 35,
        "PageUp" => 33,
        "PageDown" => 34,
        other => other
            .chars()
            .next()
            .map_or(0, |ch| u32::from(ch.to_ascii_uppercase())),
    }
}

// ------------------------------------------------------- a small WebSocket

/// Enough of RFC 6455 for Chrome on localhost: text frames, fragments,
/// pings.
struct WebSocket {
    stream: BufReader<TcpStream>,
}

impl WebSocket {
    fn connect(port: u16, path: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(CALL_TIMEOUT)).ok();
        let key = base64_encode(&random_bytes::<16>());
        let mut writer = stream.try_clone().map_err(|error| error.to_string())?;
        write!(
            writer,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .map_err(|error| error.to_string())?;
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader
            .read_line(&mut status)
            .map_err(|error| error.to_string())?;
        if !status.contains(" 101 ") {
            return Err(format!("Chrome refused the connection: {}", status.trim()));
        }
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .map_err(|error| error.to_string())?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        Ok(Self { stream: reader })
    }

    fn send(&mut self, text: &str) -> Result<(), String> {
        self.send_frame(0x1, text.as_bytes())
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut frame = vec![0x80 | opcode];
        let len = payload.len();
        if len < 126 {
            #[allow(clippy::cast_possible_truncation)]
            frame.push(0x80 | len as u8);
        } else if len <= usize::from(u16::MAX) {
            frame.push(0x80 | 126);
            #[allow(clippy::cast_possible_truncation)]
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mask = random_bytes::<4>();
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );
        self.stream
            .get_mut()
            .write_all(&frame)
            .map_err(|error| error.to_string())
    }

    /// The next text message.
    fn receive(&mut self) -> Result<String, String> {
        let mut message = Vec::new();
        loop {
            let mut head = [0u8; 2];
            self.read_exact(&mut head)?;
            let fin = head[0] & 0x80 != 0;
            let opcode = head[0] & 0x0f;
            let mut len = u64::from(head[1] & 0x7f);
            if len == 126 {
                let mut ext = [0u8; 2];
                self.read_exact(&mut ext)?;
                len = u64::from(u16::from_be_bytes(ext));
            } else if len == 127 {
                let mut ext = [0u8; 8];
                self.read_exact(&mut ext)?;
                len = u64::from_be_bytes(ext);
            }
            let mask = if head[1] & 0x80 != 0 {
                let mut mask = [0u8; 4];
                self.read_exact(&mut mask)?;
                Some(mask)
            } else {
                None
            };
            let mut payload = vec![0u8; usize::try_from(len).map_err(|_| "frame too large")?];
            self.read_exact(&mut payload)?;
            if let Some(mask) = mask {
                for (i, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[i % 4];
                }
            }
            match opcode {
                0x8 => return Err("Chrome closed the connection".to_string()),
                0x9 => self.send_frame(0xA, &payload)?,
                0xA => {}
                _ => {
                    message.extend_from_slice(&payload);
                    if fin {
                        return String::from_utf8(message).map_err(|error| error.to_string());
                    }
                }
            }
        }
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), String> {
        self.stream
            .read_exact(buffer)
            .map_err(|error| format!("lost the connection to Chrome: {error}"))
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use std::hash::{BuildHasher, Hasher};
    let mut out = [0u8; N];
    for chunk in out.chunks_mut(8) {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|time| time.as_nanos())
                .unwrap_or_default(),
        );
        let bytes = hasher.finish().to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    out
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return Err("bad base64 from Chrome".to_string()),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            #[allow(clippy::cast_possible_truncation)]
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

/// The tool's output: the JSON result, plus the line that attaches a
/// screenshot.
pub(crate) fn render(result: Value) -> Result<String, String> {
    let image = result
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| path.ends_with(".png"))
        .map(str::to_string);
    let mut text = serde_json::to_string_pretty(&result).map_err(|error| error.to_string())?;
    if let Some(path) = image {
        text.push('\n');
        text.push_str(runtime::tool_images::TOOL_IMAGE_MARKER);
        text.push_str(&path);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::{base64_decode, base64_encode, key_info, normalize_url, run, BrowserInput};
    use serde_json::json;

    #[test]
    fn helpers() {
        assert_eq!(
            base64_decode(&base64_encode(b"hello world!")).unwrap(),
            b"hello world!"
        );
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("/tmp/a.html"), "file:///tmp/a.html");
        assert_eq!(key_info("a").1, "KeyA");
        assert_eq!(key_info("Enter").2.as_deref(), Some("\r"));
    }

    /// Drives a headless Chrome: run with `--ignored` where Chrome is
    /// installed. Uses its own port and profile.
    #[test]
    #[ignore = "starts Chrome"]
    fn drives_a_page() {
        std::env::set_var("CLAW_BROWSER_PORT", "9333");
        let dir = std::env::temp_dir().join("claw-browser-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CLAW_BROWSER_PROFILE", dir.join("profile"));
        let page = dir.join("page.html");
        std::fs::write(
            &page,
            r#"<title>T</title><input id=q placeholder="Search"><button onclick="document.getElementById('out').textContent='got '+document.getElementById('q').value">Go</button><p id=out></p>"#,
        )
        .unwrap();
        let act =
            |value: serde_json::Value| run(serde_json::from_value::<BrowserInput>(value).unwrap());
        act(json!({"action": "start", "headless": true})).unwrap();
        let opened = act(json!({"action": "open", "url": page.display().to_string()})).unwrap();
        assert_eq!(opened["title"], "T");
        let listed = act(json!({"action": "elements"})).unwrap();
        let elements = listed["elements"]["elements"].as_array().unwrap();
        let input = elements.iter().find(|e| e["tag"] == "input").unwrap()["id"]
            .as_u64()
            .unwrap();
        let button = elements.iter().find(|e| e["text"] == "Go").unwrap()["id"]
            .as_u64()
            .unwrap();
        act(json!({"action": "type", "id": input, "text": "claw's test"})).unwrap();
        act(json!({"action": "click", "id": button})).unwrap();
        let read = act(json!({"action": "read", "selector": "#out"})).unwrap();
        assert_eq!(read["text"], "got claw's test");
        let shot = act(json!({"action": "screenshot"})).unwrap();
        assert!(std::fs::read(shot["path"].as_str().unwrap())
            .unwrap()
            .starts_with(b"\x89PNG"));
        let value = act(json!({"action": "eval", "script": "1 + 2"})).unwrap();
        assert_eq!(value["value"], 3);
    }
}
