//! The `X11` tool: drives the user's X11 desktop like a person at the
//! keyboard: lists and manages windows, launches programs, types, presses
//! keys, clicks, drags and scrolls, reads and sets the clipboard, and takes
//! screenshots, which are attached to the result so a vision model sees the
//! screen. It runs the standard tools: `xdotool` (input, windows), `wmctrl`
//! (window list, close, move), `ffmpeg` (screenshots) and `xclip`
//! (clipboard).
//!
//! Coordinates are screen pixels; with `window` they are relative to the
//! window's top-left corner, the same as in a screenshot of that window
//! (which is never scaled when it is at most [`MAX_SHOT_WIDTH`] wide).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

/// Longest one helper command (xdotool, wmctrl, ffmpeg, xclip) may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// OCR and accessibility walks of big windows take a while.
const OCR_TIMEOUT: Duration = Duration::from_secs(60);
const UI_TIMEOUT: Duration = Duration::from_secs(45);
/// Wider screenshots are scaled down to this, to keep vision input small.
const MAX_SHOT_WIDTH: u32 = 1600;

#[derive(Debug, Deserialize)]
pub(crate) struct X11Input {
    action: String,
    /// Window id (decimal or 0x hex) or part of its title.
    window: Option<String>,
    text: Option<String>,
    /// For `key`: xdotool key names, e.g. "ctrl+s", "Return", "alt+Tab".
    keys: Option<String>,
    x: Option<i64>,
    y: Option<i64>,
    to_x: Option<i64>,
    to_y: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    /// 1 left (default), 2 middle, 3 right.
    button: Option<u8>,
    /// Clicks for `click` (2 = double-click), steps for `scroll`.
    count: Option<u32>,
    /// `scroll`: "up", "down", "left" or "right".
    direction: Option<String>,
    /// `launch`: the shell command.
    command: Option<String>,
    /// `launch` / `wait_window`: title to wait for.
    wait_for: Option<String>,
    /// Seconds to wait for a window (default 15).
    timeout: Option<u64>,
    /// `ui_*`: an element id from `ui_find` / `ui_tree` (e.g. "3/0/2/5").
    element: Option<String>,
    /// `ui_*`: part of an application's name (e.g. "firefox").
    app: Option<String>,
    /// `ui_find`: part of the element's name or text.
    name: Option<String>,
    /// `ui_find`: its role, e.g. "push button", "text", "menu item".
    role: Option<String>,
    /// `ui_tree`: levels to show (default 4).
    depth: Option<u32>,
    /// `ocr`: an image file instead of the screen.
    image: Option<String>,
}

pub(crate) fn run(input: X11Input) -> Result<Value, String> {
    if std::env::var_os("DISPLAY").is_none() {
        return Err("no X11 display: DISPLAY is not set".to_string());
    }
    match input.action.as_str() {
        "windows" => windows(),
        "screen" => screen(),
        "screenshot" => screenshot(&input),
        "focus" => {
            let id = window_id(&input)?;
            activate(&id)?;
            Ok(json!({ "focused": id }))
        }
        "type" => {
            let text = input.text.as_deref().ok_or("type needs text")?;
            focus_if_given(&input)?;
            xdotool(&["type", "--clearmodifiers", "--delay", "12", "--", text])?;
            Ok(json!({ "typed": text.chars().count() }))
        }
        "key" => {
            let keys = input.keys.as_deref().or(input.text.as_deref()).ok_or("key needs keys")?;
            focus_if_given(&input)?;
            let mut args = vec!["key", "--clearmodifiers", "--delay", "40"];
            args.extend(keys.split_whitespace());
            xdotool(&args)?;
            Ok(json!({ "pressed": keys }))
        }
        "click" => click(&input),
        "move_mouse" => {
            let (x, y) = point(&input, input.x, input.y)?;
            xdotool(&["mousemove", &x.to_string(), &y.to_string()])?;
            Ok(json!({ "x": x, "y": y }))
        }
        "drag" => drag(&input),
        "scroll" => scroll(&input),
        "close" => {
            let id = window_id(&input)?;
            run_tool("wmctrl", &["-i", "-c", &id])?;
            Ok(json!({ "closed": id }))
        }
        "move_resize" => {
            let id = window_id(&input)?;
            let geometry = format!(
                "0,{},{},{},{}",
                input.x.unwrap_or(-1),
                input.y.unwrap_or(-1),
                input.width.unwrap_or(-1),
                input.height.unwrap_or(-1)
            );
            run_tool("wmctrl", &["-i", "-r", &id, "-e", &geometry])?;
            Ok(json!({ "window": id, "geometry": geometry, "note": "tiling window managers may ignore this" }))
        }
        "launch" => launch(&input),
        "wait_window" => {
            let title = input.wait_for.as_deref().or(input.window.as_deref()).ok_or("wait_window needs wait_for")?;
            let id = wait_window(title, input.timeout.unwrap_or(15))?;
            Ok(json!({ "window": id, "title": window_title(&id) }))
        }
        "ocr" => ocr(&input),
        "ui_apps" | "ui_find" | "ui_tree" | "ui_read" | "ui_set_text" => ui(&input),
        "ui_click" => ui_click(&input),
        "clipboard_get" => Ok(json!({ "text": run_tool("xclip", &["-selection", "clipboard", "-o"])? })),
        "clipboard_set" => {
            let text = input.text.as_deref().ok_or("clipboard_set needs text")?;
            set_clipboard(text)?;
            Ok(json!({ "copied": text.chars().count() }))
        }
        other => Err(format!(
            "unknown action `{other}`; use windows, screen, screenshot, focus, type, key, click, move_mouse, drag, scroll, close, move_resize, launch, wait_window, ocr, ui_apps, ui_find, ui_tree, ui_click, ui_read, ui_set_text, clipboard_get or clipboard_set"
        )),
    }
}

