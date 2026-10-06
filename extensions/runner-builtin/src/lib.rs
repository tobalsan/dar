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
mod transcript;

use anyhow::{anyhow, Context, Result};
use cap_chat::{ChatBackend, ChatEvent, ChatRole, ChatSession, ChatSessionParams};
use cap_runner::{ExitKind, Runner, RunnerHandle, SpawnParams};
use futures_util::StreamExt;
use host_api::{Extension, RegisterCtx};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use transcript::{summary_message, Transcript};

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
        Box::pin(cap_chat::open_guarded(
            params.agent_loop,
            tx,
            move |tx| async move {
                let resumed = params.resume_session_id.as_deref().and_then(|id| {
                    let resumed = Transcript::resume(&params.session_dir, id);
                    if resumed.is_none() {
                        tracing::warn!(
                            session = id,
                            "builtin chat session to resume not found; opening fresh"
                        );
                    }
                    resumed
                });
                let (transcript, history) = resumed.unwrap_or_else(|| {
                    let id = format!(
                        "dar-{}-{}",
                        std::process::id(),
                        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
                    );
                    (Transcript::fresh(&params.session_dir, id), Vec::new())
                });
                Ok(Box::new(BuiltinChatSession {
                    params: Arc::new(params),
                    tx,
                    session_id: transcript.id().to_string(),
                    state: Arc::new(std::sync::Mutex::new(ChatState {
                        history,
                        transcript,
                    })),
                    turns: Vec::new(),
                    turn_tail: None,
                }) as Box<dyn ChatSession>)
            },
        ))
    }
}

/// Conversation history (OpenAI chat messages, without the system prompt) and
/// its transcript. Every history change goes through [`ChatState::push`] /
/// [`ChatState::compact`] so the transcript never diverges. Locked only for
/// short synchronous sections, never across an `await`.
struct ChatState {
    history: Vec<serde_json::Value>,
    transcript: Transcript,
}

impl ChatState {
    fn push(&mut self, message: serde_json::Value) {
        self.transcript.append_message(&message);
        self.history.push(message);
    }

    fn compact(&mut self, summary: &str) {
        self.transcript.append_compaction(summary);
        self.history = vec![summary_message(summary)];
    }

    /// Answers every tool call of the trailing assistant tool-call batch that has
    /// no tool response yet, so the next request is valid for the provider.
    fn answer_unfinished_tool_calls(&mut self, reason: &str) {
        for message in self.unanswered_tool_responses(reason) {
            self.push(message);
        }
    }

    /// The tool responses [`Self::answer_unfinished_tool_calls`] would add,
    /// without touching history or the transcript.
    fn unanswered_tool_responses(&self, reason: &str) -> Vec<serde_json::Value> {
        let Some(start) = self
            .history
            .iter()
            .rposition(|message| message.get("tool_calls").is_some())
        else {
            return Vec::new();
        };
        let answered: Vec<String> = self.history[start + 1..]
            .iter()
            .filter_map(|message| message["tool_call_id"].as_str().map(str::to_string))
            .collect();
        let missing: Vec<String> = self.history[start]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|call| call["id"].as_str())
            .filter(|id| !answered.iter().any(|answered| answered == id))
            .map(str::to_string)
            .collect();
        missing
            .into_iter()
            .map(|id| {
                serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": format!("not executed: {reason}"),
                })
            })
            .collect()
    }
}

type SharedChatState = Arc<std::sync::Mutex<ChatState>>;

fn lock_state(state: &SharedChatState) -> std::sync::MutexGuard<'_, ChatState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct BuiltinChatSession {
    params: Arc<ChatSessionParams>,
    tx: tokio::sync::mpsc::Sender<ChatEvent>,
    state: SharedChatState,
    session_id: String,
    /// Accepted turns not yet reaped: at most one running, the rest waiting
    /// on their predecessor via `turn_tail`.
    turns: Vec<tokio::task::JoinHandle<()>>,
    /// Completes (sender dropped) when the most recently accepted turn ends;
    /// the next `send_turn` captures it synchronously, so turns run in
    /// submission order.
    turn_tail: Option<oneshot::Receiver<()>>,
}

/// Cancels the in-flight turn and every queued one and repairs the history
/// for the next request. Returns how many turns were cancelled.
async fn cancel_turns(turns: Vec<tokio::task::JoinHandle<()>>, state: &SharedChatState) -> usize {
    let turns: Vec<_> = turns
        .into_iter()
        .filter(|turn| !turn.is_finished())
        .collect();
    for turn in &turns {
        turn.abort();
    }
    let mut aborted = 0;
    for turn in turns {
        if turn.await.is_err() {
            aborted += 1; // otherwise finished on its own before the abort landed
        }
    }
    if aborted > 0 {
        lock_state(state).answer_unfinished_tool_calls("aborted");
    }
    aborted
}

