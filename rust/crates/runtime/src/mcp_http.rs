//! Streamable HTTP transport for MCP servers (spec 2025-03-26 / 2025-06-18).
//!
//! Every JSON-RPC message is POSTed to the server URL. A request is answered
//! either with an `application/json` body (one response or a batch) or with a
//! `text/event-stream` whose `data:` events carry JSON-RPC messages; the
//! client reads events until the response with the matching id arrives. A
//! notification is answered with `202 Accepted` and no body. No GET listening
//! stream is opened.
//!
//! Header values are marked sensitive and never appear in errors or `Debug`
//! output; transport errors are stripped of the URL, which may carry a token.

use std::collections::BTreeMap;
use std::io;
use std::process::Stdio;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value as JsonValue};

use crate::config::expand_config_value;
use crate::mcp_client::McpRemoteTransport;
use crate::mcp_stdio::{
    JsonRpcId, JsonRpcRequest, JsonRpcResponse, McpInitializeParams, McpInitializeResult,
};
use crate::sse::IncrementalSseParser;

const SESSION_ID_HEADER: &str = "mcp-session-id";
const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEADERS_HELPER_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_DELETE_TIMEOUT: Duration = Duration::from_secs(2);
const ERROR_BODY_SNIPPET_CHARS: usize = 200;

/// Marks an I/O error as an authentication failure (HTTP 401/403), so the
/// manager can report it as such instead of as a retryable transport error.
pub(crate) const AUTH_FAILURE_KIND: io::ErrorKind = io::ErrorKind::PermissionDenied;

#[derive(Debug)]
pub struct McpHttpClient {
    server_name: String,
    url: String,
    headers: HeaderMap,
    client: reqwest::Client,
    session_id: Option<String>,
    protocol_version: Option<String>,
}

