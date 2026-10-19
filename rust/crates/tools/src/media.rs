//! Media tools:
//! - `vision_analyze`: puts an image (a file or a URL) in front of the model
//!   with a question; `read_file` on an image does the same.
//! - `image_generate`: makes an image from a prompt with an OpenAI-compatible
//!   images API or a local stable-diffusion.cpp `sd`.
//! - `text_to_speech`: speaks text with an OpenAI-compatible speech API,
//!   piper, or espeak-ng, saving the audio and optionally playing it.
//!
//! Images reach the model through the `claw-image:` line that
//! [`runtime::tool_images`] attaches as vision input, so answering about
//! them needs a model that can see.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use runtime::tool_images::TOOL_IMAGE_MARKER;
use runtime::PermissionMode;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{to_pretty_json, ToolSpec};

/// Images larger than this are not attached (matches `tool_images`).
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "vision_analyze",
            description: "Look at an image: a screenshot, photo, diagram, chart, scanned page or UI mock-up, from a local file or an http(s) URL (png, jpg, gif, webp, bmp). The image is attached to the result so you can see it; answer the question about it yourself. read_file on an image path does the same. Needs a model with vision input.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "image": { "type": "string", "description": "Path or http(s) URL." },
                    "question": { "type": "string", "description": "What to look for." }
                },
                "required": ["image"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "image_generate",
            description: "Generate an image from a text prompt and save it as PNG (default ~/Pictures/claw/). Backends, first available wins: an OpenAI-compatible images API (CLAW_IMAGE_API_URL, key in CLAW_IMAGE_API_KEY or OPENAI_API_KEY, model CLAW_IMAGE_MODEL, default gpt-image-1), or a local stable-diffusion.cpp `sd` binary (CLAW_SD_BIN, model file in CLAW_SD_MODEL). The image is attached to the result so you can check it.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" },
                    "negative_prompt": { "type": "string" },
                    "size": { "type": "string", "description": "WIDTHxHEIGHT, default 1024x1024." },
                    "steps": { "type": "integer", "minimum": 1, "maximum": 150 },
                    "output": { "type": "string", "description": "Where to save the PNG." }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "text_to_speech",
            description: "Speak text aloud and/or save it as audio. Backends, first available wins: an OpenAI-compatible speech API (CLAW_TTS_API_URL, key in CLAW_TTS_API_KEY or OPENAI_API_KEY, model CLAW_TTS_MODEL), piper (CLAW_PIPER_MODEL voice file), or espeak-ng/espeak. Saves to output (default ~/Music/claw/<time>.wav or .mp3) and plays it when play=true.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "voice": { "type": "string", "description": "API voice (alloy, …) or espeak voice (en-us, de, …)." },
                    "output": { "type": "string" },
                    "play": { "type": "boolean" }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
    ]
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    let parse = |input: &Value| input.clone();
    Some(match name {
        "vision_analyze" => from(parse(input)).and_then(vision_analyze),
        "image_generate" => from(parse(input)).and_then(image_generate),
        "text_to_speech" => from(parse(input)).and_then(text_to_speech),
        _ => return None,
    })
}

fn from<T: for<'de> Deserialize<'de>>(input: Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|error| error.to_string())
}

// ───────────────────────────── vision ─────────────────────────────

#[derive(Deserialize)]
struct VisionInput {
    image: String,
    question: Option<String>,
}

/// The image format from a file's first bytes.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG") {
        Some("png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"BM") {
        Some("bmp")
    } else {
        None
    }
}

/// Width and height from a PNG, GIF or JPEG header, when easy to read.
fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match sniff(bytes)? {
        "png" if bytes.len() >= 24 => Some((
            u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            u32::from_be_bytes(bytes[20..24].try_into().ok()?),
        )),
        "gif" if bytes.len() >= 10 => Some((
            u32::from(u16::from_le_bytes([bytes[6], bytes[7]])),
            u32::from(u16::from_le_bytes([bytes[8], bytes[9]])),
        )),
        "jpeg" => {
            let mut i = 2;
            while i + 9 < bytes.len() {
                if bytes[i] != 0xff {
                    return None;
                }
                let marker = bytes[i + 1];
                let length = usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
                if (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker) {
                    return Some((
                        u32::from(u16::from_be_bytes([bytes[i + 7], bytes[i + 8]])),
                        u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]])),
                    ));
                }
                i += 2 + length;
            }
            None
        }
        _ => None,
    }
}

