//! Streaming loopback reverse proxy for `/agent/<port>/...` routes.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{OriginalUri, Path as AxumPath, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};

use dar_presence::Registry;

use super::rewrite::rewrite_html;
use super::DashState;

/// Short-TTL cache of live agent ports so every proxied request does not
/// re-scan (and prune) the presence registry directory.
pub(super) struct LivePorts {
    cache: Mutex<(Instant, HashSet<u16>)>,
}

impl LivePorts {
    const TTL: Duration = Duration::from_secs(1);

    pub(super) fn new() -> Self {
        Self {
            cache: Mutex::new((Instant::now(), HashSet::new())),
        }
    }

    /// Refreshes from the registry when the cache is stale **or** the port is
    /// absent from the cached set — a just-started agent must not 404 until
    /// the TTL expires.
    pub(super) fn contains(&self, registry: &Registry, port: u16) -> bool {
        let mut cache = self.cache.lock().unwrap();
        let (refreshed, ports) = &mut *cache;
        if refreshed.elapsed() > Self::TTL || !ports.contains(&port) {
            *ports = registry
                .read_live()
                .iter()
                .filter_map(|entry| entry.port())
                .collect();
            *refreshed = Instant::now();
        }
        ports.contains(&port)
    }
}

pub(super) async fn proxy_agent(
    State(state): State<DashState>,
    AxumPath(params): AxumPath<std::collections::HashMap<String, String>>,
    OriginalUri(original_uri): OriginalUri,
    request: Request<Body>,
) -> Response {
    let Some(port) = params.get("port") else {
        return (StatusCode::BAD_REQUEST, "invalid agent port").into_response();
    };
    let Ok(port) = port.parse::<u16>() else {
        return (StatusCode::BAD_REQUEST, "invalid agent port").into_response();
    };
    if !state.live_ports.contains(&state.registry, port) {
        return (StatusCode::NOT_FOUND, "agent dashboard not found").into_response();
    }

    let path = original_uri.path();
    // The parsed port can't strip the inbound path: parsing numerically
    // normalizes the segment ("0123" -> 123) while browser URLs carry the raw
    // text, and stripping must round-trip the exact segment.
    let Some(raw_port) = path
        .strip_prefix("/agent/")
        .and_then(|path| path.split('/').next())
    else {
        return (StatusCode::BAD_REQUEST, "invalid agent path").into_response();
    };
    let strip_prefix = format!("/agent/{raw_port}");
    let Some(rest) = path.strip_prefix(&strip_prefix) else {
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
    // Inserted after the copy so client-sent values are always overwritten —
    // the denylist forwards arbitrary request headers.
    headers.insert(
        header::HOST,
        HeaderValue::from_str(&format!("127.0.0.1:{port}")).unwrap(),
    );
    if let Some(host) = &original_host {
        headers.insert(HeaderName::from_static("x-forwarded-host"), host.clone());
    }
    // Built from the parsed port, not the raw segment: exotic-but-parseable
    // spellings ("+123", "%35123") would fail the dashboard's prefix charset
    // sanitizer, and absolute `/agent/<port>/...` links route correctly
    // regardless of which raw spelling the browser used.
    headers.insert(
        HeaderName::from_static("x-forwarded-prefix"),
        HeaderValue::from_str(&format!("/agent/{port}")).unwrap(),
    );
    let upstream = match state
        .client
        .request(parts.method, upstream)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            eprintln!("dar dash: proxy to 127.0.0.1:{port} failed: {err}");
            return (StatusCode::BAD_GATEWAY, "agent dashboard unavailable").into_response();
        }
    };
    proxy_response(upstream, port).await
}

fn forwarded_headers(headers: &HeaderMap) -> HeaderMap {
    let mut result = HeaderMap::new();
    let connection_tokens = connection_tokens(headers);
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if !is_denied_header(&lower)
            && !strips_header(&lower, &connection_tokens)
            && name != header::HOST
        {
            result.append(name.clone(), value.clone());
        }
    }
    result
}

