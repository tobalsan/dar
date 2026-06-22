//! End-to-end demo (ALG-225): the scheduler's read-only "Cron" tab appears in
//! the dashboard when the scheduler extension is linked and enabled, shows a
//! job's schedule / enabled flag / status, and refreshes via the dashboard's
//! self-poll. It is absent when the scheduler section is not present.
//!
//! Drives the real register -> start flow for the dashboard + scheduler against
//! the merged host router, exercising the actual cap-dashboard-tab wiring rather
//! than a stubbed handler.

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use host_api::{
    ConfigStore, Extension, HostPaths, HttpRegistry, RegisterCtx, ServiceRegistry, ShutdownToken,
    StartCtx,
};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

/// Boot the dashboard + scheduler through register + start against `root`, with
/// the given `extensions.*` config, returning the merged router.
async fn boot(root: &std::path::Path, config: HashMap<String, serde_json::Value>) -> axum::Router {
    std::fs::create_dir_all(root.join("data")).unwrap();
    // Minimal agent.yaml so the scheduler can resolve its runner config.
    std::fs::write(root.join("agent.yaml"), "id: demo\nname: Demo\nrunner:\n  use: fake\n").unwrap();
    let paths = HostPaths::new(root).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let shutdown = ShutdownToken::new(rx);
    let extensions: Vec<Box<dyn Extension>> = vec![
        Box::new(dashboard::DashboardExtension::default()),
        scheduler::extension(),
    ];
    let mut register_ctx = RegisterCtx {
        bus: host_api::EventBus::new(),
        http: HttpRegistry::default(),
        foreground: host_api::ForegroundRegistry::default(),
        services: ServiceRegistry::default(),
        paths: paths.clone(),
        config: ConfigStore::from_values(config.clone()),
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
            config: ConfigStore::from_values(config.clone()),
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

/// Write a one-job `cron/jobs.json` under `root`.
fn write_one_job(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("cron")).unwrap();
    std::fs::write(
        root.join("cron").join("jobs.json"),
        r#"{ "version": 1, "jobs": [
            { "id": "digest", "name": "Morning digest", "enabled": true,
              "schedule": { "cron": "0 8 * * *", "tz": "Europe/Paris" },
              "payload": { "message": "Summarize." } }
        ] }"#,
    )
    .unwrap();
}

#[tokio::test]
async fn cron_tab_present_when_scheduler_enabled_and_shows_job() {
    let temp = tempfile::tempdir().unwrap();
    write_one_job(temp.path());
    let config = HashMap::from([("scheduler".to_string(), json!({}))]);
    let router = boot(temp.path(), config).await;

    // Tab nav present, linking to the cron fragment; Runs stays the default tab.
    let (status, html) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("id=\"dash-tabs\""), "tab nav rendered: {html}");
    assert!(html.contains(">Runs<"), "default Runs tab present");
    assert!(html.contains("/tabs/cron"), "cron tab linked");
    assert!(html.contains(">Cron<"), "cron tab title in nav");
    // Content shell + innerHTML swap preserved (no body swap).
    assert!(html.contains("id=\"content\""), "content shell preserved");
    assert!(!html.contains("hx-swap=\"outerHTML\""), "no outer/body swap");

    // The fragment renders at the dashboard-owned dispatch route and shows the
    // job's schedule + tz and enabled flag. Refresh is the dashboard's shared
    // #content poller (repointed to the active tab), so the fragment carries no
    // inner poll of its own.
    let (status, frag) = get(&router, "/tabs/cron").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!frag.contains("<body"), "fragment is not a full page");
    assert!(frag.contains("Morning digest"), "job name shown: {frag}");
    assert!(frag.contains("0 8 * * * Europe/Paris"), "schedule + tz shown");
    assert!(frag.contains(">enabled<"), "enabled flag shown");

    // The index shell keeps the #content innerHTML self-poll that drives the
    // active tab's refresh.
    assert!(html.contains("hx-get=\"/content\""), "shared content poller present");
    assert!(html.contains("hx-swap=\"innerHTML\""), "innerHTML swap preserved");
}

#[tokio::test]
async fn cron_tab_absent_when_scheduler_section_missing() {
    let temp = tempfile::tempdir().unwrap();
    write_one_job(temp.path());
    // No `extensions.scheduler` section: dist runtime gate keeps the scheduler
    // dormant, so it registers no tab.
    let router = boot(temp.path(), HashMap::new()).await;

    let (status, html) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !html.contains("id=\"dash-tabs\""),
        "no tab nav when scheduler is not linked/enabled: {html}"
    );
    assert!(html.contains("id=\"content\""), "content shell preserved");

    // The cron fragment route 404s (no provider registered).
    let (status, _) = get(&router, "/tabs/cron").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cron_tab_absent_when_kill_switched() {
    let temp = tempfile::tempdir().unwrap();
    write_one_job(temp.path());
    // Present but `enabled: false` kill switch: no tab registered.
    let config = HashMap::from([("scheduler".to_string(), json!({ "enabled": false }))]);
    let router = boot(temp.path(), config).await;

    let (status, html) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !html.contains("id=\"dash-tabs\""),
        "no tab nav when scheduler is kill-switched"
    );
    let (status, _) = get(&router, "/tabs/cron").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
