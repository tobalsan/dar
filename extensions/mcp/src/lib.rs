//! User-configured MCP tool servers, attached only inside host bridge process.

use std::{collections::BTreeMap, path::Path, process::Stdio, sync::Arc, time::Duration};

pub mod credentials;
mod dashboard;
pub mod login;

use anyhow::{bail, Context, Result};
use cap_dashboard_tab::DashboardTabs;
use dar_extension_sdk::{
    tools::{
        Redactor, ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec, TOOL_REGISTRY_SERVICE,
    },
    BoxFuture, BridgeSecrets, Extension, McpBridgeMode, RegisterCtx, BRIDGE_SECRETS_SERVICE,
    MCP_BRIDGE_MODE_SERVICE,
};
use rmcp::{
    model::CallToolRequestParams,
    service::{RoleClient, RunningService},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, ConfigureCommandExt,
        StreamableHttpClientTransport, TokioChildProcess,
    },
    ServiceExt,
};
use serde::Deserialize;
use serde_json::Value;

pub struct McpExtension;

#[derive(Debug)]
pub enum DoctorStatus {
    Ok(usize),
    NeedsLogin,
    Unreachable(String),
}

pub async fn servers_needing_login(root: &Path) -> Result<Vec<String>> {
    let config = load(root)?;
    let secrets = Arc::new(BridgeSecrets::default());
    let futures = config
        .mcp_servers
        .into_iter()
        .filter_map(|(name, server)| match &server {
            Server::Http { headers, .. }
                if !headers
                    .keys()
                    .any(|key| key.eq_ignore_ascii_case("authorization"))
                    && !root
                        .join("data/mcp-auth")
                        .join(format!("{name}.json"))
                        .exists() =>
            {
                let root = root.to_path_buf();
                let secrets = Arc::clone(&secrets);
                Some(async move {
                    let result = tokio::time::timeout(
                        Duration::from_secs(15),
                        discover(&root, &name, server, &secrets),
                    )
                    .await;
                    match result {
                        Ok(Err(error)) if needs_login(&error) => Some(name),
                        _ => None,
                    }
                })
            }
            _ => None,
        });
    Ok(futures_util::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect())
}

