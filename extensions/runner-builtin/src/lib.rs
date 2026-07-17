//! Builtin runner extension.
//!
//! This crate is the native, zero-install runner entry point.  The OpenAI-compatible
//! streaming implementation will live here so composed agents can use
//! `runner.use: builtin` without spawning pi/codex/opencode helper binaries.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

mod tools;

use anyhow::{anyhow, Context, Result};
use cap_chat::{ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams};
use cap_runner::{ExitKind, Runner, RunnerHandle, SpawnParams};
use futures_util::StreamExt;
use host_api::{Extension, RegisterCtx};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex};

const EVENT_KIND: &str = "runner.builtin";

pub struct RunnerBuiltinExtension;

impl Extension for RunnerBuiltinExtension {
    fn id(&self) -> &'static str {
        "runner-builtin"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            ctx.services
                .service::<dyn Runner>("builtin", Arc::new(BuiltinRunner))?;
            ctx.services
                .service::<dyn ChatBackend>("builtin", Arc::new(BuiltinChatBackend))?;
            if let Ok(registry) = ctx
                .services
                .get_named::<dyn tool_registry::ToolRegistryHandle>(
                    tool_registry::TOOL_REGISTRY_SERVICE,
                )
            {
                tools::register_into(registry.as_ref(), ctx.paths.root().to_path_buf())?;
            }
            Ok(())
        })
    }
}

pub struct BuiltinRunner;

pub struct BuiltinChatBackend;

impl ChatBackend for BuiltinChatBackend {
    fn open<'a>(
        &'a self,
        params: ChatSessionParams,
        tx: tokio::sync::mpsc::Sender<ChatEvent>,
    ) -> cap_chat::BoxFuture<'a, Result<Box<dyn ChatSession>>> {
        Box::pin(async move {
            Ok(Box::new(BuiltinChatSession {
                params: Arc::new(params),
                tx,
                messages: Arc::new(Mutex::new(Vec::new())),
            }) as Box<dyn ChatSession>)
        })
    }
}

