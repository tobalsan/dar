//! End-to-end proxy tests: real upstream MCP servers (stdio child process and
//! in-process streamable HTTP) registered into a bridge-mode tool registry.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{extract::State, http::HeaderMap, response::IntoResponse, routing::post, Json, Router};
use host_api::{
    BridgeSecrets, ConfigStore, EventBus, Extension, ForegroundRegistry, HostPaths, HttpRegistry,
    McpBridgeMode, RegisterCtx, ServiceRegistry, ShutdownToken, BRIDGE_SECRETS_SERVICE,
    MCP_BRIDGE_MODE_SERVICE,
};
use serde_json::{json, Value};
use tool_registry::{Redactor, ToolRegistry, ToolRegistryHandle, TOOL_REGISTRY_SERVICE};

const UPSTREAM: &str = env!("CARGO_BIN_EXE_dar-mcp-test-upstream");

struct Bridge {
    registry: Arc<ToolRegistry>,
    secrets: Arc<BridgeSecrets>,
}

impl Bridge {
    fn names(&self) -> Vec<String> {
        self.registry.list().into_iter().map(|s| s.name).collect()
    }

    async fn call(&self, name: &str, args: Value) -> (bool, String) {
        let outcome = self.registry.dispatch(name, args).await;
        let redactor = Redactor::from_secret_values(self.secrets.values());
        let result = outcome.redacted(&redactor).to_mcp_result();
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        (result["isError"].as_bool().unwrap_or(false), text)
    }
}

/// Run the MCP extension's register pass in bridge mode against `mcp.json`.
async fn bridge(root: &Path, mcp_json: Value) -> Bridge {
    std::fs::write(root.join("mcp.json"), mcp_json.to_string()).unwrap();
    let registry = Arc::new(ToolRegistry::new());
    let secrets = Arc::new(BridgeSecrets::default());
    let mut services = ServiceRegistry::default();
    services
        .service::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE, registry.clone())
        .unwrap();
    services
        .service(MCP_BRIDGE_MODE_SERVICE, Arc::new(McpBridgeMode))
        .unwrap();
    services
        .service(BRIDGE_SECRETS_SERVICE, secrets.clone())
        .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let mut ctx = RegisterCtx {
        bus: EventBus::new(),
        http: HttpRegistry::disabled(),
        foreground: ForegroundRegistry::default(),
        services,
        paths: HostPaths::new(root).unwrap(),
        config: ConfigStore::from_values(HashMap::new()),
        shutdown: ShutdownToken::new(rx),
    };
    mcp::McpExtension.register(&mut ctx).await.unwrap();
    Bridge { registry, secrets }
}

#[tokio::test]
async fn stdio_upstream_is_proxied_and_bad_schema_is_isolated() {
    let root = tempfile::tempdir().unwrap();
    let bridge = bridge(
        root.path(),
        json!({ "mcpServers": { "local": { "command": UPSTREAM } } }),
    )
    .await;

    // `bad` has an invalid schema: skipped, while `echo` still registers.
    assert_eq!(bridge.names(), vec!["local__echo"]);
    let (is_error, text) = bridge
        .call("local__echo", json!({ "text": "hello from upstream" }))
        .await;
    assert!(!is_error);
    assert_eq!(text, "hello from upstream");
}

#[tokio::test]
async fn configured_env_secret_echoed_by_upstream_is_redacted() {
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("DAR_MCP_TEST_ENV_SECRET", "opaque-env-value-123");
    let bridge = bridge(
        root.path(),
        json!({ "mcpServers": { "local": {
            "command": UPSTREAM,
            "env": { "ECHO_SECRET": "$env:DAR_MCP_TEST_ENV_SECRET" }
        } } }),
    )
    .await;
    let (_, text) = bridge.call("local__echo", json!({ "text": "x" })).await;
    assert!(!text.contains("opaque-env-value-123"), "{text}");
    assert!(text.contains("[REDACTED]"), "{text}");
}

#[tokio::test]
async fn stalled_upstream_does_not_block_healthy_servers() {
    let root = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let bridge = bridge(
        root.path(),
        json!({ "mcpServers": {
            "healthy": { "command": UPSTREAM },
            "stalled": { "command": UPSTREAM, "env": { "STALL": "1" } },
            "missing": { "command": "/nonexistent/dar-mcp-server" }
        } }),
    )
    .await;
    assert_eq!(bridge.names(), vec!["healthy__echo"]);
    // Bounded by the per-server discovery timeout, not by the stalled server.
    assert!(started.elapsed() < Duration::from_secs(25));
}

// -- streamable HTTP ----------------------------------------------------------

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

async fn http_upstream(
    State(seen): State<Seen>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> impl IntoResponse {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    seen.0.lock().unwrap().push(auth.clone());
    let Some(id) = req.get("id").cloned() else {
        return axum::http::StatusCode::ACCEPTED.into_response();
    };
    let result = match req["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion": req["params"]["protocolVersion"],
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "http-upstream", "version": "0" }
        }),
        Some("tools/list") => json!({ "tools": [{
            "name": "whoami", "description": "echo auth",
            "inputSchema": { "type": "object" }
        }]}),
        // Echo both the full header and its bare token back to the caller.
        Some("tools/call") => {
            let bare = auth.split_once(' ').map(|(_, t)| t).unwrap_or("");
            json!({ "content": [{ "type": "text", "text": format!("full={auth} bare={bare}") }] })
        }
        _ => json!({}),
    };
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

async fn spawn_http_upstream() -> (std::net::SocketAddr, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route(
            "/mcp",
            post(http_upstream).get(|| async { axum::http::StatusCode::METHOD_NOT_ALLOWED }),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}

#[tokio::test]
async fn public_http_upstream_connects_without_login() {
    let (addr, _) = spawn_http_upstream().await;
    let root = tempfile::tempdir().unwrap();
    let bridge = bridge(
        root.path(),
        json!({ "mcpServers": { "public": { "url": format!("http://{addr}/mcp") } } }),
    )
    .await;
    assert_eq!(bridge.names(), vec!["public__whoami"]);
}

#[tokio::test]
async fn http_upstream_receives_static_header_and_echo_is_redacted() {
    let (addr, seen) = spawn_http_upstream().await;

    let root = tempfile::tempdir().unwrap();
    std::env::set_var("DAR_MCP_TEST_HTTP_TOKEN", "Bearer opaque-http-token-456");
    let bridge = bridge(
        root.path(),
        json!({ "mcpServers": { "remote": {
            "url": format!("http://{addr}/mcp"),
            "headers": { "Authorization": "$env:DAR_MCP_TEST_HTTP_TOKEN" }
        } } }),
    )
    .await;

    assert_eq!(bridge.names(), vec!["remote__whoami"]);
    assert!(seen
        .0
        .lock()
        .unwrap()
        .iter()
        .all(|h| h == "Bearer opaque-http-token-456"));
    let (is_error, text) = bridge.call("remote__whoami", json!({})).await;
    assert!(!is_error, "{text}");
    assert!(!text.contains("opaque-http-token-456"), "{text}");
    assert!(
        text.starts_with("full=[REDACTED] bare=[REDACTED]"),
        "{text}"
    );
}
