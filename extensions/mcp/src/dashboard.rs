use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use cap_dashboard_tab::{escape_html, DashboardTab};
use rmcp::transport::{AuthorizationManager, CredentialStore};
use serde::Deserialize;

use crate::{
    credentials::FileCredentialStore,
    login::{begin_authorization, exchange_and_save},
    Config, Server,
};

/// Upper bound on concurrently pending dashboard authorizations.
const MAX_PENDING: usize = 16;

#[derive(Clone)]
pub(crate) struct DashboardState {
    root: PathBuf,
    config: Arc<Config>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    /// Per-server login generation, bumped by Disconnect. Only touched while
    /// holding `pending`, so a start that began before a Disconnect cannot
    /// insert afterwards.
    generations: Arc<Mutex<HashMap<String, u64>>>,
    /// Serializes code exchange against Disconnect so a completing Connect
    /// popup can never re-create credentials that Disconnect just cleared.
    ops: Arc<tokio::sync::Mutex<()>>,
}

struct Pending {
    created: Instant,
    generation: u64,
    server: String,
    manager: AuthorizationManager,
    store: FileCredentialStore,
}

impl DashboardState {
    pub(crate) fn new(root: PathBuf, config: Config) -> Self {
        Self {
            root,
            config: Arc::new(config),
            pending: Arc::new(Mutex::new(HashMap::new())),
            generations: Arc::new(Mutex::new(HashMap::new())),
            ops: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn oauth_server(&self, name: &str) -> Result<&str> {
        match self.config.mcp_servers.get(name) {
            Some(Server::Http { url, headers })
                if !headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("authorization")) =>
            {
                Ok(url)
            }
            Some(_) => anyhow::bail!("server does not support OAuth login"),
            None => anyhow::bail!("unknown MCP server"),
        }
    }

    fn generation(&self, server: &str) -> u64 {
        let _pending = self.pending.lock().expect("MCP pending state poisoned");
        self.generation_locked(server)
    }

    /// Caller must hold the `pending` lock.
    fn generation_locked(&self, server: &str) -> u64 {
        *self
            .generations
            .lock()
            .expect("MCP generation state poisoned")
            .get(server)
            .unwrap_or(&0)
    }

    fn prune(&self) {
        self.pending
            .lock()
            .expect("MCP pending state poisoned")
            .retain(|_, value| pending_is_fresh(value.created));
    }
}

fn pending_is_fresh(created: Instant) -> bool {
    created.elapsed() < Duration::from_secs(600)
}

pub(crate) struct McpTab {
    state: DashboardState,
}
impl McpTab {
    pub(crate) fn new(state: DashboardState) -> Self {
        Self { state }
    }
}

impl DashboardTab for McpTab {
    fn id(&self) -> &str {
        "mcp"
    }
    fn title(&self) -> &str {
        "MCP"
    }
    fn render(&self) -> Result<String> {
        render(&self.state)
    }
}

/// Tab-scoped styles; colors and `.panel`/`.pill` come from the dashboard shell.
const STYLE: &str = "<style>\
.mcp-table{width:100%;border-collapse:collapse}\
.mcp-table th{text-align:left;font-size:.72rem;color:var(--muted);text-transform:uppercase;letter-spacing:.06em;font-weight:600}\
.mcp-table th,.mcp-table td{padding:.7rem .9rem;border-bottom:1px solid var(--border)}\
.mcp-table tr:last-child td{border-bottom:none}\
.mcp-name{font-weight:600}.mcp-muted{color:var(--muted)}\
.mcp-actions{display:flex;gap:.5rem;justify-content:flex-end}\
.mcp-hint{color:var(--muted);font-size:.8rem;margin:.9rem 0 0}\
</style>";

/// Connect popup reuses the dashboard's fleet prefix; on success the popup
/// notifies us and we re-fetch this tab in place (no page reload, so the MCP
/// tab stays selected).
const SCRIPT: &str = "<script>\
window.mcpConnect=function(s){window.open((window.__dashPrefix||'')+'/mcp/oauth/start?server='+encodeURIComponent(s),'mcp-oauth','width=700,height=800')};\
if(!window.__mcpListener){window.__mcpListener=true;window.addEventListener('message',function(e){\
if(e.origin===location.origin&&e.data==='dar-mcp-connected')htmx.ajax('GET','/tabs/mcp',{target:'#content',swap:'innerHTML'})})}\
</script>";

fn render(state: &DashboardState) -> Result<String> {
    let mut html = format!(
        "{STYLE}<main><section class=\"panel\"><h2>MCP servers</h2><table class=\"mcp-table\"><thead><tr><th>Name</th><th>Transport</th><th>Status</th><th></th></tr></thead><tbody>"
    );
    for (name, server) in &state.config.mcp_servers {
        let (transport, status) = match server {
            Server::Stdio { .. } => ("stdio", "static"),
            Server::Http { headers, .. }
                if headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("authorization")) =>
            {
                ("http", "static")
            }
            Server::Http { .. } if credential_path(&state.root, name).exists() => {
                ("http", "connected")
            }
            Server::Http { .. } => ("http", "disconnected"),
        };
        let n = escape_html(name);
        let (pill, actions) = match status {
            "connected" => (
                "completed",
                format!(
                    r##"<button onclick="mcpConnect('{n}')">Reconnect</button><button class="danger" hx-post="/mcp/disconnect?server={n}" hx-target="#content">Disconnect</button>"##
                ),
            ),
            "disconnected" => (
                "interrupted",
                format!(r#"<button onclick="mcpConnect('{n}')">Connect</button>"#),
            ),
            _ => ("other", String::new()),
        };
        html.push_str(&format!(
            r#"<tr><td class="mcp-name">{n}</td><td class="mcp-muted">{transport}</td><td><span class="pill {pill}">{status}</span></td><td><div class="mcp-actions">{actions}</div></td></tr>"#
        ));
    }
    html.push_str("</tbody></table><p class=\"mcp-hint\">Runners pick up new logins the next time they start the MCP bridge.</p></section></main>");
    html.push_str(SCRIPT);
    Ok(html)
}

fn credential_path(root: &std::path::Path, name: &str) -> PathBuf {
    root.join("data/mcp-auth").join(format!("{name}.json"))
}

#[derive(Deserialize)]
struct ServerQuery {
    server: String,
}
#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
    iss: Option<String>,
}