/// Reports `count` cancelled turns as aborted from a background task, so
/// `abort`/`close` never wait on channel capacity while their caller is the
/// one draining events. The receiver completes once every report is sent.
fn report_aborted(
    tx: tokio::sync::mpsc::Sender<ChatEvent>,
    count: usize,
) -> Option<oneshot::Receiver<()>> {
    if count == 0 {
        return None;
    }
    let (done_tx, done_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _done = done_tx;
        for _ in 0..count {
            let _ = tx
                .send(ChatEvent::TurnFinished {
                    ok: false,
                    error: Some("aborted".to_string()),
                })
                .await;
        }
    });
    Some(done_rx)
}

impl ChatSession for BuiltinChatSession {
    fn send_turn(&mut self, prompt: String) -> cap_chat::BoxFuture<'_, Result<()>> {
        let params = Arc::clone(&self.params);
        let tx = self.tx.clone();
        let state = Arc::clone(&self.state);
        let session_id = self.session_id.clone();
        Box::pin(async move {
            self.turns.retain(|turn| !turn.is_finished());
            let previous = self.turn_tail.take();
            let (done_tx, done_rx) = oneshot::channel::<()>();
            self.turn_tail = Some(done_rx);
            self.turns.push(tokio::spawn(async move {
                if let Some(previous) = previous {
                    let _ = previous.await; // resolves when the predecessor ends or is aborted
                }
                let _done = done_tx;
                if let Err(err) =
                    run_builtin_chat_turn(params, tx.clone(), state, prompt, session_id).await
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
            }));
            Ok(())
        })
    }

    fn abort(&mut self) -> cap_chat::BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let aborted = cancel_turns(std::mem::take(&mut self.turns), &self.state).await;
            if let Some(reported) = report_aborted(self.tx.clone(), aborted) {
                // The next turn waits for these reports, keeping event order.
                self.turn_tail = Some(reported);
            }
            Ok(())
        })
    }

    fn close(mut self: Box<Self>) -> cap_chat::BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let aborted = cancel_turns(std::mem::take(&mut self.turns), &self.state).await;
            let _ = report_aborted(self.tx.clone(), aborted);
            Ok(())
        })
    }
}

const COMPACT_COMMAND: &str = "/compact";

const COMPACT_PROMPT: &str = "Summarize the conversation so far so it can replace the full history. Be concise but keep the facts, decisions, open tasks, user preferences, and names needed to continue. Reply with the summary only.";

/// The request messages: system prompt (not part of the persisted history)
/// followed by the history.
fn chat_request_messages(
    params: &ChatSessionParams,
    state: &SharedChatState,
) -> Vec<serde_json::Value> {
    let mut messages = Vec::new();
    if let Some(system) = params.system_prompt.as_deref().filter(|s| !s.is_empty()) {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    messages.extend(lock_state(state).history.iter().cloned());
    messages
}

async fn send_context_usage(
    tx: &tokio::sync::mpsc::Sender<ChatEvent>,
    tokens_used: Option<u64>,
    context_window: Option<u64>,
) {
    if let Some(tokens_used) = tokens_used {
        let _ = tx
            .send(ChatEvent::ContextUsage {
                tokens_used,
                context_window,
            })
            .await;
    }
}

async fn run_builtin_chat_turn(
    params: Arc<ChatSessionParams>,
    tx: tokio::sync::mpsc::Sender<ChatEvent>,
    state: SharedChatState,
    prompt: String,
    session_id: String,
) -> Result<()> {
    let provider = params
        .provider
        .as_deref()
        .context("builtin chat requires runner.provider")?;
    let (base_url, api_key) = provider_endpoint(&params.agent_root, provider)?;
    let client = reqwest::Client::new();
    let model = params.model.as_deref().unwrap_or("openai/gpt-4o-mini");
    let opencode_session = opencode_session(provider, &session_id);
    if prompt.trim() == COMPACT_COMMAND {
        return compact_chat_history(
            &params,
            &tx,
            &state,
            &ProviderRequest {
                client: &client,
                base_url: &base_url,
                api_key: &api_key,
                opencode_session,
                model,
                messages: &[],
                tools: &[],
            },
        )
        .await;
    }
    {
        let mut guard = lock_state(&state);
        guard.answer_unfinished_tool_calls("interrupted");
        guard.push(serde_json::json!({"role": "user", "content": prompt}));
    }
    let mut bridge = match params.host_tool_bridge.clone() {
        Some(bridge) => Some(McpBridgeClient::spawn(bridge).await?),
        None => None,
    };
    let tools = match bridge.as_mut() {
        Some(bridge) => bridge.openai_tools().await?,
        None => Vec::new(),
    };
    let mut budget = ToolCallBudget::new(params.max_tool_calls);
    loop {
        let request_messages = chat_request_messages(&params, &state);
        let request = ProviderRequest {
            client: &client,
            base_url: &base_url,
            api_key: &api_key,
            opencode_session,
            model,
            messages: &request_messages,
            tools: &tools,
        };
        let outcome = stream_chat_completion_to_chat(&request, Some(&tx)).await?;
        send_context_usage(
            &tx,
            outcome.usage.as_ref().map(|usage| usage.total),
            params.context_window,
        )
        .await;
        if outcome.tool_calls.is_empty() {
            lock_state(&state)
                .push(serde_json::json!({"role": "assistant", "content": outcome.content}));
            let _ = tx
                .send(ChatEvent::TurnFinished {
                    ok: true,
                    error: None,
                })
                .await;
            return Ok(());
        }
        budget.reserve(outcome.tool_calls.len())?;
        let mut assistant =
            serde_json::json!({"role": "assistant", "tool_calls": outcome.tool_calls});
        if !outcome.content.is_empty() {
            assistant["content"] = serde_json::Value::String(outcome.content);
        }
        lock_state(&state).push(assistant);
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
            lock_state(&state).push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result_content,
            }));
        }
    }
}

