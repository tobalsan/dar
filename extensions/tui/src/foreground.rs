//! The `tui` foreground: ratatui event loop on an interactive terminal,
//! byte-for-byte `frontend-log` behavior on a non-interactive one.
//!
//! The host already enabled raw mode + the alternate screen and installed the
//! restoring panic hook before `Foreground::run` (see `agentropy-host`); the
//! [`ExclusiveTerminal`]'s `restore()`/`Drop` undoes it. This extension adds
//! no terminal lifecycle of its own — it only writes through [`TermWriter`].

use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use cap_chat::{ChatBackend, ChatEvent, ChatSession, ChatSessionParams};
use host_api::{
    ExclusiveTerminal, Foreground, LogEvent, StartCtx, APP_DONE_TOPIC, LOG_EVENTS_TOPIC,
    STARTUP_BANNER_TOPIC,
};
use orchestrator_api::{RunSnapshot, RUN_SNAPSHOT_TOPIC};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc::Sender;

use crate::app::{Action, App};
use crate::backend::{self, Resolution};
use crate::dash::{self, DashFeed};
use crate::{chat, logs, view, ChatConfig, TuiConfig};

/// Fixed per-turn ceiling: the TUI aborts the in-flight turn after this long.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);
/// Coalesced redraw cadence; dirty state is painted at most this often.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

pub struct TuiForeground {
    config: TuiConfig,
}

impl TuiForeground {
    pub fn new(config: TuiConfig) -> Self {
        Self { config }
    }
}

/// `Write` adapter from the host-owned [`ExclusiveTerminal`] into ratatui's
/// `CrosstermBackend`. Dropping it drops the wrapped terminal, whose `Drop`
/// restores raw mode/alternate screen/panic hook.
struct TermWriter(ExclusiveTerminal);

impl Write for TermWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.writer().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.writer().flush()
    }
}

impl Foreground for TuiForeground {
    fn run<'a>(
        &'a mut self,
        ctx: StartCtx,
        terminal: ExclusiveTerminal,
    ) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !terminal.is_interactive() {
                return run_non_interactive(ctx, terminal).await;
            }
            run_interactive(self.config.chat.clone(), ctx, terminal).await
        })
    }
}

fn write_event(terminal: &mut ExclusiveTerminal, event: &LogEvent) -> std::io::Result<()> {
    // Same formatter as the Logs tab, so the interactive and degrade paths
    // can never drift from frontend-log's line format.
    writeln!(terminal.writer(), "{}", logs::format_event(event))
}