/// A local copy of the image: the file itself, or a download of a URL.
fn local_image(source: &str) -> Result<PathBuf, String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let client = super::build_http_client()?;
        let response = client
            .get(source)
            .send()
            .map_err(|error| format!("download failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("download failed: HTTP {}", response.status()));
        }
        let bytes = response.bytes().map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err("the image is larger than 8 MB".to_string());
        }
        let kind =
            sniff(&bytes).ok_or("the URL did not return a png, jpeg, gif, webp or bmp image")?;
        let dir = std::env::temp_dir().join("claw-vision");
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let path = dir.join(format!("image-{}.{kind}", unique()));
        std::fs::write(&path, &bytes).map_err(|error| error.to_string())?;
        return Ok(path);
    }
    let path = expand_home(source);
    std::fs::canonicalize(&path).map_err(|error| format!("cannot open {source}: {error}"))
}

/// The tool result that shows `path` to the model.
pub(crate) fn image_result(path: &Path, question: Option<&str>) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "{} is {} MB; images over 8 MB are not attached. Shrink it first (e.g. `magick in.png -resize 50% out.png` or ffmpeg).",
            path.display(),
            metadata.len() / (1024 * 1024)
        ));
    }
    let mut head = vec![0_u8; 64 * 1024];
    let read = std::io::Read::read(
        &mut std::fs::File::open(path).map_err(|error| error.to_string())?,
        &mut head,
    )
    .map_err(|error| error.to_string())?;
    head.truncate(read);
    let format = sniff(&head).ok_or_else(|| {
        format!(
            "{} is not a png, jpeg, gif, webp or bmp image (svg must be rendered to png first)",
            path.display()
        )
    })?;
    let info = json!({
        "image": path.display().to_string(),
        "format": format,
        "bytes": metadata.len(),
        "size": dimensions(&head).map(|(w, h)| format!("{w}x{h}")),
        "question": question,
        "note": "The image is attached below as vision input.",
    });
    Ok(format!(
        "{}\n{TOOL_IMAGE_MARKER}{}",
        serde_json::to_string_pretty(&info).map_err(|error| error.to_string())?,
        path.display()
    ))
}

fn vision_analyze(input: VisionInput) -> Result<String, String> {
    let path = local_image(input.image.trim())?;
    image_result(&path, input.question.as_deref())
}

// ───────────────────────────── image_generate ─────────────────────────────

#[derive(Deserialize)]
struct GenerateInput {
    prompt: String,
    negative_prompt: Option<String>,
    size: Option<String>,
    steps: Option<u32>,
    output: Option<String>,
}

