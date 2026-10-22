//! MCP Apps host: the only module that handles untrusted view resources and calls.
use super::*;
use dar_extension_sdk::mcp_apps::{self, McpApps, ResultRecord, MCP_APPS_SERVICE};
use serde_json::{json, Value};
use std::time::Duration;

#[cfg(not(test))]
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const CALL_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct AppsConfig {
    pub sandbox_bind: Option<String>,
    pub sandbox_port: Option<u16>,
    pub sandbox_origin: Option<String>,
    pub no_reuse: Vec<String>,
    #[serde(skip)]
    pub bound_port: Arc<std::sync::OnceLock<u16>>,
}

fn catalog(state: &AppState) -> Result<Arc<dyn McpApps>> {
    state
        .start
        .get()
        .context("chat has not started")?
        .host
        .services
        .get_named::<dyn McpApps>(MCP_APPS_SERVICE)
        .context("MCP Apps catalog unavailable")
}

async fn record(state: &AppState, session: &str, id: &str) -> Result<ResultRecord> {
    let session = state.session(session).await?;
    let tool = session
        .history
        .lock()
        .expect("chat history poisoned")
        .iter()
        .filter_map(|event| event.app.as_ref())
        .find(|app| app["recordId"].as_str() == Some(id))
        .and_then(|app| app["tool"].as_str())
        .map(str::to_owned)
        .context("view is not part of this conversation")?;
    mcp_apps::read_result(&state.root, id, &tool)?.context("view result is no longer available")
}

pub(super) async fn resource(
    Path((session, id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let outcome = async {
        let record = record(&state, &session, &id).await?;
        let catalog = catalog(&state)?;
        let resource = tokio::time::timeout(CALL_TIMEOUT,
            catalog.read_ui_resource(&record.server, &record.resource_uri)).await
            .context("view loading timed out")??;
        let mut meta = resource["meta"].clone();
        meta["ui"]["permissions"] = json!({});
        let (_,approved) = policy(meta.pointer("/ui/csp"));
        meta["ui"]["csp"] = approved;
        Ok::<_, anyhow::Error>(json!({"html":resource["html"], "meta":meta,
            "server":record.server,"resourceUri":record.resource_uri,"args":record.args,"result":record.result}))
    }.await;
    match outcome {
        Ok(value) => ([(header::CACHE_CONTROL, "no-store")], Json(value)).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

pub(super) async fn rpc(
    Path((session, id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let request_id = body.get("id").cloned().unwrap_or(Value::Null);
    let method = body["method"].as_str().unwrap_or("").to_owned();
    let mut server = String::new();
    let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
    let outcome = tokio::time::timeout(CALL_TIMEOUT, async {
        anyhow::ensure!(
            body["jsonrpc"] == "2.0"
                && !method.is_empty()
                && params.is_object()
                && (request_id.is_string() || request_id.is_i64() || request_id.is_null()),
            "malformed JSON-RPC message"
        );
        let record = record(&state, &session, &id).await?;
        server = record.server.clone();
        match method.as_str() {
            "ui/update-model-context" => {
                anyhow::ensure!(
                    params.get("content").is_none_or(Value::is_array)
                        && params.get("structuredContent").is_none_or(Value::is_object),
                    "invalid model context"
                );
                let slot = state.session(&session).await?;
                let view = if state.config.apps.no_reuse.contains(&record.server) {
                    format!("{}:{}:{id}",record.server,record.resource_uri)
                } else { format!("{}:{}",record.server,record.resource_uri) };
                slot.app_context
                    .lock()
                    .expect("app context poisoned")
                    .insert(
                        view,
                        params.clone(),
                    );
                Ok(json!({}))
            }
            "ui/message" => {
                anyhow::ensure!(params["role"] == "user", "only user messages are supported");
                let content = params.get("content").context("message content required")?;
                let blocks = if let Some(blocks) = content.as_array() {
                    blocks.clone()
                } else {
                    vec![content.clone()]
                };
                anyhow::ensure!(
                    !blocks.is_empty()
                        && blocks
                            .iter()
                            .all(|block| block["type"] == "text" && block["text"].is_string()),
                    "invalid message content"
                );
                let message = blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let response = submit(
                    Arc::clone(&state),
                    session.clone(),
                    format!("app-{id}-{}", now_ms()),
                    message,
                    vec![],
                    true,
                )
                .await;
                anyhow::ensure!(
                    response.status() == StatusCode::ACCEPTED,
                    "chat did not accept the message"
                );
                Ok(json!({}))
            }
            "tools/call" => {
                let tool = params["name"].as_str().context("tool name required")?;
                anyhow::ensure!(
                    !tool.contains("__"),
                    "cross-server tool calls are forbidden"
                );
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                anyhow::ensure!(args.is_object(), "tool arguments must be an object");
                catalog(&state)?.call_tool(&record.server, tool, args).await
            }
            "resources/read" => {
                let uri = params["uri"].as_str().context("resource URI required")?;
                catalog(&state)?.read_resource(&record.server, uri).await
            }
            "notifications/message" => {
                let level = params["level"].as_str().context("log level required")?;
                anyhow::ensure!(["debug","info","notice","warning","error","critical","alert","emergency"].contains(&level)
                    && params.get("data").is_some(), "invalid log message");
                tracing::info!(session=%session,server=%record.server,level=%level,"MCP App log notification");
                Ok(json!({}))
            }
            // Browser-only operations are reported here for the same audit trail.
            "ui/open-link" => {
                let url = params["url"].as_str().context("link URL required")?;
                let url = url::Url::parse(url)?;
                anyhow::ensure!(matches!(url.scheme(),"http"|"https"),"only HTTP and HTTPS links are allowed");
                Ok(json!({}))
            }
            "ping" | "ui/request-display-mode"
            | "ui/initialize"
            | "ui/notifications/initialized"
            | "ui/notifications/size-changed" => Ok(json!({})),
            _ => anyhow::bail!("unsupported app method"),
        }
    })
    .await;
    let outcome = outcome.unwrap_or_else(|_| Err(anyhow::anyhow!("view call timed out")));
    tracing::info!(session=%session, server=%server, method=%method,
        tool=params["name"].as_str().unwrap_or(""),status=if outcome.as_ref().is_ok_and(|value|value["isError"]!=true){"ok"}else{"error"}, "MCP App call");
    Json(match outcome {
        Ok(result) => json!({"jsonrpc":"2.0","id":request_id,"result":result}),
        Err(error) => json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32000,"message":error.to_string()}}),
    }).into_response()
}

pub(super) async fn config(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"sandboxPort":state.config.apps.bound_port.get(),
        "sandboxOrigin":state.config.apps.sandbox_origin,"noReuse":state.config.apps.no_reuse}))
}

