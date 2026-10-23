//! Publishes shared tool registry and built-in artifact publisher.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use dar_artifacts::{ArtifactMetadataInput, ArtifactStore, ExportRoot};
use dar_extension_sdk::mcp_apps::ResultRecord;
use host_api::{Extension, RegisterCtx};
use serde::Deserialize;
use serde_json::{json, Value};
use tool_registry::{
    ToolContent, ToolExecutor, ToolOutcome, ToolRegistry, ToolRegistryHandle, ToolSpec,
    TOOL_REGISTRY_SERVICE,
};

mod web_preview;

pub const ARTIFACT_STORE_SERVICE: &str = "artifact-store";
/// Cross-surface ceiling. Slack accepts at most 25 MiB.
const MAX_ARTIFACT_BYTES: u64 = 25 * 1024 * 1024;

pub struct ToolRegistryHostExtension;

impl Extension for ToolRegistryHostExtension {
    fn id(&self) -> &'static str {
        "tool-registry-host"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let registry: Arc<dyn ToolRegistryHandle> = Arc::new(ToolRegistry::new());
            let store = Arc::new(ArtifactStore::open(
                ctx.paths.artifact_dir()?,
                MAX_ARTIFACT_BYTES,
            )?);
            let exports_path = ctx.paths.root().join("data/artifact-exports");
            std::fs::create_dir_all(&exports_path)?;
            let exports_path = exports_path.canonicalize()?;
            let exports = ExportRoot::open(&exports_path)?;
            registry.register_tool(
                artifact_publish_spec(&exports_path),
                Arc::new(ArtifactPublish {
                    store: Arc::clone(&store),
                    exports,
                    root: ctx.paths.root().to_path_buf(),
                }),
            )?;
            registry.register_tool(
                web_preview::spec(),
                Arc::new(web_preview::WebPreview {
                    root: ctx.paths.root().to_path_buf(),
                }),
            )?;
            ctx.services
                .service::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE, registry)?;
            ctx.services.service(ARTIFACT_STORE_SERVICE, store)?;
            Ok(())
        })
    }
}

fn artifact_publish_spec(exports_path: &Path) -> ToolSpec {
    ToolSpec::new(
        "artifact_publish",
        format!(
            "Publish a file up to 25 MiB into host-private immutable storage. Export folder: {}. Copy the file into this export folder, then pass its relative path. Web chat shows the published file to the user; use it to show a report instead of running open. Call this only after the file write finishes. Never call it in parallel with the command that writes the file.",
            exports_path.display()
        ),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path"],
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "filename": { "type": "string", "minLength": 1 },
                "mediaType": { "type": "string", "minLength": 1 },
                "caption": { "type": "string" },
                "title": { "type": "string" }
            }
        }),
    )
    .writes()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArtifactPublishArgs {
    path: String,
    filename: Option<String>,
    media_type: Option<String>,
    caption: Option<String>,
    title: Option<String>,
}

struct ArtifactPublish {
    store: Arc<ArtifactStore>,
    exports: ExportRoot,
    root: PathBuf,
}