struct BuiltinChatSession {
    params: Arc<ChatSessionParams>,
    tx: tokio::sync::mpsc::Sender<ChatEvent>,
    messages: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl ChatSession for BuiltinChatSession {
    fn send_turn(&mut self, prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
        let params = Arc::clone(&self.params);
        let tx = self.tx.clone();
        let messages = Arc::clone(&self.messages);
        Box::pin(async move {
            tokio::spawn(async move {
                if let Err(err) = run_builtin_chat_turn(params, tx.clone(), messages, prompt).await
                {
                    let message = format!("{err:#}");
                    let _ = tx.send(ChatEvent::Error(message.clone())).await;
                    let _ = tx
                        .send(ChatEvent::TurnFinished {
                            ok: false,
                            error: Some(message),
                        })
                        .await;
                }
            });
            Ok(())
        })
    }

    fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn close(self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

async fn run_builtin_chat_turn(
    params: Arc<ChatSessionParams>,
    tx: tokio::sync::mpsc::Sender<ChatEvent>,
    messages: Arc<Mutex<Vec<serde_json::Value>>>,
    prompt: String,
) -> Result<()> {
    let provider = params
        .provider
        .as_deref()
        .context("builtin chat requires runner.provider")?;
    let (base_url, api_key) = provider_endpoint(&params.agent_root, provider)?;
    let mut request_messages = {
        let mut guard = messages.lock().await;
        if guard.is_empty() {
            if let Some(system) = params.system_prompt.as_deref().filter(|s| !s.is_empty()) {
                guard.push(serde_json::json!({"role": "system", "content": system}));
            }
        }
        guard.push(serde_json::json!({"role": "user", "content": prompt}));
        guard.clone()
    };
    let client = reqwest::Client::new();
    let mut bridge = match params.host_tool_bridge.clone() {
        Some(bridge) => Some(McpBridgeClient::spawn(bridge).await?),
        None => None,
    };
    let tools = match bridge.as_mut() {
        Some(bridge) => bridge.openai_tools().await?,
        None => Vec::new(),
    };
    let model = params.model.as_deref().unwrap_or("openai/gpt-4o-mini");
    for _ in 0..8 {
        let outcome = stream_chat_completion_to_chat(
            &client,
            &base_url,
            &api_key,
            model,
            &request_messages,
            &tools,
            &tx,
        )
        .await?;
        if outcome.tool_calls.is_empty() {
            messages
                .lock()
                .await
                .push(serde_json::json!({"role": "assistant", "content": outcome.content}));
            let _ = tx
                .send(ChatEvent::TurnFinished {
                    ok: true,
                    error: None,
                })
                .await;
            return Ok(());
        }
        let mut assistant =
            serde_json::json!({"role": "assistant", "tool_calls": outcome.tool_calls});
        if !outcome.content.is_empty() {
            assistant["content"] = serde_json::Value::String(outcome.content);
        }
        request_messages.push(assistant.clone());
        messages.lock().await.push(assistant);
        let bridge = bridge
            .as_mut()
            .context("model requested a tool but no host tool bridge is available")?;
        for call in outcome.tool_calls {
            let id = call["id"].as_str().unwrap_or_default().to_string();
            let name = call["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let args_text = call["function"]["arguments"]
                .as_str()
                .unwrap_or("{}")
                .to_string();
            let args: serde_json::Value =
                serde_json::from_str(&args_text).unwrap_or_else(|_| serde_json::json!({}));
            let _ = tx
                .send(ChatEvent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args: args_text,
                })
                .await;
            let result = bridge.call_tool(&name, args).await?;
            let result_content = openai_tool_message_content(&result);
            let _ = tx
                .send(ChatEvent::ToolOutput {
                    id: id.clone(),
                    text: result_content.to_string(),
                    is_error: false,
                    done: true,
                })
                .await;
            let tool_message = serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result_content,
            });
            request_messages.push(tool_message.clone());
            messages.lock().await.push(tool_message);
        }
    }
    Err(anyhow!("builtin chat exceeded tool-call iteration limit"))
}

impl Runner for BuiltinRunner {
    fn supports_system_prompt(&self) -> bool {
        true
    }