fn is_denied_header(name: &str) -> bool {
    // `x-forwarded-host` is denied so a client-forged value can never reach
    // the agent: the proxy re-inserts it only when the inbound request
    // actually carries a Host header, so filtering alone is not enough.
    matches!(
        name,
        "authorization" | "accept-encoding" | "x-forwarded-host"
    )
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

fn strips_header(lower: &str, connection_tokens: &[String]) -> bool {
    is_hop_by_hop(lower) || connection_tokens.iter().any(|token| token == lower)
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

async fn proxy_response(response: reqwest::Response, port: u16) -> Response {
    let status = response.status();
    let is_html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.to_ascii_lowercase().starts_with("text/html"));
    // Prefix-aware pages prefix their own URLs at request time; rewriting
    // them here would double-prefix.
    let rewriting = is_html && !response.headers().contains_key("x-prefix-aware");
    let connection_tokens = connection_tokens(response.headers());
    let mut builder = Response::builder().status(status);
    for (name, value) in response.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if !strips_header(&lower, &connection_tokens)
            && (!rewriting || (name != header::CONTENT_LENGTH && name != header::CONTENT_ENCODING))
        {
            builder = builder.header(name, value);
        }
    }
    if rewriting {
        let body = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("dar dash: proxy to 127.0.0.1:{port} failed: {err}");
                return (StatusCode::BAD_GATEWAY, "agent dashboard response failed")
                    .into_response();
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

#[cfg(test)]
mod tests {
    use super::super::test_support::{entry, state};
    use super::*;
    use axum::routing::any;
    use axum::Router;
    use tower::ServiceExt;

    /// Boots `router` as the upstream agent dashboard and registers it as a
    /// live presence entry; returns the registry dir and upstream port.
    async fn spawn_upstream(router: Router) -> (tempfile::TempDir, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        Registry::new(dir.path())
            .write(&entry(
                "agent",
                "/agent",
                &format!("0.0.0.0:{port}"),
                std::process::id(),
            ))
            .unwrap();
        (dir, port)
    }

    fn proxy_app(dir: &tempfile::TempDir) -> Router {
        Router::new()
            .route("/agent/{port}", any(proxy_agent))
            .route("/agent/{port}/", any(proxy_agent))
            .route("/agent/{port}/{*rest}", any(proxy_agent))
            .with_state(state(Registry::new(dir.path())))
    }

    /// Echoes the request line and selected request headers into response
    /// headers so tests can assert what actually reached the upstream.
    async fn echo(
        method: axum::http::Method,
        uri: axum::http::Uri,
        headers: HeaderMap,
        body: String,
    ) -> Response {
        let header_or_empty = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        (
            StatusCode::CREATED,
            [
                ("x-method", method.as_str().to_string()),
                ("x-path", uri.path().to_string()),
                ("x-query", uri.query().unwrap_or_default().to_string()),
                ("x-host", header_or_empty("host")),
                ("x-forwarded-host-seen", header_or_empty("x-forwarded-host")),
                (
                    "x-forwarded-prefix-seen",
                    header_or_empty("x-forwarded-prefix"),
                ),
                ("x-authorization", header_or_empty("authorization")),
                ("x-accept-encoding", header_or_empty("accept-encoding")),
                ("x-last-event-id", header_or_empty("last-event-id")),
                ("x-range", header_or_empty("range")),
                ("x-body", body),
                ("connection", "x-upstream-remove".to_string()),
                ("x-upstream-remove", "nope".to_string()),
            ],
            "ok",
        )
            .into_response()
    }

    fn echo_upstream() -> Router {
        Router::new()
            .route("/", any(echo))
            .route("/{*rest}", any(echo))
    }

    async fn proxy_get(app: Router, uri: &str) -> Response {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn forwards_methods_paths_and_query() {
        let (dir, port) = spawn_upstream(echo_upstream()).await;
        let app = proxy_app(&dir);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/agent/{port}/echo?x=%2Fok"))
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from("hello"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()["x-method"], "PATCH");
        assert_eq!(response.headers()["x-path"], "/echo");
        assert_eq!(response.headers()["x-query"], "x=%2Fok");
        assert_eq!(response.headers()["x-body"], "hello");
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
    }

    #[tokio::test]
    async fn rewrites_host_and_sends_forwarded_prefix() {
        let (dir, port) = spawn_upstream(echo_upstream()).await;
        let response = proxy_app(&dir)
            .oneshot(
                Request::builder()
                    .uri(format!("/agent/{port}/echo"))
                    .header(header::HOST, "fleet.test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()["x-host"], format!("127.0.0.1:{port}"));
        assert_eq!(response.headers()["x-forwarded-host-seen"], "fleet.test");
        assert_eq!(
            response.headers()["x-forwarded-prefix-seen"],
            format!("/agent/{port}")
        );
    }

    #[tokio::test]
    async fn denylist_drops_authorization_and_accept_encoding() {
        let (dir, port) = spawn_upstream(echo_upstream()).await;
        let response = proxy_app(&dir)
            .oneshot(
                Request::builder()
                    .uri(format!("/agent/{port}/echo"))
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .header("x-forwarded-host", "evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()["x-authorization"], "");
        assert_eq!(response.headers()["x-accept-encoding"], "");
        // The request has no Host header, so nothing is re-inserted: a forged
        // x-forwarded-host must not leak through the denylist.
        assert_eq!(response.headers()["x-forwarded-host-seen"], "");
    }

    #[tokio::test]
    async fn forwards_last_event_id_and_range() {
        let (dir, port) = spawn_upstream(echo_upstream()).await;
        let response = proxy_app(&dir)
            .oneshot(
                Request::builder()
                    .uri(format!("/agent/{port}/echo"))
                    .header("last-event-id", "42")
                    .header(header::RANGE, "bytes=0-99")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()["x-last-event-id"], "42");
        assert_eq!(response.headers()["x-range"], "bytes=0-99");
    }

    #[test]
    fn forwarded_headers_use_denylist_and_connection_tokens() {
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
        headers.insert(
            HeaderName::from_static("x-custom"),
            HeaderValue::from_static("yes"),
        );
        headers.insert(header::HOST, HeaderValue::from_static("fleet.test"));
        headers.insert(
            HeaderName::from_static("x-forwarded-host"),
            HeaderValue::from_static("evil.example"),
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
        assert_eq!(forwarded["x-custom"], "yes");
        assert!(forwarded.get(header::ACCEPT_ENCODING).is_none());
        assert!(forwarded.get(header::AUTHORIZATION).is_none());
        assert!(forwarded.get("x-forwarded-host").is_none());
        assert!(forwarded.get(header::HOST).is_none());
        assert!(forwarded.get("x-drop").is_none());
    }

    #[tokio::test]
    async fn strips_hop_by_hop_and_connection_named_response_headers() {
        let (dir, port) = spawn_upstream(echo_upstream()).await;
        let response = proxy_get(proxy_app(&dir), &format!("/agent/{port}/echo")).await;
        assert!(response.headers().get(header::CONNECTION).is_none());
        assert!(response.headers().get("x-upstream-remove").is_none());
    }

    #[tokio::test]
    async fn trailing_slash_and_leading_zero_port() {
        let (dir, port) = spawn_upstream(echo_upstream()).await;
        let app = proxy_app(&dir);
        for suffix in ["", "/"] {
            let response = proxy_get(app.clone(), &format!("/agent/{port}{suffix}")).await;
            assert_eq!(response.status(), StatusCode::CREATED, "suffix {suffix}");
            assert_eq!(response.headers()["x-path"], "/");
        }
        let leading_zero = proxy_get(app, &format!("/agent/0{port}/echo")).await;
        assert_eq!(leading_zero.headers()["x-path"], "/echo");
        // The advertised prefix is normalized (parsed port), never the raw
        // segment, so it always passes the dashboard's charset sanitizer.
        assert_eq!(
            leading_zero.headers()["x-forwarded-prefix-seen"],
            format!("/agent/{port}")
        );
    }

    #[tokio::test]
    async fn rejects_invalid_unknown_ports() {
        let (dir, _port) = spawn_upstream(echo_upstream()).await;
        let app = proxy_app(&dir);
        let invalid = proxy_get(app.clone(), "/agent/abc/").await;
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let out_of_range = proxy_get(app.clone(), "/agent/99999/").await;
        assert_eq!(out_of_range.status(), StatusCode::BAD_REQUEST);
        let unknown = proxy_get(app, "/agent/12345/").await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rewrites_html_without_prefix_aware() {
        async fn upstream() -> Response {
            (
                [
                    (header::CONTENT_TYPE, "text/html"),
                    (header::CONTENT_ENCODING, "gzip"),
                ],
                "<script src=\"/assets/x.js\"></script><button hx-post=\"/control/pause\" onclick=\"fetch('/run-now')\"></button><a href=\"/\"></a><script>fetch('/content')</script>",
            )
                .into_response()
        }
        let (dir, port) = spawn_upstream(Router::new().route("/html", any(upstream))).await;
        let response = proxy_get(proxy_app(&dir), &format!("/agent/{port}/html")).await;
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains(&format!("src=\"/agent/{port}/assets/x.js\"")));
        assert!(html.contains(&format!("hx-post=\"/agent/{port}/control/pause\"")));
        assert!(html.contains(&format!("onclick=\"fetch('/agent/{port}/run-now')\"")));
        assert!(html.contains(&format!("fetch('/agent/{port}/content')")));
        // Bare href="/" is a documented limitation of the heuristic shim.
        assert!(html.contains("href=\"/\""));
    }

    #[tokio::test]
    async fn skips_rewrite_when_prefix_aware() {
        const BODY: &str =
            "<div hx-get=\"/content\"></div><script>new EventSource(`/chat/main/stream`)</script>";
        async fn upstream() -> Response {
            (
                [
                    (header::CONTENT_TYPE, HeaderValue::from_static("text/html")),
                    (
                        HeaderName::from_static("x-prefix-aware"),
                        HeaderValue::from_static("1"),
                    ),
                ],
                BODY,
            )
                .into_response()
        }
        let (dir, port) = spawn_upstream(Router::new().route("/html", any(upstream))).await;
        let response = proxy_get(proxy_app(&dir), &format!("/agent/{port}/html")).await;
        assert_eq!(response.headers()["x-prefix-aware"], "1");
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            BODY.len().to_string()
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], BODY.as_bytes());
    }

    #[tokio::test]
    async fn streams_non_html_untouched() {
        async fn upstream() -> Response {
            (
                [(header::CONTENT_TYPE, "application/json")],
                "{\"exact\":true}",
            )
                .into_response()
        }
        let (dir, port) = spawn_upstream(Router::new().route("/json", any(upstream))).await;
        let response = proxy_get(proxy_app(&dir), &format!("/agent/{port}/json")).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], br#"{"exact":true}"#);
    }

    #[tokio::test]
    async fn bad_gateway_for_unreachable_agent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        Registry::new(dir.path())
            .write(&entry(
                "agent",
                "/agent",
                &format!("0.0.0.0:{port}"),
                std::process::id(),
            ))
            .unwrap();
        let response = proxy_get(proxy_app(&dir), &format!("/agent/{port}/")).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn live_ports_refresh_on_miss_and_cache_hits() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        let ports = LivePorts::new();
        assert!(!ports.contains(&registry, 50000));
        // Registered after a cold cache: found via refresh-on-miss.
        let registered = entry("agent", "/agent", "0.0.0.0:50000", std::process::id());
        registry.write(&registered).unwrap();
        assert!(ports.contains(&registry, 50000));
        // Removed from the registry: still found while the TTL holds.
        registry.remove(&registered).unwrap();
        assert!(ports.contains(&registry, 50000));
    }
}
