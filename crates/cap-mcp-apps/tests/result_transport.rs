//! Approved result-capture seam: public marker/record transport catches
//! runner escaping and correlation failures; misses browser rendering.
use cap_mcp_apps::{extract_marker, prune_results, read_result, ResultRecord};

#[test]
fn retention_expires_old_results_and_preserves_recent_views() {
    let root = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut record = ResultRecord {
        tool: "canvas__draw".into(),
        pi_tool_name: None,
        server: "canvas".into(),
        resource_uri: "ui://canvas/app".into(),
        args: serde_json::json!({"label":"old"}),
        result: serde_json::json!({"content":[{"type":"text","text":"Old drawing"}]}),
        created_at: now - 31 * 24 * 60 * 60,
    };
    let expired = record.write(root.path()).unwrap();
    record.created_at = now - 29 * 24 * 60 * 60;
    let recent = record.write(root.path()).unwrap();
    prune_results(root.path()).unwrap();
    assert!(read_result(root.path(), &expired, "canvas__draw")
        .unwrap()
        .is_none());
    assert_eq!(
        read_result(root.path(), &recent, "canvas__draw")
            .unwrap()
            .unwrap()
            .result,
        serde_json::json!({"content":[{"type":"text","text":"Old drawing"}]})
    );
}

#[test]
fn runner_text_hides_markers_and_resolves_only_a_matching_record() {
    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let path = root.path().join("data/mcp-apps/results");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join(format!("{id}.json")), r#"{"tool":"canvas__draw","server":"canvas","resource_uri":"ui://canvas/app","args":{"text":"box"},"result":{"content":[{"type":"text","text":"box"}]},"created_at":42}"#).unwrap();
    for text in [
        format!("box\n[mcp-app-result:{id}]"),
        format!("\"box\\n[mcp-app-result:{id}]\""),
        format!("\"\\\"box\\\\n[mcp-app-result:{id}]\\\"\""),
    ] {
        let (clean, marker) = extract_marker(&text);
        assert!(!clean.contains("mcp-app-result"));
        assert_eq!(marker.as_deref(), Some(id));
        let record = read_result(root.path(), marker.as_deref().unwrap(), "dar.canvas__draw")
            .unwrap()
            .unwrap();
        assert_eq!(record.args, serde_json::json!({"text":"box"}));
    }
    assert!(read_result(root.path(), id, "other__draw")
        .unwrap()
        .is_none());
    assert!(read_result(
        root.path(),
        "ffffffffffffffffffffffffffffffff",
        "canvas__draw"
    )
    .unwrap()
    .is_none());
    assert!(read_result(root.path(), "../escape", "canvas__draw")
        .unwrap()
        .is_none());
    assert_eq!(extract_marker("box"), ("box".to_owned(), None));
    let (clean, marker) = extract_marker(&format!("box[mcp-app-result:{id}][mcp-app-result:{id}]"));
    assert_eq!(clean, "box");
    assert!(marker.is_none());
    std::fs::remove_file(path.join(format!("{id}.json"))).unwrap();
    assert!(read_result(root.path(), id, "canvas__draw")
        .unwrap()
        .is_none());
}

#[test]
fn pi_bridge_name_resolves_only_its_recorded_tool() {
    // ALG-530 Testing Decisions approved the public result-capture seam.
    let root = tempfile::tempdir().unwrap();
    let mut record = ResultRecord {
        tool: "tldraw__exec".into(),
        pi_tool_name: Some("mcp__dar__tldraw__exec".into()),
        server: "tldraw".into(),
        resource_uri: "ui://tldraw/app".into(),
        args: serde_json::json!({"action":"draw"}),
        result: serde_json::json!({"content":[{"type":"text","text":"drawn"}]}),
        created_at: 42,
    };
    let id = record.write(root.path()).unwrap();

    assert_eq!(
        read_result(root.path(), &id, "mcp__dar__tldraw__exec")
            .unwrap()
            .unwrap()
            .result,
        serde_json::json!({"content":[{"type":"text","text":"drawn"}]})
    );
    assert!(read_result(root.path(), &id, "mcp__other__tldraw__exec")
        .unwrap()
        .is_none());
    assert!(read_result(root.path(), &id, "mcp__dar__other__exec")
        .unwrap()
        .is_none());
    record.pi_tool_name = None;
    let legacy = record.write(root.path()).unwrap();
    assert!(read_result(root.path(), &legacy, "mcp__dar__tldraw__exec")
        .unwrap()
        .is_none());
}

#[test]
fn pi_sanitized_name_requires_a_recorded_exact_alias() {
    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let path = root.path().join("data/mcp-apps/results");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(
        path.join(format!("{id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "tool":"canva__generate-image", "pi_tool_name":"mcp__dar__canva__generate_image",
            "server":"canva", "resource_uri":"ui://canva/app", "args":{},
            "result":{"content":[{"type":"text","text":"generated"}]}, "created_at":42
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(
        read_result(root.path(), id, "mcp__dar__canva__generate_image")
            .unwrap()
            .is_some()
    );
    assert!(
        read_result(root.path(), id, "mcp__dar__canva__generate-image")
            .unwrap()
            .is_none()
    );
}

#[test]
fn host_publish_resolves_the_explicit_runner_names_only() {
    // ALG-532 Testing Decisions approve read_result: catches correlation,
    // while browser rendering and real runner emission need separate checks.
    let root = tempfile::tempdir().unwrap();
    let record = ResultRecord {
        tool: "artifact_publish".into(),
        pi_tool_name: Some("mcp__dar__artifact_publish".into()),
        server: "dar".into(),
        resource_uri: "ui://dar/artifact-viewer".into(),
        args: serde_json::json!({"path":"report.html"}),
        result: serde_json::json!({"structuredContent":{"filename":"report.html"}}),
        created_at: 42,
    };
    let id = record.write(root.path()).unwrap();
    for name in [
        "artifact_publish",
        "mcp__dar__artifact_publish",
        "dar.artifact_publish",
        "dar_artifact_publish",
    ] {
        let result = read_result(root.path(), &id, name).unwrap();
        assert!(result.is_some(), "{name} must resolve its publish result");
        assert_eq!(
            result.unwrap().result["structuredContent"]["filename"],
            "report.html"
        );
    }
    for name in [
        "other",
        "dar.other",
        "other_artifact_publish",
        "other.artifact_publish",
    ] {
        assert!(
            read_result(root.path(), &id, name).unwrap().is_none(),
            "{name}"
        );
    }
}