    fn spawn<'a>(
        &self,
        params: SpawnParams<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<RunnerHandle>> + Send + 'a>,
    > {
        Box::pin(async move { spawn_builtin(params).await })
    }
}

async fn spawn_builtin(p: SpawnParams<'_>) -> Result<RunnerHandle> {
    host_api::assert_contained(p.workspace_root, p.workspace)
        .map_err(anyhow::Error::msg)
        .map_err(|e| e.context("workspace containment check failed; refusing builtin run"))?;

    let provider = p.provider.as_deref().unwrap_or("openai-compatible");
    persist_event(
        p.store.as_ref(),
        Some(&p.run_id),
        &p.issue_id,
        serde_json::json!({
            "type": "spawn",
            "runner": p.runner_kind,
            "provider": provider,
            "workspace": p.workspace.display().to_string(),
        }),
    );

    let (kill_tx, mut kill_rx) = oneshot::channel();
    let run = BuiltinRun {
        prompt: p.prompt.clone(),
        model: p.model.clone(),
        provider: p.provider.clone(),
        agent_root: p.agent_root.to_path_buf(),
        host_tool_bridge: p.host_tool_bridge.clone(),
    };
    let issue_id = p.issue_id.clone();
    let run_id = p.run_id.clone();
    let events = Arc::clone(&p.events);
    let store = Arc::clone(&p.store);
    let done = tokio::spawn(async move {
        let run_events = Arc::clone(&events);
        let run_store = Arc::clone(&store);
        let run_id_for_run = run_id.clone();
        let issue_id_for_run = issue_id.clone();
        tokio::select! {
            _ = &mut kill_rx => ExitKind::Interrupted { reason: "killed" },
            result = async move { run_openai_compatible(run, run_events, run_store, run_id_for_run, issue_id_for_run).await } => match result {
                Ok(()) => ExitKind::Normal,
                Err(err) => {
                    let message = format!("builtin runner failed: {err:#}");
                    events.push(format!("[dar:builtin:error] {message}"));
                    persist_event(
                        store.as_ref(),
                        Some(&run_id),
                        &issue_id,
                        serde_json::json!({"type": "error", "message": message}),
                    );
                    ExitKind::Abnormal(Some(1))
                }
            }
        }
    });

    Ok(RunnerHandle::new(std::process::id(), kill_tx, done))
}

struct BuiltinRun {
    prompt: String,
    model: Option<String>,
    provider: Option<String>,
    agent_root: std::path::PathBuf,
    host_tool_bridge: Option<cap_runner::HostToolBridge>,
}

async fn run_openai_compatible(
    p: BuiltinRun,
    events: Arc<dyn cap_runner::RunnerEventSink>,
    store: Arc<dyn cap_runner::RunnerEventStore>,
    run_id: String,
    issue_id: String,
) -> Result<()> {
    let provider = p
        .provider
        .as_deref()
        .context("builtin runner requires runner.provider")?;
    let (base_url, api_key) = provider_endpoint(&p.agent_root, provider)?;
    let model = p.model.as_deref().unwrap_or("openai/gpt-4o-mini");
    let client = reqwest::Client::new();
    let mut messages = vec![serde_json::json!({"role": "user", "content": p.prompt})];
    let mut bridge = match p.host_tool_bridge {
        Some(bridge) => Some(McpBridgeClient::spawn(bridge).await?),
        None => None,
    };
    let tools = match bridge.as_mut() {
        Some(bridge) => bridge.openai_tools().await?,
        None => Vec::new(),
    };

    for _ in 0..8 {
        let request = ProviderRequest {
            client: &client,
            base_url: &base_url,
            api_key: &api_key,
            model,
            messages: &messages,
            tools: &tools,
        };
        let telemetry = RunTelemetry {
            events: events.as_ref(),
            store: store.as_ref(),
            run_id: &run_id,
            issue_id: &issue_id,
        };
        let outcome = stream_chat_completion_with_retries(&request, &telemetry).await?;
        if outcome.tool_calls.is_empty() {
            persist_event(
                store.as_ref(),
                Some(&run_id),
                &issue_id,
                serde_json::json!({
                    "type": "completion",
                    "content_len": outcome.content.len(),
                    "finish_reason": outcome.finish_reason,
                }),
            );
            return Ok(());
        }
        let mut assistant =
            serde_json::json!({"role": "assistant", "tool_calls": outcome.tool_calls});
        if !outcome.content.is_empty() {
            assistant["content"] = serde_json::Value::String(outcome.content);
        }
        messages.push(assistant);
        let bridge = bridge
            .as_mut()
            .context("model requested a tool but no host tool bridge is available")?;
        for call in outcome.tool_calls {
            let id = call["id"].as_str().unwrap_or_default().to_string();
            let name = call["function"]["name"].as_str().unwrap_or_default();
            let args: serde_json::Value =
                serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("{}"))
                    .unwrap_or_else(|_| serde_json::json!({}));
            let call_payload = serde_json::json!({
                "type": "tool_call",
                "id": id,
                "name": name,
                "arguments": args,
            });
            store.insert_event(
                Some(&run_id),
                &issue_id,
                EVENT_KIND,
                &call_payload.to_string(),
                chrono::Utc::now(),
            );
            let result = bridge.call_tool(name, args).await?;
            let result_content = openai_tool_message_content(&result);
            let result_payload = serde_json::json!({
                "type": "tool_result",
                "id": id,
                "name": name,
                "result": result,
            });
            store.insert_event(
                Some(&run_id),
                &issue_id,
                EVENT_KIND,
                &result_payload.to_string(),
                chrono::Utc::now(),
            );
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result_content,
            }));
        }
    }
    Err(anyhow!("builtin runner exceeded tool-call iteration limit"))
}