/// Piped/CI degrade path: replicate `frontend-log`'s line loop byte-for-byte
/// (same subscriptions, same `{level} {target} {message}` format, same
/// one-shot retained startup banner, same `APP_DONE_TOPIC` exit).
async fn run_non_interactive(ctx: StartCtx, mut terminal: ExclusiveTerminal) -> Result<()> {
    let mut shutdown = ctx.shutdown.clone();
    let mut app_done = ctx.host.bus.subscribe_retained::<bool>(APP_DONE_TOPIC)?;
    let mut events = ctx.host.bus.subscribe::<LogEvent>(LOG_EVENTS_TOPIC)?;
    let mut banner = ctx
        .host
        .bus
        .subscribe_retained::<Option<LogEvent>>(STARTUP_BANNER_TOPIC)?;
    let mut banner_pending = match banner.borrow_and_update().clone() {
        Some(event) => {
            write_event(&mut terminal, &event)?;
            false
        }
        None => true,
    };
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            changed = app_done.changed() => {
                if changed.is_err() || *app_done.borrow() {
                    break;
                }
            }
            changed = banner.changed(), if banner_pending => {
                match changed {
                    Ok(()) => {
                        if let Some(event) = banner.borrow_and_update().clone() {
                            write_event(&mut terminal, &event)?;
                            banner_pending = false;
                        }
                    }
                    Err(_) => banner_pending = false,
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => write_event(&mut terminal, &event)?,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    terminal.restore();
    Ok(())
}

async fn run_interactive(
    config: ChatConfig,
    ctx: StartCtx,
    terminal: ExclusiveTerminal,
) -> Result<()> {
    let mut shutdown = ctx.shutdown.clone();
    let mut app_done = ctx.host.bus.subscribe_retained::<bool>(APP_DONE_TOPIC)?;
    let mut term = Terminal::new(CrosstermBackend::new(TermWriter(terminal)))?;
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::input::spawn_reader(input_tx);
    // The chat event channel outlives the lazily opened session; `chat_tx`
    // stays alive here so the recv arm simply idles until the first turn.
    let (chat_tx, mut chat_rx) = tokio::sync::mpsc::channel::<ChatEvent>(256);
    let mut session: Option<Box<dyn ChatSession>> = None;
    // Backend id resolved lazily at first submit (registry is frozen after
    // boot, so the outcome — including Disabled — is final for this launch).
    let mut backend_id: Option<String> = None;
    // The first-turn context preamble is still owed to the backend.
    let mut preamble_pending = true;
    let mut app = App::new();
    // Logs tab feed: the log broadcast + retained startup banner (a failed
    // subscription leaves the pane on its "unavailable" placeholder).
    let mut feed = logs::LogFeed::subscribe(&ctx.host.bus, &mut app.logs);
    // Dash tab: present only when the orchestrator's retained snapshot topic
    // is subscribable at startup; otherwise absent entirely (see crate docs).
    let mut dash_feed = DashFeed::subscribe(&ctx.host.bus, &mut app.dash);
    if dash_feed.is_some() {
        app.enable_dash();
    }
    let mut redraw = tokio::time::interval(REDRAW_INTERVAL);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            changed = app_done.changed() => {
                if changed.is_err() || *app_done.borrow() {
                    break;
                }
            }
            event = input_rx.recv() => {
                let Some(event) = event else { break };
                match app.handle_event(event) {
                    Action::Quit => break,
                    Action::Submit(prompt) => {
                        if backend_id.is_none() {
                            match backend::resolve(config.backend.as_deref(), &ctx.host.services, &ctx.host.bus) {
                                Resolution::Backend { id, notice } => {
                                    if let Some(notice) = notice {
                                        app.chat.push_notice(notice);
                                    }
                                    backend_id = Some(id);
                                }
                                Resolution::Disabled => app.chat.disable(
                                    "no chat backend registered; chat input disabled".to_string(),
                                ),
                            }
                        }
                        if let Some(id) = backend_id.as_deref() {
                            submit_turn(
                                id, &config, &ctx, &chat_tx,
                                &mut session, &mut app, &mut preamble_pending, prompt,
                            )
                            .await;
                        }
                    }
                    Action::AbortTurn => {
                        if let Some(session) = session.as_mut() {
                            if let Err(e) = session.abort().await {
                                app.chat.push_error(format!("abort failed: {e:#}"));
                            }
                        }
                    }
                    // Fire-and-forget, no local state mutation: the paused
                    // badge etc. only change via the next retained snapshot
                    // (the orchestrator is the single writer of run state).
                    Action::Control(control) => dash::publish_control(&ctx.host.bus, control),
                    Action::None => {}
                }
                app.dirty = true;
            }
            delivery = feed.next(), if feed.active() => {
                feed.apply(delivery, &mut app.logs);
                app.dirty = true;
            }
            snapshot = async { dash_feed.as_mut().expect("guarded by is_some").next().await },
                       if dash_feed.is_some() => {
                match snapshot {
                    Some(snapshot) => app.dash.snapshot = snapshot,
                    // Topic owner gone: stop polling, keep the last snapshot.
                    None => dash_feed = None,
                }
                app.dirty = true;
            }
            event = chat_rx.recv() => {
                let Some(event) = event else { continue };
                if matches!(event, ChatEvent::SessionClosed { .. }) {
                    // Process is gone; the next submit opens a fresh session.
                    session = None;
                }
                app.chat.apply_event(event);
                app.dirty = true;
            }
            _ = redraw.tick() => {
                if app.chat.turn_timed_out(TURN_TIMEOUT) {
                    if let Some(session) = session.as_mut() {
                        let _ = session.abort().await;
                    }
                    app.chat.abandon_turn(
                        "turn timed out after 10 minutes; aborted - resend to retry".to_string(),
                    );
                    app.dirty = true;
                }
                if app.chat.in_flight {
                    app.spinner_tick = app.spinner_tick.wrapping_add(1);
                    app.dirty = true;
                }
                if app.dirty {
                    term.draw(|frame| view::render(frame, &app))?;
                    app.dirty = false;
                }
            }
        }
    }
    if let Some(session) = session.take() {
        let _ = session.close().await;
    }
    // Dropping `term` drops TermWriter and the ExclusiveTerminal inside it,
    // whose Drop restores the terminal the host prepared.
    Ok(())
}