pub(crate) fn router(state: DashboardState) -> Router {
    Router::new()
        .route("/oauth/start", get(start))
        .route("/oauth/callback", get(callback))
        .route("/disconnect", post(disconnect))
        .with_state(state)
}
pub(crate) fn routes() -> Vec<String> {
    vec![
        "/oauth/start".into(),
        "/oauth/callback".into(),
        "/disconnect".into(),
    ]
}

async fn start(
    State(state): State<DashboardState>,
    Query(query): Query<ServerQuery>,
    headers: HeaderMap,
) -> Response {
    match start_inner(&state, &query.server, &headers).await {
        Ok(url) => Redirect::temporary(url.as_str()).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Html(escape_html(&error.to_string())),
        )
            .into_response(),
    }
}

async fn start_inner(state: &DashboardState, name: &str, headers: &HeaderMap) -> Result<url::Url> {
    state.prune();
    let url = state.oauth_server(name)?.to_owned();
    let host = loopback_host(headers)?;
    // Only the dashboard itself may start a login (blocks cross-site slot
    // exhaustion). Browsers send Sec-Fetch-Site on every navigation.
    if !matches!(
        headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()),
        Some("same-origin")
    ) {
        anyhow::bail!("start MCP login from the dashboard MCP tab");
    }
    let generation = state.generation(name);
    if state
        .pending
        .lock()
        .expect("MCP pending state poisoned")
        .len()
        >= MAX_PENDING
    {
        anyhow::bail!("too many pending MCP authorizations; retry in a few minutes");
    }
    let redirect = format!("http://{host}/mcp/oauth/callback");
    let (manager, store, auth_url) =
        begin_authorization(&state.root, name, &url, &redirect).await?;
    let parsed_auth_url = url::Url::parse(&auth_url)?;
    let oauth_state = parsed_auth_url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .context("authorization URL missing state")?;
    let mut pending = state.pending.lock().expect("MCP pending state poisoned");
    if pending.len() >= MAX_PENDING {
        anyhow::bail!("too many pending MCP authorizations; retry in a few minutes");
    }
    if state.generation_locked(name) != generation {
        anyhow::bail!("server was disconnected while starting login; try again");
    }
    pending.insert(
        oauth_state,
        Pending {
            created: Instant::now(),
            generation,
            server: name.to_owned(),
            manager,
            store,
        },
    );
    Ok(parsed_auth_url)
}

