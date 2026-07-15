//! `linear_graphql` — an extension-registered host tool.
//!
//! Registered through the shared [`ToolRegistry`] during the `tracker-linear`
//! extension's `register()` pass (not via env/prompt hints). It runs **in the
//! host runtime** via the MCP bridge, using the host-held Linear auth
//! (`LINEAR_OAUTH_TOKEN` or `LINEAR_API_KEY`, loaded from the agent's `.env`)
//! and the configured Linear endpoint. An OAuth app token is sent as
//! `Authorization: Bearer <token>`; a personal API key is sent raw. The agent
//! sees only the input schema and a structured success /
//! failure outcome — the raw token is never returned.
//!
//! Failure modes are all structured (`ToolOutcome::error`, i.e. `isError: true`)
//! so a failed call returns to the agent and the run continues:
//!   - missing auth (`LINEAR_OAUTH_TOKEN`/`LINEAR_API_KEY` unset/empty),
//!   - invalid arguments (missing/empty `query`, or `variables` not an object),
//!   - transport failure (connection refused, timeout, …),
//!   - response body read failure,
//!   - non-2xx HTTP status,
//!   - non-JSON response body,
//!   - GraphQL errors in the response body.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use host_api::AgentEnv;
use serde::Deserialize;
use serde_json::{json, Value};
use tool_registry::{Redactor, ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec};

use crate::{
    resolve_linear_auth_header, resolve_linear_auth_header_from, API_KEY_ENV, DEFAULT_ENDPOINT,
    OAUTH_TOKEN_ENV,
};

/// Per-extension config for `extensions.tracker-linear` relevant to the tool.
#[derive(Debug, Clone, Default, Deserialize)]
struct TrackerLinearToolConfig {
    #[serde(default)]
    endpoint: Option<String>,
}

/// Resolve the GraphQL endpoint for the `linear_graphql` tool from the
/// extension config, falling back to the default Linear endpoint.
pub(crate) fn linear_graphql_endpoint(config: Option<&Value>) -> String {
    config
        .and_then(|v| serde_json::from_value::<TrackerLinearToolConfig>(v.clone()).ok())
        .and_then(|c| c.endpoint)
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string())
}

/// The tool name agents call by.
pub const TOOL_NAME: &str = "linear_graphql";

/// Where the executor reads its Linear API key from. Production uses the env var
/// (host-held secret); tests inject a static value so they never touch process
/// env or real auth.
#[derive(Clone)]
pub enum AuthSource {
    /// Resolve the `Authorization` header value from `LINEAR_OAUTH_TOKEN`
    /// (sent as `Bearer <token>`) or `LINEAR_API_KEY` (sent raw) at call time.
    Env(Option<Arc<dyn AgentEnv>>),
    /// A fixed `Authorization` header value, for tests.
    #[cfg(test)]
    Static(String),
}

impl AuthSource {
    /// Resolve the `Authorization` header value, or `None` when unset/empty.
    fn resolve(&self) -> Option<String> {
        match self {
            AuthSource::Env(Some(env)) => resolve_linear_auth_header_from(env.as_ref()),
            AuthSource::Env(None) => resolve_linear_auth_header(),
            #[cfg(test)]
            AuthSource::Static(k) => Some(k.clone()).filter(|k| !k.is_empty()),
        }
    }
}

/// The in-host `linear_graphql` executor: holds the endpoint, an HTTP client and
/// the auth source. Never stores the resolved token; it is read per call and
/// only ever sent as the `Authorization` header.
pub struct LinearGraphqlTool {
    client: reqwest::Client,
    endpoint: String,
    auth: AuthSource,
}

impl LinearGraphqlTool {
    pub fn new(endpoint: String, auth: AuthSource) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building reqwest client for linear_graphql tool")?;
        Ok(Self {
            client,
            endpoint: if endpoint.is_empty() {
                DEFAULT_ENDPOINT.to_string()
            } else {
                endpoint
            },
            auth,
        })
    }

    /// The MCP/registry tool spec (name + description + JSON input schema).
    pub fn spec() -> ToolSpec {
        ToolSpec::new(
            TOOL_NAME,
            "Execute a Linear GraphQL query or mutation against the host's \
             configured Linear API using host-held auth. Returns the GraphQL \
             `data` on success; GraphQL errors, HTTP failures, missing auth and \
             invalid arguments are returned as structured errors.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The GraphQL query or mutation document.",
                    },
                    "variables": {
                        "type": "object",
                        "description": "Optional GraphQL variables object.",
                    }
                },
                "required": ["query"],
                "additionalProperties": false,
            }),
        )
        // GraphQL documents may be queries (read) or mutations (write); mark
        // both so logs flag a potential state change.
        .with_access(true, true)
    }
}