#[derive(Debug, serde::Deserialize)]
struct AgentProviderConfig {
    #[serde(default, alias = "base_url")]
    api_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct AgentConfigFile {
    #[serde(default)]
    providers: HashMap<String, AgentProviderConfig>,
}

fn openai_tool_message_content(result: &serde_json::Value) -> serde_json::Value {
    let Some(content) = result.get("content").and_then(|v| v.as_array()) else {
        return serde_json::Value::String(result.to_string());
    };
    let has_image = content
        .iter()
        .any(|part| part.get("type").and_then(|v| v.as_str()) == Some("image"));
    if !has_image {
        return serde_json::Value::String(result.to_string());
    }
    serde_json::Value::Array(
        content
            .iter()
            .filter_map(|part| match part.get("type").and_then(|v| v.as_str()) {
                Some("text") => Some(serde_json::json!({
                    "type": "text",
                    "text": part.get("text").and_then(|v| v.as_str()).unwrap_or_default(),
                })),
                Some("image") => {
                    let data = part
                        .get("data")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let mime_type = part
                        .get("mimeType")
                        .and_then(|v| v.as_str())
                        .unwrap_or("image/png");
                    Some(serde_json::json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:{mime_type};base64,{data}") },
                    }))
                }
                _ => None,
            })
            .collect(),
    )
}

fn provider_endpoint(agent_root: &Path, provider: &str) -> Result<(String, String)> {
    let path = agent_root.join("agent.yaml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading provider config from {}", path.display()))?;
    let config: AgentConfigFile = serde_yaml::from_str(&content)
        .with_context(|| format!("parsing provider config from {}", path.display()))?;
    let provider_config = config
        .providers
        .get(provider)
        .with_context(|| format!("builtin provider {provider:?} is not configured in providers"))?;
    let base = resolve_config_value(provider_config.api_url.as_deref())
        .with_context(|| format!("builtin provider {provider:?} missing api_url"))?;
    let key = resolve_config_value(provider_config.api_key.as_deref())
        .with_context(|| format!("builtin provider {provider:?} missing api_key"))?;
    Ok((base, key))
}

fn resolve_config_value(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(name) = value.strip_prefix("$env:") {
        return std::env::var(name).ok();
    }
    Some(value.to_string())
}

#[derive(Default)]
struct ChatOutcome {
    content: String,
    tool_calls: Vec<serde_json::Value>,
    finish_reason: Option<String>,
}

#[derive(Default, Clone)]
struct ToolCallDelta {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug)]
struct ProviderHttpError {
    status: reqwest::StatusCode,
    body: String,
}

impl std::fmt::Display for ProviderHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "builtin provider returned HTTP {}: {}",
            self.status, self.body
        )
    }
}

impl std::error::Error for ProviderHttpError {}

fn is_transient_provider_error(err: &anyhow::Error) -> bool {
    let Some(provider_err) = err.downcast_ref::<ProviderHttpError>() else {
        return false;
    };
    matches!(
        provider_err.status,
        reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    )
}

struct ProviderRequest<'a> {
    client: &'a reqwest::Client,
    base_url: &'a str,
    api_key: &'a str,
    model: &'a str,
    messages: &'a [serde_json::Value],
    tools: &'a [serde_json::Value],
}

struct RunTelemetry<'a> {
    events: &'a dyn cap_runner::RunnerEventSink,
    store: &'a dyn cap_runner::RunnerEventStore,
    run_id: &'a str,
    issue_id: &'a str,
}