pub async fn doctor_statuses(root: &Path) -> Result<Vec<(String, DoctorStatus)>> {
    let config = load(root)?;
    let secrets = Arc::new(BridgeSecrets::default());
    let futures = config.mcp_servers.into_iter().map(|(name, server)| async {
        let result = match server.resolve(&secrets) {
            Ok(server) => {
                tokio::time::timeout(
                    Duration::from_secs(15),
                    discover(root, &name, server, &secrets),
                )
                .await
            }
            Err(error) => return (name, DoctorStatus::Unreachable(error.to_string())),
        };
        let status = match result {
            Ok(Ok((_client, tools))) => DoctorStatus::Ok(tools.len()),
            Ok(Err(error)) if needs_login(&error) => DoctorStatus::NeedsLogin,
            Ok(Err(error)) => {
                let redactor = Redactor::from_secret_values(secrets.values());
                DoctorStatus::Unreachable(redactor.redact(&format!("{error:#}")))
            }
            Err(_) => DoctorStatus::Unreachable("connection timed out".into()),
        };
        (name, status)
    });
    Ok(futures_util::future::join_all(futures).await)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    #[serde(default)]
    mcp_servers: BTreeMap<String, Server>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum Server {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

type Client = RunningService<RoleClient, ()>;

impl Server {
    fn resolve(self, secrets: &BridgeSecrets) -> Result<Self> {
        match self {
            Self::Stdio { command, args, env } => {
                let env = expand_map(env)?;
                secrets.extend(env.values().cloned());
                Ok(Self::Stdio { command, args, env })
            }
            Self::Http { url, headers } => {
                let headers = expand_map(headers)?;
                secrets.extend(headers.values().flat_map(|value| {
                    let mut values = vec![value.clone()];
                    if let Some((_, credential)) = value.split_once(' ') {
                        if !credential.is_empty() {
                            values.push(credential.to_owned());
                        }
                    }
                    values
                }));
                Ok(Self::Http { url, headers })
            }
        }
    }
}

impl Extension for McpExtension {
    fn id(&self) -> &'static str {
        "mcp"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let config = load(ctx.paths.root())?;
            if config.mcp_servers.is_empty() {
                return Ok(());
            }
            if ctx
                .services
                .get::<McpBridgeMode>(MCP_BRIDGE_MODE_SERVICE)
                .is_err()
            {
                let state = dashboard::DashboardState::new(ctx.paths.root().to_path_buf(), config);
                DashboardTabs::shared(&mut ctx.services)?
                    .add(Arc::new(dashboard::McpTab::new(state.clone())))?;
                ctx.http.mount(host_api::HttpMount {
                    namespace: "/mcp".into(),
                    router: dashboard::router(state),
                    routes: dashboard::routes(),
                    claim_root: false,
                })?;
                return Ok(());
            }
            let registry = ctx
                .services
                .get::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)?;
            let secrets = ctx.services.get::<BridgeSecrets>(BRIDGE_SECRETS_SERVICE)?;
            let root = ctx.paths.root().to_path_buf();
            let tasks = config.mcp_servers.into_iter().map(|(name, server)| {
                let root = root.clone();
                let secrets = Arc::clone(&secrets);
                async move {
                    let resolved = server.resolve(&secrets);
                    let result = match resolved {
                        Ok(server) => {
                            tokio::time::timeout(
                                Duration::from_secs(15),
                                discover(&root, &name, server, &secrets),
                            )
                            .await
                        }
                        Err(error) => return (name, Ok(Err(error))),
                    };
                    (name, result)
                }
            });
            for (name, result) in futures_util::future::join_all(tasks).await {
                let client = match result {
                    Ok(Ok(client)) => client,
                    Ok(Err(error)) => {
                        tracing::warn!(server=%name, "MCP server unavailable: {error:#}");
                        continue;
                    }
                    Err(_) => {
                        tracing::warn!(server=%name, "MCP server connection timed out");
                        continue;
                    }
                };
                let (client, tools) = client;
                let client = Arc::new(client);
                for tool in tools {
                    let mapped = format!("{name}__{}", tool.name);
                    if !valid_tool_name(&mapped) {
                        tracing::warn!(server=%name, tool=%tool.name, "skipping invalid mapped MCP tool name");
                        continue;
                    }
                    let schema = match serde_json::to_value(&tool.input_schema) {
                        Ok(schema) => schema,
                        Err(error) => {
                            tracing::warn!(server=%name, tool=%tool.name, "skipping MCP tool with invalid schema: {error}");
                            continue;
                        }
                    };
                    if let Err(error) = registry.register_tool(
                        ToolSpec::new(mapped, tool.description.unwrap_or_default(), schema),
                        Arc::new(Proxy {
                            client: Arc::clone(&client),
                            server: name.clone(),
                            upstream: tool.name.to_string(),
                        }),
                    ) {
                        tracing::warn!(server=%name, tool=%tool.name, "skipping MCP tool: {error:#}");
                    }
                }
                // Executors retain client and therefore transport for bridge lifetime.
            }
            Ok(())
        })
    }
}

async fn discover(
    root: &Path,
    name: &str,
    server: Server,
    secrets: &Arc<BridgeSecrets>,
) -> Result<(Client, Vec<rmcp::model::Tool>)> {
    let client = connect(root, name, server, secrets).await?;
    let tools = client.list_all_tools().await?;
    Ok((client, tools))
}