/// Runs `program`; its stdout, or its stderr as the error.
fn run_tool(program: &str, args: &[&str]) -> Result<String, String> {
    run_tool_with_timeout(program, args, COMMAND_TIMEOUT)
}

fn run_tool_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    use std::io::Read;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!("`{program}` is not installed")
            } else {
                format!("{program}: {error}")
            }
        })?;
    // Read both pipes on threads, so a chatty program cannot block.
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    // A window manager that never confirms a request would hang xdotool.
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{program} {} timed out after {}s",
                    args.first().unwrap_or(&""),
                    timeout.as_secs()
                ));
            }
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if status.success() {
        Ok(String::from_utf8_lossy(&stdout).trim_end().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
        Err(format!(
            "{program} {} failed: {}",
            args.first().unwrap_or(&""),
            if stderr.is_empty() {
                status.to_string()
            } else {
                stderr
            }
        ))
    }
}

fn xdotool(args: &[&str]) -> Result<String, String> {
    run_tool("xdotool", args)
}

/// Windows from `wmctrl -lGpx`: id, desktop, pid, geometry, class, title.
fn windows() -> Result<Value, String> {
    let monitors = monitors();
    let list = run_tool("wmctrl", &["-l", "-G", "-p", "-x"])?;
    let active = xdotool(&["getactivewindow"])
        .ok()
        .and_then(|id| id.trim().parse::<u64>().ok());
    let windows: Vec<Value> = list
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 8 {
                return None;
            }
            let id = u64::from_str_radix(fields[0].trim_start_matches("0x"), 16).ok()?;
            let title = fields[9..].join(" ");
            let geometry = window_geometry(&id.to_string()).unwrap_or((
                fields[3].parse().unwrap_or(0),
                fields[4].parse().unwrap_or(0),
                fields[5].parse().unwrap_or(0),
                fields[6].parse().unwrap_or(0),
            ));
            Some(json!({
                "id": id,
                "title": title,
                "class": fields[7],
                "desktop": fields[1].parse::<i64>().unwrap_or(-1),
                "pid": fields[2].parse::<u64>().unwrap_or(0),
                // wmctrl's positions are off under some window managers
                // (dwm); xdotool's are right.
                "x": geometry.0,
                "y": geometry.1,
                "width": geometry.2,
                "height": geometry.3,
                "visible": visible(geometry, &monitors),
                "active": Some(id) == active,
            }))
        })
        .collect();
    Ok(json!({ "windows": windows }))
}

fn screen() -> Result<Value, String> {
    let size = xdotool(&["getdisplaygeometry"])?;
    let mut parts = size
        .split_whitespace()
        .map(|n| n.parse::<i64>().unwrap_or(0));
    let mouse = xdotool(&["getmouselocation", "--shell"])?;
    let field = |key: &str| {
        mouse
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .and_then(|value| value.parse::<i64>().ok())
    };
    let monitors: Vec<Value> = monitors()
        .iter()
        .map(|(x, y, w, h)| json!({ "x": x, "y": y, "width": w, "height": h }))
        .collect();
    Ok(json!({
        "width": parts.next(),
        "height": parts.next(),
        "monitors": monitors,
        "mouse": { "x": field("X"), "y": field("Y"), "window": field("WINDOW") },
        "activeWindow": xdotool(&["getactivewindow"]).ok().and_then(|id| id.parse::<u64>().ok()),
    }))
}