pub(super) async fn start(state: Arc<AppState>) -> Result<()> {
    let start = state.start.get().context("chat has not started")?;
    let Some(dashboard) = start.host.http_addr() else {
        return Ok(());
    };
    let yaml: Value = serde_yaml::from_str(&fs::read_to_string(state.root.join("agent.yaml"))?)?;
    let fixed = yaml
        .pointer("/dashboard/port")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        != 0;
    let bind = state
        .config
        .apps
        .sandbox_bind
        .clone()
        .unwrap_or_else(|| dashboard.ip().to_string());
    let port = match state.config.apps.sandbox_port {
        Some(port) => port,
        None if fixed => dashboard
            .port()
            .checked_add(1)
            .context("dashboard port leaves no sandbox port")?,
        None => 0,
    };
    let listener = tokio::net::TcpListener::bind((bind.as_str(), port)).await?;
    let port = listener.local_addr()?.port();
    state
        .config
        .apps
        .bound_port
        .set(port)
        .map_err(|_| anyhow::anyhow!("sandbox already started"))?;
    let mut shutdown = start.shutdown.clone();
    let router = Router::new()
        .route("/sandbox/{record}", get(proxy))
        .with_state(state);
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
        {
            tracing::warn!(%error,"MCP Apps sandbox listener stopped");
        }
    });
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct ProxyQuery {
    session: String,
    #[serde(rename = "hostOrigin")]
    host_origin: String,
}