/// Open the session lazily on first submit, then send the turn — with the
/// context preamble prepended while it is still pending, consumed only once
/// a turn was actually accepted (a failed first turn keeps it owed). Failures
/// end the turn with an error block instead of crashing the foreground.
#[allow(clippy::too_many_arguments)]
async fn submit_turn(
    backend_id: &str,
    config: &ChatConfig,
    ctx: &StartCtx,
    chat_tx: &Sender<ChatEvent>,
    session: &mut Option<Box<dyn ChatSession>>,
    app: &mut App,
    preamble_pending: &mut bool,
    prompt: String,
) {
    if session.is_none() {
        match open_session(backend_id, config, ctx, chat_tx.clone()).await {
            Ok(opened) => *session = Some(opened),
            Err(e) => {
                app.chat.fail_turn(format!("cannot open chat session: {e:#}"));
                return;
            }
        }
    }
    let outbound = if *preamble_pending {
        let snapshot = ctx
            .host
            .bus
            .read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
            .ok();
        let preamble = chat::build_preamble(snapshot.as_ref(), ctx.paths.root());
        format!("{preamble}\n{prompt}")
    } else {
        prompt
    };
    let opened = session.as_mut().expect("session opened above");
    if let Err(e) = opened.send_turn(outbound).await {
        app.chat.fail_turn(format!("sending turn failed: {e:#}"));
        return;
    }
    *preamble_pending = false;
}

/// Open one session on the resolved backend under `data/tui/sessions`.
async fn open_session(
    backend_id: &str,
    config: &ChatConfig,
    ctx: &StartCtx,
    tx: Sender<ChatEvent>,
) -> Result<Box<dyn ChatSession>> {
    let backend = ctx.host.services.get_named::<dyn ChatBackend>(backend_id)?;
    // data_dir containment-checks against an existing data/ parent.
    std::fs::create_dir_all(ctx.paths.root().join("data"))?;
    let session_dir = ctx.paths.data_dir("tui")?.join("sessions");
    std::fs::create_dir_all(&session_dir)?;
    let snap = ctx
        .host
        .bus
        .read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC)
        .ok()
        .filter(|s| s.version > 0);
    let model = snap.as_ref().and_then(|s| s.agent.model.clone());
    let provider = snap.as_ref().and_then(|s| s.agent.provider.clone());
    let params = ChatSessionParams::builder(
        config.command.as_deref().unwrap_or(""),
        ctx.paths.root(),
        &session_dir,
    )
    .model(model)
    .provider(provider)
    .host_tool_bridge(host_tool_bridge(ctx))
    .build();
    backend.open(params, tx).await
}

