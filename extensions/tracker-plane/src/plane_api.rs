//! `plane_api` — an extension-registered host tool (section 7).
//!
//! Registered through the shared [`ToolRegistry`] during the `tracker-plane`
//! extension's `register()` pass (not via env/prompt hints). It runs **in the
//! host runtime** via the MCP bridge, using the host-held Plane auth
//! (`PLANE_BOT_TOKEN` / `PLANE_OAUTH_TOKEN`, sent as `Authorization: Bearer
//! <token>`, or `PLANE_API_KEY`, sent as the `X-API-Key` header — loaded from the agent's
//! `.env`) against the configured Plane REST API base. The agent sees only the
//! input schema and a structured success / failure outcome — the raw token is
//! never returned, and is redacted from every error message.
//!
//! The agent supplies a REST `path` relative to the Plane API root; a leading
//! `/api/v1/` is optional and stripped so `{base}/api/v1/{path}` never doubles
//! up. Failure modes are all structured (`ToolOutcome::error`, i.e.
//! `isError: true`) so a failed call returns to the agent and the run continues
//! (section 7.2):
//!   - `invalid_args` — missing/empty `path`, unknown `method`, or non-object
//!     `body`,
//!   - `missing_auth` — `PLANE_BOT_TOKEN`/`PLANE_OAUTH_TOKEN`/`PLANE_API_KEY` unset/empty,
//!   - `transport_error` — connection refused, timeout, DNS, …,
//!   - `response_read_error` — the response body could not be read,
//!   - `http_error` — non-2xx HTTP status (body redacted + truncated),
//!   - `non_json_response` — a non-empty body that is not valid JSON.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tool_registry::{Redactor, ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec};

use crate::{
    parse_ext_config, resolve_plane_auth, PlaneAuth, API_KEY_ENV, BOT_TOKEN_ENV, DEFAULT_API_URL,
    OAUTH_TOKEN_ENV,
};

/// The tool name agents call by.
pub const TOOL_NAME: &str = "plane_api";

/// Resolve the Plane REST API base URL for the `plane_api` tool from the
/// extension config (`extensions.tracker-plane.api_url`), falling back to the
/// default Plane cloud API (section 7.3).
pub(crate) fn plane_api_base(config: Option<&Value>) -> String {
    let cfg = parse_ext_config(config);
    if cfg.api_url.is_empty() {
        DEFAULT_API_URL.to_string()
    } else {
        cfg.api_url
    }
}

/// Where the executor reads its Plane credential from. Production uses the env
/// vars (host-held secret); tests inject a static value so they never touch
/// process env or real auth.
#[derive(Clone)]
pub enum AuthSource {
    /// Resolve the credential from `PLANE_BOT_TOKEN` / `PLANE_OAUTH_TOKEN`
    /// (Bearer) or `PLANE_API_KEY` (`X-API-Key`) at call time.
    Env,
    /// A fixed credential, for tests.
    #[cfg(test)]
    Static(PlaneAuth),
    /// No credential, for the missing-auth test.
    #[cfg(test)]
    Missing,
}

impl AuthSource {
    /// Resolve the credential, or `None` when unset/empty.
    fn resolve(&self) -> Option<PlaneAuth> {
        match self {
            AuthSource::Env => resolve_plane_auth(),
            #[cfg(test)]
            AuthSource::Static(a) => Some(a.clone()),
            #[cfg(test)]
            AuthSource::Missing => None,
        }
    }
}

/// The in-host `plane_api` executor: holds the REST base, an HTTP client and the
/// auth source. Never stores the resolved token; it is read per call and only
/// ever sent as the credential header.
pub struct PlaneApiTool {
    client: reqwest::Client,
    base: String,
    auth: AuthSource,
}