async fn connect(
    root: &Path,
    server_name: &str,
    server: Server,
    secrets: &Arc<BridgeSecrets>,
) -> Result<Client> {
    match server {
        Server::Stdio { command, args, env } => {
            let transport =
                TokioChildProcess::new(tokio::process::Command::new(command).configure(|cmd| {
                    // Child diagnostics may contain configured secrets; discard instead of inheriting
                    // unredacted stderr into bridge logs.
                    cmd.args(args)
                        .envs(env)
                        .current_dir(root)
                        .stderr(Stdio::null());
                }))?;
            Ok(().serve(transport).await?)
        }
        Server::Http { url, headers } => {
            let has_static_auth = headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization"));
            let headers = headers
                .into_iter()
                .map(|(name, value)| {
                    Ok((
                        name.parse::<http::HeaderName>()?,
                        value.parse::<http::HeaderValue>()?,
                    ))
                })
                .collect::<Result<std::collections::HashMap<_, _>>>()?;
            let config =
                StreamableHttpClientTransportConfig::with_uri(url.clone()).custom_headers(headers);
            if has_static_auth {
                return Ok(
                    ().serve(StreamableHttpClientTransport::from_config(config))
                        .await?,
                );
            }
            let store = credentials::FileCredentialStore::new(root, server_name)?
                .with_secrets(Arc::clone(secrets));
            secrets.extend(store.secret_values()?);
            let mut manager = rmcp::transport::AuthorizationManager::new(&url).await?;
            manager.set_credential_store(store);
            if !manager.initialize_from_store().await? {
                // No stored login: the server may be public or authenticate via
                // a custom header. Only an auth rejection means "log in".
                return match ().serve(StreamableHttpClientTransport::from_config(config)).await {
                    Ok(client) => Ok(client),
                    Err(error) => {
                        let error = anyhow::Error::from(error);
                        if needs_login(&error) {
                            bail!("not logged in; run `dar mcp login {server_name}`");
                        }
                        Err(error)
                    }
                };
            }
            let auth_client = rmcp::transport::AuthClient::new(reqwest::Client::new(), manager);
            Ok(()
                .serve(StreamableHttpClientTransport::with_client(
                    auth_client,
                    config,
                ))
                .await?)
        }
    }
}

struct Proxy {
    client: Arc<Client>,
    server: String,
    upstream: String,
}

#[async_trait::async_trait]
impl ToolExecutor for Proxy {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let arguments = match args {
            Value::Object(map) => Some(map),
            _ => bail!("tool arguments must be an object"),
        };
        let mut params = CallToolRequestParams::new(self.upstream.clone());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let result =
            match tokio::time::timeout(Duration::from_secs(120), self.client.call_tool(params))
                .await
            {
                Ok(Ok(result)) => result,
                // Most likely an expired OAuth session that could not refresh.
                Ok(Err(error)) => {
                    return Ok(ToolOutcome::error(format!(
                        "MCP server {} call failed: {error:#}. If its login expired, run `dar mcp login {}`",
                        self.server, self.server
                    )))
                }
                Err(_) => {
                    return Ok(ToolOutcome::error(
                        "upstream MCP tool call timed out after 120 seconds",
                    ))
                }
            };
        let value = serde_json::to_value(&result)?;
        let text = result_text(&value);
        Ok(
            if value
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                ToolOutcome::error(text)
            } else {
                ToolOutcome::ok(text)
            },
        )
    }
}

fn load(root: &Path) -> Result<Config> {
    let path = root.join("mcp.json");
    if !path.exists() {
        return Ok(Config {
            mcp_servers: BTreeMap::new(),
        });
    }
    let config: Config = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    for name in config.mcp_servers.keys() {
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            bail!("invalid MCP server name {name:?}: expected ^[A-Za-z0-9-]+$");
        }
    }
    Ok(config)
}

fn expand_map(values: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    values
        .into_iter()
        .map(|(key, value)| {
            let value = match value.strip_prefix("$env:") {
                Some(name) => std::env::var(name)
                    .with_context(|| format!("environment variable {name} is not set"))?,
                None => value,
            };
            Ok((key, value))
        })
        .collect()
}

