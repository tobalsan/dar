//! MCP Apps host: the only module that handles untrusted view resources and calls.
use super::*;
use dar_extension_sdk::mcp_apps::{self, McpApps, ResultRecord, MCP_APPS_SERVICE};
use serde_json::{json, Value};
use sha2::Digest;
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
    pub sandbox_origins: HashMap<String, String>,
    pub no_reuse: Vec<String>,
    #[serde(skip)]
    pub bound_port: Arc<std::sync::OnceLock<u16>>,
    #[serde(skip)]
    tickets: Arc<std::sync::Mutex<HashMap<String, PreparedView>>>,
}

#[derive(Clone, Debug)]
struct PreparedView {
    session: String,
    id: String,
    record: ResultRecord,
    chat_origin: String,
    app_origin: String,
    html: String,
    csp: String,
    created_at: u64,
}

fn origin(value: &str) -> Result<url::Url> {
    let parsed = url::Url::parse(value)?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https")
            && parsed.origin().ascii_serialization() == value
            && parsed.host_str().is_some_and(|host| !host.ends_with('.')),
        "invalid sandbox origin"
    );
    Ok(parsed)
}

fn cookie_scope(origin: &url::Url) -> Result<&str> {
    let host = origin.host_str().context("sandbox hostname required")?;
    Ok(if matches!(origin.host(), Some(url::Host::Domain(_))) {
        psl::domain_str(host).unwrap_or(host)
    } else {
        host
    })
}

fn chat_origin(headers: &HeaderMap) -> Result<String> {
    let referer = url::Url::parse(
        headers
            .get(header::REFERER)
            .context("chat Referer required")?
            .to_str()?,
    )?;
    let chat = referer.origin().ascii_serialization();
    let parsed = origin(&chat)?;
    anyhow::ensure!(
        matches_host(headers, &parsed),
        "chat Host and Referer disagree"
    );
    Ok(chat)
}

fn matches_host(headers: &HeaderMap, assigned: &url::Url) -> bool {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| url::Url::parse(&format!("{}://{host}", assigned.scheme())).ok())
        .is_some_and(|host| {
            host.origin() == assigned.origin()
                && host.username().is_empty()
                && host.password().is_none()
                && host.path() == "/"
                && host.query().is_none()
                && host.fragment().is_none()
        })
}

async fn app_origin(state: &AppState, server: &str, chat: &str) -> Result<String> {
    let port = state
        .config
        .apps
        .bound_port
        .get()
        .context("sandbox listener unavailable")?;
    let config = &state.config.apps;
    let tools = if let Some(catalog) = external_catalog(state)? {
        tokio::time::timeout(CALL_TIMEOUT, catalog.list_tools())
            .await
            .context("view catalog timed out")??
    } else {
        json!([])
    };
    let mut servers = config
        .sandbox_origins
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    servers.insert("dar".to_owned());
    servers.insert(server.to_owned());
    servers.extend(
        tools
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["server"].as_str())
            .map(str::to_owned),
    );
    let chat_url = origin(chat)?;
    let chat_scope = cookie_scope(&chat_url)?;
    let chat_host = chat_url
        .host_str()
        .context("chat hostname required")?
        .to_owned();
    let mut scopes: HashMap<String, (String, bool, String)> = HashMap::new();
    let mut selected = None;
    for name in servers {
        let shared = !config.sandbox_origins.contains_key(&name) && config.sandbox_origin.is_some();
        let assigned = if config.sandbox_origins.get(&name).map(String::as_str) == Some("loopback")
        {
            if chat_host == "localhost" || chat_host.ends_with(".localhost") {
                format!("http://127.0.0.1:{port}")
            } else if chat_host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
            {
                format!("http://localhost:{port}")
            } else {
                anyhow::bail!("loopback app origin requires local chat");
            }
        } else if let Some(value) = config
            .sandbox_origins
            .get(&name)
            .or(config.sandbox_origin.as_ref())
        {
            anyhow::ensure!(
                value != "loopback",
                "loopback is valid only in sandbox_origins.<server>"
            );
            value.clone()
        } else {
            anyhow::ensure!(
                chat_host == "localhost"
                    || chat_host.ends_with(".localhost")
                    || chat_host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback()),
                "remote access requires a browser-visible sandbox origin"
            );
            let label = if !name.is_empty()
                && name.len() <= 63
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !name.starts_with('-')
                && !name.ends_with('-')
            {
                name.clone()
            } else {
                let hash = format!("{:x}", sha2::Sha256::digest(name.as_bytes()));
                format!("mcp-{}", &hash[..59])
            };
            format!("http://{label}.localhost:{port}")
        };
        let parsed = origin(&assigned)?;
        let scope = cookie_scope(&parsed)?.to_owned();
        anyhow::ensure!(scope != chat_scope, "chat and app must use different cookie scopes. Set sandbox_origins.<server>: loopback to use the other loopback host");
        if let Some((other, other_shared, other_origin)) = scopes.get(&scope) {
            anyhow::ensure!(
                shared && *other_shared && other_origin == &assigned,
                "sandbox cookie scope shared by {other} and {name}"
            );
        }
        scopes.insert(scope, (name.clone(), shared, assigned.clone()));
        if name == server {
            selected = Some(assigned);
        }
    }
    selected.context("sandbox origin unavailable")
}