/// Build the host MCP bridge descriptor for the chat backend, or `None` when no
/// extension registered any tool. Mirrors the orchestrator's worker-spawn path
/// so interactive chat advertises the same registry tools as issue workers: the
/// backend is pointed at `<this binary> __mcp-bridge --dir <agent root>`, a
/// host-owned process that re-loads the agent's config/secrets and executes
/// registered tools in-host; the chat agent only sees tool schemas and results.
fn host_tool_bridge(ctx: &StartCtx) -> Option<cap_chat::HostToolBridge> {
    let registry = ctx
        .host
        .services
        .get_named::<dyn tool_registry::ToolRegistryHandle>(tool_registry::TOOL_REGISTRY_SERVICE)
        .ok()?;
    if registry.is_empty() {
        return None;
    }
    let command = std::env::current_exe().ok()?.to_string_lossy().into_owned();
    Some(cap_chat::HostToolBridge {
        command,
        args: vec![
            "__mcp-bridge".to_string(),
            "--dir".to_string(),
            ctx.paths.root().display().to_string(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use host_api::Extension as _;

    use crate::TuiExtension;

    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Register frontend-log (the topic owner) + tui on one fresh bus and run
    /// the selected foreground non-interactively, feeding it a banner and two
    /// log events, then flipping APP_DONE. Returns the captured output.
    async fn capture_non_interactive(foreground_id: &str) -> String {
        let temp = tempfile::tempdir().unwrap();
        let paths = host_api::HostPaths::new(temp.path()).unwrap();
        let (_register_tx, register_rx) = tokio::sync::watch::channel(false);
        let mut ctx = host_api::RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::disabled(),
            foreground: host_api::ForegroundRegistry::default(),
            services: host_api::ServiceRegistry::default(),
            paths: paths.clone(),
            config: host_api::ConfigStore::default(),
            shutdown: host_api::ShutdownToken::new(register_rx),
        };
        frontend_log::FrontendLogExtension
            .register(&mut ctx)
            .await
            .unwrap();
        TuiExtension.register(&mut ctx).await.unwrap();
        let provider = ctx.foreground.select(Some(foreground_id)).unwrap().unwrap();
        let config = ctx.config.clone();
        let host = ctx.into_start_services().unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let start_ctx = StartCtx {
            shutdown: host_api::ShutdownToken::new(shutdown_rx),
            paths,
            config,
            host: host.clone(),
        };
        let buf = SharedBuf::default();
        let terminal = ExclusiveTerminal::non_interactive(Box::new(buf.clone()));
        let mut foreground = (provider.factory)();
        let task = tokio::spawn(async move { foreground.run(start_ctx, terminal).await });

        // The banner is retained, so it prints as soon as the foreground has
        // subscribed — and all subscriptions happen before the banner is
        // first read, so its appearance proves the log subscription is live.
        host.bus
            .publish(
                STARTUP_BANNER_TOPIC,
                Some(LogEvent {
                    level: "INFO".to_string(),
                    target: "issue=- event=startup".to_string(),
                    message: "agentropy running; dashboard on http://127.0.0.1:7878/".to_string(),
                }),
            )
            .unwrap();
        wait_for(&buf, "agentropy running").await;

        host.bus
            .publish(
                LOG_EVENTS_TOPIC,
                LogEvent {
                    level: "INFO".to_string(),
                    target: "issue=ISSUE-1 event=dispatched".to_string(),
                    message: "runner started".to_string(),
                },
            )
            .unwrap();
        host.bus
            .publish(
                LOG_EVENTS_TOPIC,
                LogEvent {
                    level: "WARN".to_string(),
                    target: "issue=ISSUE-1 event=stalled".to_string(),
                    message: "no events for 30s".to_string(),
                },
            )
            .unwrap();
        wait_for(&buf, "no events for 30s").await;

        // Flipping APP_DONE must end the run with Ok.
        host.bus.publish(APP_DONE_TOPIC, true).unwrap();
        task.await.unwrap().unwrap();
        drop(shutdown_tx);
        buf.contents()
    }

    async fn wait_for(buf: &SharedBuf, needle: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !buf.contents().contains(needle) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("output never contained {needle:?}"));
    }

    #[tokio::test]
    async fn non_interactive_output_is_byte_identical_to_frontend_log() {
        let tui_output = capture_non_interactive("tui").await;
        let logs_output = capture_non_interactive("logs").await;
        assert_eq!(tui_output, logs_output);
        assert_eq!(
            tui_output,
            "INFO issue=- event=startup agentropy running; dashboard on http://127.0.0.1:7878/\n\
             INFO issue=ISSUE-1 event=dispatched runner started\n\
             WARN issue=ISSUE-1 event=stalled no events for 30s\n"
        );
    }

    // -- chat turn against the M1 stub-script pi backend ----------------------

    /// Mirrors chat-pi's ECHO stub: answers each prompt with thinking + text +
    /// agent_end and each abort with the aborted error event. Every prompt
    /// line is appended to `prompts.log` in the cwd (= agent root) so tests
    /// can assert exactly what reached the backend.
    const ECHO_STUB: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"prompt"'*)
      printf '%s\n' "$line" >> prompts.log
      printf '%s\n' '{"type":"message_update","message":{},"assistantMessageEvent":{"type":"thinking_delta","delta":"hmm"}}'
      printf '%s\n' '{"type":"message_update","message":{},"assistantMessageEvent":{"type":"text_delta","delta":"pong"}}'
      printf '%s\n' '{"type":"agent_end"}'
      ;;
    *'"type":"abort"'*)
      printf '%s\n' '{"type":"message_update","message":{},"assistantMessageEvent":{"type":"error","reason":"aborted"}}'
      ;;
  esac
done"#;

    fn start_ctx_with_pi_backend(root: &Path) -> (StartCtx, tokio::sync::watch::Sender<bool>) {
        let paths = host_api::HostPaths::new(root).unwrap();
        let (_register_tx, register_rx) = tokio::sync::watch::channel(false);
        let mut ctx = host_api::RegisterCtx {
            bus: host_api::EventBus::new(),
            http: host_api::HttpRegistry::disabled(),
            foreground: host_api::ForegroundRegistry::default(),
            services: host_api::ServiceRegistry::default(),
            paths: paths.clone(),
            config: host_api::ConfigStore::default(),
            shutdown: host_api::ShutdownToken::new(register_rx),
        };
        ctx.services
            .register::<dyn ChatBackend>("pi", Arc::new(chat_pi::PiChatBackend))
            .unwrap();
        let config = ctx.config.clone();
        let host = ctx.into_start_services().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let ctx = StartCtx {
            shutdown: host_api::ShutdownToken::new(shutdown_rx),
            paths,
            config,
            host,
        };
        (ctx, shutdown_tx)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chat_turn_flows_from_submit_to_finished_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("stub-pi.sh");
        std::fs::write(&script, ECHO_STUB).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (ctx, _shutdown_tx) = start_ctx_with_pi_backend(temp.path());
        let config = ChatConfig {
            backend: None, // exercises the "pi" fallback
            command: Some(script.to_str().unwrap().to_string()),
        };
        let (chat_tx, mut chat_rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
        let mut session: Option<Box<dyn ChatSession>> = None;
        let mut app = App::new();

        // Type "ping" + Enter, exactly as the event loop would see it.
        for c in ['p', 'i', 'n', 'g'] {
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        let Action::Submit(prompt) = app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))) else {
            panic!("Enter did not submit");
        };
        let mut preamble_pending = true;
        submit_turn(
            "pi",
            &config,
            &ctx,
            &chat_tx,
            &mut session,
            &mut app,
            &mut preamble_pending,
            prompt,
        )
        .await;
        assert!(session.is_some(), "session opens lazily on first submit");
        assert!(!preamble_pending, "accepted first turn consumes the preamble");
        assert!(
            temp.path().join("data/tui/sessions").is_dir(),
            "session dir created under data/tui"
        );

        while app.chat.in_flight {
            let event = tokio::time::timeout(Duration::from_secs(5), chat_rx.recv())
                .await
                .expect("timed out waiting for chat event")
                .expect("chat event channel closed");
            app.chat.apply_event(event);
        }
        assert_eq!(
            app.chat.blocks,
            vec![
                crate::chat::ChatBlock::User("ping".to_string()),
                crate::chat::ChatBlock::Thinking("hmm".to_string()),
                crate::chat::ChatBlock::Assistant("pong".to_string()),
            ]
        );

        session.take().unwrap().close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn preamble_is_prepended_to_the_first_turn_only() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("stub-pi.sh");
        std::fs::write(&script, ECHO_STUB).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir(temp.path().join("issues")).unwrap();
        std::fs::write(
            temp.path().join("issues/ISSUE-1.md"),
            "state: todo\ntitle: Fix the thing\n",
        )
        .unwrap();

        let (ctx, _shutdown_tx) = start_ctx_with_pi_backend(temp.path());
        let config = ChatConfig {
            backend: None,
            command: Some(script.to_str().unwrap().to_string()),
        };
        let (chat_tx, mut chat_rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
        let mut session: Option<Box<dyn ChatSession>> = None;
        let mut app = App::new();
        let mut preamble_pending = true;

        for prompt_text in ["first question", "second question"] {
            app.chat.input = prompt_text.to_string();
            let prompt = app.chat.submit().unwrap();
            submit_turn(
                "pi",
                &config,
                &ctx,
                &chat_tx,
                &mut session,
                &mut app,
                &mut preamble_pending,
                prompt,
            )
            .await;
            while app.chat.in_flight {
                let event = tokio::time::timeout(Duration::from_secs(5), chat_rx.recv())
                    .await
                    .expect("timed out waiting for chat event")
                    .expect("chat event channel closed");
                app.chat.apply_event(event);
            }
        }
        session.take().unwrap().close().await.unwrap();

        // The stub logs every prompt line it received (cwd = agent root).
        let log = std::fs::read_to_string(temp.path().join("prompts.log")).unwrap();
        let prompts: Vec<&str> = log.lines().collect();
        assert_eq!(prompts.len(), 2, "one prompt line per turn: {log:?}");
        assert!(prompts[0].contains("[context]"), "turn 1 carries the preamble");
        assert!(
            prompts[0].contains("ISSUE-1.md"),
            "issues listing reaches the backend"
        );
        assert!(prompts[0].contains("first question"));
        assert!(
            !prompts[1].contains("[context]"),
            "turn 2 must NOT repeat the preamble"
        );
        assert!(prompts[1].contains("second question"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_session_open_becomes_an_error_block_not_a_crash() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _shutdown_tx) = start_ctx_with_pi_backend(temp.path());
        let config = ChatConfig {
            backend: None,
            command: None,
        };
        let (chat_tx, _chat_rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
        let mut session: Option<Box<dyn ChatSession>> = None;
        let mut app = App::new();
        app.chat.input = "hello".to_string();
        let prompt = app.chat.submit().unwrap();

        let mut preamble_pending = true;
        submit_turn(
            "fake", // resolved id without a registered backend
            &config,
            &ctx,
            &chat_tx,
            &mut session,
            &mut app,
            &mut preamble_pending,
            prompt,
        )
        .await;
        assert!(session.is_none());
        assert!(!app.chat.in_flight, "failed open releases the gate");
        assert!(
            preamble_pending,
            "a failed first turn keeps the preamble owed"
        );
        match app.chat.blocks.last() {
            Some(crate::chat::ChatBlock::Error(message)) => {
                assert!(message.contains("cannot open chat session"));
            }
            other => panic!("expected an error block, got {other:?}"),
        }
    }
}
