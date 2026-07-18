//! `dar dash` — the fleet aggregator.
//!
//! A small, stateless HTTP server that presents one unified view over every
//! live agent dashboard on this host. It owns no cross-process state: each
//! request rebuilds entirely from the presence registry (read + prune dead).
//!
//! The shell exposes discovery endpoints plus a streaming reverse proxy for
//! each live agent.  Browser requests to `/agent/<port>/...` stay same-origin
//! with the fleet shell while this process forwards them to loopback dashboards.
//!
//! Endpoints:
//! * `GET /api/agents` — machine-readable JSON list of live agents.
//! * `GET /` — a sidebar SPA whose content area is an agent iframe.
//! * `/agent/{port}` and `/agent/{port}/{*rest}` — streaming loopback proxy
//!   routes. HTML responses have root-absolute dashboard URLs rewritten for
//!   the proxy prefix; non-HTML responses stream unchanged.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{OriginalUri, Path as AxumPath, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;

use dar_presence::{PresenceEntry, Registry};
use regex::Regex;

/// Options for the aggregator server.
pub struct DashOptions {
    pub bind: IpAddr,
    pub port: u16,
    pub registry_dir: PathBuf,
}

impl DashOptions {
    pub fn resolve(bind: Option<IpAddr>, port: Option<u16>, registry_dir: Option<PathBuf>) -> Self {
        Self {
            bind: bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            port: port.unwrap_or(7878),
            registry_dir: registry_dir.unwrap_or_else(dar_presence::default_registry_dir),
        }
    }
}

#[derive(Clone)]
struct DashState {
    registry: Registry,
    client: reqwest::Client,
}

/// Boot the aggregator and serve until Ctrl-C.
pub async fn serve(opts: DashOptions) -> Result<()> {
    let state = DashState {
        registry: Registry::new(&opts.registry_dir),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            // reqwest 0.12 accepts a Duration rather than `None`; this is
            // effectively unbounded while preserving long-lived SSE streams.
            .timeout(Duration::from_secs(60 * 60 * 24 * 365 * 100))
            .build()
            .context("building dash proxy client")?,
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/agents", get(api_agents))
        .route("/agent/{port}", any(proxy_agent))
        .route("/agent/{port}/", any(proxy_agent))
        .route("/agent/{port}/{*rest}", any(proxy_agent))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((opts.bind, opts.port))
        .await
        .with_context(|| format!("binding dar dash on {}:{}", opts.bind, opts.port))?;
    let local = listener.local_addr().ok();
    if let Some(addr) = local {
        println!(
            "dar dash listening on {} (browse http://{}:{}/) (registry {})",
            addr,
            display_host(addr.ip()),
            addr.port(),
            opts.registry_dir.display()
        );
    }
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("dar dash server")
}

fn display_host(ip: IpAddr) -> String {
    if ip.is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        ip.to_string()
    }
}