async fn stream_chat_completion_with_retries(
    request: &ProviderRequest<'_>,
    telemetry: &RunTelemetry<'_>,
) -> Result<ChatOutcome> {
    let mut last_error = None;
    for attempt in 0..3 {
        match stream_chat_completion(request, telemetry).await {
            Ok(outcome) => return Ok(outcome),
            Err(err) if is_transient_provider_error(&err) && attempt < 2 => {
                let delay = Duration::from_millis(500 * (attempt + 1) as u64);
                persist_event(
                    telemetry.store,
                    Some(telemetry.run_id),
                    telemetry.issue_id,
                    serde_json::json!({
                        "type": "retry",
                        "attempt": attempt + 1,
                        "reason": err.to_string(),
                        "delay_ms": delay.as_millis(),
                    }),
                );
                tokio::time::sleep(delay).await;
                last_error = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("builtin provider retry loop exhausted")))
}

async fn stream_chat_completion(
    request: &ProviderRequest<'_>,
    telemetry: &RunTelemetry<'_>,
) -> Result<ChatOutcome> {
    let url = format!(
        "{}/chat/completions",
        request.base_url.trim_end_matches('/')
    );
    let mut body = serde_json::json!({
        "model": request.model,
        "stream": true,
        "messages": request.messages,
    });
    if !request.tools.is_empty() {
        body["tools"] = serde_json::Value::Array(request.tools.to_vec());
    }
    let response = request
        .client
        .post(&url)
        .bearer_auth(request.api_key)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("posting builtin runner request to {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!(ProviderHttpError { status, body: text }));
    }
    let mut outcome = ChatOutcome::default();
    let mut tool_deltas: Vec<ToolCallDelta> = Vec::new();
    let mut buf = String::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading provider stream chunk")?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = buf.find('\n') {
            let line: String = buf.drain(..=idx).collect();
            handle_sse_line(
                line.trim_end(),
                telemetry.events,
                telemetry.store,
                telemetry.run_id,
                telemetry.issue_id,
                &mut outcome,
                &mut tool_deltas,
            )?;
        }
    }
    if !buf.trim().is_empty() {
        handle_sse_line(
            buf.trim_end(),
            telemetry.events,
            telemetry.store,
            telemetry.run_id,
            telemetry.issue_id,
            &mut outcome,
            &mut tool_deltas,
        )?;
    }
    outcome.tool_calls = tool_deltas
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "type": "function",
                "function": {"name": t.name, "arguments": t.arguments},
            })
        })
        .collect();
    Ok(outcome)
}

async fn stream_chat_completion_to_chat(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
    tx: &tokio::sync::mpsc::Sender<ChatEvent>,
) -> Result<ChatOutcome> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut body = serde_json::json!({"model": model, "stream": true, "messages": messages});
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools.to_vec());
    }
    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("posting builtin chat request to {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!(ProviderHttpError { status, body: text }));
    }
    let mut outcome = ChatOutcome::default();
    let mut tool_deltas: Vec<ToolCallDelta> = Vec::new();
    let mut buf = String::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading provider stream chunk")?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = buf.find('\n') {
            let line: String = buf.drain(..=idx).collect();
            handle_chat_sse_line(line.trim_end(), tx, &mut outcome, &mut tool_deltas).await?;
        }
    }
    if !buf.trim().is_empty() {
        handle_chat_sse_line(buf.trim_end(), tx, &mut outcome, &mut tool_deltas).await?;
    }
    outcome.tool_calls = tool_deltas
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "type": "function",
                "function": {"name": t.name, "arguments": t.arguments},
            })
        })
        .collect();
    Ok(outcome)
}

async fn handle_chat_sse_line(
    line: &str,
    tx: &tokio::sync::mpsc::Sender<ChatEvent>,
    outcome: &mut ChatOutcome,
    tool_deltas: &mut Vec<ToolCallDelta>,
) -> Result<()> {
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return Ok(());
    };
    if data == "[DONE]" {
        return Ok(());
    }
    let value: serde_json::Value =
        serde_json::from_str(data).context("parsing provider SSE JSON")?;
    if let Some(reason) = value["choices"][0]["finish_reason"].as_str() {
        outcome.finish_reason = Some(reason.to_string());
    }
    let delta = &value["choices"][0]["delta"];
    if let Some(text) = delta["content"].as_str() {
        if !text.is_empty() {
            outcome.content.push_str(text);
            let _ = tx
                .send(ChatEvent::Delta {
                    role: ChatRole::Assistant,
                    text: text.to_string(),
                })
                .await;
        }
    }
    if let Some(text) = delta["reasoning_content"].as_str() {
        if !text.is_empty() {
            let _ = tx
                .send(ChatEvent::Delta {
                    role: ChatRole::Thinking,
                    text: text.to_string(),
                })
                .await;
        }
    }
    if let Some(calls) = delta["tool_calls"].as_array() {
        for call in calls {
            let idx = call["index"].as_u64().unwrap_or(tool_deltas.len() as u64) as usize;
            if tool_deltas.len() <= idx {
                tool_deltas.resize_with(idx + 1, ToolCallDelta::default);
            }
            let slot = &mut tool_deltas[idx];
            if let Some(id) = call["id"].as_str() {
                slot.id.push_str(id);
            }
            if let Some(name) = call["function"]["name"].as_str() {
                slot.name.push_str(name);
            }
            if let Some(args) = call["function"]["arguments"].as_str() {
                slot.arguments.push_str(args);
            }
        }
    }
    Ok(())
}