impl PlaneApiTool {
    pub fn new(base: String, auth: AuthSource) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building reqwest client for plane_api tool")?;
        Ok(Self {
            client,
            base: if base.is_empty() {
                DEFAULT_API_URL.to_string()
            } else {
                base
            },
            auth,
        })
    }

    /// The MCP/registry tool spec (name + description + JSON input schema,
    /// section 7.1).
    pub fn spec() -> ToolSpec {
        ToolSpec::new(
            TOOL_NAME,
            "Call the host's configured Plane REST API using host-held auth. \
             Supply an HTTP `method` (default GET) and a `path` relative to the \
             Plane API root, e.g. \
             `workspaces/{workspace}/projects/{project}/work-items/` (a leading \
             `/api/v1/` is optional). Returns the JSON response on success; \
             missing auth, invalid arguments, transport failures and non-2xx \
             HTTP responses are returned as structured errors.",
            json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PATCH", "PUT", "DELETE"],
                        "description": "HTTP method (default GET).",
                    },
                    "path": {
                        "type": "string",
                        "description": "Plane REST path relative to the API root, e.g. workspaces/{workspace}/projects/{project}/work-items/. A leading /api/v1/ is optional.",
                    },
                    "body": {
                        "type": "object",
                        "description": "Optional JSON request body for POST/PATCH/PUT.",
                    },
                    "query": {
                        "type": "object",
                        "description": "Optional query parameters.",
                    }
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        )
        // GET reads; POST/PATCH/PUT/DELETE write. Mark both so logs flag a
        // potential state change.
        .with_access(true, true)
    }

    fn url_for(&self, path: &str) -> String {
        format!(
            "{}/api/v1/{}",
            self.base.trim_end_matches('/'),
            normalize_path(path)
        )
    }
}

#[async_trait::async_trait]
impl ToolExecutor for PlaneApiTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        // --- validate arguments (structured failure, not a host fault) ---
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error_code(
                "invalid_args",
                "plane_api requires a 'path' string argument",
                None::<String>,
            ));
        };
        if path.trim().is_empty() {
            return Ok(ToolOutcome::error_code(
                "invalid_args",
                "plane_api 'path' must not be empty",
                None::<String>,
            ));
        }
        let method_str = args
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let method = match method_str.as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            "PATCH" => reqwest::Method::PATCH,
            "PUT" => reqwest::Method::PUT,
            "DELETE" => reqwest::Method::DELETE,
            other => {
                return Ok(ToolOutcome::error_code(
                    "invalid_args",
                    format!("plane_api 'method' {other:?} is not a supported HTTP method"),
                    None::<String>,
                ));
            }
        };
        let body = match args.get("body") {
            None | Some(Value::Null) => None,
            Some(v @ Value::Object(_)) => Some(v.clone()),
            Some(_) => {
                return Ok(ToolOutcome::error_code(
                    "invalid_args",
                    "plane_api 'body' must be an object",
                    None::<String>,
                ));
            }
        };

        // --- resolve host-held auth ---
        let Some(auth) = self.auth.resolve() else {
            return Ok(ToolOutcome::error_code(
                "missing_auth",
                format!(
                    "plane_api is not configured: none of {BOT_TOKEN_ENV}, {OAUTH_TOKEN_ENV}, or {API_KEY_ENV} is set in the host environment (no Plane auth token is set)"
                ),
                Some(format!(
                    "Set {BOT_TOKEN_ENV}, {OAUTH_TOKEN_ENV}, or {API_KEY_ENV} in the agent .env"
                )),
            ));
        };
        let (header_name, header_value) = auth.header();
        let redactor = auth_redactor(&auth);

        // --- execute in-host; the token only leaves as the credential header ---
        let url = self.url_for(path);
        let mut rb = self
            .client
            .request(method, &url)
            .header(header_name, &header_value);
        if let Some(Value::Object(map)) = args.get("query") {
            let pairs: Vec<(String, String)> = map
                .iter()
                .map(|(k, v)| {
                    let value = v
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| v.to_string());
                    (k.clone(), value)
                })
                .collect();
            rb = rb.query(&pairs);
        }
        if let Some(body) = &body {
            rb = rb.json(body);
        }

        let response = match rb.send().await {
            Ok(resp) => resp,
            Err(err) => {
                // Transport failure (refused connection, timeout, DNS, …). Redact
                // defensively even though reqwest does not echo request headers.
                return Ok(ToolOutcome::error_code(
                    "transport_error",
                    format!(
                        "plane_api transport error: {}",
                        redactor.redact(&err.to_string())
                    ),
                    None::<String>,
                ));
            }
        };

        let status = response.status();
        let text = match response.text().await {
            Ok(text) => text,
            Err(err) => {
                return Ok(ToolOutcome::error_code(
                    "response_read_error",
                    format!("plane_api failed reading response body: {err}"),
                    None::<String>,
                ));
            }
        };

        if !status.is_success() {
            return Ok(ToolOutcome::error_code(
                "http_error",
                format!(
                    "plane_api HTTP {}: {}",
                    status.as_u16(),
                    truncate(&redactor.redact(&text), 500)
                ),
                None::<String>,
            ));
        }

        // Empty body (e.g. HTTP 204) is a success with no payload.
        if text.trim().is_empty() {
            return Ok(ToolOutcome::ok("null"));
        }

        let parsed: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(err) => {
                return Ok(ToolOutcome::error_code(
                    "non_json_response",
                    format!("plane_api received a non-JSON response: {err}"),
                    None::<String>,
                ));
            }
        };
        let rendered = serde_json::to_string(&parsed).unwrap_or_else(|_| "{}".to_string());
        Ok(ToolOutcome::ok(rendered))
    }
}

