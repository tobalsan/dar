//! Host-side MCP Apps connections, independent from each runner's bridge.
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use base64::Engine;
use dar_extension_sdk::{mcp_apps::McpApps, BoxFuture, BridgeSecrets};
use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams, Tool};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::{discover, Client, Config, Server};

pub(crate) struct Catalog {
    root: PathBuf,
    servers: BTreeMap<String, ServerConnection>,
    secrets: Arc<BridgeSecrets>,
}
struct ServerConnection {
    config: Server,
    connected: Mutex<Option<Arc<Connected>>>,
}
struct Connected {
    client: Client,
    tools: Vec<Tool>,
    resources: Mutex<BTreeMap<String, Value>>,
}

impl Catalog {
    pub(crate) fn new(root: PathBuf, config: Config) -> Self {
        Self {
            root,
            servers: config
                .mcp_servers
                .into_iter()
                .map(|(name, config)| {
                    (
                        name,
                        ServerConnection {
                            config,
                            connected: Mutex::new(None),
                        },
                    )
                })
                .collect(),
            secrets: Arc::new(BridgeSecrets::default()),
        }
    }

    async fn connection(&self, name: &str) -> Result<Arc<Connected>> {
        let server = self.servers.get(name).context("unknown MCP server")?;
        let mut slot = server.connected.lock().await;
        if let Some(connection) = slot.as_ref() {
            return Ok(Arc::clone(connection));
        }
        let (client, tools) = tokio::time::timeout(
            Duration::from_secs(15),
            discover(
                &self.root,
                name,
                server.config.clone().resolve(&self.secrets)?,
                &self.secrets,
            ),
        )
        .await
        .context("MCP Apps server discovery timed out")??;
        let connection = Arc::new(Connected {
            client,
            tools,
            resources: Mutex::new(BTreeMap::new()),
        });
        *slot = Some(Arc::clone(&connection));
        Ok(connection)
    }
}

pub(crate) fn meta(tool: &Tool) -> Value {
    tool.meta
        .as_ref()
        .map(|meta| json!(meta))
        .unwrap_or_else(|| json!({}))
}
pub(crate) fn resource_uri(tool: &Tool) -> Option<String> {
    let meta = meta(tool);
    meta.pointer("/ui/resourceUri")
        .or_else(|| meta.get("ui/resourceUri"))
        .and_then(Value::as_str)
        .filter(|uri| uri.starts_with("ui://"))
        .map(str::to_owned)
}
pub(crate) fn visible(tool: &Tool, audience: &str) -> bool {
    match meta(tool).pointer("/ui/visibility") {
        None => true,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(audience)),
        Some(_) => false,
    }
}

impl McpApps for Catalog {
    fn list_tools(&self) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            let connections = futures_util::future::join_all(
                self.servers
                    .keys()
                    .map(|name| async move { (name, self.connection(name).await) }),
            )
            .await;
            let mut tools = Vec::new();
            for (name, connection) in connections {
                let connection = match connection {
                    Ok(connection) => connection,
                    Err(_) => {
                        tracing::warn!(server=%name, "MCP Apps catalog server unavailable");
                        continue;
                    }
                };
                for tool in &connection.tools {
                    if let Some(uri) = resource_uri(tool) {
                        let meta = meta(tool);
                        tools.push(json!({"name":format!("{name}__{}", tool.name), "server":name,
                            "resourceUri":uri, "visibility":meta.pointer("/ui/visibility").cloned().unwrap_or_else(|| json!(["model","app"])), "meta":meta}));
                    }
                }
            }
            Ok(Value::Array(tools))
        })
    }
    fn read_ui_resource<'a>(
        &'a self,
        server: &'a str,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let connection = self.connection(server).await?;
            if !connection
                .tools
                .iter()
                .any(|tool| resource_uri(tool).as_deref() == Some(uri))
            {
                bail!("resource is not a declared MCP App view");
            }
            let mut cache = connection.resources.lock().await;
            if let Some(value) = cache.get(uri) {
                return Ok(value.clone());
            }
            let raw = tokio::time::timeout(
                Duration::from_secs(30),
                connection
                    .client
                    .read_resource(ReadResourceRequestParams::new(uri)),
            )
            .await
            .context("MCP App resource read timed out")??;
            let raw = serde_json::to_value(raw)?;
            let content = raw["contents"]
                .as_array()
                .and_then(|contents| {
                    contents.iter().find(|content| {
                        content["uri"].as_str() == Some(uri)
                            && content["mimeType"].as_str() == Some("text/html;profile=mcp-app")
                    })
                })
                .context("MCP App HTML resource was not returned")?;
            let html = match content["text"].as_str() {
                Some(html) => html.to_owned(),
                None => {
                    let blob = content["blob"]
                        .as_str()
                        .context("MCP App resource contains no HTML")?;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(blob)
                        .context("MCP App HTML blob is not valid base64")?;
                    String::from_utf8(bytes).context("MCP App HTML blob is not valid UTF-8")?
                }
            };
            let value = json!({"html":html,"meta":content.get("_meta").cloned().unwrap_or_else(|| json!({}))});
            cache.insert(uri.to_owned(), value.clone());
            Ok(value)
        })
    }
    fn call_tool<'a>(
        &'a self,
        server: &'a str,
        tool: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let connection = self.connection(server).await?;
            if !connection
                .tools
                .iter()
                .any(|candidate| candidate.name == tool && visible(candidate, "app"))
            {
                bail!("tool is not app-visible on this MCP server");
            }
            let arguments = args
                .as_object()
                .context("tool arguments must be an object")?
                .clone();
            let result = tokio::time::timeout(
                Duration::from_secs(120),
                connection.client.call_tool(
                    CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments),
                ),
            )
            .await
            .context("MCP App tool call timed out")??;
            Ok(serde_json::to_value(result)?)
        })
    }
    fn read_resource<'a>(&'a self, server: &'a str, uri: &'a str) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let connection = self.connection(server).await?;
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                connection
                    .client
                    .read_resource(ReadResourceRequestParams::new(uri)),
            )
            .await
            .context("MCP App resource read timed out")??;
            Ok(serde_json::to_value(result)?)
        })
    }
}