async fn callback(
    State(state): State<DashboardState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    state.prune();
    let _ops = state.ops.lock().await;
    let pending = {
        let mut pending = state.pending.lock().expect("MCP pending state poisoned");
        pending
            .remove(&query.state)
            .filter(|p| p.generation == state.generation_locked(&p.server))
    };
    let Some(mut pending) = pending else {
        return (
            StatusCode::BAD_REQUEST,
            Html("Unknown or expired OAuth state".to_string()),
        )
            .into_response();
    };
    match exchange_and_save(&mut pending.manager, &pending.store, &query.code, &query.state, query.iss.as_deref()).await {
        Ok(()) => Html(format!("Connected {}. You can close this tab.<script>if(window.opener)window.opener.postMessage('dar-mcp-connected',location.origin);window.close()</script>", escape_html(&pending.server))).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Html(escape_html(&format!("Authorization failed: {error}")))).into_response(),
    }
}

async fn disconnect(
    State(state): State<DashboardState>,
    Query(query): Query<ServerQuery>,
    headers: HeaderMap,
) -> Response {
    let result = async {
        // CSRF: a cross-site form cannot set custom headers, and a cross-site
        // fetch with one fails the (unanswered) CORS preflight.
        if !headers.contains_key("hx-request") || !same_origin(&headers) {
            anyhow::bail!("cross-site request rejected");
        }
        state.oauth_server(&query.server)?;
        let _ops = state.ops.lock().await;
        // Invalidate in-flight Connect popups and starts still in discovery.
        {
            let mut pending = state.pending.lock().expect("MCP pending state poisoned");
            pending.retain(|_, p| p.server != query.server);
            *state
                .generations
                .lock()
                .expect("MCP generation state poisoned")
                .entry(query.server.clone())
                .or_default() += 1;
        }
        let store = FileCredentialStore::new(&state.root, &query.server)?;
        // Serialize with token refresh/exchange so a concurrent save can't undo this.
        let _guard = store
            .acquire_refresh_guard()
            .await?
            .context("credential lock unavailable")?;
        store.clear().await?;
        render(&state)
    }
    .await;
    match result {
        Ok(html) => Html(html).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Html(escape_html(&error.to_string())),
        )
            .into_response(),
    }
}

/// The request's Host, restricted to loopback: providers only accept plain-http
/// redirect URIs on loopback, and it keeps a spoofed Host from steering the
/// registered redirect elsewhere.
fn loopback_host(headers: &HeaderMap) -> Result<String> {
    let host = headers
        .get(header::HOST)
        .context("missing Host header")?
        .to_str()?;
    let authority: axum::http::uri::Authority = host.parse()?;
    if !matches!(authority.host(), "localhost" | "127.0.0.1" | "[::1]") {
        anyhow::bail!(
            "open the dashboard via http://127.0.0.1:<port> to connect, or run `dar mcp login <server>`"
        );
    }
    Ok(authority.to_string())
}