impl McpHttpClient {
    /// Resolves the configured headers (`${VAR}` / `~` expansion, then the
    /// `headersHelper` output on top) and builds the client. Nothing is sent.
    pub async fn connect(server_name: &str, transport: &McpRemoteTransport) -> io::Result<Self> {
        let mut raw_headers = transport
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), expand_config_value(value)))
            .collect::<BTreeMap<_, _>>();
        if let Some(helper) = transport.headers_helper.as_deref() {
            raw_headers.extend(run_headers_helper(server_name, &transport.url, helper).await?);
        }

        let mut headers = HeaderMap::new();
        for (name, value) in raw_headers {
            let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                invalid_input(format!(
                    "MCP server `{server_name}`: invalid header name `{name}`"
                ))
            })?;
            let mut header_value = HeaderValue::from_str(&value).map_err(|_| {
                invalid_input(format!(
                    "MCP server `{server_name}`: header `{name}` has an invalid value"
                ))
            })?;
            header_value.set_sensitive(true);
            headers.insert(header_name, header_value);
        }

        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|error| io::Error::other(describe_reqwest_error(error)))?;

        Ok(Self {
            server_name: server_name.to_string(),
            url: transport.url.clone(),
            headers,
            client,
            session_id: None,
            protocol_version: None,
        })
    }

    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    #[must_use]
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    /// `initialize`, then `notifications/initialized`. Records the session id
    /// (if the server assigns one) and the negotiated protocol version, both
    /// sent on every later request.
    pub async fn initialize(
        &mut self,
        id: JsonRpcId,
        params: McpInitializeParams,
    ) -> io::Result<JsonRpcResponse<McpInitializeResult>> {
        self.session_id = None;
        self.protocol_version = None;
        let response: JsonRpcResponse<McpInitializeResult> =
            self.request(id, "initialize", Some(params)).await?;
        if let Some(result) = &response.result {
            self.protocol_version = Some(result.protocol_version.clone());
            self.notify("notifications/initialized").await?;
        }
        Ok(response)
    }

    pub async fn request<TParams: Serialize, TResult: DeserializeOwned>(
        &mut self,
        id: JsonRpcId,
        method: &str,
        params: Option<TParams>,
    ) -> io::Result<JsonRpcResponse<TResult>> {
        let request = JsonRpcRequest::new(id.clone(), method, params);
        let body = serde_json::to_vec(&request).map_err(invalid_data)?;
        let response = self.post(body).await?;
        let message = self.read_response_message(response, &id, method).await?;
        let response: JsonRpcResponse<TResult> =
            serde_json::from_value(message).map_err(invalid_data)?;
        if response.jsonrpc != "2.0" {
            return Err(invalid_data(format!(
                "MCP response for {method} used unsupported jsonrpc version `{}`",
                response.jsonrpc
            )));
        }
        Ok(response)
    }

    /// Sends a notification; the server answers 202 with no body, and any
    /// body it does send is ignored.
    pub async fn notify(&mut self, method: &str) -> io::Result<()> {
        let body = serde_json::to_vec(&json!({ "jsonrpc": "2.0", "method": method }))
            .map_err(invalid_data)?;
        self.post(body).await.map(drop)
    }

    /// Ends the session: a best-effort `DELETE` when the server assigned a
    /// session id. Stateless servers get nothing.
    pub async fn shutdown(&mut self) {
        let Some(session_id) = self.session_id.take() else {
            return;
        };
        let request = self
            .client
            .delete(&self.url)
            .headers(self.headers.clone())
            .header(SESSION_ID_HEADER, session_id)
            .timeout(SESSION_DELETE_TIMEOUT);
        let _ = request.send().await;
        self.protocol_version = None;
    }

    async fn post(&mut self, body: Vec<u8>) -> io::Result<Response> {
        let mut request = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body);
        if let Some(session_id) = &self.session_id {
            request = request.header(SESSION_ID_HEADER, session_id);
        }
        if let Some(version) = &self.protocol_version {
            request = request.header(PROTOCOL_VERSION_HEADER, version);
        }

        let response = request.send().await.map_err(transport_error)?;
        if let Some(session_id) = response
            .headers()
            .get(SESSION_ID_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            self.session_id = Some(session_id.to_string());
        }
        self.check_status(response).await
    }

    async fn check_status(&self, response: Response) -> io::Result<Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(io::Error::new(
                AUTH_FAILURE_KIND,
                format!(
                    "authentication failed for MCP server `{}` (HTTP {status})",
                    self.server_name
                ),
            ));
        }
        if status == StatusCode::NOT_FOUND && self.session_id.is_some() {
            // The session expired; the manager resets and re-initializes.
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "MCP session expired (HTTP 404)",
            ));
        }
        let body = response.text().await.unwrap_or_default();
        let snippet = body
            .trim()
            .chars()
            .take(ERROR_BODY_SNIPPET_CHARS)
            .collect::<String>();
        Err(io::Error::other(if snippet.is_empty() {
            format!("HTTP {status}")
        } else {
            format!("HTTP {status}: {snippet}")
        }))
    }

    async fn read_response_message(
        &self,
        mut response: Response,
        id: &JsonRpcId,
        method: &str,
    ) -> io::Result<JsonValue> {
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if content_type.starts_with("text/event-stream") {
            let mut parser = IncrementalSseParser::new();
            let mut pending = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
                pending.extend_from_slice(&chunk);
                // Feed only whole lines so a UTF-8 sequence is never split.
                let Some(end) = pending.iter().rposition(|byte| *byte == b'\n') else {
                    continue;
                };
                let complete = pending.drain(..=end).collect::<Vec<_>>();
                for event in parser.push_chunk(&String::from_utf8_lossy(&complete)) {
                    if let Some(message) = matching_sse_message(&event.data, id) {
                        return Ok(message);
                    }
                }
            }
            let mut events = parser.push_chunk(&String::from_utf8_lossy(&pending));
            events.extend(parser.finish());
            return events
                .iter()
                .find_map(|event| matching_sse_message(&event.data, id))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("MCP event stream ended before the response to {method}"),
                    )
                });
        }

        let body = response.bytes().await.map_err(transport_error)?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Err(invalid_data(format!(
                "MCP server sent an empty body for {method} ({})",
                response_status_hint(&content_type)
            )));
        }
        let value: JsonValue = serde_json::from_slice(&body).map_err(invalid_data)?;
        matching_response(value, id).ok_or_else(|| {
            invalid_data(format!(
                "MCP response for {method} has no JSON-RPC response with id {id:?}"
            ))
        })
    }
}

fn response_status_hint(content_type: &str) -> &str {
    if content_type.is_empty() {
        "no content type"
    } else {
        content_type.split(';').next().unwrap_or(content_type)
    }
}

/// One SSE `data:` payload, if it is the response to `id`. Server requests
/// and notifications on the stream are skipped.
fn matching_sse_message(data: &str, id: &JsonRpcId) -> Option<JsonValue> {
    if data.trim().is_empty() {
        return None;
    }
    serde_json::from_str(data)
        .ok()
        .and_then(|value| matching_response(value, id))
}