#[async_trait::async_trait]
impl ToolExecutor for LinearGraphqlTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        // --- validate arguments (structured failure, not a host fault) ---
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error_code(
                "invalid_args",
                "linear_graphql requires a 'query' string argument",
                None::<String>,
            ));
        };
        if query.trim().is_empty() {
            return Ok(ToolOutcome::error_code(
                "invalid_args",
                "linear_graphql 'query' must not be empty",
                None::<String>,
            ));
        }
        let variables = match args.get("variables") {
            None | Some(Value::Null) => json!({}),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => {
                return Ok(ToolOutcome::error_code(
                    "invalid_args",
                    "linear_graphql 'variables' must be an object",
                    None::<String>,
                ));
            }
        };

        // --- resolve host-held auth ---
        let Some(auth_header) = self.auth.resolve() else {
            return Ok(ToolOutcome::error_code(
                "missing_auth",
                format!(
                    "linear_graphql is not configured: neither {OAUTH_TOKEN_ENV} nor {API_KEY_ENV} is set in the host environment (no Linear auth token is set)"
                ),
                Some(format!("Set {OAUTH_TOKEN_ENV} or {API_KEY_ENV} in the agent .env")),
            ));
        };

        let redactor = match &self.auth {
            AuthSource::Env(Some(env)) => Redactor::from_secret_values(
                env.secret_values().into_iter().chain([auth_header.clone()]),
            ),
            _ => auth_redactor(&auth_header),
        };

        // --- execute in-host; the token only leaves as the Authorization header ---
        let body = json!({ "query": query, "variables": variables });
        let response = match self
            .client
            .post(&self.endpoint)
            .header("Authorization", &auth_header)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                // Transport failure (refused connection, timeout, DNS, …). Keep
                // the message free of the token (reqwest does not echo headers).
                return Ok(ToolOutcome::error_code(
                    "transport_error",
                    format!("linear_graphql transport error: {err}"),
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
                    format!("linear_graphql failed reading response body: {err}"),
                    None::<String>,
                ));
            }
        };

        if !status.is_success() {
            return Ok(ToolOutcome::error_code(
                "http_error",
                format!(
                    "linear_graphql HTTP {}: {}",
                    status.as_u16(),
                    truncate(&redactor.redact(&text), 500)
                ),
                None::<String>,
            ));
        }

        let parsed: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(err) => {
                return Ok(ToolOutcome::error_code(
                    "non_json_response",
                    format!("linear_graphql received a non-JSON response: {err}"),
                    None::<String>,
                ));
            }
        };

        // GraphQL errors are reported with HTTP 200 + an `errors` array.
        if let Some(errors) = parsed.get("errors") {
            let non_empty = errors.as_array().map(|a| !a.is_empty()).unwrap_or(true);
            if non_empty {
                return Ok(ToolOutcome::error_code(
                    "graphql_error",
                    format!(
                        "linear_graphql GraphQL errors: {}",
                        redactor.redact(&errors.to_string())
                    ),
                    None::<String>,
                ));
            }
        }

        // Success: return just the `data` payload (or the whole body if absent).
        let data = parsed.get("data").cloned().unwrap_or(parsed);
        let rendered = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());
        Ok(ToolOutcome::ok(rendered))
    }
}

/// Register the `linear_graphql` tool against the shared registry, reading the
/// host's Linear API key from the environment at call time. Called from the
/// extension's `register()` pass.
pub fn register_into(
    registry: &dyn ToolRegistryHandle,
    endpoint: String,
    agent_env: Option<Arc<dyn AgentEnv>>,
) -> Result<()> {
    let tool = LinearGraphqlTool::new(endpoint, AuthSource::Env(agent_env))?;
    registry.register_tool(LinearGraphqlTool::spec(), Arc::new(tool))
}