/// If the browser sent an Origin, it must match the Host we were addressed as.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin_authority = origin
        .to_str()
        .ok()
        .and_then(|o| url::Url::parse(o).ok())
        .map(|u| match u.port() {
            Some(port) => format!("{}:{port}", u.host_str().unwrap_or_default()),
            None => u.host_str().unwrap_or_default().to_owned(),
        });
    host.is_some() && origin_authority.as_deref() == host
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_status_and_escapes_rendered_names() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data/mcp-auth")).unwrap();
        std::fs::write(root.path().join("data/mcp-auth/connected.json"), "{}").unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({"mcpServers": {
            "local": {"command":"echo"},
            "header": {"url":"http://x", "headers":{"Authorization":"x"}},
            "connected": {"url":"http://x"},
            "evil<script": {"url":"http://x"}
        }}))
        .unwrap();
        let html = render(&DashboardState::new(root.path().to_path_buf(), config)).unwrap();
        assert!(html.contains(
            r#"local</td><td class="mcp-muted">stdio</td><td><span class="pill other">static"#
        ));
        assert!(html.contains(
            r#"header</td><td class="mcp-muted">http</td><td><span class="pill other">static"#
        ));
        assert!(html.contains(r#"connected</td><td class="mcp-muted">http</td><td><span class="pill completed">connected"#));
        assert!(html.contains(r#"evil&lt;script</td><td class="mcp-muted">http</td><td><span class="pill interrupted">disconnected"#));
        assert!(!html.contains("evil<script"));
    }

    #[tokio::test]
    async fn unknown_callback_state_is_bad_request_and_ttl_expires() {
        let root = tempfile::tempdir().unwrap();
        let config: Config =
            serde_json::from_value(serde_json::json!({"mcpServers":{"remote":{"url":"http://x"}}}))
                .unwrap();
        let state = DashboardState::new(root.path().to_path_buf(), config);
        let response = callback(
            State(state),
            Query(CallbackQuery {
                code: "x".into(),
                state: "missing".into(),
                iss: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!pending_is_fresh(Instant::now() - Duration::from_secs(601)));
    }

    #[tokio::test]
    async fn disconnect_clears_credentials() {
        use rmcp::transport::{CredentialStore, StoredCredentials};
        let root = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(root.path(), "remote").unwrap();
        let credentials: StoredCredentials = serde_json::from_value(serde_json::json!({"client_id":"id","token_response":null,"granted_scopes":[],"token_received_at":null,"issuer":null})).unwrap();
        store.save(credentials).await.unwrap();
        let config: Config =
            serde_json::from_value(serde_json::json!({"mcpServers":{"remote":{"url":"http://x"}}}))
                .unwrap();
        let state = DashboardState::new(root.path().to_path_buf(), config);
        insert_pending(&state, "s1", "remote", Instant::now()).await;
        let query = || {
            Query(ServerQuery {
                server: "remote".into(),
            })
        };

        // Cross-site: no htmx header, or a foreign Origin, is rejected.
        let forged = disconnect(
            State(state.clone()),
            query(),
            headers(&[("host", "127.0.0.1:7878")]),
        )
        .await;
        assert_eq!(forged.status(), StatusCode::BAD_REQUEST);
        let foreign = headers(&[
            ("host", "127.0.0.1:7878"),
            ("hx-request", "true"),
            ("origin", "http://evil.example"),
        ]);
        assert_eq!(
            disconnect(State(state.clone()), query(), foreign)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert!(store.load().await.unwrap().is_some());

        let ok = headers(&[
            ("host", "127.0.0.1:7878"),
            ("hx-request", "true"),
            ("origin", "http://127.0.0.1:7878"),
        ]);
        let response = disconnect(State(state.clone()), query(), ok).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(store.load().await.unwrap().is_none());
        // In-flight Connect popups for the server are invalidated too.
        assert!(state.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn expired_pending_is_pruned_and_pending_is_capped() {
        let root = tempfile::tempdir().unwrap();
        let config: Config =
            serde_json::from_value(serde_json::json!({"mcpServers":{"remote":{"url":"http://x"}}}))
                .unwrap();
        let state = DashboardState::new(root.path().to_path_buf(), config);
        insert_pending(
            &state,
            "old",
            "remote",
            Instant::now() - Duration::from_secs(601),
        )
        .await;
        state.prune();
        assert!(state.pending.lock().unwrap().is_empty());

        for i in 0..MAX_PENDING {
            insert_pending(&state, &i.to_string(), "remote", Instant::now()).await;
        }
        let same_site = headers(&[
            ("host", "127.0.0.1:7878"),
            ("sec-fetch-site", "same-origin"),
        ]);
        let error = start_inner(&state, "remote", &same_site).await.unwrap_err();
        assert!(error.to_string().contains("too many pending"), "{error}");
    }

    #[tokio::test]
    async fn cross_site_start_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let config: Config =
            serde_json::from_value(serde_json::json!({"mcpServers":{"remote":{"url":"http://x"}}}))
                .unwrap();
        let state = DashboardState::new(root.path().to_path_buf(), config);
        for site in [None, Some("cross-site"), Some("same-site")] {
            let mut h = headers(&[("host", "127.0.0.1:7878")]);
            if let Some(site) = site {
                h.insert("sec-fetch-site", site.parse().unwrap());
            }
            let error = start_inner(&state, "remote", &h).await.unwrap_err();
            assert!(error.to_string().contains("dashboard MCP tab"), "{error}");
        }
    }

    #[tokio::test]
    async fn callback_after_disconnect_cannot_resurrect_login() {
        let root = tempfile::tempdir().unwrap();
        let config: Config =
            serde_json::from_value(serde_json::json!({"mcpServers":{"remote":{"url":"http://x"}}}))
                .unwrap();
        let state = DashboardState::new(root.path().to_path_buf(), config);
        let stale = state.generation("remote");
        let ok = headers(&[("host", "127.0.0.1:7878"), ("hx-request", "true")]);
        let query = Query(ServerQuery {
            server: "remote".into(),
        });
        assert_eq!(
            disconnect(State(state.clone()), query, ok).await.status(),
            StatusCode::OK
        );

        // A popup whose start began before Disconnect is rejected at callback,
        // before any code exchange is attempted.
        insert_pending(&state, "late", "remote", Instant::now()).await;
        state
            .pending
            .lock()
            .unwrap()
            .get_mut("late")
            .unwrap()
            .generation = stale;
        let response = callback(
            State(state.clone()),
            Query(CallbackQuery {
                code: "c".into(),
                state: "late".into(),
                iss: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!credential_path(root.path(), "remote").exists());
    }

    #[test]
    fn redirect_host_must_be_loopback() {
        assert_eq!(
            loopback_host(&headers(&[("host", "127.0.0.1:7878")])).unwrap(),
            "127.0.0.1:7878"
        );
        assert_eq!(
            loopback_host(&headers(&[("host", "localhost:7878")])).unwrap(),
            "localhost:7878"
        );
        assert!(loopback_host(&headers(&[("host", "evil.example:7878")])).is_err());
        assert!(loopback_host(&headers(&[("host", "127.0.0.1.evil.example")])).is_err());
        assert!(loopback_host(&headers(&[])).is_err());
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    async fn insert_pending(state: &DashboardState, key: &str, server: &str, created: Instant) {
        let pending = Pending {
            created,
            generation: state.generation(server),
            server: server.into(),
            manager: AuthorizationManager::new("http://127.0.0.1:9/mcp")
                .await
                .unwrap(),
            store: FileCredentialStore::new(&state.root, server).unwrap(),
        };
        state.pending.lock().unwrap().insert(key.into(), pending);
    }
}