/// The window `input.window` names: an id, or the first visible window whose
/// title contains it.
fn window_id(input: &X11Input) -> Result<String, String> {
    let spec = input
        .window
        .as_deref()
        .map(str::trim)
        .ok_or("this action needs `window` (an id or part of the title)")?;
    resolve_window(spec)
}

fn resolve_window(spec: &str) -> Result<String, String> {
    if let Some(hex) = spec.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16)
            .map(|id| id.to_string())
            .map_err(|_| format!("bad window id {spec}"));
    }
    if spec.chars().all(|ch| ch.is_ascii_digit()) && !spec.is_empty() {
        return Ok(spec.to_string());
    }
    let found =
        xdotool(&["search", "--onlyvisible", "--name", &regex_escape(spec)]).unwrap_or_default();
    found
        .lines()
        .next()
        .map(str::to_string)
        .ok_or_else(|| format!("no visible window titled like \"{spec}\" (see action `windows`)"))
}

/// xdotool matches titles as regular expressions; the caller gives text.
fn regex_escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if "\\.+*?()|[]{}^$".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn window_title(id: &str) -> String {
    xdotool(&["getwindowname", id]).unwrap_or_default()
}

/// Brings window `id` to the front and gives it the keyboard, and checks
/// that it really has it: typing must never land in another window.
fn activate(id: &str) -> Result<(), String> {
    if !on_screen(id) {
        return Err(format!(
            "window {id} ({}) is not on screen: it is on another tag/workspace or minimised. \
             Nothing was done. Switch to its workspace first (for dwm: `key` super+<tag number>), \
             or ask the user.",
            window_title(id)
        ));
    }
    // Without --sync: some window managers (dwm) never confirm, and
    // xdotool would wait forever.
    let _ = xdotool(&["windowactivate", id]);
    sleep(Duration::from_millis(250));
    if has_focus(id) {
        return Ok(());
    }
    let _ = xdotool(&["windowraise", id]);
    let _ = xdotool(&["windowfocus", id]);
    sleep(Duration::from_millis(150));
    if has_focus(id) {
        return Ok(());
    }
    Err(format!(
        "could not give window {id} ({}) the keyboard focus; the window manager refused. \
         Nothing was typed or clicked. Click on the window first, or ask the user to focus it.",
        window_title(id)
    ))
}

/// For clicks and screenshots, which need the window on screen but not the
/// keyboard (a click focuses it anyway): raises it, or says why it can't.
fn raise_visible(id: &str) -> Result<(), String> {
    if !on_screen(id) {
        return Err(format!(
            "window {id} ({}) is not on screen: it is on another tag/workspace or minimised. \
             Nothing was done. Switch to its workspace first (for dwm: `key` super+<tag number>), \
             or ask the user.",
            window_title(id)
        ));
    }
    let _ = xdotool(&["windowraise", id]);
    sleep(Duration::from_millis(100));
    Ok(())
}

fn has_focus(id: &str) -> bool {
    [&["getwindowfocus"][..], &["getactivewindow"][..]]
        .iter()
        .any(|args| xdotool(args).is_ok_and(|focused| focused.trim() == id))
}

/// Whether some of the window is on the visible screen (window managers
/// move windows of hidden workspaces off it).
fn on_screen(id: &str) -> bool {
    window_geometry(id).is_ok_and(|geometry| visible(geometry, &monitors()))
}

/// Whether a window rectangle overlaps one of the monitors.
fn visible((x, y, w, h): (i64, i64, i64, i64), monitors: &[(i64, i64, i64, i64)]) -> bool {
    w > 0
        && h > 0
        && monitors
            .iter()
            .any(|&(mx, my, mw, mh)| x < mx + mw && x + w > mx && y < my + mh && y + h > my)
}

