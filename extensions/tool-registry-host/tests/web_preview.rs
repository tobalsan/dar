//! ALG-537 Testing Decisions approve registry/result transport: HTTP policy,
//! text, and persisted records; browser framing is covered separately by E2E.
use std::sync::Arc;

use dar_extension_sdk::mcp_apps::{extract_marker, read_result, ResultRecord};
use host_api::{Extension, RegisterCtx};
use serde_json::json;
use tool_registry::{ToolOutcome, ToolRegistryHandle, TOOL_REGISTRY_SERVICE};
use tool_registry_host::ToolRegistryHostExtension;

async fn registry(root: &std::path::Path) -> (Arc<dyn ToolRegistryHandle>, tempfile::TempDir) {
    let vault = tempfile::tempdir().unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let mut ctx = RegisterCtx {
        bus: Default::default(),
        http: host_api::HttpRegistry::disabled(),
        foreground: Default::default(),
        services: Default::default(),
        paths: host_api::HostPaths::new(root)
            .unwrap()
            .with_artifact_root(vault.path())
            .unwrap(),
        config: Default::default(),
        shutdown: host_api::ShutdownToken::new(rx),
    };
    ToolRegistryHostExtension.register(&mut ctx).await.unwrap();
    (
        ctx.services
            .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
            .unwrap(),
        vault,
    )
}