/// `GET /api/agents` — JSON list of live agents (dead pids pruned on read).
async fn api_agents(State(state): State<DashState>) -> Response {
    let agents = state.registry.read_live();
    let agents: Vec<_> = agents
        .iter()
        .map(|entry| {
            serde_json::json!({
                "id": entry.id,
                "label": agent_label(entry),
                "folder": entry.folder,
                "workflow": entry.workflow,
                "addr": entry.addr,
                "pid": entry.pid,
                "started_at": entry.started_at,
            })
        })
        .collect();
    let body = serde_json::json!({ "agents": agents });
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// `GET /` — sidebar + iframe shell over the live agents.
async fn index(State(state): State<DashState>) -> Response {
    let agents = state.registry.read_live();
    match render_shell(&agents) {
        Ok(html) => Html(html).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response(),
    }
}

fn agent_url(entry: &PresenceEntry) -> String {
    entry
        .port()
        .map(|port| format!("/agent/{port}/"))
        .unwrap_or_default()
}

async fn proxy_agent(
    State(state): State<DashState>,
    AxumPath(params): AxumPath<std::collections::HashMap<String, String>>,
    OriginalUri(original_uri): OriginalUri,
    request: Request<Body>,
) -> Response {
    let Some(port) = params.get("port") else {
        return (StatusCode::BAD_REQUEST, "invalid agent port").into_response();
    };
    let Ok(port_number) = port.parse::<u32>() else {
        return (StatusCode::BAD_REQUEST, "invalid agent port").into_response();
    };
    let Ok(port) = u16::try_from(port_number) else {
        return (StatusCode::BAD_REQUEST, "invalid agent port").into_response();
    };
    if !state
        .registry
        .read_live()
        .iter()
        .any(|entry| entry.port() == Some(port))
    {
        return (StatusCode::NOT_FOUND, "agent dashboard not found").into_response();
    }

    let path = original_uri.path();
    let Some(raw_port) = path
        .strip_prefix("/agent/")
        .and_then(|path| path.split('/').next())
    else {
        return (StatusCode::BAD_REQUEST, "invalid agent path").into_response();
    };
    let prefix = format!("/agent/{raw_port}");
    let Some(rest) = path.strip_prefix(&prefix) else {
        return (StatusCode::BAD_REQUEST, "invalid agent path").into_response();
    };
    let mut upstream_path = if rest.is_empty() {
        "/".to_string()
    } else {
        rest.to_string()
    };
    if let Some(query) = original_uri.query() {
        upstream_path.push('?');
        upstream_path.push_str(query);
    }
    let upstream = format!("http://127.0.0.1:{port}{upstream_path}");
    let original_host = request.headers().get(header::HOST).cloned();
    let (parts, body) = request.into_parts();
    let mut headers = forwarded_headers(&parts.headers);
    headers.insert(
        header::HOST,
        HeaderValue::from_str(&format!("127.0.0.1:{port}")).unwrap(),
    );
    if let Some(host) = &original_host {
        headers.insert(HeaderName::from_static("x-forwarded-host"), host.clone());
    }
    let upstream = match state
        .client
        .request(parts.method, upstream)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return (StatusCode::BAD_GATEWAY, "agent dashboard unavailable").into_response(),
    };
    proxy_response(upstream, port, original_host).await
}

fn forwarded_headers(headers: &HeaderMap) -> HeaderMap {
    let mut result = HeaderMap::new();
    let connection_tokens = connection_tokens(headers);
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if is_forwarded_header(&lower)
            && !is_hop_by_hop(&lower)
            && !connection_tokens.iter().any(|token| token == &lower)
            && name != header::HOST
        {
            result.append(name.clone(), value.clone());
        }
    }
    result
}

fn is_forwarded_header(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "accept-language"
            | "content-type"
            | "content-length"
            | "user-agent"
            | "if-modified-since"
            | "if-none-match"
            | "cache-control"
            | "cookie"
    ) || name.starts_with("hx-")
}

fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect()
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "proxy-authenticate"
            | "proxy-authorization"
    )
}

async fn proxy_response(
    response: reqwest::Response,
    port: u16,
    original_host: Option<HeaderValue>,
) -> Response {
    let status = response.status();
    let is_html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.to_ascii_lowercase().starts_with("text/html"));
    let connection_tokens = connection_tokens(response.headers());
    let mut builder = Response::builder().status(status);
    for (name, value) in response.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if !is_hop_by_hop(&lower)
            && !connection_tokens.iter().any(|token| token == &lower)
            && (!is_html || (name != header::CONTENT_LENGTH && name != header::CONTENT_ENCODING))
        {
            builder = builder.header(name, value);
        }
    }
    if let Some(host) = original_host {
        builder = builder.header(HeaderName::from_static("x-forwarded-host"), host);
    }
    if is_html {
        let body = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(_) => {
                return (StatusCode::BAD_GATEWAY, "agent dashboard response failed").into_response()
            }
        };
        let body = rewrite_html(&String::from_utf8_lossy(&body), port);
        builder
            .header(header::CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .unwrap()
    } else {
        builder
            .body(Body::from_stream(response.bytes_stream()))
            .unwrap()
    }
}

// This is deliberately heuristic: it rewrites dar's HTML attributes and inline
// JavaScript string literals, not arbitrary JavaScript syntax.
fn rewrite_html(html: &str, port: u16) -> String {
    let prefix = format!("/agent/{port}");
    let protected = Regex::new(r"(?is)<!--.*?-->|<style\b[^>]*>.*?</style>").unwrap();
    let mut rewritten = String::with_capacity(html.len());
    let mut end = 0;
    for block in protected.find_iter(html) {
        rewritten.push_str(&rewrite_html_fragment(&html[end..block.start()], &prefix));
        rewritten.push_str(block.as_str());
        end = block.end();
    }
    rewritten.push_str(&rewrite_html_fragment(&html[end..], &prefix));
    rewritten
}