/// Monitor rectangles (x, y, width, height) from `xrandr --listmonitors`,
/// else the display size.
fn monitors() -> Vec<(i64, i64, i64, i64)> {
    let listed: Vec<_> = run_tool("xrandr", &["--listmonitors"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| parse_monitor(line.split_whitespace().nth(2)?))
        .collect();
    if !listed.is_empty() {
        return listed;
    }
    let size = xdotool(&["getdisplaygeometry"]).unwrap_or_default();
    let mut size = size
        .split_whitespace()
        .map(|n| n.parse::<i64>().unwrap_or(0));
    vec![(0, 0, size.next().unwrap_or(0), size.next().unwrap_or(0))]
}

/// `1920/530x1080/300+1920+0` -> (1920, 0, 1920, 1080).
fn parse_monitor(spec: &str) -> Option<(i64, i64, i64, i64)> {
    let (size, rest) = spec.split_once('+')?;
    let (x, y) = rest.split_once('+')?;
    let (w, h) = size.split_once('x')?;
    let number = |text: &str| text.split('/').next()?.parse::<i64>().ok();
    Some((x.parse().ok()?, y.parse().ok()?, number(w)?, number(h)?))
}

fn focus_if_given(input: &X11Input) -> Result<(), String> {
    if input.window.is_some() {
        activate(&window_id(input)?)?;
    }
    Ok(())
}

/// (x, y, width, height) of a window.
fn window_geometry(id: &str) -> Result<(i64, i64, i64, i64), String> {
    let shell = xdotool(&["getwindowgeometry", "--shell", id])?;
    let field = |key: &str| {
        shell
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0)
    };
    Ok((field("X"), field("Y"), field("WIDTH"), field("HEIGHT")))
}

/// A point in screen coordinates: as given, or relative to `window`.
fn point(input: &X11Input, x: Option<i64>, y: Option<i64>) -> Result<(i64, i64), String> {
    let (x, y) = (x.ok_or("needs x")?, y.ok_or("needs y")?);
    if input.window.is_some() {
        let id = window_id(input)?;
        let (wx, wy, ..) = window_geometry(&id)?;
        raise_visible(&id)?;
        return Ok((wx + x, wy + y));
    }
    Ok((x, y))
}

fn click(input: &X11Input) -> Result<Value, String> {
    let button = input.button.unwrap_or(1).clamp(1, 9).to_string();
    let count = input.count.unwrap_or(1).clamp(1, 5).to_string();
    let mut args = Vec::new();
    let target = if input.x.is_some() || input.y.is_some() {
        let (x, y) = point(input, input.x, input.y)?;
        args.extend([
            "mousemove".to_string(),
            x.to_string(),
            y.to_string(),
            "sleep".to_string(),
            "0.05".to_string(),
        ]);
        Some((x, y))
    } else {
        None
    };
    args.extend(["click", "--repeat", &count, "--delay", "80", &button].map(str::to_string));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    xdotool(&args)?;
    Ok(
        json!({ "clicked": target.map(|(x, y)| json!({"x": x, "y": y})), "button": button, "count": count }),
    )
}

fn drag(input: &X11Input) -> Result<Value, String> {
    let (x, y) = point(input, input.x, input.y)?;
    let (to_x, to_y) = point(input, input.to_x, input.to_y)?;
    let button = input.button.unwrap_or(1).clamp(1, 3).to_string();
    xdotool(&["mousemove", &x.to_string(), &y.to_string()])?;
    xdotool(&["mousedown", &button])?;
    sleep(Duration::from_millis(80));
    // In steps, so the application sees a drag rather than a jump.
    for step in 1..=10 {
        let px = x + (to_x - x) * step / 10;
        let py = y + (to_y - y) * step / 10;
        xdotool(&["mousemove", &px.to_string(), &py.to_string()])?;
        sleep(Duration::from_millis(20));
    }
    xdotool(&["mouseup", &button])?;
    Ok(json!({ "from": {"x": x, "y": y}, "to": {"x": to_x, "y": to_y} }))
}

fn scroll(input: &X11Input) -> Result<Value, String> {
    let direction = input.direction.as_deref().unwrap_or("down");
    let button = match direction {
        "up" => "4",
        "down" => "5",
        "left" => "6",
        "right" => "7",
        other => {
            return Err(format!(
                "unknown direction `{other}`; use up, down, left or right"
            ))
        }
    };
    if input.x.is_some() || input.y.is_some() {
        let (x, y) = point(input, input.x, input.y)?;
        xdotool(&["mousemove", &x.to_string(), &y.to_string()])?;
    }
    let count = input.count.unwrap_or(3).clamp(1, 50).to_string();
    xdotool(&["click", "--repeat", &count, "--delay", "30", button])?;
    Ok(json!({ "scrolled": direction, "steps": count }))
}

fn launch(input: &X11Input) -> Result<Value, String> {
    let command = input.command.as_deref().ok_or("launch needs command")?;
    // Detached in a session of its own, so it outlives claw and gets no
    // terminal.
    let child = Command::new("setsid")
        .args(["-f", "sh", "-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start `{command}`: {error}"))?;
    let mut child = child;
    let _ = child.wait();
    let Some(title) = input.wait_for.as_deref() else {
        return Ok(json!({ "launched": command }));
    };
    let id = wait_window(title, input.timeout.unwrap_or(15))?;
    Ok(json!({ "launched": command, "window": id, "title": window_title(&id) }))
}

