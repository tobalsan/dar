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

// User-approved seam: Testing Decisions / Apps catalog, real stdio JSON-RPC.
// Catches capability negotiation, retained metadata and raw calls/resources;
// misses vendor OAuth policies and browser rendering.
#[tokio::test]
async fn apps_catalog_exposes_ui_metadata_resources_and_raw_results() {
    use dar_extension_sdk::mcp_apps::{McpApps, MCP_APPS_SERVICE};
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("mcp.json"),
        json!({"mcpServers": {
            "canvas": {"command": UPSTREAM, "env": {"APPS": "1"}}
        }})
        .to_string(),
    )
    .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let mut ctx = RegisterCtx {
        bus: EventBus::new(),
        http: HttpRegistry::disabled(),
        foreground: ForegroundRegistry::default(),
        services: ServiceRegistry::default(),
        paths: HostPaths::new(root.path()).unwrap(),
        config: ConfigStore::from_values(HashMap::new()),
        shutdown: ShutdownToken::new(rx),
    };
    mcp::McpExtension.register(&mut ctx).await.unwrap();
    let apps = ctx.services.get::<dyn McpApps>(MCP_APPS_SERVICE).unwrap();
    let tools = apps.list_tools().await.unwrap();
    assert_eq!(tools[0]["name"], "canvas__draw");
    assert_eq!(tools[0]["resourceUri"], "ui://canvas/app");
    assert_eq!(
        tools[0]["meta"]["ui"]["visibility"],
        json!(["model", "app"])
    );
    let resource = apps
        .read_ui_resource("canvas", "ui://canvas/app")
        .await
        .unwrap();
    assert_eq!(resource["html"], "<html><body>Canvas</body></html>");
    assert_eq!(
        resource["meta"]["ui"]["csp"]["connectDomains"],
        json!(["https://canvas.example"])
    );
    let raw = apps
        .call_tool("canvas", "refresh", json!({"text":"updated"}))
        .await
        .unwrap();
    assert_eq!(raw["content"][0]["text"], "updated");
    assert_eq!(raw["structuredContent"], json!({"revision":7}));
    assert_eq!(raw["_meta"], json!({"private":"retained"}));
    assert!(apps
        .call_tool("canvas", "model-only", json!({}))
        .await
        .is_err());
    assert!(apps
        .call_tool("canvas", "other__refresh", json!({}))
        .await
        .is_err());
    assert!(apps.call_tool("other", "refresh", json!({})).await.is_err());
    let legacy = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "canvas__legacy")
        .unwrap();
    assert_eq!(legacy["visibility"], json!(["model", "app"]));
    let cached = apps
        .read_ui_resource("canvas", "ui://canvas/app")
        .await
        .unwrap();
    assert_eq!(cached["meta"]["readCount"], 1);
    let fresh = apps
        .read_resource("canvas", "ui://canvas/app")
        .await
        .unwrap();
    assert_eq!(fresh["contents"][0]["_meta"]["readCount"], 2);
}

// Approved bridge/result-capture seam: model-visible tools and actual results.
// Catches hidden app-only tools and identical-call correlation; misses UI relay.
#[tokio::test]
async fn ui_bridge_hides_app_only_tools_and_captures_each_identical_call() {
    use dar_extension_sdk::mcp_apps::{extract_marker, read_result};
    let root = tempfile::tempdir().unwrap();
    let bridge = bridge(
        root.path(),
        json!({"mcpServers": {
            "canvas": {"command": UPSTREAM,"env":{"APPS":"1"}}
        }}),
    )
    .await;
    assert_eq!(
        bridge.names(),
        vec!["canvas__draw", "canvas__legacy", "canvas__model-only"]
    );
    let (first, second) = tokio::join!(
        bridge.call("canvas__draw", json!({"text":"same"})),
        bridge.call("canvas__draw", json!({"text":"same"})),
    );
    let (text, first_id) = extract_marker(&first.1);
    let (_, second_id) = extract_marker(&second.1);
    assert_eq!(text, "same\n{\"revision\":7}");
    assert_ne!(first_id, second_id);
    for id in [first_id.unwrap(), second_id.unwrap()] {
        let record = read_result(root.path(), &id, "canvas__draw")
            .unwrap()
            .unwrap();
        assert_eq!(record.result["_meta"], json!({"private":"retained"}));
        assert_eq!(record.args, json!({"text":"same"}));
        assert_eq!(record.resource_uri, "ui://canvas/app");
    }
}