fn rewrite_html_fragment(html: &str, prefix: &str) -> String {
    let attrs = Regex::new(r#"(?i)((?:^|[\s<])(?:href|src|action|formaction|poster|data-src|data-href|hx-get|hx-post|hx-put|hx-patch|hx-delete|data-tab-url)\s*=\s*[\"'])(/[^/\"'][^\"']*)"#).unwrap();
    let rewritten = attrs.replace_all(html, |captures: &regex::Captures<'_>| {
        format!("{}{}{}", &captures[1], prefix, &captures[2])
    });
    let scripts = Regex::new(r"(?is)(<script\b[^>]*>)(.*?)(</script>)").unwrap();
    let strings = Regex::new(r#"(?:\"(/[^/\"][^\"]*)\"|'(/[^/'][^']*)'|(^|[=(,:;\s])`(/[^/`][^`]*)`)"#).unwrap();
    let double_quoted_handlers =
        Regex::new(r#"(?is)((?:^|[\s<])on[\w:-]*\s*=\s*\")(.*?)\""#).unwrap();
    let single_quoted_handlers = Regex::new(r"(?is)((?:^|[\s<])on[\w:-]*\s*=\s*')(.*?)'").unwrap();
    let rewrite_strings = |code: &str| {
        strings
            .replace_all(code, |string: &regex::Captures<'_>| {
                if let Some(path) = string.get(1) {
                    format!("\"{prefix}{}\"", path.as_str())
                } else if let Some(path) = string.get(2) {
                    format!("'{prefix}{}'", path.as_str())
                } else {
                    format!("{}{}{}{}{}", &string[3], '`', prefix, &string[4], '`')
                }
            })
            .into_owned()
    };
    let rewritten = scripts.replace_all(&rewritten, |captures: &regex::Captures<'_>| {
        format!(
            "{}{}{}",
            &captures[1],
            rewrite_strings(&captures[2]),
            &captures[3]
        )
    });
    let rewritten =
        double_quoted_handlers.replace_all(&rewritten, |captures: &regex::Captures<'_>| {
            format!("{}{}\"", &captures[1], rewrite_strings(&captures[2]))
        });
    single_quoted_handlers
        .replace_all(&rewritten, |captures: &regex::Captures<'_>| {
            format!("{}{}'", &captures[1], rewrite_strings(&captures[2]))
        })
        .into_owned()
}

/// `id · <workflow-dir basename>-<path hash>`, or plain `id` when the
/// workflow's directory is the agent folder (the default workflow).
fn agent_label(entry: &PresenceEntry) -> String {
    let Some(workflow) = entry.workflow.as_deref() else {
        return entry.id.clone();
    };
    let folder = Path::new(&entry.folder);
    let wf_dir = Path::new(workflow).parent();
    match wf_dir {
        Some(dir) if dir != folder => {
            let base = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if base.is_empty() {
                entry.id.clone()
            } else {
                let file_name = entry.file_name();
                let hash = file_name
                    .strip_suffix(".json")
                    .and_then(|name| name.rsplit_once('-'))
                    .map(|(_, hash)| hash)
                    .unwrap_or("workflow");
                format!("{} \u{b7} {}-{hash}", entry.id, base)
            }
        }
        _ => entry.id.clone(),
    }
}