fn wait_window(title: &str, timeout: u64) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(timeout.clamp(1, 120));
    loop {
        if let Ok(id) = resolve_window(title) {
            return Ok(id);
        }
        if Instant::now() > deadline {
            return Err(format!(
                "no window titled like \"{title}\" after {timeout}s"
            ));
        }
        sleep(Duration::from_millis(250));
    }
}

fn set_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = Command::new("xclip")
        .args(["-selection", "clipboard", "-i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("xclip: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("xclip has no stdin")?
        .write_all(text.as_bytes())
        .map_err(|error| error.to_string())?;
    child.wait().map_err(|error| error.to_string())?;
    Ok(())
}

/// A captured PNG and the screen region it shows.
struct Capture {
    path: String,
    window: Option<String>,
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    /// Screen pixels per image pixel.
    scale: f64,
}

/// Captures the screen, a window, or a region into a PNG and names it with
/// a `claw-image:` line so the image reaches the model.
fn screenshot(input: &X11Input) -> Result<Value, String> {
    let shot = capture(input, Some(MAX_SHOT_WIDTH))?;
    let (x, y, scale) = (shot.x, shot.y, shot.scale);
    Ok(json!({
        "path": shot.path,
        "window": shot.window,
        "region": { "x": x, "y": y, "width": shot.width, "height": shot.height },
        "imagePixelToScreen": format!(
            "screen x = {x} + image x * {scale:.3}, screen y = {y} + image y * {scale:.3}"
        ),
        "note": "With `window` set, click/drag/scroll coordinates are relative to the window, i.e. image pixels times the scale.",
    }))
}

/// Captures what `input` names (a window, a region, or the active monitor),
/// scaled down to `max_width` if given.
fn capture(input: &X11Input, max_width: Option<u32>) -> Result<Capture, String> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    let monitors = monitors();
    let sw = monitors.iter().map(|m| m.0 + m.2).max().unwrap_or(0);
    let sh = monitors.iter().map(|m| m.1 + m.3).max().unwrap_or(0);
    let (mut x, mut y, mut w, mut h, window) = if input.window.is_some() {
        let id = window_id(input)?;
        raise_visible(&id)?;
        let (x, y, w, h) = window_geometry(&id)?;
        (x, y, w, h, Some(id))
    } else if let (Some(x), Some(y), Some(w), Some(h)) =
        (input.x, input.y, input.width, input.height)
    {
        (x, y, w, h, None)
    } else {
        // The monitor showing the active window (or the first one).
        let active = xdotool(&["getactivewindow"])
            .ok()
            .and_then(|id| window_geometry(id.trim()).ok());
        let monitor = monitors
            .iter()
            .copied()
            .find(|&m| active.is_some_and(|a| visible(a, &[m])))
            .or_else(|| monitors.first().copied())
            .unwrap_or((0, 0, sw, sh));
        (monitor.0, monitor.1, monitor.2, monitor.3, None)
    };
    // Keep the region on the screen, with even sizes (the encoder wants them).
    x = x.clamp(0, (sw - 2).max(0));
    y = y.clamp(0, (sh - 2).max(0));
    w = w.min(sw - x).max(2) & !1;
    h = h.min(sh - y).max(2) & !1;
    let dir = std::env::temp_dir().join("claw-x11");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_millis())
        .unwrap_or_default();
    let path: PathBuf = dir.join(format!("shot-{stamp}.png"));
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let out_width = (w as u32).min(max_width.unwrap_or(u32::MAX));
    let scale_filter = format!("scale={out_width}:-2");
    let video_size = format!("{w}x{h}");
    let source = format!("{display}+{x},{y}");
    let path_text = path.display().to_string();
    let mut args = vec![
        "-loglevel",
        "error",
        "-y",
        "-f",
        "x11grab",
        "-video_size",
        &video_size,
        "-i",
        &source,
        "-frames:v",
        "1",
    ];
    if i64::from(out_width) < w {
        args.extend(["-vf", &scale_filter]);
    }
    args.push(&path_text);
    run_tool("ffmpeg", &args)?;
    #[allow(clippy::cast_precision_loss)]
    let scale = w as f64 / f64::from(out_width);
    Ok(Capture {
        path: path_text,
        window,
        x,
        y,
        width: w,
        height: h,
        scale,
    })
}