/// The response to `id` in a message or batch.
fn matching_response(value: JsonValue, id: &JsonRpcId) -> Option<JsonValue> {
    match value {
        JsonValue::Array(messages) => messages
            .into_iter()
            .find(|message| is_response_to(message, id)),
        message if is_response_to(&message, id) => Some(message),
        _ => None,
    }
}

fn is_response_to(message: &JsonValue, id: &JsonRpcId) -> bool {
    let Some(object) = message.as_object() else {
        return false;
    };
    (object.contains_key("result") || object.contains_key("error"))
        && object
            .get("id")
            .and_then(|value| serde_json::from_value::<JsonRpcId>(value.clone()).ok())
            .is_some_and(|message_id| message_id == *id)
}

/// Runs `headersHelper` through the shell; its stdout must be a JSON object
/// of string header values. Neither its output nor its stderr is echoed in
/// errors, since both may carry secrets.
async fn run_headers_helper(
    server_name: &str,
    url: &str,
    helper: &str,
) -> io::Result<BTreeMap<String, String>> {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(expand_config_value(helper))
        .env("CLAUDE_CODE_MCP_SERVER_NAME", server_name)
        .env("CLAUDE_CODE_MCP_SERVER_URL", url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let output = tokio::time::timeout(HEADERS_HELPER_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("headersHelper for MCP server `{server_name}` timed out"),
            )
        })??;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "headersHelper for MCP server `{server_name}` exited with {}",
            output.status
        )));
    }
    serde_json::from_slice::<BTreeMap<String, String>>(&output.stdout).map_err(|_| {
        invalid_data(format!(
            "headersHelper for MCP server `{server_name}` must print a JSON object of string values"
        ))
    })
}

fn transport_error(error: reqwest::Error) -> io::Error {
    let kind = if error.is_timeout() {
        io::ErrorKind::TimedOut
    } else if error.is_connect() {
        io::ErrorKind::ConnectionRefused
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, describe_reqwest_error(error))
}