fn auth_redactor(auth_header: &str) -> Redactor {
    let mut secrets = vec![auth_header.to_string()];
    if let Some(token) = auth_header.strip_prefix("Bearer ") {
        secrets.push(token.to_string());
    }
    Redactor::from_secret_values(secrets)
}

fn truncate(s: &str, max: usize) -> String {
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

    fn tool_for(server_url: &str) -> LinearGraphqlTool {
        LinearGraphqlTool::new(
            server_url.to_string(),
            AuthSource::Static("test-key".to_string()),
        )
        .unwrap()
    }

    #[test]
    fn spec_advertises_query_and_variables() {
        let spec = LinearGraphqlTool::spec();
        assert_eq!(spec.name, "linear_graphql");
        assert_eq!(spec.input_schema["properties"]["query"]["type"], "string");
        assert_eq!(
            spec.input_schema["properties"]["variables"]["type"],
            "object"
        );
        assert_eq!(spec.input_schema["required"][0], "query");
    }

    #[tokio::test]
    async fn missing_query_is_structured_error() {
        let tool = tool_for("http://127.0.0.1:1");
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "invalid_args");
        assert!(out.text.contains("requires a 'query'"));
    }

    #[tokio::test]
    async fn empty_query_is_structured_error() {
        let tool = tool_for("http://127.0.0.1:1");
        let out = tool.execute(json!({ "query": "   " })).await.unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("must not be empty"));
    }

    #[tokio::test]
    async fn non_object_variables_is_structured_error() {
        let tool = tool_for("http://127.0.0.1:1");
        let out = tool
            .execute(json!({ "query": "{ viewer { id } }", "variables": "nope" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("'variables' must be an object"));
    }

    #[tokio::test]
    async fn missing_auth_is_structured_error() {
        let tool = LinearGraphqlTool::new(
            "http://127.0.0.1:1".to_string(),
            AuthSource::Static(String::new()),
        )
        .unwrap();
        let out = tool
            .execute(json!({ "query": "{ viewer { id } }" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "missing_auth");
        assert!(out.text.contains("no Linear auth token is set"));
        assert!(out.text.contains("LINEAR_OAUTH_TOKEN"));
        assert!(out.text.contains("LINEAR_API_KEY"));
    }

    #[tokio::test]
    async fn transport_failure_is_structured_error() {
        // Port 1 refuses connections — a transport error, not a host fault.
        let tool = tool_for("http://127.0.0.1:1");
        let out = tool
            .execute(json!({ "query": "{ viewer { id } }" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "transport_error");
        assert!(out.text.contains("transport error"));
        // The token must never appear in any error text.
        assert!(!out.text.contains("test-key"));
    }

    // --- mocked Linear HTTP -------------------------------------------------

    use std::sync::Arc as StdArc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A one-shot HTTP/1.1 server: accepts a single connection, reads the
    /// request (and asserts on it via `inspect`), then writes `status` +
    /// `body`. Returns the bound `http://addr` URL.
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
    async fn success_returns_data_payload() {
        let sent_auth = StdArc::new(std::sync::Mutex::new(String::new()));
        let captured = StdArc::clone(&sent_auth);
        let url = mock_server(
            "200 OK",
            r#"{"data":{"viewer":{"id":"u-1","name":"Ada"}}}"#,
            StdArc::new(move |req: &str| {
                // Auth header is sent host-side; assert it carries our key.
                for line in req.lines() {
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("authorization: ") {
                        *captured.lock().unwrap() = v.trim().to_string();
                    }
                }
                assert!(req.contains("viewer"));
            }),
        )
        .await;

        let tool = tool_for(&url);
        let out = tool
            .execute(json!({ "query": "{ viewer { id name } }" }))
            .await
            .unwrap();

        assert!(!out.is_error, "unexpected error: {}", out.text);
        let data: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(data["viewer"]["id"], "u-1");
        assert_eq!(data["viewer"]["name"], "Ada");
        // The host did send the token upstream, but it never reaches the agent
        // (the outcome carries only `data`).
        assert_eq!(*sent_auth.lock().unwrap(), "test-key");
        assert!(!out.text.contains("test-key"));
    }

    #[tokio::test]
    async fn read_through_auth_rotates_between_tool_calls() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::env::remove_var(OAUTH_TOKEN_ENV);
        std::env::remove_var(API_KEY_ENV);
        std::fs::write(&path, format!("{API_KEY_ENV}=linear-old\n")).unwrap();
        agent_env::load_agent_env(dir.path()).unwrap();
        let env = agent_env::provider(dir.path());

        let old_url = mock_server(
            "200 OK",
            r#"{"data":{"ok":true}}"#,
            StdArc::new(|req| {
                assert!(req
                    .to_ascii_lowercase()
                    .contains("authorization: linear-old"));
            }),
        )
        .await;
        let old_tool = LinearGraphqlTool::new(old_url, AuthSource::Env(Some(env.clone()))).unwrap();
        assert!(
            !old_tool
                .execute(json!({"query":"{ viewer { id } }"}))
                .await
                .unwrap()
                .is_error
        );

        std::fs::write(&path, format!("{API_KEY_ENV}=linear-new\n")).unwrap();
        let new_url = mock_server(
            "401 Unauthorized",
            r#"{"echo":"linear-old linear-new"}"#,
            StdArc::new(|req| {
                assert!(req
                    .to_ascii_lowercase()
                    .contains("authorization: linear-new"));
            }),
        )
        .await;
        let new_tool = LinearGraphqlTool::new(new_url, AuthSource::Env(Some(env))).unwrap();
        let out = new_tool
            .execute(json!({"query":"{ viewer { id } }"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(!out.text.contains("linear-old"));
        assert!(!out.text.contains("linear-new"));
        std::env::remove_var(API_KEY_ENV);
    }

    #[tokio::test]
    async fn variables_are_forwarded() {
        let url = mock_server(
            "200 OK",
            r#"{"data":{"ok":true}}"#,
            StdArc::new(|req: &str| {
                assert!(req.contains("\"variables\""));
                assert!(req.contains("ALG-261"));
            }),
        )
        .await;

        let tool = tool_for(&url);
        let out = tool
            .execute(json!({
                "query": "query($id: String!){ issue(id:$id){ id } }",
                "variables": { "id": "ALG-261" }
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "unexpected error: {}", out.text);
    }

    #[tokio::test]
    async fn graphql_errors_are_structured_error() {
        let url = mock_server(
            "200 OK",
            r#"{"errors":[{"message":"Field 'bogus' doesn't exist"}],"data":null}"#,
            no_inspect(),
        )
        .await;

        let tool = tool_for(&url);
        let out = tool.execute(json!({ "query": "{ bogus }" })).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "graphql_error");
        assert!(out.text.contains("GraphQL errors"));
        assert!(out.text.contains("bogus"));
    }

    #[tokio::test]
    async fn graphql_errors_redact_echoed_authorization_header() {
        let url = mock_server(
            "200 OK",
            r#"{"errors":[{"message":"Authorization: test-key"}],"data":null}"#,
            no_inspect(),
        )
        .await;

        let tool = tool_for(&url);
        let out = tool
            .execute(json!({ "query": "{ viewer { id } }" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "graphql_error");
        assert!(!out.text.contains("test-key"));
        assert!(out.text.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn non_json_body_is_structured_error() {
        let url = mock_server("200 OK", "not json at all", no_inspect()).await;
        let tool = tool_for(&url);
        let out = tool
            .execute(json!({ "query": "{ viewer { id } }" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("non-JSON response"));
    }

    #[tokio::test]
    async fn http_failure_redacts_echoed_authorization_header() {
        let url = mock_server(
            "401 Unauthorized",
            r#"{"error":"Authorization: test-key"}"#,
            no_inspect(),
        )
        .await;

        let tool = tool_for(&url);
        let out = tool
            .execute(json!({ "query": "{ viewer { id } }" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "http_error");
        assert!(out.text.contains("HTTP 401"));
        assert!(!out.text.contains("test-key"));
        assert!(out.text.contains("[REDACTED]"));
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
            .execute(json!({ "query": "{ viewer { id } }" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error.as_ref().unwrap().code, "http_error");
        assert!(out.text.contains("HTTP 401"));
        assert!(!out.text.contains("test-key"));
    }
}