/// Reads the text in a window, region or image file with tesseract, with
/// each line's place on the screen (to click it).
fn ocr(input: &X11Input) -> Result<Value, String> {
    let (image, x, y, window) = match input.image.as_deref() {
        Some(path) => (path.to_string(), 0, 0, None),
        None => {
            let shot = capture(input, None)?;
            (shot.path, shot.x, shot.y, shot.window)
        }
    };
    let tsv = run_tool_with_timeout("tesseract", &[&image, "stdout", "--psm", "3", "tsv"], OCR_TIMEOUT)
        .map_err(|error| {
            if error.contains("not installed") {
                "OCR needs tesseract: install it (Gentoo: emerge app-text/tesseract; Debian: apt install tesseract-ocr)".to_string()
            } else {
                error
            }
        })?;
    let lines = ocr_lines(&tsv, x, y);
    let text = lines
        .iter()
        .map(|line| line["text"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let shown: Vec<Value> = lines.into_iter().take(300).collect();
    Ok(json!({
        "text": text,
        "lines": shown,
        "window": window,
        "image": image,
        "note": "x, y, width, height are screen pixels: click a line at its centre with `click` (no window).",
    }))
}

/// Lines from tesseract's TSV: their words joined, with screen boxes.
fn ocr_lines(tsv: &str, origin_x: i64, origin_y: i64) -> Vec<Value> {
    let mut lines: Vec<((i64, i64, i64, i64), Vec<String>, (i64, i64, i64, i64))> = Vec::new();
    for row in tsv.lines().skip(1) {
        let fields: Vec<&str> = row.split('\t').collect();
        if fields.len() < 12 || fields[0] != "5" {
            continue;
        }
        let word = fields[11].trim();
        let confidence = fields[10].parse::<f64>().unwrap_or(-1.0);
        if word.is_empty() || confidence < 30.0 {
            continue;
        }
        let number = |i: usize| fields[i].parse::<i64>().unwrap_or(0);
        let key = (number(1), number(2), number(3), number(4));
        let (left, top, width, height) = (number(6), number(7), number(8), number(9));
        match lines.iter_mut().find(|(line, ..)| *line == key) {
            Some((_, words, (x0, y0, x1, y1))) => {
                words.push(word.to_string());
                *x0 = (*x0).min(left);
                *y0 = (*y0).min(top);
                *x1 = (*x1).max(left + width);
                *y1 = (*y1).max(top + height);
            }
            None => lines.push((
                key,
                vec![word.to_string()],
                (left, top, left + width, top + height),
            )),
        }
    }
    lines
        .into_iter()
        .map(|(_, words, (x0, y0, x1, y1))| {
            json!({
                "text": words.join(" "),
                "x": origin_x + x0,
                "y": origin_y + y0,
                "width": x1 - x0,
                "height": y1 - y0,
            })
        })
        .collect()
}

/// The accessibility helper: Python with GObject's AT-SPI bindings, given a
/// JSON request as its argument, printing a JSON reply. Element ids are
/// child-index paths from the desktop ("3/0/2/5"), found again on each call.
const UI_SCRIPT: &str = r#"
import json, sys
try:
    import gi
    gi.require_version('Atspi', '2.0')
    from gi.repository import Atspi
except Exception as error:
    print(json.dumps({'error': 'AT-SPI is not available (install python gobject and at-spi2-core): %s' % error}))
    sys.exit(0)

req = json.loads(sys.argv[1])
op = req['op']
LIMIT = int(req.get('limit') or 80)
S = Atspi.StateType

def states(node):
    try:
        found = node.get_state_set()
    except Exception:
        return set()
    names = {'focused': S.FOCUSED, 'enabled': S.ENABLED, 'checked': S.CHECKED, 'selected': S.SELECTED,
             'editable': S.EDITABLE, 'showing': S.SHOWING, 'expanded': S.EXPANDED, 'active': S.ACTIVE}
    return {name for name, state in names.items() if found.contains(state)}

def extents(node):
    try:
        rect = node.get_component_iface().get_extents(Atspi.CoordType.SCREEN)
        return [rect.x, rect.y, rect.width, rect.height]
    except Exception:
        return None

def text_of(node, limit=200):
    try:
        iface = node.get_text_iface()
        if iface:
            count = iface.get_character_count()
            return iface.get_text(0, min(count, limit))
    except Exception:
        pass
    try:
        value = node.get_value_iface()
        if value:
            return str(value.get_current_value())
    except Exception:
        pass
    return None

def actions(node):
    try:
        iface = node.get_action_iface()
        return [iface.get_action_name(i) for i in range(iface.get_n_actions())] if iface else []
    except Exception:
        return []

def describe(node, path):
    item = {'id': path, 'role': node.get_role_name(), 'name': (node.get_name() or '')[:100]}
    found = states(node) - {'enabled', 'showing'}
    if found: item['states'] = sorted(found)
    text = text_of(node, 120)
    if text and text != item['name']: item['text'] = text
    box = extents(node)
    if box and box[2] > 0: item['box'] = box
    acts = actions(node)
    if acts: item['actions'] = acts
    return item

def children(node):
    try:
        count = min(node.get_child_count(), 400)
    except Exception:
        return
    for i in range(count):
        try:
            child = node.get_child_at_index(i)
        except Exception:
            continue
        if child is not None:
            yield i, child

def apps():
    desktop = Atspi.get_desktop(0)
    for i, app in children(desktop):
        yield str(i), app

def resolve(path):
    parts = [int(p) for p in path.split('/')]
    node = Atspi.get_desktop(0)
    for index in parts:
        node = node.get_child_at_index(index)
        if node is None:
            raise Exception('element %s is gone; find it again' % path)
    return node

def matching_apps():
    want = (req.get('app') or '').lower()
    for path, app in apps():
        name = (app.get_name() or '').lower()
        if not want or want in name:
            yield path, app

def visible(node):
    return 'showing' in states(node)

def walk(node, path, depth, out, test):
    if len(out) >= LIMIT or depth < 0:
        return
    for i, child in children(node):
        if len(out) >= LIMIT:
            return
        child_path = '%s/%d' % (path, i)
        if not visible(child) and child.get_role_name() not in ('frame', 'window', 'dialog'):
            continue
        if test(child):
            out.append(describe(child, child_path))
        walk(child, child_path, depth - 1, out, test)

try:
    if op == 'ui_apps':
        result = []
        for path, app in apps():
            windows = [{'id': '%s/%d' % (path, i), 'role': w.get_role_name(), 'name': w.get_name()}
                       for i, w in children(app)][:10]
            result.append({'id': path, 'app': app.get_name(), 'windows': windows})
        print(json.dumps({'apps': result}))
    elif op in ('ui_find', 'ui_tree'):
        name = (req.get('name') or '').lower()
        role = (req.get('role') or '').lower()
        def test(node):
            if op == 'ui_tree':
                return True
            if role and role not in node.get_role_name().lower():
                return False
            if name:
                label = ((node.get_name() or '') + ' ' + (text_of(node, 300) or '')).lower()
                return name in label
            return True
        out = []
        depth = int(req.get('depth') or (4 if op == 'ui_tree' else 30))
        roots = [(req['element'], resolve(req['element']))] if req.get('element') else list(matching_apps())
        for path, root in roots:
            walk(root, path, depth, out, test)
        print(json.dumps({'elements': out, 'more': len(out) >= LIMIT}))
    elif op == 'ui_read':
        node = resolve(req['element'])
        item = describe(node, req['element'])
        item['text'] = text_of(node, 20000)
        print(json.dumps(item))
    elif op == 'ui_click':
        node = resolve(req['element'])
        acts = actions(node)
        preferred = ['click', 'press', 'activate', 'jump', 'toggle', 'open', 'select']
        for want in preferred:
            if want in acts:
                node.get_action_iface().do_action(acts.index(want))
                print(json.dumps({'done': want, 'element': describe(node, req['element'])}))
                break
        else:
            print(json.dumps({'box': extents(node), 'element': describe(node, req['element'])}))
    elif op == 'ui_set_text':
        node = resolve(req['element'])
        editable = node.get_editable_text_iface()
        if editable and editable.set_text_contents(req.get('text') or ''):
            print(json.dumps({'done': 'set', 'element': describe(node, req['element'])}))
        else:
            try:
                node.get_component_iface().grab_focus()
            except Exception:
                pass
            print(json.dumps({'focus': True, 'box': extents(node)}))
    else:
        print(json.dumps({'error': 'unknown ui action'}))
except Exception as error:
    print(json.dumps({'error': str(error)}))
"#;

/// Runs one accessibility request.
fn ui_request(input: &X11Input, op: &str) -> Result<Value, String> {
    let request = json!({
        "op": op,
        "app": input.app,
        "name": input.name.as_deref().or(input.text.as_deref().filter(|_| op == "ui_find")),
        "role": input.role,
        "element": input.element,
        "depth": input.depth,
        "text": input.text,
        "limit": input.count,
    });
    let output = run_tool_with_timeout(
        "python3",
        &["-c", UI_SCRIPT, &request.to_string()],
        UI_TIMEOUT,
    )?;
    let reply: Value = serde_json::from_str(output.lines().last().unwrap_or_default())
        .map_err(|_| format!("the accessibility helper failed: {output}"))?;
    if let Some(error) = reply.get("error").and_then(Value::as_str) {
        return Err(error.to_string());
    }
    Ok(reply)
}

/// `ui_apps`, `ui_find`, `ui_tree`, `ui_read`, and `ui_set_text`, which types
/// into the field when it cannot set its text directly.
fn ui(input: &X11Input) -> Result<Value, String> {
    if matches!(input.action.as_str(), "ui_read" | "ui_set_text") && input.element.is_none() {
        return Err(format!(
            "{} needs `element` (an id from ui_find)",
            input.action
        ));
    }
    let reply = ui_request(input, &input.action)?;
    if input.action == "ui_set_text" && reply.get("focus").is_some() {
        let text = input.text.as_deref().unwrap_or_default();
        if let Some((x, y)) = box_center(&reply["box"]) {
            xdotool(&[
                "mousemove",
                &x.to_string(),
                &y.to_string(),
                "sleep",
                "0.05",
                "click",
                "1",
            ])?;
        }
        xdotool(&["key", "--clearmodifiers", "ctrl+a"])?;
        xdotool(&["type", "--clearmodifiers", "--delay", "12", "--", text])?;
        return Ok(json!({ "typed": text.chars().count() }));
    }
    Ok(reply)
}

/// Uses the element's own action (press, click, …), or clicks its middle.
fn ui_click(input: &X11Input) -> Result<Value, String> {
    if input.element.is_none() {
        return Err("ui_click needs `element` (an id from ui_find)".to_string());
    }
    let reply = ui_request(input, "ui_click")?;
    if reply.get("done").is_some() {
        return Ok(reply);
    }
    let (x, y) = box_center(&reply["box"]).ok_or("the element has no place on the screen")?;
    xdotool(&[
        "mousemove",
        &x.to_string(),
        &y.to_string(),
        "sleep",
        "0.05",
        "click",
        "1",
    ])?;
    Ok(json!({ "clicked": { "x": x, "y": y }, "element": reply["element"] }))
}

fn box_center(rect: &Value) -> Option<(i64, i64)> {
    let numbers: Vec<i64> = rect.as_array()?.iter().filter_map(Value::as_i64).collect();
    match numbers[..] {
        [x, y, w, h] if w > 0 && h > 0 => Some((x + w / 2, y + h / 2)),
        _ => None,
    }
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
    use super::{parse_monitor, regex_escape, render, resolve_window, visible};
    use serde_json::json;

    #[test]
    fn titles_are_matched_literally_and_ids_pass_through() {
        assert_eq!(regex_escape("a.b (c)"), "a\\.b \\(c\\)");
        assert_eq!(resolve_window("0x1a").unwrap(), "26");
        assert_eq!(resolve_window("123").unwrap(), "123");
    }

    #[test]
    fn monitors_parse_and_hidden_windows_are_not_visible() {
        assert_eq!(
            parse_monitor("1920/530x1080/300+1920+0"),
            Some((1920, 0, 1920, 1080))
        );
        let monitors = [(0, 0, 1920, 1080), (1920, 0, 1920, 1080)];
        assert!(visible((1920, 0, 1918, 1057), &monitors));
        assert!(!visible((-3827, 11, 955, 1047), &monitors));
        assert!(!visible((3841, 1, 100, 100), &monitors));
    }

    #[test]
    fn screenshots_are_named_for_the_model() {
        let text = render(json!({ "path": "/tmp/claw-x11/shot-1.png" })).unwrap();
        assert!(text.ends_with("\nclaw-image: /tmp/claw-x11/shot-1.png"));
        assert!(!render(json!({ "typed": 3 }))
            .unwrap()
            .contains("claw-image"));
    }
}

#[cfg(test)]
mod more_tests {
    use super::ocr_lines;

    #[test]
    fn ocr_words_become_lines_with_screen_boxes() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\t10\t20\t30\t10\t95\tHello\n\
                   5\t1\t1\t1\t1\t2\t45\t21\t40\t10\t90\tworld\n\
                   5\t1\t1\t1\t2\t1\t10\t40\t20\t10\t10\tnoise\n";
        let lines = ocr_lines(tsv, 100, 200);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["text"], "Hello world");
        assert_eq!(lines[0]["x"], 110);
        assert_eq!(lines[0]["width"], 75);
    }
}
