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