fn external_catalog(state: &AppState) -> Result<Option<Arc<dyn McpApps>>> {
    Ok(state
        .start
        .get()
        .context("chat has not started")?
        .host
        .services
        .get_named::<dyn McpApps>(MCP_APPS_SERVICE)
        .ok())
}

fn catalog(state: &AppState, server: &str) -> Result<Arc<dyn McpApps>> {
    if server == "dar" {
        let store = state
            .start
            .get()
            .context("chat has not started")?
            .host
            .services
            .get_named::<dar_extension_sdk::artifacts::ArtifactStore>("artifact-store")
            .context("artifact store unavailable")?;
        return Ok(Arc::new(super::artifacts::ArtifactApp { store }));
    }
    external_catalog(state)?.context("MCP Apps catalog unavailable")
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
    headers: HeaderMap,
) -> Response {
    let outcome = async {
        let chat = chat_origin(&headers)?;
        let record = record(&state, &session, &id).await?;
        let app = app_origin(&state, &record.server, &chat).await?;
        let catalog = catalog(&state, &record.server)?;
        let resource = tokio::time::timeout(CALL_TIMEOUT,
            catalog.read_ui_resource(&record.server, &record.resource_uri)).await
            .context("view loading timed out")??;
        let mut meta = resource["meta"].clone();
        meta["ui"]["permissions"] = json!({});
        let (csp,approved) = policy(meta.pointer("/ui/csp"));
        meta["ui"]["csp"] = approved;
        let ticket = uuid::Uuid::new_v4().simple().to_string();
        let mut sandbox_url = url::Url::parse(&format!("{app}/sandbox/{id}"))?;
        sandbox_url.query_pairs_mut().append_pair("session", &session).append_pair("ticket", &ticket);
        let mut content_url = sandbox_url.clone();
        content_url.set_path(&format!("/content/{id}"));
        let prepared = PreparedView { session: session.clone(), id: id.clone(), record: record.clone(),
            chat_origin: chat, app_origin: app.clone(), html: resource["html"].as_str().context("view HTML required")?.to_owned(),
            csp, created_at: now_ms() };
        let mut tickets = state.config.apps.tickets.lock().expect("view tickets poisoned");
        tickets.retain(|_, view| now_ms().saturating_sub(view.created_at) < 60 * 60 * 1000);
        if tickets.len() >= 512 {
            if let Some(oldest) = tickets.iter().min_by_key(|(_, view)| view.created_at).map(|(key, _)| key.clone()) {
                tickets.remove(&oldest);
            }
        }
        tickets.insert(ticket, prepared);
        Ok::<_, anyhow::Error>(json!({"html":resource["html"], "meta":meta,
            "sandboxOrigin":app,"sandboxUrl":sandbox_url.as_str(),"contentUrl":content_url.as_str(),
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

pub(super) async fn download(
    Path((session, id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let outcome = async {
        let record = record(&state, &session, &id).await?;
        anyhow::ensure!(record.server == "dar", "not an artifact view");
        let uri = record
            .result
            .pointer("/structuredContent/uri")
            .and_then(Value::as_str)
            .context("artifact URI unavailable")?;
        let store = state
            .start
            .get()
            .context("chat has not started")?
            .host
            .services
            .get_named::<dar_extension_sdk::artifacts::ArtifactStore>("artifact-store")
            .context("artifact store unavailable")?;
        let (metadata, bytes) = super::artifacts::read(store, uri).await?;
        let fallback: String = metadata
            .filename
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let encoded: String =
            url::form_urlencoded::byte_serialize(metadata.filename.as_bytes()).collect();
        let disposition = format!(
            "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
            encoded.replace('+', "%20")
        );
        Ok::<_, anyhow::Error>(
            (
                [
                    (header::CONTENT_DISPOSITION, disposition),
                    (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                    (header::CACHE_CONTROL, "no-store".to_owned()),
                ],
                bytes,
            )
                .into_response(),
        )
    }
    .await;
    outcome.unwrap_or_else(|error| (StatusCode::NOT_FOUND, error.to_string()).into_response())
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
                let view = if record.server == "dar" {
                    let filename = record.result.pointer("/structuredContent/filename")
                        .and_then(Value::as_str).context("artifact filename unavailable")?;
                    let mut key = vec![session.as_str(), record.server.as_str(), record.resource_uri.as_str(), filename];
                    if state.config.apps.no_reuse.contains(&record.server) { key.push(&id); }
                    serde_json::to_string(&key)?
                } else if state.config.apps.no_reuse.contains(&record.server) {
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
                catalog(&state, &record.server)?.call_tool(&record.server, tool, args).await
            }
            "resources/read" => {
                let uri = params["uri"].as_str().context("resource URI required")?;
                if record.server == "dar" {
                    anyhow::ensure!(record.result.pointer("/structuredContent/uri").and_then(Value::as_str) == Some(uri), "artifact is not part of this view result");
                }
                catalog(&state, &record.server)?.read_resource(&record.server, uri).await
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
    let router = sandbox_router(state);
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

pub(super) fn sandbox_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/sandbox/{record}", get(proxy))
        .route("/content/{record}", get(content))
        .with_state(state)
}

#[derive(Deserialize)]
pub(super) struct ProxyQuery {
    session: String,
    ticket: String,
}

async fn prepared(
    state: &AppState,
    id: &str,
    query: &ProxyQuery,
    headers: &HeaderMap,
) -> Result<PreparedView> {
    let view = state
        .config
        .apps
        .tickets
        .lock()
        .expect("view tickets poisoned")
        .get(&query.ticket)
        .cloned()
        .context("view ticket unavailable")?;
    anyhow::ensure!(
        view.id == id
            && view.session == query.session
            && now_ms().saturating_sub(view.created_at) < 60 * 60 * 1000,
        "view ticket does not match this request"
    );
    let assigned = origin(&view.app_origin)?;
    anyhow::ensure!(
        matches_host(headers, &assigned),
        "view belongs to another sandbox hostname"
    );
    let current = record(state, &query.session, id).await?;
    anyhow::ensure!(
        current.server == view.record.server
            && current.resource_uri == view.record.resource_uri
            && current.tool == view.record.tool
            && current.args == view.record.args
            && current.result == view.record.result
            && current.created_at == view.record.created_at,
        "view record changed"
    );
    Ok(view)
}

fn html_response(outcome: Result<(String, String)>) -> Response {
    match outcome {
        Ok((csp, html)) => (
            [
                (header::CONTENT_SECURITY_POLICY, csp),
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

// Both pages use the same prepared resource snapshot. The wrapper and content
// share one untrusted server origin, while their policies serve different jobs.
async fn proxy(
    Path(id): Path<String>,
    Query(query): Query<ProxyQuery>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    html_response(async {
        let view = prepared(&state, &id, &query, &headers).await?;
        let mut content = url::Url::parse(&format!("{}/content/{id}", view.app_origin))?;
        content.query_pairs_mut().append_pair("session", &view.session).append_pair("ticket", &query.ticket);
        let html = include_str!("sandbox.html")
            .replace("__HOST_ORIGIN__", &serde_json::to_string(&view.chat_origin)?)
            .replace("__CONTENT_URL__", &serde_json::to_string(content.as_str())?);
        let csp = format!("default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; worker-src 'none'; frame-src {}; base-uri 'none'; object-src 'none'; form-action 'none'; frame-ancestors {};", view.app_origin, view.chat_origin);
        Ok((csp, html))
    }.await)
}

async fn content(
    Path(id): Path<String>,
    Query(query): Query<ProxyQuery>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    html_response(
        async {
            let view = prepared(&state, &id, &query, &headers).await?;
            let csp = format!(
                "{} frame-ancestors {} {};",
                view.csp, view.chat_origin, view.app_origin
            );
            tracing::info!(session=%view.session,server=%view.record.server,csp=%csp,"MCP App CSP");
            Ok((csp, view.html))
        }
        .await,
    )
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
    (format!("default-src 'none'; script-src 'self' 'unsafe-inline' blob: {resources}; style-src 'self' 'unsafe-inline' {resources}; img-src 'self' data: blob: {resources}; media-src 'self' data: {resources}; font-src 'self' {resources}; worker-src 'none'; connect-src {}; frame-src {}; base-uri {}; object-src 'none'; form-action 'none';",
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
            let mut descriptor = json!({"recordId":id,"tool":record.tool,"piToolName":record.pi_tool_name,"server":record.server,"resourceUri":record.resource_uri,"args":record.args,"callId":event.id});
            if record.server == "dar" {
                descriptor["artifactFilename"] =
                    record.result["structuredContent"]["filename"].clone();
                descriptor["artifactTitle"] = record.result["structuredContent"]["title"].clone();
            }
            event.app = Some(descriptor);
        }
    }
}