/// The error and its sources, without the URL (it may carry a token).
fn describe_reqwest_error(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn invalid_input(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use serde_json::{json, Value as JsonValue};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio::runtime::Builder;

    use super::{matching_response, JsonRpcId};
    use crate::config::{
        ConfigSource, McpRemoteServerConfig, McpServerConfig, ScopedMcpServerConfig,
    };
    use crate::mcp_stdio::{McpServerManager, McpServerManagerError};

    /// How the mock server answers.
    #[derive(Clone, Default)]
    struct MockOptions {
        sse: bool,
        session_id: Option<&'static str>,
        /// When set, requests without exactly this `Authorization` get 401.
        expected_authorization: Option<String>,
    }

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        method: String,
        headers: BTreeMap<String, String>,
        body: JsonValue,
    }

    type Recorded = Arc<Mutex<Vec<RecordedRequest>>>;

    /// A small Streamable HTTP MCP server on 127.0.0.1: one `echo` tool,
    /// keep-alive connections, every request recorded.
    async fn start_mock_server(options: MockOptions) -> (String, Recorded) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
        let recorded: Recorded = Arc::default();
        let log = Arc::clone(&recorded);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let options = options.clone();
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    while let Some(request) = read_request(&mut stream).await {
                        log.lock().unwrap().push(request.clone());
                        let response = respond(&options, &request);
                        if stream.get_mut().write_all(&response).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (url, recorded)
    }

    async fn read_request(
        stream: &mut BufReader<tokio::net::TcpStream>,
    ) -> Option<RecordedRequest> {
        let mut request_line = String::new();
        if stream.read_line(&mut request_line).await.ok()? == 0 {
            return None;
        }
        let method = request_line.split_whitespace().next()?.to_string();
        let mut headers = BTreeMap::new();
        loop {
            let mut line = String::new();
            stream.read_line(&mut line).await.ok()?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':')?;
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
        let length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.ok()?;
        Some(RecordedRequest {
            method,
            headers,
            body: serde_json::from_slice(&body).unwrap_or(JsonValue::Null),
        })
    }

    fn respond(options: &MockOptions, request: &RecordedRequest) -> Vec<u8> {
        if request.method != "POST" {
            return http_response("405 Method Not Allowed", &[], "");
        }
        if let Some(expected) = &options.expected_authorization {
            if request.headers.get("authorization") != Some(expected) {
                return http_response("401 Unauthorized", &[], "");
            }
        }
        let Some(id) = request.body.get("id").cloned() else {
            return http_response("202 Accepted", &[], "");
        };
        let result = match request.body["method"].as_str() {
            Some("initialize") => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock", "version": "1.0.0"},
            }),
            Some("tools/list") => json!({"tools": [{
                "name": "echo",
                "description": "Echoes text",
                "inputSchema": {"type": "object"},
            }]}),
            Some("tools/call") => json!({"content": [{
                "type": "text",
                "text": format!("echo:{}", request.body["params"]["arguments"]["text"]
                    .as_str()
                    .unwrap_or_default()),
            }]}),
            _ => json!({}),
        };
        let message = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        let session = options
            .session_id
            .filter(|_| request.body["method"] == "initialize")
            .map(|id| ("Mcp-Session-Id", id));
        let mut headers = session.into_iter().collect::<Vec<_>>();
        if options.sse {
            headers.push(("Content-Type", "text/event-stream"));
            // A server notification first, then the response split over lines.
            let notice = json!({"jsonrpc": "2.0", "method": "notifications/message",
                "params": {"level": "info", "data": "working"}});
            let body = format!(
                ": keep-alive\n\nevent: message\ndata: {notice}\n\nid: 1\ndata: {message}\n\n"
            );
            http_response("200 OK", &headers, &body)
        } else {
            headers.push(("Content-Type", "application/json"));
            http_response("200 OK", &headers, &message)
        }
    }

    fn http_response(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut text = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        text.push_str("\r\n");
        text.push_str(body);
        text.into_bytes()
    }

    fn http_servers(
        url: &str,
        headers: &[(&str, &str)],
    ) -> BTreeMap<String, ScopedMcpServerConfig> {
        BTreeMap::from([(
            "remote".to_string(),
            ScopedMcpServerConfig {
                required: false,
                scope: ConfigSource::Local,
                config: McpServerConfig::Http(McpRemoteServerConfig {
                    url: url.to_string(),
                    headers: headers
                        .iter()
                        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                        .collect(),
                    headers_helper: None,
                    oauth: None,
                }),
            },
        )])
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    /// Discovers and calls `echo`, returning the call's text and the log.
    async fn discover_and_call(
        options: MockOptions,
        headers: &[(&str, &str)],
    ) -> (String, Vec<RecordedRequest>) {
        let (url, recorded) = start_mock_server(options).await;
        let mut manager = McpServerManager::from_servers(&http_servers(&url, headers));
        assert!(manager.unsupported_servers().is_empty());

        let tools = manager.discover_tools().await.expect("discover tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].qualified_name, "mcp__remote__echo");

        let response = manager
            .call_tool("mcp__remote__echo", Some(json!({"text": "hi"})))
            .await
            .expect("call tool");
        let result = response.result.expect("tool result");
        let text = result.content[0].data["text"].as_str().unwrap().to_string();
        manager.shutdown().await.expect("shutdown");
        let log = recorded.lock().unwrap().clone();
        (text, log)
    }

    fn rpc_methods(log: &[RecordedRequest]) -> Vec<String> {
        log.iter()
            .filter(|request| request.method == "POST")
            .map(|request| {
                request.body["method"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn http_server_discovers_and_calls_tools_over_json_responses() {
        block_on(async {
            let (text, log) = discover_and_call(MockOptions::default(), &[]).await;
            assert_eq!(text, "echo:hi");
            assert_eq!(
                rpc_methods(&log),
                vec![
                    "initialize",
                    "notifications/initialized",
                    "tools/list",
                    "tools/call"
                ]
            );
            let first = &log[0];
            assert_eq!(first.headers["content-type"], "application/json");
            assert_eq!(
                first.headers["accept"],
                "application/json, text/event-stream"
            );
            assert!(!first.headers.contains_key("mcp-protocol-version"));
            // Stateless server: no session id is invented, and the negotiated
            // version is sent once initialize has answered.
            for request in &log[1..] {
                assert!(!request.headers.contains_key("mcp-session-id"));
                assert_eq!(request.headers["mcp-protocol-version"], "2025-06-18");
            }
            // No GET listening stream and no DELETE without a session.
            assert!(log.iter().all(|request| request.method == "POST"));
        });
    }

    #[test]
    fn http_server_reads_responses_from_event_streams() {
        block_on(async {
            let options = MockOptions {
                sse: true,
                ..MockOptions::default()
            };
            let (text, log) = discover_and_call(options, &[]).await;
            assert_eq!(text, "echo:hi");
            assert_eq!(rpc_methods(&log).len(), 4);
        });
    }

    #[test]
    fn http_server_echoes_the_session_id_and_deletes_it_on_shutdown() {
        block_on(async {
            let options = MockOptions {
                session_id: Some("session-abc"),
                ..MockOptions::default()
            };
            let (_, log) = discover_and_call(options, &[]).await;
            assert!(!log[0].headers.contains_key("mcp-session-id"));
            for request in &log[1..] {
                assert_eq!(request.headers["mcp-session-id"], "session-abc");
            }
            assert_eq!(
                log.last().map(|request| request.method.as_str()),
                Some("DELETE")
            );
        });
    }

    #[test]
    fn http_server_reports_authentication_failures_without_header_values() {
        block_on(async {
            let options = MockOptions {
                expected_authorization: Some("Bearer right-token".to_string()),
                ..MockOptions::default()
            };
            let (url, recorded) = start_mock_server(options).await;
            let mut manager = McpServerManager::from_servers(&http_servers(
                &url,
                &[("Authorization", "Bearer wrong-token")],
            ));

            let error = manager.discover_tools().await.expect_err("401 must fail");
            assert!(
                matches!(error, McpServerManagerError::Authentication { .. }),
                "{error:?}"
            );
            let message = error.to_string();
            assert!(
                message.contains("authentication failed for MCP server `remote`"),
                "{message}"
            );
            assert!(message.contains("401"), "{message}");
            assert!(!message.contains("wrong-token"), "{message}");
            assert!(!format!("{error:?}").contains("wrong-token"));
            // Not retried: a bad credential stays bad.
            assert_eq!(recorded.lock().unwrap().len(), 1);

            let report = manager.discover_tools_best_effort().await;
            assert!(report.tools.is_empty());
            assert_eq!(report.failed_servers.len(), 1);
            assert_eq!(report.failed_servers[0].server_name, "remote");
            assert!(report.failed_servers[0]
                .error
                .contains("authentication failed"));
        });
    }

    #[test]
    fn http_server_expands_environment_variables_in_headers() {
        let _guard = crate::test_env_lock();
        std::env::set_var("CLAW_TEST_MCP_HTTP_TOKEN", "secret-from-env");
        let options = MockOptions {
            expected_authorization: Some("Bearer secret-from-env".to_string()),
            ..MockOptions::default()
        };
        let (text, log) = block_on(discover_and_call(
            options,
            &[("Authorization", "Bearer ${CLAW_TEST_MCP_HTTP_TOKEN}")],
        ));
        std::env::remove_var("CLAW_TEST_MCP_HTTP_TOKEN");
        assert_eq!(text, "echo:hi");
        assert!(log
            .iter()
            .all(|request| request.headers["authorization"] == "Bearer secret-from-env"));
    }

    #[test]
    fn http_server_merges_headers_helper_output() {
        let options = MockOptions {
            expected_authorization: Some("Bearer from-helper".to_string()),
            ..MockOptions::default()
        };
        block_on(async {
            let (url, _) = start_mock_server(options).await;
            let mut servers = http_servers(&url, &[("Authorization", "Bearer static")]);
            if let Some(McpServerConfig::Http(config)) =
                servers.get_mut("remote").map(|server| &mut server.config)
            {
                config.headers_helper =
                    Some(r#"printf '{"Authorization":"Bearer from-helper"}'"#.to_string());
            }
            let mut manager = McpServerManager::from_servers(&servers);
            let tools = manager.discover_tools().await.expect("discover tools");
            assert_eq!(tools.len(), 1);
        });
    }

    #[test]
    fn http_server_connection_errors_surface_as_failed_servers() {
        block_on(async {
            // Bind then drop, so nothing listens on the port.
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
            drop(listener);
            let mut manager = McpServerManager::from_servers(&http_servers(&url, &[]));
            let report = manager.discover_tools_best_effort().await;
            assert!(report.tools.is_empty());
            assert!(report.unsupported_servers.is_empty());
            assert_eq!(report.failed_servers.len(), 1);
            assert!(report.failed_servers[0].error.contains("transport failed"));
            assert!(!report.failed_servers[0].error.contains(&url));
        });
    }

    #[test]
    fn matching_response_finds_the_id_in_a_batch_and_skips_requests() {
        let batch = json!([
            {"jsonrpc": "2.0", "id": 9, "method": "ping"},
            {"jsonrpc": "2.0", "id": 7, "result": {"ok": true}},
        ]);
        let found = matching_response(batch, &JsonRpcId::Number(7)).expect("response");
        assert_eq!(found["result"]["ok"], json!(true));

        let other = json!({"jsonrpc": "2.0", "id": 8, "result": {}});
        assert!(matching_response(other, &JsonRpcId::Number(7)).is_none());
    }
}