/// `/compact`: replaces the history with one model-written summary message.
/// Any failure leaves the history untouched. The usage reported afterwards is
/// the summary's completion size, an estimate of the new context (the summary
/// call's own prompt size describes the history that was just dropped).
async fn compact_chat_history(
    params: &ChatSessionParams,
    tx: &tokio::sync::mpsc::Sender<ChatEvent>,
    state: &SharedChatState,
    base: &ProviderRequest<'_>,
) -> Result<()> {
    if lock_state(state).history.is_empty() {
        let _ = tx
            .send(ChatEvent::TurnFinished {
                ok: true,
                error: None,
            })
            .await;
        return Ok(());
    }
    let mut messages = chat_request_messages(params, state);
    messages.extend(lock_state(state).unanswered_tool_responses("interrupted"));
    messages.push(serde_json::json!({"role": "user", "content": COMPACT_PROMPT}));
    let request = ProviderRequest {
        messages: &messages,
        ..*base
    };
    let outcome = stream_chat_completion_to_chat(&request, None).await?;
    let summary = outcome.content.trim();
    if summary.is_empty() {
        return Err(anyhow!("compaction produced an empty summary"));
    }
    lock_state(state).compact(summary);
    let _ = tx
        .send(ChatEvent::Delta {
            role: ChatRole::Assistant,
            text: "Context compacted.".to_string(),
        })
        .await;
    send_context_usage(
        tx,
        outcome
            .usage
            .map(|usage| usage.completion.unwrap_or(usage.total)),
        params.context_window,
    )
    .await;
    let _ = tx
        .send(ChatEvent::TurnFinished {
            ok: true,
            error: None,
        })
        .await;
    Ok(())
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
        max_tool_calls: p.max_tool_calls,
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
    max_tool_calls: Option<u32>,
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

    let mut budget = ToolCallBudget::new(p.max_tool_calls);
    loop {
        let request = ProviderRequest {
            client: &client,
            base_url: &base_url,
            api_key: &api_key,
            opencode_session: opencode_session(provider, &run_id),
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
        budget.reserve(outcome.tool_calls.len())?;
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
}

/// Default for `runner.max_tool_calls`.
const DEFAULT_MAX_TOOL_CALLS: u32 = 100;

/// Counts tool calls executed within one turn against `runner.max_tool_calls`.
struct ToolCallBudget {
    used: u32,
    max: u32,
}

impl ToolCallBudget {
    fn new(configured: Option<u32>) -> Self {
        Self {
            used: 0,
            max: configured.unwrap_or(DEFAULT_MAX_TOOL_CALLS),
        }
    }

    /// Reserves a whole batch before it enters history, so a rejected batch
    /// never leaves tool calls without matching tool responses.
    fn reserve(&mut self, calls: usize) -> Result<()> {
        let calls = u32::try_from(calls).unwrap_or(u32::MAX);
        if self.used.saturating_add(calls) > self.max {
            return Err(anyhow!(
                "builtin runner exceeded runner.max_tool_calls ({})",
                self.max
            ));
        }
        self.used += calls;
        Ok(())
    }
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

/// OpenCode Go rejects requests without `x-opencode-session`; only send it to
/// providers named `opencode` / `opencode-go`.
fn opencode_session<'a>(provider: &str, session_id: &'a str) -> Option<&'a str> {
    matches!(provider, "opencode" | "opencode-go").then_some(session_id)
}

fn with_opencode_headers(
    builder: reqwest::RequestBuilder,
    session: Option<&str>,
) -> reqwest::RequestBuilder {
    match session {
        Some(id) => builder
            .header("x-opencode-session", id)
            .header("x-opencode-client", "dar"),
        None => builder,
    }
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
    usage: Option<TokenUsage>,
}

/// Provider-reported token usage of one model response.
struct TokenUsage {
    /// `prompt_tokens + completion_tokens`, else `total_tokens`.
    total: u64,
    completion: Option<u64>,
}

impl TokenUsage {
    fn from_chunk(usage: &serde_json::Value) -> Option<Self> {
        let prompt = usage["prompt_tokens"].as_u64();
        let completion = usage["completion_tokens"].as_u64();
        let total = match (prompt, completion) {
            (Some(prompt), Some(completion)) => prompt + completion,
            _ => usage["total_tokens"].as_u64()?,
        };
        Some(Self { total, completion })
    }
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

#[derive(Clone, Copy)]
struct ProviderRequest<'a> {
    client: &'a reqwest::Client,
    base_url: &'a str,
    api_key: &'a str,
    opencode_session: Option<&'a str>,
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
    let response = with_opencode_headers(
        request.client.post(&url).bearer_auth(request.api_key),
        request.opencode_session,
    )
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
    request: &ProviderRequest<'_>,
    tx: Option<&tokio::sync::mpsc::Sender<ChatEvent>>,
) -> Result<ChatOutcome> {
    let ProviderRequest {
        client,
        base_url,
        api_key,
        opencode_session,
        model,
        messages,
        tools,
    } = *request;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut body = serde_json::json!({
        "model": model,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools.to_vec());
    }
    let response = with_opencode_headers(client.post(&url).bearer_auth(api_key), opencode_session)
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
    tx: Option<&tokio::sync::mpsc::Sender<ChatEvent>>,
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
    if !value["error"].is_null() {
        return Err(anyhow!("builtin provider stream error: {}", value["error"]));
    }
    if let Some(usage) = TokenUsage::from_chunk(&value["usage"]) {
        outcome.usage = Some(usage);
    }
    let delta = &value["choices"][0]["delta"];
    if let Some(text) = delta["content"].as_str() {
        if !text.is_empty() {
            outcome.content.push_str(text);
            if let Some(tx) = tx {
                let _ = tx
                    .send(ChatEvent::Delta {
                        role: ChatRole::Assistant,
                        text: text.to_string(),
                    })
                    .await;
            }
        }
    }
    if let Some(text) = delta["reasoning_content"].as_str() {
        if !text.is_empty() {
            if let Some(tx) = tx {
                let _ = tx
                    .send(ChatEvent::Delta {
                        role: ChatRole::Thinking,
                        text: text.to_string(),
                    })
                    .await;
            }
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
            .kill_on_drop(true)
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
mod chat_tests;

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

    #[test]
    fn tool_call_budget_defaults_to_100() {
        let mut budget = ToolCallBudget::new(None);
        assert_eq!(budget.max, 100);
        budget.reserve(100).unwrap();
        let err = budget.reserve(1).unwrap_err().to_string();
        assert_eq!(err, "builtin runner exceeded runner.max_tool_calls (100)");
    }

    #[test]
    fn tool_call_budget_honors_configured_limit() {
        let mut budget = ToolCallBudget::new(Some(3));
        budget.reserve(2).unwrap();
        let err = budget.reserve(2).unwrap_err().to_string();
        assert_eq!(err, "builtin runner exceeded runner.max_tool_calls (3)");
        // A rejected batch consumes nothing; one that fits exactly still runs.
        budget.reserve(1).unwrap();
        assert!(budget.reserve(1).is_err());
    }

    #[test]
    fn opencode_session_header_only_for_opencode_providers() {
        assert_eq!(opencode_session("opencode", "s1"), Some("s1"));
        assert_eq!(opencode_session("opencode-go", "s1"), Some("s1"));
        assert_eq!(opencode_session("requesty", "s1"), None);
    }
}
