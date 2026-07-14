//! Publishes shared tool registry and built-in artifact publisher.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use dar_artifacts::{ArtifactMetadataInput, ArtifactStore, ExportRoot};
use host_api::{Extension, RegisterCtx};
use serde::Deserialize;
use serde_json::{json, Value};
use tool_registry::{
    ToolContent, ToolExecutor, ToolOutcome, ToolRegistry, ToolRegistryHandle, ToolSpec,
    TOOL_REGISTRY_SERVICE,
};

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
            let exports = ExportRoot::open(exports_path)?;
            registry.register_tool(
                artifact_publish_spec(),
                Arc::new(ArtifactPublish {
                    store: Arc::clone(&store),
                    exports,
                }),
            )?;
            ctx.services
                .service::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE, registry)?;
            ctx.services.service(ARTIFACT_STORE_SERVICE, store)?;
            Ok(())
        })
    }
}

fn artifact_publish_spec() -> ToolSpec {
    ToolSpec::new(
        "artifact_publish",
        "Publish a file up to 25 MiB from data/artifact-exports into host-private immutable storage.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path"],
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "filename": { "type": "string", "minLength": 1 },
                "mediaType": { "type": "string", "minLength": 1 },
                "caption": { "type": "string" }
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
}

struct ArtifactPublish {
    store: Arc<ArtifactStore>,
    exports: ExportRoot,
}

#[async_trait::async_trait]
impl ToolExecutor for ArtifactPublish {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
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
        let sha256 = metadata.sha256_hex();
        let resource = ToolContent::ResourceLink {
            uri: format!("dar-artifact://{}", metadata.id),
            name: metadata.filename,
            mime_type: metadata.media_type,
            bytes: metadata.bytes,
            sha256,
            caption: metadata.caption,
        };
        Ok(ToolOutcome {
            text: "artifact published".to_string(),
            content: vec![resource],
            is_error: false,
            error: None,
        })
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

    #[test]
    fn artifact_limit_matches_cross_surface_policy() {
        assert_eq!(MAX_ARTIFACT_BYTES, 25 * 1024 * 1024);
    }

    #[tokio::test]
    async fn publish_returns_only_resource_link() {
        let dir = tempfile::tempdir().unwrap();
        let exports_dir = dir.path().join("exports");
        fs::create_dir(&exports_dir).unwrap();
        fs::write(exports_dir.join("report.txt"), "hello").unwrap();
        let publish = ArtifactPublish {
            store: Arc::new(ArtifactStore::open(dir.path().join("vault"), 1024).unwrap()),
            exports: ExportRoot::open(exports_dir).unwrap(),
        };
        let outcome = publish
            .execute(json!({"path": "report.txt"}))
            .await
            .unwrap();
        assert!(!outcome.is_error);
        assert!(
            matches!(outcome.content.as_slice(), [ToolContent::ResourceLink { uri, name, bytes: 5, .. }] if uri.starts_with("dar-artifact://") && name == "report.txt")
        );
    }
}