fn handle_sse_line(
    line: &str,
    events: &dyn cap_runner::RunnerEventSink,
    store: &dyn cap_runner::RunnerEventStore,
    run_id: &str,
    issue_id: &str,
    outcome: &mut ChatOutcome,
    tool_deltas: &mut Vec<ToolCallDelta>,
) -> Result<()> {
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return Ok(());
    };
    if data == "[DONE]" {
        return Ok(());
    }
    let value: serde_json::Value =
        serde_json::from_str(data).context("parsing provider SSE JSON")?;
    if let Some(reason) = value["choices"][0]["finish_reason"].as_str() {
        outcome.finish_reason = Some(reason.to_string());
    }
    let delta = &value["choices"][0]["delta"];
    if let Some(text) = delta["content"].as_str() {
        if !text.is_empty() {
            outcome.content.push_str(text);
            events.push(text.to_string());
            persist_event(
                store,
                Some(run_id),
                issue_id,
                serde_json::json!({"type": "text_delta", "text": text}),
            );
        }
    }
    if let Some(text) = delta["reasoning_content"].as_str() {
        if !text.is_empty() {
            events.push(text.to_string());
            persist_event(
                store,
                Some(run_id),
                issue_id,
                serde_json::json!({"type": "reasoning_delta", "text": text}),
            );
        }
    }
    if let Some(calls) = delta["tool_calls"].as_array() {
        for call in calls {
            let idx = call["index"].as_u64().unwrap_or(tool_deltas.len() as u64) as usize;
            if tool_deltas.len() <= idx {
                tool_deltas.resize_with(idx + 1, ToolCallDelta::default);
            }
            let slot = &mut tool_deltas[idx];
            if let Some(id) = call["id"].as_str() {
                slot.id.push_str(id);
            }
            if let Some(name) = call["function"]["name"].as_str() {
                slot.name.push_str(name);
            }
            if let Some(args) = call["function"]["arguments"].as_str() {
                slot.arguments.push_str(args);
            }
        }
    }
    Ok(())
}

struct McpBridgeClient {
    _child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl McpBridgeClient {
    async fn spawn(bridge: cap_runner::HostToolBridge) -> Result<Self> {
        let mut child = Command::new(&bridge.command)
            .args(&bridge.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("spawning builtin host tool bridge {}", bridge.command))?;
        let stdin = child
            .stdin
            .take()
            .context("host tool bridge stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("host tool bridge stdout unavailable")?;
        let mut client = Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_id: 1,
        };
        client.request("initialize", serde_json::json!({})).await?;
        Ok(client)
    }

    async fn openai_tools(&mut self) -> Result<Vec<serde_json::Value>> {
        let response = self.request("tools/list", serde_json::json!({})).await?;
        let tools = response["tools"].as_array().cloned().unwrap_or_default();
        Ok(tools.into_iter().map(|tool| serde_json::json!({
            "type": "function",
            "function": {
                "name": tool["name"].clone(),
                "description": tool["description"].clone(),
                "parameters": tool.get("inputSchema").cloned().unwrap_or_else(|| serde_json::json!({"type":"object"})),
            }
        })).collect())
    }

    async fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        )
        .await
    }

    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        while let Some(line) = self.stdout.next_line().await? {
            let response: serde_json::Value = serde_json::from_str(&line)?;
            if response["id"].as_u64() == Some(id) {
                if let Some(error) = response.get("error") {
                    return Err(anyhow!("host tool bridge {method} error: {error}"));
                }
                return Ok(response["result"].clone());
            }
        }
        Err(anyhow!(
            "host tool bridge exited while waiting for {method}"
        ))
    }
}