struct Page {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Page {
    async fn serve(response: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0; 4096];
                let count = stream.read(&mut request).await.unwrap();
                if count == 0 {
                    continue;
                }
                let request = String::from_utf8_lossy(&request[..count]);
                assert!(request.starts_with("GET "));
                assert!(!request.lines().any(|line| {
                    let name = line.split(':').next().unwrap_or_default();
                    name.eq_ignore_ascii_case("cookie")
                        || name.eq_ignore_ascii_case("authorization")
                }));
                let path = request.split_whitespace().nth(1).unwrap();
                let response =
                    response(path).replacen("\r\n\r\n", "\r\nConnection: close\r\n\r\n", 1);
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self { url, task }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn record(root: &std::path::Path, outcome: &ToolOutcome) -> ResultRecord {
    assert!(!outcome.is_error, "{}", outcome.text);
    let (_, id) = extract_marker(&outcome.text);
    read_result(root, &id.expect("view marker"), "web_preview")
        .unwrap()
        .expect("captured preview")
}

#[tokio::test]
async fn web_preview_plain_page_returns_live_text_and_captured_view() {
    let root = tempfile::tempdir().unwrap();
    let (registry, _vault) = registry(root.path()).await;
    let page = Page::serve(|_| "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into()).await;
    let args = json!({"url": format!("{}/", page.url), "title": "Dev app"});
    let outcome = registry.dispatch("web_preview", args.clone()).await;
    let captured = record(root.path(), &outcome);
    assert!(outcome
        .text
        .starts_with(&format!("web preview shown: {}/\n", page.url)));
    assert_eq!(captured.server, "dar");
    assert_eq!(captured.resource_uri, "ui://dar/web-preview");
    assert_eq!(
        captured.pi_tool_name.as_deref(),
        Some("mcp__dar__web_preview")
    );
    assert_eq!(captured.args, args);
    assert_eq!(
        captured.result["structuredContent"],
        json!({
            "url": format!("{}/", page.url), "finalUrl": format!("{}/", page.url),
            "title": "Dev app", "frameable": true, "reason": null, "status": 200
        })
    );
    assert_eq!(outcome.to_mcp_result()["content"][0]["text"], outcome.text);
}

#[tokio::test]
async fn web_preview_x_frame_options_shows_link_card_even_with_empty_header() {
    let root = tempfile::tempdir().unwrap();
    let (registry, _vault) = registry(root.path()).await;
    let page = Page::serve(|path| {
        format!(
            "HTTP/1.1 200 OK\r\nX-Frame-Options: {}\r\nContent-Length: 0\r\n\r\n",
            if path == "/deny" { "DENY" } else { "" }
        )
    })
    .await;
    for path in ["/deny", "/empty"] {
        let url = format!("{}{path}", page.url);
        let outcome = registry.dispatch("web_preview", json!({"url": url})).await;
        assert_eq!(
            extract_marker(&outcome.text).0,
            format!("web preview: {url} refuses framing, link card shown")
        );
        let captured = record(root.path(), &outcome);
        assert_eq!(captured.result["structuredContent"]["frameable"], false);
        assert_eq!(
            captured.result["structuredContent"]["reason"],
            "x-frame-options"
        );
        assert_eq!(captured.result["structuredContent"]["status"], 200);
        assert!(captured.result["structuredContent"]["title"].is_null());
    }
}

#[tokio::test]
async fn web_preview_unsafe_urls_return_error_without_record_or_credentials() {
    let root = tempfile::tempdir().unwrap();
    let (registry, _vault) = registry(root.path()).await;
    let page = Page::serve(|_| "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into()).await;
    let host = page.url.strip_prefix("http://").unwrap();
    for url in [
        format!("http://user:example-password@{host}/"),
        format!("http://user@{host}/"),
        "ftp://example.com/page".into(),
        "http://example.com/page".into(),
        "https://user:example-password@example.com/".into(),
        "not a URL".into(),
    ] {
        let outcome = registry.dispatch("web_preview", json!({"url": url})).await;
        assert!(outcome.is_error, "unsafe URL accepted");
        assert!(!outcome.text.contains("example-password"));
        assert!(extract_marker(&outcome.text).1.is_none());
        assert!(!root.path().join("data/mcp-apps/results").exists());
    }
}

#[tokio::test]
async fn web_preview_enforces_each_csp_frame_ancestors_but_ignores_report_only() {
    let root = tempfile::tempdir().unwrap();
    let (registry, _vault) = registry(root.path()).await;
    let page = Page::serve(|path| {
        let headers = match path {
            "/self" => "Content-Security-Policy: default-src 'self'; frame-ancestors 'self'\r\n",
            "/multiple" => "Content-Security-Policy: frame-ancestors *\r\nContent-Security-Policy: frame-ancestors 'none'\r\n",
            "/combined" => "Content-Security-Policy: default-src 'self', FRAME-ANCESTORS 'none'\r\n",
            "/star" => "Content-Security-Policy: frame-ancestors *\r\n",
            "/report-only" => "Content-Security-Policy-Report-Only: frame-ancestors 'none'\r\n",
            _ => panic!("unexpected test path"),
        };
        format!("HTTP/1.1 200 OK\r\n{headers}Content-Length: 0\r\n\r\n")
    }).await;
    for (path, expected_reason) in [
        ("/self", Some("frame-ancestors")),
        ("/multiple", Some("frame-ancestors")),
        ("/combined", Some("frame-ancestors")),
        ("/star", None),
        ("/report-only", None),
    ] {
        let outcome = registry
            .dispatch("web_preview", json!({"url": format!("{}{path}", page.url)}))
            .await;
        let captured = record(root.path(), &outcome);
        assert_eq!(
            captured.result["structuredContent"]["reason"],
            json!(expected_reason),
            "{path}"
        );
        assert_eq!(
            captured.result["structuredContent"]["frameable"],
            expected_reason.is_none(),
            "{path}"
        );
    }
}

#[tokio::test]
async fn web_preview_closed_port_captures_unreachable_link_card() {
    let root = tempfile::tempdir().unwrap();
    let (registry, _vault) = registry(root.path()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    drop(listener);
    let outcome = registry.dispatch("web_preview", json!({"url": url})).await;
    let captured = record(root.path(), &outcome);
    assert_eq!(
        extract_marker(&outcome.text).0,
        format!("web preview: {url} is not reachable from the agent host, link card shown")
    );
    assert_eq!(
        captured.result["structuredContent"],
        json!({
            "url": url, "finalUrl": url, "title": null,
            "frameable": false, "reason": "unreachable", "status": null
        })
    );
}

#[tokio::test]
async fn web_preview_times_out_after_five_seconds_while_other_calls_progress() {
    use std::time::{Duration, Instant};
    let root = tempfile::tempdir().unwrap();
    let (registry, _vault) = registry(root.path()).await;
    let hanging = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", hanging.local_addr().unwrap());
    let pending = tokio::spawn({
        let registry = Arc::clone(&registry);
        let url = url.clone();
        async move { registry.dispatch("web_preview", json!({"url": url})).await }
    });
    let page = Page::serve(|_| "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into()).await;
    let ready = tokio::time::timeout(
        Duration::from_secs(1),
        registry.dispatch("web_preview", json!({"url": page.url})),
    )
    .await
    .expect("another preview must progress during the hang");
    assert_eq!(
        record(root.path(), &ready).result["structuredContent"]["frameable"],
        true
    );
    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(6), pending)
        .await
        .expect("header check must time out in five seconds")
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(4));
    let captured = record(root.path(), &outcome);
    assert_eq!(
        captured.result["structuredContent"]["reason"],
        "unreachable"
    );
    assert_eq!(captured.result["structuredContent"]["finalUrl"], url);
    assert!(captured.result["structuredContent"]["status"].is_null());
}

#[tokio::test]
async fn web_preview_redirects_reject_unsafe_targets_and_six_hops_without_record() {
    let root = tempfile::tempdir().unwrap();
    let (registry, _vault) = registry(root.path()).await;
    let target = Page::serve(|_| "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into()).await;
    let credentials = target
        .url
        .replacen("http://", "http://user:example-password@", 1);
    let page = Page::serve(move |path| {
        let location = match path {
            "/credentials" => credentials.clone(),
            "/remote-http" => "http://example.com/".into(),
            "/ftp" => "ftp://example.com/".into(),
            "/six" => "/chain/1".into(),
            "/five" => "/chain/2".into(),
            "/chain/6" => return "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into(),
            _ => format!("/chain/{}", path.strip_prefix("/chain/").unwrap().parse::<usize>().unwrap() + 1),
        };
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nSet-Cookie: preview=example; Path=/\r\nContent-Length: 0\r\n\r\n")
    }).await;
    for path in ["/credentials", "/remote-http", "/ftp", "/six"] {
        let outcome = registry
            .dispatch("web_preview", json!({"url": format!("{}{path}", page.url)}))
            .await;
        assert!(
            outcome.is_error,
            "redirect must be refused: {path}: {}",
            outcome.text
        );
        assert!(!outcome.text.contains("example-password"));
        assert!(extract_marker(&outcome.text).1.is_none());
        assert!(!root.path().join("data/mcp-apps/results").exists());
    }
    let outcome = registry
        .dispatch("web_preview", json!({"url": format!("{}/five", page.url)}))
        .await;
    let captured = record(root.path(), &outcome);
    assert_eq!(
        captured.result["structuredContent"]["finalUrl"],
        format!("{}/chain/6", page.url)
    );
    assert_eq!(captured.result["structuredContent"]["frameable"], true);
}

#[tokio::test]
async fn web_preview_checks_final_redirect_headers_and_retains_last_url_on_failure() {
    let root = tempfile::tempdir().unwrap();
    let (registry, _vault) = registry(root.path()).await;
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_url = format!("http://{}/last", closed.local_addr().unwrap());
    drop(closed);
    let destination = closed_url.clone();
    let page = Page::serve(move |path| match path {
        "/start" => "HTTP/1.1 302 Found\r\nLocation: /blocked\r\nContent-Length: 0\r\n\r\n".into(),
        "/blocked" => {
            "HTTP/1.1 200 OK\r\nX-Frame-Options: DENY\r\nContent-Length: 0\r\n\r\n".into()
        }
        "/unreachable" => {
            format!("HTTP/1.1 302 Found\r\nLocation: {destination}\r\nContent-Length: 0\r\n\r\n")
        }
        _ => panic!("unexpected test path"),
    })
    .await;
    let outcome = registry
        .dispatch("web_preview", json!({"url": format!("{}/start", page.url)}))
        .await;
    let captured = record(root.path(), &outcome);
    assert_eq!(
        captured.result["structuredContent"]["finalUrl"],
        format!("{}/blocked", page.url)
    );
    assert_eq!(
        captured.result["structuredContent"]["reason"],
        "x-frame-options"
    );
    let outcome = registry
        .dispatch(
            "web_preview",
            json!({"url": format!("{}/unreachable", page.url)}),
        )
        .await;
    let captured = record(root.path(), &outcome);
    assert_eq!(captured.result["structuredContent"]["finalUrl"], closed_url);
    assert_eq!(
        captured.result["structuredContent"]["reason"],
        "unreachable"
    );
    assert!(captured.result["structuredContent"]["status"].is_null());
}

#[tokio::test]
async fn web_preview_accepts_localhost_http_errors_without_waiting_for_the_body() {
    let root = tempfile::tempdir().unwrap();
    let (registry, _vault) = registry(root.path()).await;
    let page = Page::serve(|path| {
        format!(
            "HTTP/1.1 {} Error\r\nContent-Length: 1000000\r\n\r\n",
            if path == "/not-found" { 404 } else { 500 }
        )
    })
    .await;
    for (host, path, status) in [
        ("127.0.0.1", "/not-found", 404),
        ("localhost", "/server-error", 500),
    ] {
        let url = format!("{}{path}", page.url.replace("127.0.0.1", host));
        let outcome = registry.dispatch("web_preview", json!({"url": url})).await;
        let captured = record(root.path(), &outcome);
        assert_eq!(captured.result["structuredContent"]["frameable"], true);
        assert_eq!(captured.result["structuredContent"]["status"], status);
    }
}

#[tokio::test]
async fn web_preview_capture_failure_keeps_plain_text_without_marker() {
    let root = tempfile::tempdir().unwrap();
    let (registry, _vault) = registry(root.path()).await;
    std::fs::write(root.path().join("data/mcp-apps"), "not a directory").unwrap();
    let page = Page::serve(|_| "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into()).await;
    let outcome = registry
        .dispatch("web_preview", json!({"url": page.url}))
        .await;
    assert!(!outcome.is_error);
    assert_eq!(outcome.text, format!("web preview shown: {}/", page.url));
    assert_eq!(outcome.to_mcp_result()["content"][0]["text"], outcome.text);
    assert!(extract_marker(&outcome.text).1.is_none());
}