/// Register the `plane_api` tool against the shared registry, reading the host's
/// Plane credential from the environment at call time. Called from the
/// extension's `register()` pass (section 7.3).
pub fn register_into(registry: &dyn ToolRegistryHandle, base: String) -> Result<()> {
    let tool = PlaneApiTool::new(base, AuthSource::Env)?;
    registry.register_tool(PlaneApiTool::spec(), Arc::new(tool))
}

/// Strip a leading slash and an optional `api/v1/` prefix so the caller's path
/// composes cleanly onto the `{base}/api/v1/` root without doubling it.
fn normalize_path(path: &str) -> String {
    let trimmed = path.trim().trim_start_matches('/');
    trimmed
        .strip_prefix("api/v1/")
        .unwrap_or(trimmed)
        .to_string()
}

/// Redactor for one resolved credential: hides the full header value and, for a
/// Bearer credential, the bare token too.
fn auth_redactor(auth: &PlaneAuth) -> Redactor {
    let (_, value) = auth.header();
    let mut secrets = vec![value.clone()];
    if let Some(token) = value.strip_prefix("Bearer ") {
        secrets.push(token.to_string());
    }
    Redactor::from_secret_values(secrets)
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_for(server_url: &str) -> PlaneApiTool {
        PlaneApiTool::new(
            server_url.to_string(),
            AuthSource::Static(PlaneAuth::ApiKey("test-key".to_string())),
        )
        .unwrap()
    }

    #[test]
    fn spec_advertises_path_and_method() {
        let spec = PlaneApiTool::spec();
        assert_eq!(spec.name, "plane_api");
        assert_eq!(spec.input_schema["properties"]["path"]["type"], "string");
        assert_eq!(spec.input_schema["properties"]["method"]["type"], "string");
        assert_eq!(spec.input_schema["required"][0], "path");
    }

    #[test]
    fn normalize_path_strips_optional_api_v1_prefix() {
        assert_eq!(normalize_path("work-items/"), "work-items/");
        assert_eq!(normalize_path("/work-items/"), "work-items/");
        assert_eq!(normalize_path("/api/v1/work-items/"), "work-items/");
        assert_eq!(normalize_path("api/v1/workspaces/a/"), "workspaces/a/");
    }

    #[test]
    fn plane_api_base_defaults_and_reads_config() {
        assert_eq!(plane_api_base(None), DEFAULT_API_URL);
        assert_eq!(
            plane_api_base(Some(&json!({ "api_url": "https://plane.internal" }))),
            "https://plane.internal"
        );
    }

    #[tokio::test]
    async fn missing_path_is_structured_error() {
        let tool = tool_for("http://127.0.0.1:1");
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "invalid_args");
        assert!(out.text.contains("requires a 'path'"));
    }

    #[tokio::test]
    async fn empty_path_is_structured_error() {
        let tool = tool_for("http://127.0.0.1:1");
        let out = tool.execute(json!({ "path": "   " })).await.unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("must not be empty"));
    }

    #[tokio::test]
    async fn unknown_method_is_structured_error() {
        let tool = tool_for("http://127.0.0.1:1");
        let out = tool
            .execute(json!({ "path": "work-items/", "method": "TRACE" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "invalid_args");
        assert!(out.text.contains("not a supported HTTP method"));
    }

    #[tokio::test]
    async fn non_object_body_is_structured_error() {
        let tool = tool_for("http://127.0.0.1:1");
        let out = tool
            .execute(json!({ "path": "work-items/", "method": "POST", "body": "nope" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("'body' must be an object"));
    }

    #[tokio::test]
    async fn missing_auth_is_structured_error() {
        let tool =
            PlaneApiTool::new("http://127.0.0.1:1".to_string(), AuthSource::Missing).unwrap();
        let out = tool
            .execute(json!({ "path": "work-items/" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "missing_auth");
        assert!(out.text.contains("no Plane auth token is set"));
        assert!(out.text.contains("PLANE_BOT_TOKEN"));
        assert!(out.text.contains("PLANE_OAUTH_TOKEN"));
        assert!(out.text.contains("PLANE_API_KEY"));
    }

    #[tokio::test]
    async fn transport_failure_is_structured_error() {
        // Port 1 refuses connections — a transport error, not a host fault.
        let tool = tool_for("http://127.0.0.1:1");
        let out = tool
            .execute(json!({ "path": "work-items/" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "transport_error");
        assert!(out.text.contains("transport error"));
        // The token must never appear in any error text.
        assert!(!out.text.contains("test-key"));
    }

    // --- mocked Plane HTTP --------------------------------------------------

    use std::sync::Arc as StdArc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A one-shot HTTP/1.1 server: accepts a single connection, reads the
    /// request (and asserts on it via `inspect`), then writes `status` + `body`.
    /// Returns the bound `http://addr` URL.
    async fn mock_server(
        status_line: &'static str,
        body: &'static str,
        inspect: StdArc<dyn Fn(&str) + Send + Sync>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            inspect(&request);
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        });
        format!("http://{addr}")
    }

    fn no_inspect() -> StdArc<dyn Fn(&str) + Send + Sync> {
        StdArc::new(|_: &str| {})
    }

    #[tokio::test]
    async fn success_returns_json_payload() {
        let sent_auth = StdArc::new(std::sync::Mutex::new(String::new()));
        let captured = StdArc::clone(&sent_auth);
        let url = mock_server(
            "200 OK",
            r#"{"id":"wi-1","name":"Move tracker"}"#,
            StdArc::new(move |req: &str| {
                for line in req.lines() {
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("x-api-key: ") {
                        *captured.lock().unwrap() = v.trim().to_string();
                    }
                }
                // The optional /api/v1/ prefix composes onto the base.
                assert!(req.contains("GET /api/v1/work-items/"));
            }),
        )
        .await;

        let tool = tool_for(&url);
        let out = tool
            .execute(json!({ "path": "work-items/" }))
            .await
            .unwrap();

        assert!(!out.is_error, "unexpected error: {}", out.text);
        let data: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(data["id"], "wi-1");
        assert_eq!(data["name"], "Move tracker");
        // The host did send the token upstream, but it never reaches the agent.
        assert_eq!(*sent_auth.lock().unwrap(), "test-key");
        assert!(!out.text.contains("test-key"));
    }

    #[tokio::test]
    async fn body_is_forwarded() {
        let url = mock_server(
            "200 OK",
            r#"{"ok":true}"#,
            StdArc::new(|req: &str| {
                assert!(req.starts_with("POST "));
                assert!(req.contains("New work item"));
            }),
        )
        .await;

        let tool = tool_for(&url);
        let out = tool
            .execute(json!({
                "path": "work-items/",
                "method": "POST",
                "body": { "name": "New work item" }
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "unexpected error: {}", out.text);
    }

    #[tokio::test]
    async fn http_failure_is_structured_error() {
        let url = mock_server(
            "401 Unauthorized",
            r#"{"error":"authentication required"}"#,
            no_inspect(),
        )
        .await;

        let tool = tool_for(&url);
        let out = tool
            .execute(json!({ "path": "work-items/" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "http_error");
        assert!(out.text.contains("HTTP 401"));
        assert!(!out.text.contains("test-key"));
    }

    #[tokio::test]
    async fn http_failure_redacts_echoed_credential() {
        let url = mock_server(
            "401 Unauthorized",
            r#"{"error":"X-API-Key test-key rejected"}"#,
            no_inspect(),
        )
        .await;

        let tool = tool_for(&url);
        let out = tool
            .execute(json!({ "path": "work-items/" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "http_error");
        assert!(out.text.contains("HTTP 401"));
        assert!(!out.text.contains("test-key"));
        assert!(out.text.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn non_json_body_is_structured_error() {
        let url = mock_server("200 OK", "not json at all", no_inspect()).await;
        let tool = tool_for(&url);
        let out = tool
            .execute(json!({ "path": "work-items/" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "non_json_response");
        assert!(out.text.contains("non-JSON response"));
    }
}
