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
//!   routes. HTML responses that do not declare `x-prefix-aware` have
//!   root-absolute dashboard URLs rewritten for the proxy prefix; prefix-aware
//!   and non-HTML responses stream unchanged.

mod proxy;
mod rewrite;

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;

use dar_presence::{PresenceEntry, Registry};

use proxy::LivePorts;

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
    live_ports: Arc<LivePorts>,
}

/// Boot the aggregator and serve until Ctrl-C.
pub async fn serve(opts: DashOptions) -> Result<()> {
    let state = DashState {
        registry: Registry::new(&opts.registry_dir),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            // No total timeout: reqwest has none by default, and proxied SSE
            // streams must stay open indefinitely.
            .build()
            .context("building dash proxy client")?,
        live_ports: Arc::new(LivePorts::new()),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/agents", get(api_agents))
        .route("/agent/{port}", any(proxy::proxy_agent))
        .route("/agent/{port}/", any(proxy::proxy_agent))
        .route("/agent/{port}/{*rest}", any(proxy::proxy_agent))
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

fn agent_url(entry: &PresenceEntry) -> Option<String> {
    entry.port().map(|port| format!("/agent/{port}/"))
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
        // No parseable port means the agent isn't proxyable; skip its entry.
        let Some(url) = agent_url(a) else {
            continue;
        };
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
mod test_support {
    use super::*;

    pub(super) fn state(registry: Registry) -> DashState {
        DashState {
            registry,
            client: reqwest::Client::new(),
            live_ports: Arc::new(LivePorts::new()),
        }
    }

    pub(super) fn entry(id: &str, folder: &str, addr: &str, pid: u32) -> PresenceEntry {
        entry_wf(id, folder, &format!("{folder}/WORKFLOW.md"), addr, pid)
    }

    pub(super) fn entry_wf(
        id: &str,
        folder: &str,
        workflow: &str,
        addr: &str,
        pid: u32,
    ) -> PresenceEntry {
        PresenceEntry {
            id: id.to_string(),
            folder: folder.to_string(),
            workflow: Some(workflow.to_string()),
            addr: addr.to_string(),
            pid,
            started_at: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{entry, entry_wf, state};
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn agent_url_uses_proxy_path() {
        let e = entry("ALG-1", "/agents/a", "0.0.0.0:53124", 1);
        assert_eq!(agent_url(&e).as_deref(), Some("/agent/53124/"));
        let e = entry("ALG-2", "/agents/b", "not-an-address", 1);
        assert_eq!(agent_url(&e), None);
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
    fn render_skips_entries_without_parseable_port() {
        let agents = vec![entry("ALG-BAD", "/agents/bad", "not-an-address", 1)];
        let html = render_shell(&agents).unwrap();
        assert!(!html.contains("data-src=\"\""));
        assert!(!html.contains("ALG-BAD"));
    }

    #[test]
    fn render_escapes_ids() {
        let agents = vec![entry("<script>", "/a", "0.0.0.0:1", 1)];
        let html = render_shell(&agents).unwrap();
        assert!(!html.contains("<script>x"));
        assert!(html.contains("&lt;script&gt;"));
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