fn render_shell(agents: &[PresenceEntry]) -> Result<String> {
    let mut items = String::new();
    let mut first_url = String::new();
    if agents.is_empty() {
        items.push_str("<li class=\"empty\">No live agents</li>");
    }
    for a in agents {
        let url = agent_url(a);
        if url.is_empty() {
            continue;
        }
        if first_url.is_empty() {
            first_url = url.clone();
        }
        let folder = a.folder.rsplit('/').next().unwrap_or(&a.folder);
        items.push_str(&format!(
            "<li><button class=\"agent\" data-src=\"{url}\" onclick=\"pick(this)\">\
             <span class=\"aid\">{id}</span>\
             <span class=\"afolder\">{folder}</span></button></li>",
            url = he(&url),
            id = he(&agent_label(a)),
            folder = he(folder),
        ));
    }
    let initial = if first_url.is_empty() {
        "about:blank".to_string()
    } else {
        first_url
    };
    Ok(format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>dar fleet</title>
<style>
  :root {{ color-scheme: dark light; }}
  * {{ box-sizing: border-box; }}
  html, body {{ height: 100%; margin: 0; font-family: ui-sans-serif, system-ui, sans-serif; }}
  .layout {{ display: grid; grid-template-columns: 260px 1fr; height: 100%; }}
  .sidebar {{ border-right: 1px solid #8884; overflow-y: auto; padding: 8px; }}
  .sidebar h1 {{ font-size: 13px; text-transform: uppercase; letter-spacing: .08em; opacity: .6; padding: 8px; margin: 0; }}
  .sidebar ul {{ list-style: none; margin: 0; padding: 0; }}
  .sidebar li {{ margin: 2px 0; }}
  .agent {{ width: 100%; text-align: left; background: none; border: 0; border-radius: 6px; padding: 8px; cursor: pointer; display: flex; flex-direction: column; gap: 2px; color: inherit; }}
  .agent:hover {{ background: #8882; }}
  .agent.active {{ background: #4a90d9; color: #fff; }}
  .aid {{ font-weight: 600; font-size: 13px; }}
  .afolder {{ font-size: 11px; opacity: .7; }}
  .empty {{ padding: 8px; opacity: .5; font-size: 13px; }}
  .content {{ height: 100%; }}
  .content iframe {{ width: 100%; height: 100%; border: 0; }}
</style>
</head>
<body>
<div class="layout">
  <nav class="sidebar">
    <h1>Agents</h1>
    <ul id="agents">{items}</ul>
  </nav>
  <main class="content">
    <iframe id="view" src="{initial}"></iframe>
  </main>
</div>
<script>
  function pick(btn) {{
    document.querySelectorAll('.agent').forEach(function (b) {{ b.classList.remove('active'); }});
    btn.classList.add('active');
    document.getElementById('view').src = btn.dataset.src;
  }}
  // Mark the first agent active to match the initial iframe src.
  var first = document.querySelector('.agent');
  if (first) {{ first.classList.add('active'); }}
  // Refresh the agent list periodically so dead agents drop and new ones appear.
  async function refresh() {{
    try {{
      var res = await fetch('/api/agents');
      if (!res.ok) return;
      var data = await res.json();
      var current = document.querySelector('.agent.active');
      var currentSrc = current ? current.dataset.src : null;
      var ul = document.getElementById('agents');
      ul.innerHTML = '';
      if (!data.agents.length) {{
        ul.innerHTML = '<li class="empty">No live agents</li>';
        return;
      }}
      data.agents.forEach(function (a) {{
        var port = (a.addr || '').split(':').pop();
        if (!/^\d+$/.test(port) || Number(port) > 65535) return;
        var url = '/agent/' + port + '/';
        var li = document.createElement('li');
        var b = document.createElement('button');
        b.className = 'agent';
        b.dataset.src = url;
        b.onclick = function () {{ pick(b); }};
        var folder = (a.folder || '').split('/').pop();
        b.innerHTML = '<span class="aid"></span><span class="afolder"></span>';
        b.querySelector('.aid').textContent = a.label;
        b.querySelector('.afolder').textContent = folder;
        if (url === currentSrc) b.classList.add('active');
        li.appendChild(b);
        ul.appendChild(li);
      }});
      if (!document.querySelector('.agent.active')) {{
        var f = document.querySelector('.agent');
        if (f) {{ f.classList.add('active'); document.getElementById('view').src = f.dataset.src; }}
      }}
    }} catch (e) {{}}
  }}
  setInterval(refresh, 5000);
</script>
</body>
</html>"#,
        items = items,
        initial = he(&initial),
    ))
}

/// Minimal HTML-attribute/text escaping.
fn he(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    fn state(registry: Registry) -> DashState {
        DashState {
            registry,
            client: reqwest::Client::new(),
        }
    }

    fn entry(id: &str, folder: &str, addr: &str, pid: u32) -> PresenceEntry {
        entry_wf(id, folder, &format!("{folder}/WORKFLOW.md"), addr, pid)
    }

    fn entry_wf(id: &str, folder: &str, workflow: &str, addr: &str, pid: u32) -> PresenceEntry {
        PresenceEntry {
            id: id.to_string(),
            folder: folder.to_string(),
            workflow: Some(workflow.to_string()),
            addr: addr.to_string(),
            pid,
            started_at: 0,
        }
    }

    #[test]
    fn agent_url_uses_proxy_path() {
        let e = entry("ALG-1", "/agents/a", "0.0.0.0:53124", 1);
        assert_eq!(agent_url(&e), "/agent/53124/");
        let e = entry("ALG-2", "/agents/b", "not-an-address", 1);
        assert_eq!(agent_url(&e), "");
    }

    #[test]
    fn agent_label_is_plain_id_for_default_workflow() {
        let e = entry("ALG-1", "/agents/one", "0.0.0.0:1", 1);
        assert_eq!(agent_label(&e), "ALG-1");
    }

    #[test]
    fn agent_label_is_plain_id_without_workflow() {
        let mut e = entry("ALG-1", "/agents/one", "0.0.0.0:1", 1);
        e.workflow = None;
        assert_eq!(agent_label(&e), "ALG-1");
    }

    #[test]
    fn agent_label_appends_workflow_dir_basename_for_external_workflow() {
        let e = entry_wf(
            "ALG-1",
            "/agents/one",
            "/tmp/wf-a/WORKFLOW.md",
            "0.0.0.0:1",
            1,
        );
        assert!(agent_label(&e).starts_with("ALG-1 \u{b7} wf-a-"));
    }

    #[test]
    fn render_lists_live_agents() {
        let agents = vec![
            entry("ALG-1", "/agents/one", "0.0.0.0:50001", 1),
            entry("ALG-2", "/agents/two", "0.0.0.0:50002", 2),
        ];
        let html = render_shell(&agents).unwrap();
        assert!(html.contains("ALG-1"));
        assert!(html.contains("ALG-2"));
        assert!(html.contains("/agent/50001/"));
        assert!(html.contains("/agent/50002/"));
        // First agent's dashboard is loaded by default.
        assert!(html.contains("src=\"/agent/50001/\""));
    }

    #[test]
    fn render_labels_external_workflow_entry() {
        let agents = vec![entry_wf(
            "ALG-1",
            "/agents/one",
            "/tmp/wf-a/WORKFLOW.md",
            "0.0.0.0:50001",
            1,
        )];
        let html = render_shell(&agents).unwrap();
        assert!(html.contains("ALG-1 \u{b7} wf-a-"));
    }

    #[test]
    fn labels_distinguish_workflow_dirs_with_same_basename() {
        let a = entry_wf(
            "worker",
            "/agents/worker",
            "/projects/one/worker/WORKFLOW.md",
            "0.0.0.0:50001",
            1,
        );
        let b = entry_wf(
            "worker",
            "/agents/worker",
            "/projects/two/worker/WORKFLOW.md",
            "0.0.0.0:50002",
            2,
        );
        let a_label = agent_label(&a);
        let b_label = agent_label(&b);
        assert_ne!(a_label, b_label);

        let html = render_shell(&[a, b]).unwrap();
        assert!(html.contains(&a_label));
        assert!(html.contains(&b_label));
    }

    #[test]
    fn render_empty_when_no_agents() {
        let html = render_shell(&[]).unwrap();
        assert!(html.contains("No live agents"));
        assert!(html.contains("about:blank"));
    }

    #[test]
    fn render_escapes_ids() {
        let agents = vec![entry("<script>", "/a", "0.0.0.0:1", 1)];
        let html = render_shell(&agents).unwrap();
        assert!(!html.contains("<script>x"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn rewrite_html_prefixes_only_root_absolute_dashboard_urls() {
        let html = r#"<script>
  app.es = new EventSource(`/chat/${SESSION}/stream`);
  fetch(`/chat/${SESSION}/send`, { method: 'POST' });
  fetch(`/static/no-interp`);
  fetch(`//cdn.example/x`);
  const markdown = s => s.replace(/```x```/g, () => `ok`);
</script><script src="/assets/x.js"></script><button hx-post="/control/pause" onclick="fetch('/run-now')"></button><button onclick="fetch('/scheduler/jobs/abc/run-now')"></button><button onclick='fetch("/scheduler/jobs/abc/run-now")'></button><script>fetch('/content'); new EventSource('/events')</script><a href="https://example.com/x"></a><img src="//cdn.example/x"><a href="page.html"></a><a href="/"></a><div foo-href="/not-an-attribute"></div><style>.x { background: url('/style') }</style><!-- <a href="/comment"> -->"#;
        let rewritten = rewrite_html(html, 50123);
        assert!(rewritten.contains("EventSource(`/agent/50123/chat/${SESSION}/stream`)"));
        assert!(rewritten.contains("fetch(`/agent/50123/chat/${SESSION}/send`"));
        assert!(rewritten.contains("fetch(`/agent/50123/static/no-interp`)"));
        assert!(rewritten.contains("`//cdn.example/x`"));
        assert!(rewritten.contains("/```x```/g"));
        assert!(rewritten.contains("src=\"/agent/50123/assets/x.js\""));
        assert!(rewritten.contains("hx-post=\"/agent/50123/control/pause\""));
        assert!(rewritten.contains("fetch('/agent/50123/content')"));
        assert!(rewritten.contains("onclick=\"fetch('/agent/50123/run-now')\""));
        assert!(rewritten.contains("onclick=\"fetch('/agent/50123/scheduler/jobs/abc/run-now')\""));
        assert!(rewritten.contains("onclick='fetch(\"/agent/50123/scheduler/jobs/abc/run-now\")'"));
        assert!(rewritten.contains("EventSource('/agent/50123/events')"));
        assert!(rewritten.contains("https://example.com/x"));
        assert!(rewritten.contains("//cdn.example/x"));
        assert!(rewritten.contains("href=\"page.html\""));
        assert!(rewritten.contains("href=\"/\""), "{rewritten}");
        assert!(rewritten.contains("foo-href=\"/not-an-attribute\""));
        assert!(rewritten.contains("url('/style')"));
        assert!(rewritten.contains("href=\"/comment\""));
    }

    #[test]
    fn forwarded_headers_use_allowlist_and_connection_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static("text/html"));
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        headers.insert(header::COOKIE, HeaderValue::from_static("session=ok"));
        headers.insert(
            HeaderName::from_static("hx-request"),
            HeaderValue::from_static("true"),
        );
        headers.insert(header::CONNECTION, HeaderValue::from_static("x-drop"));
        headers.insert(
            HeaderName::from_static("x-drop"),
            HeaderValue::from_static("no"),
        );

        let forwarded = forwarded_headers(&headers);
        assert_eq!(forwarded[header::ACCEPT], "text/html");
        assert_eq!(forwarded[header::COOKIE], "session=ok");
        assert_eq!(forwarded["hx-request"], "true");
        assert!(forwarded.get(header::ACCEPT_ENCODING).is_none());
        assert!(forwarded.get(header::AUTHORIZATION).is_none());
        assert!(forwarded.get("x-drop").is_none());
    }

    #[tokio::test]
    async fn proxy_routes_methods_query_headers_and_html_rewriting() {
        async fn upstream(
            method: axum::http::Method,
            uri: axum::http::Uri,
            headers: HeaderMap,
            body: String,
        ) -> Response {
            if uri.path() == "/html" {
                return ([(header::CONTENT_TYPE, "text/html"), (header::CONTENT_ENCODING, "gzip")], "<script src=\"/assets/x.js\"></script><button hx-post=\"/control/pause\" onclick=\"fetch('/run-now')\"></button><a href=\"/\"></a><script>fetch('/content')</script>").into_response();
            }
            if uri.path() == "/json" {
                return (
                    [(header::CONTENT_TYPE, "application/json")],
                    "{\"exact\":true}",
                )
                    .into_response();
            }
            (
                StatusCode::CREATED,
                [
                    ("x-method", method.as_str().to_string()),
                    ("x-query", uri.query().unwrap_or_default().to_string()),
                    ("x-path", uri.path().to_string()),
                    (
                        "x-host",
                        headers
                            .get(header::HOST)
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_string(),
                    ),
                    (
                        "x-authorization",
                        headers
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    ("x-body", body),
                    ("connection", "x-upstream-remove".to_string()),
                    ("x-upstream-remove", "nope".to_string()),
                ],
                "ok",
            )
                .into_response()
        }
        let upstream = Router::new()
            .route("/", any(upstream))
            .route("/{*rest}", any(upstream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        registry
            .write(&entry(
                "agent",
                "/agent",
                &format!("0.0.0.0:{port}"),
                std::process::id(),
            ))
            .unwrap();
        let app = Router::new()
            .route("/", get(index))
            .route("/agent/{port}", any(proxy_agent))
            .route("/agent/{port}/", any(proxy_agent))
            .route("/agent/{port}/{*rest}", any(proxy_agent))
            .with_state(state(registry));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/agent/{port}/echo?x=%2Fok"))
                    .header(header::CONTENT_TYPE, "text/plain")
                    .header(header::HOST, "fleet.test")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::from("hello"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let response_headers = response.headers().clone();
        let response_body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&response_body)
        );
        assert_eq!(response_headers["x-method"], "PATCH");
        assert_eq!(response_headers["x-query"], "x=%2Fok");
        assert_eq!(response_headers["x-host"], format!("127.0.0.1:{port}"));
        assert_eq!(response_headers["x-body"], "hello");
        assert_eq!(response_headers["x-authorization"], "");
        assert!(response_headers.get(header::CONNECTION).is_none());
        assert!(response_headers.get("x-upstream-remove").is_none());
        for method in ["GET", "POST", "DELETE"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("/agent/{port}/echo"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(response.headers()["x-method"], method);
        }
        for suffix in ["", "/"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/agent/{port}{suffix}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED, "suffix {suffix}");
            assert_eq!(response.headers()["x-path"], "/");
        }
        let leading_zero = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/agent/0{port}/echo"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(leading_zero.headers()["x-path"], "/echo");
        let invalid = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/abc/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let invalid = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/99999/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/12345/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let html = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/agent/{port}/html"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(html.headers().get(header::CONTENT_ENCODING).is_none());
        let body = axum::body::to_bytes(html.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains(&format!("/agent/{port}/control/pause")));
        assert!(html.contains(&format!("onclick=\"fetch('/agent/{port}/run-now')\"")));
        assert!(html.contains("href=\"/\""));
        let json = app
            .oneshot(
                Request::builder()
                    .uri(format!("/agent/{port}/json"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let json = axum::body::to_bytes(json.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&json[..], br#"{"exact":true}"#);
    }

    #[tokio::test]
    async fn proxy_returns_bad_gateway_for_unreachable_registered_agent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        registry
            .write(&entry(
                "agent",
                "/agent",
                &format!("0.0.0.0:{port}"),
                std::process::id(),
            ))
            .unwrap();
        let app = Router::new()
            .route("/agent/{port}", any(proxy_agent))
            .route("/agent/{port}/", any(proxy_agent))
            .route("/agent/{port}/{*rest}", any(proxy_agent))
            .with_state(state(registry));
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/agent/{port}/"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn api_agents_returns_only_live() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        let mut live = entry(
            "ALG-LIVE",
            "/agents/live",
            "0.0.0.0:51000",
            std::process::id(),
        );
        live.workflow = None;
        reg.write(&live).unwrap();
        reg.write(&entry("ALG-DEAD", "/agents/dead", "0.0.0.0:51001", 999_999))
            .unwrap();
        let state = state(reg);
        let resp = api_agents(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let agents = json["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["id"], "ALG-LIVE");
        assert_eq!(agents[0]["label"], "ALG-LIVE");
        assert!(agents[0]["workflow"].is_null());
    }

    #[tokio::test]
    async fn api_agents_labels_same_named_workflows_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        for (workflow, port) in [
            ("/projects/one/worker/WORKFLOW.md", 51000),
            ("/projects/two/worker/WORKFLOW.md", 51001),
        ] {
            reg.write(&entry_wf(
                "worker",
                "/agents/worker",
                workflow,
                &format!("0.0.0.0:{port}"),
                std::process::id(),
            ))
            .unwrap();
        }
        let resp = api_agents(State(state(reg))).await;
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let agents = json["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2);
        assert_ne!(agents[0]["label"], agents[1]["label"]);
    }

    #[tokio::test]
    async fn index_renders_seeded_registry() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::new(dir.path());
        reg.write(&entry(
            "ALG-X",
            "/agents/x",
            "0.0.0.0:52000",
            std::process::id(),
        ))
        .unwrap();
        let state = state(reg);
        let resp = index(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("ALG-X"));
        assert!(html.contains("/agent/52000/"));
    }
}