fn persist_event(
    store: &dyn cap_runner::RunnerEventStore,
    run_id: Option<&str>,
    issue_id: &str,
    payload: serde_json::Value,
) {
    store.insert_event(
        run_id,
        issue_id,
        EVENT_KIND,
        &payload.to_string(),
        chrono::Utc::now(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopSink;
    impl cap_runner::RunnerEventSink for NoopSink {
        fn push(&self, _line: String) {}
    }

    #[derive(Default)]
    struct RecordingSink(std::sync::Mutex<Vec<String>>);
    impl cap_runner::RunnerEventSink for RecordingSink {
        fn push(&self, line: String) {
            self.0.lock().unwrap().push(line);
        }
    }

    struct NoopStore;
    impl cap_runner::RunnerEventStore for NoopStore {
        fn insert_event(
            &self,
            _run_id: Option<&str>,
            _issue_identifier: &str,
            _kind: &'static str,
            _payload: &str,
            _ts: chrono::DateTime<chrono::Utc>,
        ) {
        }
    }

    #[test]
    fn extension_registers_builtin_runner_id() {
        let ext = RunnerBuiltinExtension;
        assert_eq!(ext.id(), "runner-builtin");
    }

    #[test]
    fn streams_reasoning_content_deltas() {
        let mut outcome = ChatOutcome::default();
        let mut calls = Vec::new();
        let sink = RecordingSink::default();
        let store = NoopStore;
        handle_sse_line(
            r#"data: {"choices":[{"delta":{"reasoning_content":"Thinking"}}]}"#,
            &sink,
            &store,
            "run",
            "ISSUE-1",
            &mut outcome,
            &mut calls,
        )
        .unwrap();
        assert_eq!(sink.0.lock().unwrap().as_slice(), ["Thinking"]);
    }

    #[test]
    fn treats_provider_502_as_transient() {
        let err = anyhow!(ProviderHttpError {
            status: reqwest::StatusCode::BAD_GATEWAY,
            body: "router failed".to_string(),
        });
        assert!(is_transient_provider_error(&err));
    }

    #[test]
    fn provider_endpoint_requires_configured_provider() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("agent.yaml"), "providers: {}\n").unwrap();

        let err = provider_endpoint(temp.path(), "requesty").unwrap_err();

        assert!(err
            .to_string()
            .contains("provider \"requesty\" is not configured"));
    }

    #[test]
    fn provider_endpoint_resolves_configured_env_values() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("agent.yaml"),
            "providers:\n  requesty:\n    api_url: $env:DAR_BUILTIN_TEST_URL\n    api_key: $env:DAR_BUILTIN_TEST_KEY\n",
        )
        .unwrap();
        std::env::set_var("DAR_BUILTIN_TEST_URL", "https://example.test/v1");
        std::env::set_var("DAR_BUILTIN_TEST_KEY", "secret");

        let endpoint = provider_endpoint(temp.path(), "requesty").unwrap();

        assert_eq!(
            endpoint,
            ("https://example.test/v1".to_string(), "secret".to_string())
        );
    }

    #[test]
    fn accumulates_streamed_tool_call_deltas() {
        let mut outcome = ChatOutcome::default();
        let mut calls = Vec::new();
        let sink = NoopSink;
        let store = NoopStore;
        for line in [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"echo_","arguments":"{\"text\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"upper","arguments":"\"hi\"}"}}]}}]}"#,
        ] {
            handle_sse_line(
                line,
                &sink,
                &store,
                "run",
                "ISSUE-1",
                &mut outcome,
                &mut calls,
            )
            .unwrap();
        }
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "echo_upper");
        assert_eq!(calls[0].arguments, r#"{"text":"hi"}"#);
    }
}
