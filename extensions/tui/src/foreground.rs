//! The `tui` foreground: ratatui event loop on an interactive terminal,
//! byte-for-byte `frontend-log` behavior on a non-interactive one.
//!
//! The host already enabled raw mode + the alternate screen and installed the
//! restoring panic hook before `Foreground::run` (see `dar-host`); the
//! [`ExclusiveTerminal`]'s `restore()`/`Drop` undoes it. This extension adds
//! no terminal lifecycle of its own — it only writes through [`TermWriter`].

use std::time::Duration;
use std::{collections::VecDeque, io::Write};

use anyhow::Result;
use cap_chat::{ChatBackend, ChatCoordinator, ChatEvent, ChatSession, CHAT_COORDINATOR_SERVICE};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
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

/// Coalesced redraw cadence; dirty state is painted at most this often.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// Human-readable timeout notice shown when a turn is abandoned, e.g.
/// "turn timed out after 60 minutes; aborted - resend to retry".
fn timeout_notice(timeout: Duration) -> String {
    let secs = timeout.as_secs();
    let unit = if secs % 60 == 0 {
        let mins = secs / 60;
        format!("{mins} minute{}", if mins == 1 { "" } else { "s" })
    } else {
        format!("{secs} second{}", if secs == 1 { "" } else { "s" })
    };
    format!("turn timed out after {unit}; aborted - resend to retry")
}

pub struct TuiForeground {
    config: TuiConfig,
}

impl TuiForeground {
    pub fn new(config: TuiConfig) -> Self {
        Self { config }
    }
}

/// RAII guard that turns OFF the additive terminal modes (mouse capture,
/// bracketed paste) the Chat tab turns on. Its `Drop` runs on every exit
/// path — normal return, an early `?`, or a panic unwind — so the user's
/// shell is never left with mouse reporting on (which would emit escape
/// garbage and break native selection). The host's own restore handles raw
/// mode + the alternate screen; this only undoes what this extension added.
/// It writes to the real stdout (the same fd the host's alt screen lives on),
/// matching how `dar-host`'s `restore_terminal` reaches the terminal.
struct TerminalModeGuard;

