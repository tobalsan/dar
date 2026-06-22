//! End-to-end demo: a registering extension's tab appears in the dashboard nav
//! and its fragment renders at `/tabs/{id}`, while a dashboard with zero
//! registered tabs renders no tab nav.
//!
//! Drives the real register -> start flow for both extensions and exercises the
//! merged host router, so this exercises the actual contract wiring rather than
//! a stubbed handler.

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use host_api::{
    ConfigStore, Extension, HostPaths, HttpRegistry, RegisterCtx, ServiceRegistry, ShutdownToken,
    StartCtx,
};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Boot the given extensions through register + start against a temp root with
/// HTTP enabled, returning the merged router for request testing.
async fn boot(extensions: Vec<Box<dyn Extension>>, root: &std::path::Path) -> axum::Router {
    std::fs::create_dir_all(root.join("data")).unwrap();
    let paths = HostPaths::new(root).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let shutdown = ShutdownToken::new(rx);
    let mut register_ctx = RegisterCtx {
        bus: host_api::EventBus::new(),
        http: HttpRegistry::default(),
        foreground: host_api::ForegroundRegistry::default(),
        services: ServiceRegistry::default(),
        paths: paths.clone(),
        config: ConfigStore::from_values(HashMap::new()),
        shutdown: shutdown.clone(),
    };
    for ext in &extensions {
        ext.register(&mut register_ctx).await.unwrap();
    }
    let host = register_ctx.into_start_services().unwrap();
    let router = host.router.as_ref().clone();
    for ext in &extensions {
        let ctx = StartCtx {
            shutdown: shutdown.clone(),
            paths: paths.clone(),
            config: ConfigStore::from_values(HashMap::new()),
            host: host.clone(),
        };
        ext.start(ctx).await.unwrap();
    }
    router
}

async fn get(router: &axum::Router, path: &str) -> (StatusCode, String) {
    let resp = router
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn example_tab_appears_in_nav_and_renders_fragment() {
    let temp = tempfile::tempdir().unwrap();
    let router = boot(
        vec![
            Box::new(dashboard::DashboardExtension::default()),
            example::extension(),
        ],
        temp.path(),
    )
    .await;

    // Tab nav present on the index page, linking to the fragment route.
    let (status, html) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("id=\"dash-tabs\""),
        "tab nav rendered: {html}"
    );
    assert!(html.contains(">Runs<"), "default Runs tab present");
    assert!(html.contains("/tabs/example"), "example tab linked");
    assert!(html.contains(">Example<"), "example tab title in nav");
    // htmx #content innerHTML-swap shell preserved (no body swap).
    assert!(html.contains("id=\"content\""), "content shell preserved");
    assert!(
        !html.contains("hx-swap=\"outerHTML\""),
        "no outer/body swap"
    );

    // The fragment renders at the dashboard-owned dispatch route.
    let (status, frag) = get(&router, "/tabs/example").await;
    assert_eq!(status, StatusCode::OK);
    assert!(frag.contains("Example tab"), "fragment body served: {frag}");
    assert!(!frag.contains("<body"), "fragment is not a full page");

    // Unknown tab id 404s.
    let (status, _) = get(&router, "/tabs/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dashboard_without_registrants_has_no_tab_nav() {
    let temp = tempfile::tempdir().unwrap();
    let router = boot(
        vec![Box::new(dashboard::DashboardExtension::default())],
        temp.path(),
    )
    .await;

    let (status, html) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !html.contains("id=\"dash-tabs\""),
        "no tab nav when zero tabs registered"
    );
    assert!(html.contains("id=\"content\""), "content shell preserved");
}
