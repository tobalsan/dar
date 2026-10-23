//! Built-in, model-only artifact and web preview views, with verified vault resources.
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

pub(super) fn resource_policy() -> Value {
    json!({
        "resourceDomains":["https://cdn.jsdelivr.net","https://cdnjs.cloudflare.com","https://unpkg.com","https://fonts.googleapis.com","https://fonts.gstatic.com"],
        "connectDomains":[],"frameDomains":[]
    })
}

pub(super) fn open_media_type(filename: &str, declared: Option<&str>) -> Option<&'static str> {
    let extension = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let fallback = match extension.as_str() {
        "html" | "htm" => "text/html",
        "md" | "markdown" => "text/markdown",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" | "rs" | "py" | "js" | "cjs" | "mjs" | "ts" | "tsx" | "jsx" | "json"
        | "yaml" | "yml" | "toml" | "css" | "sh" | "bash" | "zsh" | "sql" | "go" | "java" | "c"
        | "h" | "cpp" | "hpp" | "cs" | "rb" | "php" | "swift" | "xml" | "csv" => "text/plain",
        _ => "application/octet-stream",
    };
    let mime = declared
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "text/html" => Some("text/html"),
        "text/markdown" => Some("text/markdown"),
        "application/pdf" => Some("application/pdf"),
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "image/svg+xml" => Some("image/svg+xml"),
        value
            if value.starts_with("text/")
                || matches!(
                    value,
                    "application/json"
                        | "application/javascript"
                        | "application/xml"
                        | "application/x-sh"
                        | "application/toml"
                        | "application/yaml"
                        | "application/x-yaml"
                ) =>
        {
            Some("text/plain; charset=utf-8")
        }
        _ => None,
    }
}

pub(super) fn open_markdown(text: &str) -> Result<String> {
    let data = serde_json::to_string(text)?.replace('<', "\\u003c");
    Ok(format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><body><script>{}\n{}\n{}\nconst page = DarArtifactDocument.markdown({data}); document.open(); document.write(page); document.close();</script></body></html>",
        include_str!("vendor/marked.min.js"),
        include_str!("vendor/purify.min.js"),
        include_str!("artifact-document.js"),
    ))
}

impl McpApps for ArtifactApp {
    fn list_tools(&self) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async {
            Ok(json!([{"name":"artifact_publish", "server":"dar",
                "resourceUri":"ui://dar/artifact-viewer", "visibility":["model"]},
                {"name":"web_preview", "server":"dar",
                "resourceUri":"ui://dar/web-preview", "visibility":["model"]}]))
        })
    }

    fn read_ui_resource<'a>(
        &'a self,
        server: &'a str,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            anyhow::ensure!(server == "dar", "unknown built-in view server");
            let (html, csp) = if uri == "ui://dar/web-preview" {
                (
                    include_str!("web-preview.html").to_owned(),
                    json!({
                        "resourceDomains":[], "connectDomains":[], "frameDomains":[]
                    }),
                )
            } else {
                anyhow::ensure!(uri == "ui://dar/artifact-viewer", "unknown built-in view");
                let html = include_str!("artifact-viewer.html")
                    .replace("/*__MARKED__*/", include_str!("vendor/marked.min.js"))
                    .replace("/*__DOMPURIFY__*/", include_str!("vendor/purify.min.js"))
                    .replace(
                        "/*__ARTIFACT_DOCUMENT__*/",
                        include_str!("artifact-document.js"),
                    );
                (html, resource_policy())
            };
            Ok(json!({"html":html,"meta":{"ui":{"prefersBorder":true,"csp":csp}}}))
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