impl TerminalModeGuard {
    /// Enable the modes and hand back the guard that disables them on drop.
    fn enable() -> Self {
        let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
        Self
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste
        );
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
    let turn_timeout = config.turn_timeout();
    let mut term = Terminal::new(CrosstermBackend::new(TermWriter(terminal)))?;
    // Enable mouse-wheel scroll + bracketed paste for the Chat tab. The guard's
    // Drop turns them back off on EVERY exit path (normal return, an early `?`,
    // or a panic unwind) so the shell is never left with mouse reporting on.
    let _mode_guard = TerminalModeGuard::enable();
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::input::spawn_reader(input_tx);
    // The chat event channel outlives the lazily opened session; `chat_tx`
    // stays alive here so the recv arm simply idles until the first turn.
    let (chat_tx, mut chat_rx) = tokio::sync::mpsc::channel::<ChatEvent>(256);
    // When chat-web is enabled it registers the agent-wide coordinator before
    // this foreground starts.  The TUI then joins the same backend session and
    // receives the same live events as every browser subscriber.
    let coordinator = ctx
        .host
        .services
        .get_named::<dyn ChatCoordinator>(CHAT_COORDINATOR_SERVICE)
        .ok();
    let mut shared_events = coordinator
        .as_ref()
        .map(|coordinator| coordinator.subscribe());
    let mut local_shared_users = VecDeque::new();
    let mut session: Option<Box<dyn ChatSession>> = None;
    // Backend id resolved lazily at first submit (registry is frozen after
    // boot, so the outcome — including Disabled — is final for this launch).
    let mut backend_id: Option<String> = None;
    // The first-turn context preamble is still owed to the backend.
    let mut preamble_pending = true;
    // Set by `/new`: the next session open skips resume so `pi` forks a
    // brand-new file instead of continuing the conversation just closed.
    let mut suppress_resume = false;
    let mut app = App::new();
    // Hydrate the Chat transcript from the session that will be resumed on the
    // first turn, so a restarted `dar` shows the prior conversation instead of
    // an empty pane. Display-only: these blocks are replayed into the view, not
    // re-sent to the backend (`pi` already holds them in the resumed session),
    // so the preamble stays owed and no turn is counted. Resolved from the same
    // newest-wins archive `open_session` resumes from, and only at launch; a
    // fresh start (empty dir / no resumable session) leaves the pane empty.
    // After a later `/new` forks a fresh (empty) file, that file is the next
    // restart's resume target, so the following launch hydrates nothing.
    // Best-effort: any read failure simply yields no hydration.
    if let Ok(session_dir) = sessions_dir(&config, &ctx) {
        if let Some(id) = crate::archive::newest_session_id(&session_dir) {
            let (messages, truncated) = crate::archive::read_recent(&session_dir, &id);
            app.chat.hydrate(&messages, truncated);
        }
    }
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
                        if let Some(coordinator) = coordinator.as_ref() {
                            let outbound = if preamble_pending {
                                let snapshot = ctx.host.bus.read_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC).ok();
                                format!("{}\n{prompt}", chat::build_preamble(snapshot.as_ref(), ctx.paths.root()))
                            } else { prompt.clone() };
                            if let Err(error) = coordinator.send_turn(outbound.clone(), prompt.clone()).await {
                                app.chat.fail_turn(format!("sending turn failed: {error:#}"));
                            } else {
                                preamble_pending = false;
                                local_shared_users.push_back(prompt);
                            }
                            continue;
                        }
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
                                &mut session, &mut app, &mut preamble_pending,
                                &mut suppress_resume, prompt,
                            )
                            .await;
                        }
                    }
                    Action::NewSession => {
                        if let Some(coordinator) = coordinator.as_ref() {
                            if let Err(error) = coordinator.new_session().await {
                                app.chat.push_error(format!("starting new session failed: {error:#}"));
                            }
                            preamble_pending = true;
                            continue;
                        }
                        // Close the live session, suppress resume for the next
                        // open (so the next `pi` spawn forks a fresh file),
                        // re-arm the preamble, and mark the boundary. Because
                        // resume is newest-wins (ALG-302) the freshly forked
                        // file becomes the next restart's resume target, so
                        // "start fresh" is sticky across restarts.
                        if let Some(session) = session.take() {
                            let _ = session.close().await;
                        }
                        suppress_resume = true;
                        preamble_pending = true;
                        // Drop any hydrated/prior transcript so the fresh
                        // session opens empty, matching a cold launch.
                        app.chat.clear_transcript();
                        app.chat.push_notice(
                            "— started a fresh session —".to_string(),
                        );
                    }
                    Action::AbortTurn => {
                        if let Some(coordinator) = coordinator.as_ref() {
                            if let Err(error) = coordinator.abort().await {
                                app.chat.push_error(format!("abort failed: {error:#}"));
                            }
                            continue;
                        }
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
            event = async { shared_events.as_mut().expect("guarded by is_some").recv().await },
                    if shared_events.is_some() => {
                match event {
                    Ok(event) => {
                        if let ChatEvent::User { text } = &event {
                            if local_shared_users.front() == Some(text) {
                                local_shared_users.pop_front();
                                continue;
                            }
                        }
                        app.chat.apply_event(event);
                        app.dirty = true;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        app.chat.push_error(format!("shared chat stream lagged by {skipped} events"));
                        app.dirty = true;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => shared_events = None,
                }
            }
            _ = redraw.tick() => {
                if app.chat.turn_timed_out(turn_timeout) {
                    if let Some(coordinator) = coordinator.as_ref() {
                        let _ = coordinator.abort().await;
                    } else if let Some(session) = session.as_mut() {
                        let _ = session.abort().await;
                    }
                    app.chat.abandon_turn(timeout_notice(turn_timeout));
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
    // `_mode_guard` drops here (and on any early `?`/panic above), disabling
    // mouse capture + bracketed paste before `term`'s drop lets the host
    // restore raw mode + the alternate screen.
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
    suppress_resume: &mut bool,
    prompt: String,
) {
    if session.is_none() {
        match open_session(backend_id, config, ctx, chat_tx.clone(), *suppress_resume).await {
            Ok(opened) => {
                *session = Some(opened);
                // The fork happened: a fresh file is open, so resume is no
                // longer suppressed for any later re-open this launch.
                *suppress_resume = false;
            }
            Err(e) => {
                app.chat
                    .fail_turn(format!("cannot open chat session: {e:#}"));
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

/// Open one session on the resolved backend under `data/chat/sessions`.
async fn open_session(
    backend_id: &str,
    config: &ChatConfig,
    ctx: &StartCtx,
    tx: Sender<ChatEvent>,
    suppress_resume: bool,
) -> Result<Box<dyn ChatSession>> {
    let backend = ctx.host.services.get_named::<dyn ChatBackend>(backend_id)?;
    // data_dir containment-checks against an existing data/ parent.
    std::fs::create_dir_all(ctx.paths.root().join("data"))?;
    let session_dir = sessions_dir(config, ctx)?;
    std::fs::create_dir_all(&session_dir)?;
    // Resume the newest archived session so restarting `dar` drops the human
    // back into the conversation they left. Best-effort: a missing/empty dir or
    // a malformed newest file yields `None`, opening a fresh session — never an
    // error surfaced to the human. Backends that don't understand resume (only
    // `chat-pi` does today) simply ignore the id and open fresh.
    //
    // `/new` sets `suppress_resume`, forcing a brand-new file even though an
    // archive exists: the newest session is the one we just closed, and the
    // human asked to leave it behind.
    let resume_session_id = (!suppress_resume)
        .then(|| crate::archive::newest_session_id(&session_dir))
        .flatten();
    // Build params through the shared SDK helper so the TUI talks to the same
    // agent identity (model/provider from the retained RunSnapshot, retained
    // `system.context` as system_prompt, host tool bridge, agent root cwd) as
    // every out-of-tree chat surface. The TUI-only bits — its configured
    // backend command and session resume — are layered on top.
    let params = dar_extension_sdk::chat::agent_session_params(ctx, &session_dir)
        .command(config.command.as_deref().unwrap_or(""))
        .resume_session_id(resume_session_id)
        .build();
    backend.open(params, tx).await
}

/// Resolve the sessions dir from the chat config via the shared resolver, so
/// the foreground and the `session_list` tool read the same corpus.
fn sessions_dir(config: &ChatConfig, ctx: &StartCtx) -> Result<std::path::PathBuf> {
    crate::sessions_dir(
        &crate::TuiConfig {
            chat: config.clone(),
        },
        &ctx.paths,
    )
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
    use crate::{DEFAULT_TURN_TIMEOUT, MAX_TURN_TIMEOUT_SECS};

    #[test]
    fn default_turn_timeout_is_sixty_minutes() {
        assert_eq!(DEFAULT_TURN_TIMEOUT, Duration::from_secs(3600));
        // An omitted config resolves to the 60-minute default.
        assert_eq!(ChatConfig::default().turn_timeout(), DEFAULT_TURN_TIMEOUT);
    }

    #[test]
    fn configured_turn_timeout_overrides_default() {
        let config = ChatConfig {
            turn_timeout_secs: Some(1800),
            ..Default::default()
        };
        assert_eq!(config.turn_timeout(), Duration::from_secs(1800));
    }

    #[test]
    fn timeout_notice_reports_the_configured_duration() {
        // The default surfaces as minutes (no "10 minutes" regression).
        assert_eq!(
            timeout_notice(DEFAULT_TURN_TIMEOUT),
            "turn timed out after 60 minutes; aborted - resend to retry"
        );
        assert_eq!(
            timeout_notice(Duration::from_secs(60)),
            "turn timed out after 1 minute; aborted - resend to retry"
        );
        // Non-minute durations fall back to seconds.
        assert_eq!(
            timeout_notice(Duration::from_secs(90)),
            "turn timed out after 90 seconds; aborted - resend to retry"
        );
        // Sanity: the upper bound formats without panicking.
        let _ = timeout_notice(Duration::from_secs(MAX_TURN_TIMEOUT_SECS));
    }

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
                    time: "2026-07-18 10:00:00".to_string(),
                    level: "INFO".to_string(),
                    target: "issue=- event=startup".to_string(),
                    message: "dar running; dashboard on http://127.0.0.1:7878/".to_string(),
                }),
            )
            .unwrap();
        wait_for(&buf, "dar running").await;

        host.bus
            .publish(
                LOG_EVENTS_TOPIC,
                LogEvent {
                    time: "2026-07-18 10:00:00".to_string(),
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
                    time: "2026-07-18 10:00:00".to_string(),
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
            "2026-07-18 10:00:00 INFO issue=- event=startup dar running; dashboard on http://127.0.0.1:7878/\n\
             2026-07-18 10:00:00 INFO issue=ISSUE-1 event=dispatched runner started\n\
             2026-07-18 10:00:00 WARN issue=ISSUE-1 event=stalled no events for 30s\n"
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
        start_ctx_with_pi_backend_and_snapshot(root, None)
    }

    fn start_ctx_with_pi_backend_and_snapshot(
        root: &Path,
        snapshot: Option<RunSnapshot>,
    ) -> (StartCtx, tokio::sync::watch::Sender<bool>) {
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
        if let Some(snapshot) = snapshot {
            ctx.bus
                .register_retained::<RunSnapshot>(RUN_SNAPSHOT_TOPIC, snapshot)
                .unwrap();
        }
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
            sessions_dir: None,
            turn_timeout_secs: None,
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
        let mut suppress_resume = false;
        submit_turn(
            "pi",
            &config,
            &ctx,
            &chat_tx,
            &mut session,
            &mut app,
            &mut preamble_pending,
            &mut suppress_resume,
            prompt,
        )
        .await;
        assert!(session.is_some(), "session opens lazily on first submit");
        assert!(
            !preamble_pending,
            "accepted first turn consumes the preamble"
        );
        assert!(
            temp.path().join("data/chat/sessions").is_dir(),
            "session dir created under data/chat"
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

        let (ctx, _shutdown_tx) =
            start_ctx_with_pi_backend_and_snapshot(temp.path(), Some(RunSnapshot::empty()));
        let config = ChatConfig {
            backend: None,
            command: Some(script.to_str().unwrap().to_string()),
            sessions_dir: None,
            turn_timeout_secs: None,
        };
        let (chat_tx, mut chat_rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
        let mut session: Option<Box<dyn ChatSession>> = None;
        let mut app = App::new();
        let mut preamble_pending = true;
        let mut suppress_resume = false;

        for prompt_text in ["first question", "second question"] {
            app.chat.input.clear();
            app.chat.input.insert_str(prompt_text);
            let prompt = app.chat.submit().unwrap();
            submit_turn(
                "pi",
                &config,
                &ctx,
                &chat_tx,
                &mut session,
                &mut app,
                &mut preamble_pending,
                &mut suppress_resume,
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
        assert!(
            prompts[0].contains("[context]"),
            "turn 1 carries the preamble"
        );
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
            sessions_dir: None,
            turn_timeout_secs: None,
        };
        let (chat_tx, _chat_rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
        let mut session: Option<Box<dyn ChatSession>> = None;
        let mut app = App::new();
        app.chat.input.insert_str("hello");
        let prompt = app.chat.submit().unwrap();

        let mut preamble_pending = true;
        let mut suppress_resume = false;
        submit_turn(
            "fake", // resolved id without a registered backend
            &config,
            &ctx,
            &chat_tx,
            &mut session,
            &mut app,
            &mut preamble_pending,
            &mut suppress_resume,
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

    // -- resume wiring --------------------------------------------------------

    /// Stub that records the argv it was launched with to `argv.log` (one arg
    /// per line) then answers each prompt like ECHO_STUB so the turn finishes.
    const ARGV_STUB: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a" >> argv.log; done
while IFS= read -r line; do
  case "$line" in
    *'"type":"prompt"'*)
      printf '%s\n' '{"type":"agent_end"}'
      ;;
  esac
done"#;

    /// Drive one turn through `submit_turn` against the argv-recording stub and
    /// return the argv the backend was spawned with.
    async fn argv_after_one_turn(root: &Path, script: &Path) -> Vec<String> {
        argv_after_one_turn_with(root, script, false).await
    }

    /// As [`argv_after_one_turn`], but lets the caller pre-arm `suppress_resume`
    /// so a `/new`-style open (resume forced off) can be exercised directly.
    async fn argv_after_one_turn_with(
        root: &Path,
        script: &Path,
        suppress_resume_init: bool,
    ) -> Vec<String> {
        let (ctx, _shutdown_tx) = start_ctx_with_pi_backend(root);
        let config = ChatConfig {
            backend: None,
            command: Some(script.to_str().unwrap().to_string()),
            sessions_dir: None,
            turn_timeout_secs: None,
        };
        let (chat_tx, mut chat_rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
        let mut session: Option<Box<dyn ChatSession>> = None;
        let mut app = App::new();
        app.chat.input.insert_str("ping");
        let prompt = app.chat.submit().unwrap();
        let mut preamble_pending = true;
        let mut suppress_resume = suppress_resume_init;
        submit_turn(
            "pi",
            &config,
            &ctx,
            &chat_tx,
            &mut session,
            &mut app,
            &mut preamble_pending,
            &mut suppress_resume,
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
        session.take().unwrap().close().await.unwrap();
        let log = std::fs::read_to_string(root.join("argv.log")).unwrap();
        log.lines().map(str::to_string).collect()
    }

    fn write_stub(root: &Path) -> std::path::PathBuf {
        let script = root.join("stub-pi.sh");
        std::fs::write(&script, ARGV_STUB).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_emits_resume_with_id_of_newest_prior_session() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_stub(temp.path());
        // Seed a prior session archive under the default sessions dir.
        let sessions = temp.path().join("data/chat/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("2024-01-01T00:00:00Z_old.jsonl"),
            "{\"type\":\"session\",\"id\":\"old-id\"}\n",
        )
        .unwrap();
        std::fs::write(
            sessions.join("2024-06-15T12:30:00Z_new.jsonl"),
            "{\"type\":\"session\",\"id\":\"newest-id\"}\n",
        )
        .unwrap();

        let argv = argv_after_one_turn(temp.path(), &script).await;
        let idx = argv
            .iter()
            .position(|a| a == "--session")
            .expect("--session must be emitted when a prior session exists");
        assert_eq!(argv[idx + 1], "newest-id");
        // Never the interactive picker flags, which hang the RPC TUI.
        assert!(!argv.iter().any(|a| a == "--resume" || a == "--continue"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_omits_resume_when_no_prior_session_exists() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_stub(temp.path());
        // No sessions seeded: fresh session, no --session.
        let argv = argv_after_one_turn(temp.path(), &script).await;
        assert!(
            !argv.iter().any(|a| a == "--session"),
            "fresh launch must not emit --session: {argv:?}"
        );
    }

    /// `/new` (suppress_resume) must open a brand-new file: even though a prior
    /// session is archived (and would otherwise be resumed), the open omits
    /// `--session` so `pi` forks a fresh session.
    #[tokio::test(flavor = "multi_thread")]
    async fn slash_new_open_omits_resume_despite_a_prior_session() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_stub(temp.path());
        // Seed a prior session that would be the resume target on a normal open.
        let sessions = temp.path().join("data/chat/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("2024-06-15T12:30:00Z_prev.jsonl"),
            "{\"type\":\"session\",\"id\":\"prev-id\"}\n",
        )
        .unwrap();

        let argv = argv_after_one_turn_with(temp.path(), &script, true).await;
        assert!(
            !argv.iter().any(|a| a == "--session"),
            "/new must fork a fresh file (no --session) even with a prior session: {argv:?}"
        );
    }

    /// Stub that, on launch, writes a session file whose name sorts newest and
    /// whose header carries `fresh-id` — standing in for the file `pi` forks on
    /// a `/new` open. It then answers prompts so the turn finishes.
    const FRESH_FILE_STUB: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a" >> argv.log; done
printf '%s\n' '{"type":"session","id":"fresh-id"}' \
  > data/chat/sessions/2099-01-01T00:00:00Z_fresh.jsonl
while IFS= read -r line; do
  case "$line" in
    *'"type":"prompt"'*)
      printf '%s\n' '{"type":"agent_end"}'
      ;;
  esac
done"#;

    /// End-to-end newest-wins: after a `/new` open forks a fresh file, that
    /// file is newest, so the *next* (ordinary) open resumes it — "start fresh"
    /// is sticky across restarts.
    #[tokio::test(flavor = "multi_thread")]
    async fn fresh_file_from_slash_new_becomes_the_next_resume_target() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("stub-pi.sh");
        std::fs::write(&script, FRESH_FILE_STUB).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sessions = temp.path().join("data/chat/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        // A prior session that `/new` deliberately leaves behind.
        std::fs::write(
            sessions.join("2024-06-15T12:30:00Z_prev.jsonl"),
            "{\"type\":\"session\",\"id\":\"prev-id\"}\n",
        )
        .unwrap();

        // First open under `/new`: forks the fresh file, no --session.
        let first = argv_after_one_turn_with(temp.path(), &script, true).await;
        assert!(
            !first.iter().any(|a| a == "--session"),
            "the /new open must be fresh: {first:?}"
        );
        assert!(
            sessions.join("2099-01-01T00:00:00Z_fresh.jsonl").exists(),
            "the fork wrote the fresh session file"
        );

        // Next ordinary open (e.g. after a restart): newest-wins resolves the
        // freshly forked file, so it — not the abandoned prior session — is the
        // resume target.
        let second = argv_after_one_turn(temp.path(), &script).await;
        let idx = second
            .iter()
            .position(|a| a == "--session")
            .expect("the next open resumes the newest (freshly forked) session");
        assert_eq!(
            second[idx + 1],
            "fresh-id",
            "resume target is the fresh file, not the abandoned prior session"
        );
    }
}
