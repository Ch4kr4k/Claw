//! The context window of the server behind `local/` models.
//!
//! llama.cpp's `llama-server` reports the window it was started with
//! (`-c`) at `/props`; that is the authority, since a stale
//! `CLAW_CONTEXT_WINDOW` (say 32768 while the server still runs `-c 16384`)
//! makes every budget derived from it wrong. Servers without `/props`
//! (Ollama, vLLM, remote APIs) fall back to `CLAW_CONTEXT_WINDOW`.
//!
//! The probe names the model (`/props?model=<id>`): a llama-server in router
//! mode serves several models, each with its own `-c`, and needs the name to
//! answer; a single-model server ignores it.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a probed (or reported) window is trusted before `/props` is
/// asked again, so a server restarted with a new `-c` is picked up.
const CACHE_TTL: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Largest `/props` reply read; it holds the chat template, so it is not tiny.
const MAX_PROPS_BYTES: u64 = 4 * 1024 * 1024;

/// The last probed (or reported) window, and the model it belongs to.
static CACHE: Mutex<Option<(String, Instant, Option<u32>)>> = Mutex::new(None);

/// The context window of `model` (a `local/` model) in tokens: what
/// `/props` reports, else `CLAW_CONTEXT_WINDOW`. `None` when neither is known.
#[must_use]
pub fn local_context_window(model: &str) -> Option<u32> {
    if let Ok(cache) = CACHE.lock() {
        if let Some((cached_model, at, window)) = cache.as_ref() {
            if cached_model == model && at.elapsed() < CACHE_TTL {
                return window.or_else(env_context_window);
            }
        }
    }
    let probed = local_base_url()
        .and_then(|base_url| probe_props_context_window(&base_url, wire_model_id(model)));
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((model.to_string(), Instant::now(), probed));
    }
    probed.or_else(env_context_window)
}

/// Records a window the server reported for `model` in an overflow error, so
/// budgets built from now on use it without waiting for the next probe.
pub fn note_local_context_window(model: &str, tokens: u32) {
    if tokens == 0 {
        return;
    }
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((model.to_string(), Instant::now(), Some(tokens)));
    }
}

/// The reply budget for `model` (a `local/` model): a quarter of the
/// window, so a reply always has room and the rest is left for the
/// conversation. `None` when the window is unknown.
#[must_use]
pub fn local_max_output_tokens(model: &str) -> Option<u32> {
    local_context_window(model).map(local_max_output_tokens_for_window)
}

#[must_use]
pub fn local_max_output_tokens_for_window(window: u32) -> u32 {
    (window / 4).max(1024)
}

/// The id the server knows the model by: `local/` is claw's routing prefix
/// and is not sent on the wire.
fn wire_model_id(model: &str) -> &str {
    model.strip_prefix("local/").unwrap_or(model)
}

fn env_context_window() -> Option<u32> {
    std::env::var("CLAW_CONTEXT_WINDOW")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|window| *window > 0)
}

fn local_base_url() -> Option<String> {
    ["CLAW_LOCAL_BASE_URL", "OPENAI_BASE_URL"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Asks `<server>/props` for `n_ctx`. Plain `http://` only: local servers
/// do not use TLS, and a remote API has no `/props`.
fn probe_props_context_window(base_url: &str, model_id: &str) -> Option<u32> {
    let (authority, mut path) = props_target(base_url)?;
    if !model_id.is_empty() {
        path.push_str("?model=");
        path.push_str(&percent_encode(model_id));
    }
    let address = if authority.contains(':') && !authority.ends_with(']') {
        authority.clone()
    } else {
        format!("{authority}:80")
    };
    let socket = address.to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&socket, CONNECT_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(READ_TIMEOUT)).ok()?;
    // HTTP/1.0 keeps the reply unchunked and closes the connection after it.
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {authority}\r\nAccept: application/json\r\n\r\n"
    )
    .ok()?;
    let mut response = Vec::new();
    stream
        .take(MAX_PROPS_BYTES)
        .read_to_end(&mut response)
        .ok()?;
    parse_props_response(&String::from_utf8_lossy(&response))
}

/// Splits a base URL like `http://127.0.0.1:8080/v1` into the authority
/// and the `/props` path next to the API root (`/props`).
fn props_target(base_url: &str) -> Option<(String, String)> {
    let rest = base_url.trim().strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if authority.is_empty() {
        return None;
    }
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    let root = path
        .strip_suffix("v1")
        .unwrap_or(path)
        .trim_end_matches('/');
    let props = if root.is_empty() {
        "/props".to_string()
    } else {
        format!("/{root}/props")
    };
    Some((authority.to_string(), props))
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn parse_props_response(response: &str) -> Option<u32> {
    let (head, body) = response.split_once("\r\n\r\n")?;
    let status_ok = head
        .lines()
        .next()
        .is_some_and(|line| line.split_whitespace().nth(1) == Some("200"));
    if !status_ok {
        return None;
    }
    let props: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    props
        .pointer("/default_generation_settings/n_ctx")
        .or_else(|| props.get("n_ctx"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|n_ctx| u32::try_from(n_ctx).ok())
        .filter(|n_ctx| *n_ctx > 0)
}

#[cfg(test)]
mod tests {
    use super::{
        local_max_output_tokens_for_window, parse_props_response, percent_encode, props_target,
        wire_model_id,
    };

    #[test]
    fn probe_names_the_model_without_the_routing_prefix() {
        assert_eq!(wire_model_id("local/qwen3coder30A3B"), "qwen3coder30A3B");
        assert_eq!(
            percent_encode(wire_model_id("local/Qwen/Qwen3.6-27B:Q4_K_M")),
            "Qwen%2FQwen3.6-27B%3AQ4_K_M"
        );
    }

    #[test]
    fn props_path_sits_next_to_the_api_root() {
        assert_eq!(
            props_target("http://127.0.0.1:8080/v1"),
            Some(("127.0.0.1:8080".to_string(), "/props".to_string()))
        );
        assert_eq!(
            props_target("http://localhost:8080/v1/"),
            Some(("localhost:8080".to_string(), "/props".to_string()))
        );
        assert_eq!(
            props_target("http://box/llama/v1"),
            Some(("box".to_string(), "/llama/props".to_string()))
        );
        assert_eq!(props_target("https://api.example.com/v1"), None);
    }

    #[test]
    fn reads_n_ctx_from_llama_server_props() {
        let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
            {\"default_generation_settings\":{\"n_ctx\":16384},\"total_slots\":1}";
        assert_eq!(parse_props_response(response), Some(16384));
        let flat = "HTTP/1.1 200 OK\r\n\r\n{\"n_ctx\":32768}";
        assert_eq!(parse_props_response(flat), Some(32768));
    }

    #[test]
    fn ignores_errors_and_other_servers() {
        assert_eq!(
            parse_props_response("HTTP/1.1 404 Not Found\r\n\r\n{\"n_ctx\":4096}"),
            None
        );
        assert_eq!(
            parse_props_response("HTTP/1.1 200 OK\r\n\r\nnot json"),
            None
        );
        assert_eq!(parse_props_response("HTTP/1.1 200 OK\r\n\r\n{}"), None);
    }

    #[test]
    fn reply_budget_is_a_quarter_of_the_window() {
        assert_eq!(local_max_output_tokens_for_window(32768), 8192);
        assert_eq!(local_max_output_tokens_for_window(16384), 4096);
        assert_eq!(local_max_output_tokens_for_window(2048), 1024);
    }
}
