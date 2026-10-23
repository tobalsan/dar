//! Built-in, model-only artifact viewer and verified vault resources.
use anyhow::{Context, Result};
use base64::Engine;
use dar_extension_sdk::{
    artifacts::{ArtifactMetadata, ArtifactStore},
    mcp_apps::{BoxFuture, McpApps},
};
use serde_json::{json, Value};
use std::{io::Read, sync::Arc};

pub(super) struct ArtifactApp {
    pub store: Arc<ArtifactStore>,
}

impl McpApps for ArtifactApp {
    fn list_tools(&self) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async {
            Ok(json!([{"name":"artifact_publish", "server":"dar",
                "resourceUri":"ui://dar/artifact-viewer", "visibility":["model"]}]))
        })
    }

    fn read_ui_resource<'a>(
        &'a self,
        server: &'a str,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            anyhow::ensure!(
                server == "dar" && uri == "ui://dar/artifact-viewer",
                "unknown artifact viewer"
            );
            let html = include_str!("artifact-viewer.html")
                .replace("/*__MARKED__*/", include_str!("vendor/marked.min.js"))
                .replace("/*__DOMPURIFY__*/", include_str!("vendor/purify.min.js"));
            Ok(
                json!({"html":html,"meta":{"ui":{"prefersBorder":true,"csp":{
                    "resourceDomains":["https://cdn.jsdelivr.net","https://cdnjs.cloudflare.com","https://unpkg.com","https://fonts.googleapis.com","https://fonts.gstatic.com"],
                    "connectDomains":[],"frameDomains":[]
                }}}}),
            )
        })
    }

    fn call_tool<'a>(&'a self, _: &'a str, _: &'a str, _: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async { anyhow::bail!("artifact views cannot call tools") })
    }

    fn read_resource<'a>(&'a self, server: &'a str, uri: &'a str) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            anyhow::ensure!(server == "dar", "unknown artifact server");
            let (metadata, bytes) = read(Arc::clone(&self.store), uri).await?;
            let media_type = metadata
                .media_type
                .as_deref()
                .unwrap_or("application/octet-stream");
            let mut resource = json!({"uri":uri,"mimeType":media_type});
            if media_type.starts_with("text/") || media_type == "image/svg+xml" {
                resource["text"] =
                    json!(String::from_utf8(bytes).context("artifact is not valid UTF-8")?);
            } else {
                resource["blob"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
            }
            Ok(json!({"contents":[resource]}))
        })
    }
}

pub(super) async fn read(
    store: Arc<ArtifactStore>,
    uri: &str,
) -> Result<(ArtifactMetadata, Vec<u8>)> {
    let id = uri
        .strip_prefix("dar-artifact://")
        .context("invalid artifact URI")?
        .parse()?;
    tokio::task::spawn_blocking(move || {
        let mut artifact = store.open_verified(id)?;
        let metadata = artifact.metadata().clone();
        let mut bytes = Vec::new();
        artifact.read_to_end(&mut bytes)?;
        Ok((metadata, bytes))
    })
    .await?
}