fn image_generate(input: GenerateInput) -> Result<String, String> {
    if input.prompt.trim().is_empty() {
        return Err("prompt must not be empty".to_string());
    }
    let size = input
        .size
        .clone()
        .unwrap_or_else(|| "1024x1024".to_string());
    let (width, height) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?)))
        .ok_or("size must look like 1024x1024")?;
    let output = match &input.output {
        Some(path) => expand_home(path),
        None => default_dir("Pictures").join(format!("image-{}.png", unique())),
    };
    if let Some(dir) = output.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let backend = if let Ok(base) = std::env::var("CLAW_IMAGE_API_URL") {
        generate_with_api(&base, &input.prompt, &size, &output)?;
        "api"
    } else if let Some(sd) = sd_binary() {
        let model = std::env::var("CLAW_SD_MODEL").map_err(|_| {
            "found stable-diffusion.cpp `sd`, but CLAW_SD_MODEL (the model file) is not set"
        })?;
        let mut command = Command::new(sd);
        command
            .args(["-m", &model, "-p", &input.prompt, "-o"])
            .arg(&output)
            .args(["-W", &width.to_string(), "-H", &height.to_string()])
            .args(["--steps", &input.steps.unwrap_or(20).to_string()]);
        if let Some(negative) = &input.negative_prompt {
            command.args(["-n", negative]);
        }
        let result = command
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("could not run sd: {error}"))?;
        if !result.status.success() || !output.exists() {
            return Err(format!(
                "sd failed: {}",
                String::from_utf8_lossy(&result.stderr)
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("no output")
            ));
        }
        "stable-diffusion.cpp"
    } else {
        return Err("No image generator is set up. Either point CLAW_IMAGE_API_URL at an OpenAI-compatible API (e.g. https://api.openai.com/v1, with OPENAI_API_KEY) or install stable-diffusion.cpp's `sd` and set CLAW_SD_MODEL to a model file (CLAW_SD_BIN if `sd` is not on PATH).".to_string());
    };
    let shown = image_result(&output, None)?;
    Ok(format!(
        "{{\"saved\": {:?}, \"backend\": {backend:?}}}\n{shown}",
        output.display().to_string()
    ))
}

fn sd_binary() -> Option<String> {
    if let Ok(bin) = std::env::var("CLAW_SD_BIN") {
        return Some(bin);
    }
    ["sd", "sd-cli", "stable-diffusion"]
        .into_iter()
        .find(|name| on_path(name))
        .map(str::to_string)
}

fn generate_with_api(base: &str, prompt: &str, size: &str, output: &Path) -> Result<(), String> {
    let key = std::env::var("CLAW_IMAGE_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .unwrap_or_default();
    let model = std::env::var("CLAW_IMAGE_MODEL").unwrap_or_else(|_| "gpt-image-1".to_string());
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|error| error.to_string())?;
    let mut request = client
        .post(format!("{}/images/generations", base.trim_end_matches('/')))
        .json(&json!({ "model": model, "prompt": prompt, "size": size, "n": 1 }));
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .map_err(|error| format!("image API: {error}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .map_err(|error| format!("image API reply: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "image API: HTTP {status}: {}",
            body.pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("")
        ));
    }
    let item = body
        .pointer("/data/0")
        .ok_or("image API returned no image")?;
    let bytes = if let Some(data) = item.get("b64_json").and_then(Value::as_str) {
        base64_decode(data).ok_or("image API returned bad base64")?
    } else if let Some(url) = item.get("url").and_then(Value::as_str) {
        client
            .get(url)
            .send()
            .and_then(reqwest::blocking::Response::bytes)
            .map_err(|error| error.to_string())?
            .to_vec()
    } else {
        return Err("image API returned neither b64_json nor url".to_string());
    };
    std::fs::write(output, bytes).map_err(|error| error.to_string())
}

pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        }))
    };
    let clean: Vec<u8> = text
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut acc = 0_u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= value(c)? << (18 - 6 * i);
        }
        let bytes = acc.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

// ───────────────────────────── text_to_speech ─────────────────────────────

#[derive(Deserialize)]
struct SpeechInput {
    text: String,
    voice: Option<String>,
    output: Option<String>,
    play: Option<bool>,
}