fn result_text(value: &Value) -> String {
    let mut output = Vec::new();
    for item in value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    output.push(text.to_owned());
                }
            }
            Some("image") | Some("audio") => {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("media");
                let mime = item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("application/octet-stream");
                let bytes = item
                    .get("data")
                    .and_then(Value::as_str)
                    .map(|s| s.len() * 3 / 4)
                    .unwrap_or(0);
                output.push(format!("[{kind}: {mime}, {bytes} bytes]"));
            }
            Some("resource_link") | Some("resourceLink") => {
                output.push(format!(
                    "[resource: {}]",
                    item.get("uri").and_then(Value::as_str).unwrap_or("unknown")
                ));
            }
            Some("resource") => {
                if let Some(text) = item.pointer("/resource/text").and_then(Value::as_str) {
                    output.push(text.to_owned());
                } else if let Some(uri) = item.pointer("/resource/uri").and_then(Value::as_str) {
                    output.push(format!("[resource: {uri}]"));
                }
            }
            _ => output.push(item.to_string()),
        }
    }
    if let Some(structured) = value.get("structuredContent") {
        output.push(structured.to_string());
    }
    output.join("\n")
}

fn needs_login(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("not logged in")
        || message.contains("auth required")
        || message.contains("401")
        || message.contains("authorization")
}

fn valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_and_validates_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{"mcpServers":{"local":{"command":"echo","args":["x"]}}}"#,
        )
        .unwrap();
        assert_eq!(load(dir.path()).unwrap().mcp_servers.len(), 1);
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{"mcpServers":{"bad name":{"url":"http://x"}}}"#,
        )
        .unwrap();
        assert!(load(dir.path()).is_err());
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{"mcpServers":{"bad__name":{"url":"http://x"}}}"#,
        )
        .unwrap();
        assert!(load(dir.path()).is_err());
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{"mcpServers":{"bad_name":{"url":"http://x"}}}"#,
        )
        .unwrap();
        assert!(load(dir.path()).is_err());
    }
    #[test]
    fn expands_env_and_checks_tool_names() {
        std::env::set_var("DAR_MCP_TEST", "secret");
        let map = expand_map(BTreeMap::from([(
            "Authorization".into(),
            "$env:DAR_MCP_TEST".into(),
        )]))
        .unwrap();
        assert_eq!(map["Authorization"], "secret");
        assert!(valid_tool_name("server__tool-1"));
        assert!(!valid_tool_name("bad.tool"));
        assert!(!valid_tool_name(&"x".repeat(65)));
    }
    #[test]
    fn classifies_auth_rejection_as_needs_login() {
        let rmcp_401 =
            anyhow::anyhow!("Transport error: Auth required, when send initialize request");
        assert!(needs_login(&rmcp_401));
        assert!(!needs_login(&anyhow::anyhow!("connection refused")));
    }

    #[tokio::test]
    async fn auto_login_probe_selects_only_unauthorized_server() {
        use axum::{http::StatusCode, routing::any, Router};
        let unauthorized = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unauthorized_addr = unauthorized.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                unauthorized,
                Router::new().fallback(any(|| async { StatusCode::UNAUTHORIZED })),
            )
            .await
            .unwrap();
        });
        let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let public_addr = public.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                public,
                Router::new().fallback(any(|| async { StatusCode::OK })),
            )
            .await
            .unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("mcp.json"),
            serde_json::json!({"mcpServers": {
                "locked": {"url": format!("http://{unauthorized_addr}")},
                "public": {"url": format!("http://{public_addr}")},
                "local": {"command":"do-not-spawn"}
            }})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            servers_needing_login(root.path()).await.unwrap(),
            vec!["locked"]
        );
    }

    #[test]
    fn preserves_non_text_results() {
        let value = serde_json::json!({"content":[
            {"type":"image","mimeType":"image/png","data":"YWJj"},
            {"type":"resource","resource":{"text":"embedded"}},
            {"type":"resource_link","uri":"file:///x"}
        ],"structuredContent":{"answer":42}});
        assert_eq!(
            result_text(&value),
            "[image: image/png, 3 bytes]\nembedded\n[resource: file:///x]\n{\"answer\":42}"
        );
    }
}