#[tokio::test]
async fn ui_bridge_records_unique_pi_alias_for_hyphenated_tool() {
    use dar_extension_sdk::mcp_apps::{extract_marker, read_result};
    let root = tempfile::tempdir().unwrap();
    let bridge = bridge(
        root.path(),
        json!({"mcpServers": {
            "canva": {"command": UPSTREAM,"env":{"APPS":"alias"}}
        }}),
    )
    .await;
    assert_eq!(bridge.names(), vec!["canva__generate-image"]);
    let (_, text) = bridge
        .call("canva__generate-image", json!({"text":"generated"}))
        .await;
    let (_, id) = extract_marker(&text);
    let id = id.unwrap();
    let record = read_result(root.path(), &id, "mcp__dar__canva__generate_image")
        .unwrap()
        .unwrap();
    assert_eq!(record.tool, "canva__generate-image");
    assert_eq!(record.result["content"][0]["text"], "generated");
}

#[tokio::test]
async fn ui_bridge_does_not_guess_a_colliding_pi_alias() {
    use dar_extension_sdk::mcp_apps::{extract_marker, read_result};
    let root = tempfile::tempdir().unwrap();
    let bridge = bridge(
        root.path(),
        json!({"mcpServers": {
            "canva": {"command": UPSTREAM,"env":{"APPS":"alias-collision"}}
        }}),
    )
    .await;
    assert_eq!(
        bridge.names(),
        vec!["canva__generate-image", "canva__generate_image"]
    );
    for raw in ["canva__generate-image", "canva__generate_image"] {
        let (_, text) = bridge.call(raw, json!({"text":"generated"})).await;
        let (_, id) = extract_marker(&text);
        let id = id.unwrap();
        assert!(read_result(root.path(), &id, raw).unwrap().is_some());
        assert!(
            read_result(root.path(), &id, "mcp__dar__canva__generate_image")
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn ui_bridge_rejects_alias_colliding_with_a_long_pi_tool_name() {
    use dar_extension_sdk::mcp_apps::{extract_marker, read_result};
    let root = tempfile::tempdir().unwrap();
    let bridge = bridge(
        root.path(),
        json!({"mcpServers": {
            "canva": {"command": UPSTREAM,"env":{"APPS":"alias-long-collision"}}
        }}),
    )
    .await;
    let long = "canva__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let short = "canva__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_53c600fb";
    assert_eq!(bridge.names(), vec![short, long]);
    for raw in [long, short] {
        let (_, text) = bridge.call(raw, json!({"text":"generated"})).await;
        let (_, id) = extract_marker(&text);
        let id = id.unwrap();
        assert!(read_result(root.path(), &id, raw).unwrap().is_some());
        assert!(read_result(
            root.path(),
            &id,
            "mcp__dar__canva__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_53c600fb"
        )
        .unwrap()
        .is_none());
    }
}

#[tokio::test]
async fn ui_bridge_rejects_a_forced_hash_shadow_name() {
    use dar_extension_sdk::mcp_apps::{extract_marker, read_result};
    let root = tempfile::tempdir().unwrap();
    let bridge = bridge(
        root.path(),
        json!({"mcpServers": {
            "canva": {"command": UPSTREAM,"env":{"APPS":"alias-forced-collision"}}
        }}),
    )
    .await;
    for raw in [
        "canva__generate-image",
        "canva__generate_image",
        "canva__generate_image_ce231ecd",
    ] {
        let (_, text) = bridge.call(raw, json!({"text":"generated"})).await;
        let (_, id) = extract_marker(&text);
        let id = id.unwrap();
        assert!(read_result(root.path(), &id, raw).unwrap().is_some());
        assert!(
            read_result(root.path(), &id, "mcp__dar__canva__generate_image_ce231ecd")
                .unwrap()
                .is_none()
        );
    }
}

// Approved doctor/dashboard public HTTP seam: a real provider registration
// rejection must be distinguished from a view-host failure, without its body.
#[tokio::test]
async fn rejected_oauth_client_is_clear_in_dashboard_and_doctor() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let vendor = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let vendor_origin = format!("http://{}", vendor.local_addr().unwrap());
    let metadata = json!({
        "issuer":vendor_origin,
        "authorization_endpoint":format!("{vendor_origin}/authorize"),
        "token_endpoint":format!("{vendor_origin}/token"),
        "registration_endpoint":format!("{vendor_origin}/register"),
        "response_types_supported":["code"],
        "grant_types_supported":["authorization_code","refresh_token"],
        "code_challenge_methods_supported":["S256"]
    });
    let allow_registration = Arc::new(AtomicBool::new(false));
    let provider_registration = Arc::clone(&allow_registration);
    tokio::spawn(async move {
        let router = Router::new()
            .route(
                "/register",
                post(move |Json(request): Json<Value>| {
                    let allowed = provider_registration.load(Ordering::SeqCst);
                    async move { if allowed {
                        Json(json!({"client_id":"dar-test-client","redirect_uris":request["redirect_uris"]})).into_response()
                    } else { (
                        axum::http::StatusCode::FORBIDDEN,
                        "untrusted-client-private-vendor-detail",
                    ).into_response() } }
                }),
            )
            .fallback(move || {
                let metadata = metadata.clone();
                async move { Json(metadata) }
            });
        axum::serve(vendor, router).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("mcp.json"),
        json!({"mcpServers": {
            "vendor": {"url":vendor_origin}
        }})
        .to_string(),
    )
    .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let mut ctx = RegisterCtx {
        bus: EventBus::new(),
        http: HttpRegistry::default(),
        foreground: ForegroundRegistry::default(),
        services: ServiceRegistry::default(),
        paths: HostPaths::new(root.path()).unwrap(),
        config: ConfigStore::from_values(HashMap::new()),
        shutdown: ShutdownToken::new(rx),
    };
    mcp::McpExtension.register(&mut ctx).await.unwrap();
    let dashboard = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dashboard_origin = format!("http://{}", dashboard.local_addr().unwrap());
    let router = ctx.http.into_router();
    tokio::spawn(async move {
        axum::serve(dashboard, router).await.unwrap();
    });
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("{dashboard_origin}/mcp/oauth/start?server=vendor"))
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let message = response.text().await.unwrap();
    assert!(
        message.contains("Vendor rejected Dar OAuth client registration"),
        "{message}"
    );
    assert!(!message.contains("untrusted-client-private-vendor-detail"));
    let statuses = mcp::doctor_statuses(root.path()).await.unwrap();
    match &statuses[0].1 {
        mcp::DoctorStatus::Unreachable(reason) => {
            assert!(reason.contains("Vendor rejected Dar OAuth client registration"))
        }
        status => panic!("expected vendor rejection diagnostic, got {status:?}"),
    }
    let tabs = ctx
        .services
        .get::<cap_dashboard_tab::DashboardTabs>(cap_dashboard_tab::DASHBOARD_TABS_SERVICE)
        .unwrap();
    let tab = tabs
        .snapshot()
        .into_iter()
        .find(|tab| tab.id() == "mcp")
        .unwrap();
    assert!(tab
        .render()
        .unwrap()
        .contains("Vendor rejected Dar OAuth client registration"));
    allow_registration.store(true, Ordering::SeqCst);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = client
        .get(format!("{dashboard_origin}/mcp/oauth/start?server=vendor"))
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    let authorization = url::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    let state = authorization
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let mut callback = url::Url::parse(&format!("{dashboard_origin}/mcp/oauth/callback")).unwrap();
    callback.query_pairs_mut().extend_pairs(&[
        ("state", state.as_str()),
        ("error", "unauthorized_client"),
        (
            "error_description",
            "untrusted-authorization-private-detail",
        ),
    ]);
    let response = client.get(callback).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let message = response.text().await.unwrap();
    assert!(
        message.contains("Vendor rejected Dar OAuth authorization"),
        "{message}"
    );
    assert!(!message.contains("untrusted-authorization-private-detail"));
    assert!(tab
        .render()
        .unwrap()
        .contains("Vendor rejected Dar OAuth authorization"));
    match &mcp::doctor_statuses(root.path()).await.unwrap()[0].1 {
        mcp::DoctorStatus::Unreachable(reason) => {
            assert!(reason.contains("Vendor rejected Dar OAuth authorization"))
        }
        status => panic!("expected authorization rejection diagnostic, got {status:?}"),
    }
}

#[tokio::test]
async fn ui_bridge_delivers_upstream_rpc_failures_to_the_open_view() {
    use dar_extension_sdk::mcp_apps::{extract_marker, read_result};
    let root = tempfile::tempdir().unwrap();
    let bridge = bridge(
        root.path(),
        json!({"mcpServers": {
            "canvas": {"command": UPSTREAM,"env":{"APPS":"1"}}
        }}),
    )
    .await;
    let (is_error, text) = bridge
        .call("canvas__draw", json!({"text":"rpc-error"}))
        .await;
    assert!(is_error);
    let (_, id) = extract_marker(&text);
    let record = read_result(
        root.path(),
        &id.expect("view needs the failure result"),
        "canvas__draw",
    )
    .unwrap()
    .unwrap();
    assert_eq!(record.result["isError"], true);
    assert!(record.result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("call failed"));
}

#[tokio::test]
async fn apps_catalog_reads_blob_html_and_rejects_invalid_encoding() {
    use dar_extension_sdk::mcp_apps::{McpApps, MCP_APPS_SERVICE};
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("mcp.json"),
        json!({"mcpServers": {
            "canvas": {"command": UPSTREAM,"env":{"APPS":"blob"}}
        }})
        .to_string(),
    )
    .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let mut ctx = RegisterCtx {
        bus: EventBus::new(),
        http: HttpRegistry::disabled(),
        foreground: ForegroundRegistry::default(),
        services: ServiceRegistry::default(),
        paths: HostPaths::new(root.path()).unwrap(),
        config: ConfigStore::from_values(HashMap::new()),
        shutdown: ShutdownToken::new(rx),
    };
    mcp::McpExtension.register(&mut ctx).await.unwrap();
    let apps = ctx.services.get::<dyn McpApps>(MCP_APPS_SERVICE).unwrap();
    let resource = apps
        .read_ui_resource("canvas", "ui://canvas/blob")
        .await
        .unwrap();
    assert_eq!(resource["html"], "<h1>Blob</h1>");
    assert_eq!(resource["meta"]["ui"]["prefersBorder"], true);
    assert!(apps
        .read_ui_resource("canvas", "ui://canvas/invalid-utf8")
        .await
        .is_err());
    assert!(apps
        .read_ui_resource("canvas", "ui://canvas/invalid-base64")
        .await
        .is_err());
}

// ALG-532 Testing Decisions approve MCP configuration rejection at its public
// interface: catches the diagnostic, while browser origin enforcement is separate.
#[tokio::test]
async fn dar_server_name_is_reserved_for_the_builtin_app() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("mcp.json"),
        json!({"mcpServers":{"dar":{"command":"missing-reserved-server"}}}).to_string(),
    )
    .unwrap();
    let error = mcp::doctor_statuses(root.path())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("dar") && error.contains("reserved"),
        "{error}"
    );

    let (_tx, rx) = tokio::sync::watch::channel(false);
    let mut ctx = RegisterCtx {
        bus: EventBus::new(),
        http: HttpRegistry::disabled(),
        foreground: ForegroundRegistry::default(),
        services: ServiceRegistry::default(),
        paths: HostPaths::new(root.path()).unwrap(),
        config: ConfigStore::default(),
        shutdown: ShutdownToken::new(rx),
    };
    let error = mcp::McpExtension
        .register(&mut ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("dar") && error.contains("reserved"),
        "{error}"
    );
}