fn text_to_speech(input: SpeechInput) -> Result<String, String> {
    if input.text.trim().is_empty() {
        return Err("text must not be empty".to_string());
    }
    let api = std::env::var("CLAW_TTS_API_URL").ok();
    let extension = if api.is_some() { "mp3" } else { "wav" };
    let output = match &input.output {
        Some(path) => expand_home(path),
        None => default_dir("Music").join(format!("speech-{}.{extension}", unique())),
    };
    if let Some(dir) = output.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let backend = if let Some(base) = api {
        speak_with_api(&base, &input.text, input.voice.as_deref(), &output)?;
        "api"
    } else if let (true, Ok(model)) = (on_path("piper"), std::env::var("CLAW_PIPER_MODEL")) {
        let mut child = Command::new("piper")
            .args(["--model", &model, "--output_file"])
            .arg(&output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run piper: {error}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            std::io::Write::write_all(&mut stdin, input.text.as_bytes())
                .map_err(|error| error.to_string())?;
        }
        let result = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        if !result.status.success() {
            return Err(format!(
                "piper failed: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            ));
        }
        "piper"
    } else if let Some(espeak) = ["espeak-ng", "espeak"]
        .into_iter()
        .find(|name| on_path(name))
    {
        let mut command = Command::new(espeak);
        command.arg("-w").arg(&output);
        if let Some(voice) = &input.voice {
            command.args(["-v", voice]);
        }
        let result = command
            .arg(&input.text)
            .output()
            .map_err(|error| format!("could not run {espeak}: {error}"))?;
        if !result.status.success() {
            return Err(format!(
                "{espeak} failed: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            ));
        }
        espeak
    } else {
        return Err("No speech engine is set up. Install espeak-ng (Gentoo: emerge app-accessibility/espeak-ng), or piper with CLAW_PIPER_MODEL set to a voice .onnx, or point CLAW_TTS_API_URL at an OpenAI-compatible API (e.g. https://api.openai.com/v1, with OPENAI_API_KEY).".to_string());
    };
    let mut played = None;
    if input.play.unwrap_or(false) {
        played = ["paplay", "pw-play", "aplay", "ffplay"]
            .into_iter()
            .find(|player| on_path(player))
            .map(|player| {
                let mut command = Command::new(player);
                if player == "ffplay" {
                    command.args(["-nodisp", "-autoexit", "-loglevel", "quiet"]);
                }
                let ok = command
                    .arg(&output)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                json!({ "player": player, "ok": ok })
            });
    }
    to_pretty_json(json!({
        "saved": output.display().to_string(),
        "backend": backend,
        "played": played,
    }))
}

fn speak_with_api(
    base: &str,
    text: &str,
    voice: Option<&str>,
    output: &Path,
) -> Result<(), String> {
    let key = std::env::var("CLAW_TTS_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .unwrap_or_default();
    let model = std::env::var("CLAW_TTS_MODEL").unwrap_or_else(|_| "gpt-4o-mini-tts".to_string());
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|error| error.to_string())?;
    let mut request = client
        .post(format!("{}/audio/speech", base.trim_end_matches('/')))
        .json(&json!({ "model": model, "input": text, "voice": voice.unwrap_or("alloy") }));
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .map_err(|error| format!("speech API: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("speech API: HTTP {}", response.status()));
    }
    let bytes = response.bytes().map_err(|error| error.to_string())?;
    std::fs::write(output, bytes).map_err(|error| error.to_string())
}

// ───────────────────────────── helpers ─────────────────────────────

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map_or_else(
            || PathBuf::from(path),
            |home| PathBuf::from(home).join(rest),
        ),
        None => PathBuf::from(path),
    }
}

/// `~/<folder>/claw`, or the temp dir without a home.
fn default_dir(folder: &str) -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || std::env::temp_dir().join("claw"),
        |home| PathBuf::from(home).join(folder).join("claw"),
    )
}

fn unique() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{:03}", now.as_secs(), now.subsec_millis())
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_base64() {
        assert_eq!(
            super::base64_decode("aGVsbG8gd29ybGQ=").unwrap(),
            b"hello world"
        );
        assert_eq!(super::base64_decode("YQ==").unwrap(), b"a");
    }

    #[test]
    fn shows_a_png_to_the_model() {
        // A 1x1 PNG.
        let png = super::base64_decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
        )
        .unwrap();
        let path =
            std::env::temp_dir().join(format!("claw-vision-test-{}.png", std::process::id()));
        std::fs::write(&path, png).unwrap();
        let out = super::image_result(&path, Some("what colour?")).unwrap();
        assert!(out.contains("\"size\": \"1x1\""), "{out}");
        assert!(
            out.ends_with(&format!("claw-image: {}", path.display())),
            "{out}"
        );
        let _ = std::fs::remove_file(&path);
        assert!(super::image_result(std::path::Path::new("Cargo.toml"), None).is_err());
    }
}