#[async_trait::async_trait]
impl ToolExecutor for ArtifactPublish {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let input = args.clone();
        let args: ArtifactPublishArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(error) => {
                return Ok(ToolOutcome::error_code(
                    "invalid_arguments",
                    error.to_string(),
                    None::<String>,
                ))
            }
        };
        let filename = match args.filename {
            Some(filename) => validate_filename(filename)?,
            None => Path::new(&args.path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("invalid artifact filename"))?,
        };
        let metadata = match self.store.stage_from_export_root(
            &self.exports,
            &args.path,
            ArtifactMetadataInput {
                filename,
                media_type: args.media_type,
                caption: args.caption,
            },
        ) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Ok(ToolOutcome::error_code(
                    "artifact_publish_failed",
                    error.to_string(),
                    None::<String>,
                ))
            }
        };
        let structured = json!({
            "uri": format!("dar-artifact://{}", metadata.id),
            "filename": metadata.filename,
            "mediaType": metadata.media_type,
            "bytes": metadata.bytes,
            "title": args.title,
        });
        let sha256 = metadata.sha256_hex();
        let resource = ToolContent::ResourceLink {
            uri: format!("dar-artifact://{}", metadata.id),
            name: metadata.filename,
            mime_type: metadata.media_type,
            bytes: metadata.bytes,
            sha256,
            caption: metadata.caption,
        };
        let mut outcome = ToolOutcome {
            text: "artifact published".to_string(),
            content: vec![
                ToolContent::Text {
                    text: "artifact published".to_string(),
                },
                resource,
            ],
            is_error: false,
            error: None,
        };
        let mut result = outcome.to_mcp_result();
        result["structuredContent"] = structured;
        let record = ResultRecord {
            tool: "artifact_publish".into(),
            pi_tool_name: Some("mcp__dar__artifact_publish".into()),
            server: "dar".into(),
            resource_uri: "ui://dar/artifact-viewer".into(),
            args: input,
            result,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        match record.write(&self.root) {
            Ok(id) => {
                outcome.text.push_str(&format!("\n[mcp-app-result:{id}]"));
                outcome.content[0] = ToolContent::Text {
                    text: outcome.text.clone(),
                };
            }
            Err(_) => dar_extension_sdk::log::event(
                "",
                "artifact.capture-failed",
                "Artifact view result capture failed; using text fallback",
            ),
        }
        Ok(outcome)
    }
}

fn validate_filename(filename: String) -> Result<String> {
    if filename.is_empty()
        || Path::new(&filename)
            .file_name()
            .and_then(|value| value.to_str())
            != Some(filename.as_str())
    {
        bail!("invalid artifact filename");
    }
    Ok(filename)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn publish_description_shows_absolute_export_folder() {
        let dir = tempfile::tempdir().unwrap();
        let vault = tempfile::tempdir().unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let mut ctx = RegisterCtx {
            bus: Default::default(),
            http: host_api::HttpRegistry::disabled(),
            foreground: Default::default(),
            services: Default::default(),
            paths: host_api::HostPaths::new(dir.path())
                .unwrap()
                .with_artifact_root(vault.path())
                .unwrap(),
            config: Default::default(),
            shutdown: host_api::ShutdownToken::new(rx),
        };
        ToolRegistryHostExtension.register(&mut ctx).await.unwrap();
        let registry = ctx
            .services
            .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
            .unwrap();
        let spec = registry
            .list()
            .into_iter()
            .find(|spec| spec.name == "artifact_publish")
            .unwrap();
        let exports = dir
            .path()
            .join("data/artifact-exports")
            .canonicalize()
            .unwrap();
        assert!(exports.is_absolute());
        assert!(spec.description.contains(exports.to_str().unwrap()));
    }

    #[test]
    fn artifact_limit_matches_cross_surface_policy() {
        assert_eq!(MAX_ARTIFACT_BYTES, 25 * 1024 * 1024);
    }

    #[tokio::test]
    async fn publish_keeps_resource_link_alongside_app_marker() {
        let dir = tempfile::tempdir().unwrap();
        let exports_dir = dir.path().join("exports");
        fs::create_dir(&exports_dir).unwrap();
        fs::write(exports_dir.join("report.txt"), "hello").unwrap();
        let publish = ArtifactPublish {
            store: Arc::new(ArtifactStore::open(dir.path().join("vault"), 1024).unwrap()),
            exports: ExportRoot::open(exports_dir).unwrap(),
            root: dir.path().to_path_buf(),
        };
        let outcome = publish
            .execute(json!({"path": "report.txt"}))
            .await
            .unwrap();
        assert!(!outcome.is_error);
        assert!(
            matches!(outcome.content.as_slice(), [ToolContent::Text { .. }, ToolContent::ResourceLink { uri, name, bytes: 5, .. }] if uri.starts_with("dar-artifact://") && name == "report.txt")
        );
    }
}