// A separate origin serves this proxy only. The CSP comes from server-owned
// metadata looked up by result id, never from a browser-supplied policy.
async fn proxy(
    Path(id): Path<String>,
    Query(query): Query<ProxyQuery>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let outcome = async {
        let origin = url::Url::parse(&query.host_origin)?;
        anyhow::ensure!(
            matches!(origin.scheme(), "http" | "https")
                && origin.origin().ascii_serialization() == query.host_origin,
            "invalid host origin"
        );
        let record = record(&state, &query.session, &id).await?;
        let catalog = catalog(&state)?;
        let resource = tokio::time::timeout(
            CALL_TIMEOUT,
            catalog.read_ui_resource(&record.server, &record.resource_uri),
        )
        .await
        .context("view loading timed out")??;
        let (csp, _) = policy(resource.pointer("/meta/ui/csp"));
        tracing::info!(session=%query.session,server=%record.server,csp=%csp,"MCP App CSP");
        let html = include_str!("sandbox.html").replace(
            "__HOST_ORIGIN__",
            &serde_json::to_string(&query.host_origin)?,
        );
        Ok::<_, anyhow::Error>((csp, html))
    }
    .await;
    match outcome {
        Ok((csp, html)) => (
            [
                (
                    header::CONTENT_SECURITY_POLICY,
                    format!("{csp} frame-ancestors {};", query.host_origin),
                ),
                (header::CACHE_CONTROL, "no-store".to_owned()),
                (header::CONTENT_TYPE, "text/html; charset=utf-8".to_owned()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            ],
            html,
        )
            .into_response(),
        Err(error) => (StatusCode::NOT_FOUND, error.to_string()).into_response(),
    }
}

fn policy(declared: Option<&Value>) -> (String, Value) {
    let mut approved = json!({});
    for key in [
        "connectDomains",
        "resourceDomains",
        "frameDomains",
        "baseUriDomains",
    ] {
        let values = declared
            .and_then(|csp| csp[key].as_array())
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|value| {
                let valid = (!value
                    .contains(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | ';' | '\\')))
                    && url::Url::parse(&value.replace("*.", "wildcard.")).is_ok_and(|url| {
                        (matches!(url.scheme(), "https" | "wss")
                            || (url.scheme() == "http" && url.host_str() == Some("localhost")))
                            && url.username().is_empty()
                            && url.password().is_none()
                            && url.query().is_none()
                            && url.fragment().is_none()
                            && url.path() == "/"
                            && url.host_str().is_some()
                    });
                if !valid {
                    tracing::warn!(domain=%value,"MCP App dropped invalid CSP domain");
                }
                valid
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        approved[key] = json!(values);
    }
    if declared.is_none() {
        return ("default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; media-src 'self' data:; connect-src 'none'; frame-src 'none'; base-uri 'self'; object-src 'none'; form-action 'none';".to_owned(),approved);
    }
    let join = |key: &str| {
        approved[key]
            .as_array()
            .expect("array above")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let resources = join("resourceDomains");
    let connections = join("connectDomains");
    let frames = join("frameDomains");
    let bases = join("baseUriDomains");
    (format!("default-src 'none'; script-src 'self' 'unsafe-inline' {resources}; style-src 'self' 'unsafe-inline' {resources}; img-src 'self' data: {resources}; media-src 'self' data: {resources}; font-src 'self' {resources}; connect-src {}; frame-src {}; base-uri {}; object-src 'none'; form-action 'none';",
        if connections.is_empty(){"'none'"}else{&connections},if frames.is_empty(){"'none'"}else{&frames},if bases.is_empty(){"'self'"}else{&bases}),approved)
}

pub(super) fn capture_result(
    root: Option<&std::path::Path>,
    event: &mut WireEvent,
    history: &VecDeque<WireEvent>,
) {
    if event.kind != "tool_output" {
        return;
    }
    let (text, id) = mcp_apps::extract_marker(event.text.as_deref().unwrap_or(""));
    event.text = Some(text);
    let call = history
        .iter()
        .rev()
        .find(|call| call.kind == "tool_call" && call.id == event.id);
    if let (Some(root), Some(id), Some(call)) = (root, id, call) {
        if let Ok(Some(record)) =
            mcp_apps::read_result(root, &id, call.name.as_deref().unwrap_or(""))
        {
            event.app = Some(
                json!({"recordId":id,"tool":record.tool,"piToolName":record.pi_tool_name,"server":record.server,"resourceUri":record.resource_uri,"args":record.args,"callId":event.id}),
            );
        }
    }
}
