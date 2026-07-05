//! TUI application state and key handling: a plain `Tab` enum + match.
//! Side effects (session opens, turns, aborts) stay in the event loop —
//! `handle_event` only mutates state and names the side effect to perform.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

use crate::chat::ChatState;
use crate::dash::{Control, DashState};
use crate::logs::LogsState;

/// PageUp/PageDown transcript scroll step, in lines.
const SCROLL_PAGE: usize = 10;
/// Mouse-wheel transcript scroll step, in lines.
const SCROLL_WHEEL: usize = 3;

/// One pane. The live tab list is `App::tabs`: Dash joins it only when the
/// orchestrator's snapshot topic was subscribable at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Chat,
    Logs,
    Dash,
}

impl Tab {
    pub fn title(self) -> &'static str {
        match self {
            Tab::Chat => "Chat",
            Tab::Logs => "Logs",
            Tab::Dash => "Dash",
        }
    }
}

/// Side effect the event loop must perform after a key was handled.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Submit(String),
    /// The `/new` slash command: close the live session and fork a fresh one.
    NewSession,
    AbortTurn,
    /// Publish this control on `orchestrator.control` (fire-and-forget).
    Control(Control),
}

pub struct App {
    pub tab: Tab,
    /// Tab-bar render + Tab/Shift+Tab cycle order.
    pub tabs: Vec<Tab>,
    pub chat: ChatState,
    pub logs: LogsState,
    pub dash: DashState,
    pub dirty: bool,
    pub spinner_tick: usize,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            tab: Tab::Chat,
            tabs: vec![Tab::Chat, Tab::Logs],
            chat: ChatState::default(),
            logs: LogsState::default(),
            dash: DashState::default(),
            dirty: true,
            spinner_tick: 0,
        }
    }

    /// Add the Dash tab — called only when the orchestrator's retained
    /// snapshot topic was subscribable at startup (see crate docs).
    pub fn enable_dash(&mut self) {
        self.tabs.push(Tab::Dash);
    }

    fn cycle_tab(&mut self, step: isize) {
        let len = self.tabs.len() as isize;
        let pos = self
            .tabs
            .iter()
            .position(|tab| *tab == self.tab)
            .unwrap_or(0) as isize;
        self.tab = self.tabs[(pos + step).rem_euclid(len) as usize];
    }

    pub fn handle_event(&mut self, event: Event) -> Action {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) if self.tab == Tab::Chat => {
                // Bracketed paste: insert verbatim (embedded newlines and all)
                // at the cursor, never submitting.
                self.chat.input.insert_str(&text);
                Action::None
            }
            _ => Action::None, // resize etc. — the loop redraws regardless
        }
    }

    /// Mouse wheel scrolls the active transcript; everything else (clicks,
    /// drags) is left to the terminal so Shift-drag selection still works.
    fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        match mouse.kind {
            MouseEventKind::ScrollUp => match self.tab {
                Tab::Chat => self.chat.scroll_up(SCROLL_WHEEL),
                Tab::Logs => self.logs.scroll_up(SCROLL_WHEEL),
                Tab::Dash => {}
            },
            MouseEventKind::ScrollDown => match self.tab {
                Tab::Chat => self.chat.scroll_down(SCROLL_WHEEL),
                Tab::Logs => self.logs.scroll_down(SCROLL_WHEEL),
                Tab::Dash => {}
            },
            // Down/Up/Drag with no modifier would be a click — ignored so the
            // terminal's own selection (with Shift held) is unaffected.
            _ => return Action::None,
        }
        Action::None
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        // Ctrl+C quits everywhere (raw mode delivers it as a key event).
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match key.code {
            // Global tab cycling (chat input never needs the Tab key).
            KeyCode::Tab => {
                self.cycle_tab(1);
                Action::None
            }
            KeyCode::BackTab => {
                self.cycle_tab(-1);
                Action::None
            }
            _ => match self.tab {
                Tab::Chat => self.handle_chat_key(key),
                Tab::Logs => self.handle_logs_key(key),
                Tab::Dash => self.handle_dash_key(key),
            },
        }
    }

    /// Chat input is always focused on the Chat tab; there is no focus toggle.
    /// `q` deliberately types a "q" here (it only quits on Logs/Dash tabs).
    /// Enter submits; Shift/Alt+Enter inserts a newline (compose multi-line).
    /// Full cursor navigation/editing runs against the multi-line buffer.
    fn handle_chat_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        // `/new` (exact, trimmed) is the one slash command the TUI parses:
        // intercept it before `submit()` so it never reaches the backend as a
        // turn. Any other text — including input that merely begins with `/` —
        // falls through to a normal submit. Checked here, before the `input`
        // mutable borrow below, and only on a plain Enter (no Shift/Alt, which
        // compose a newline).
        if key.code == KeyCode::Enter && !(shift || alt) && self.chat.input.text().trim() == "/new"
        {
            self.chat.input.clear();
            return Action::NewSession;
        }
        let input = &mut self.chat.input;
        match key.code {
            // Shift+Enter / Alt+Enter compose a newline instead of sending.
            KeyCode::Enter if shift || alt => {
                input.insert_newline();
                Action::None
            }
            KeyCode::Enter => match self.chat.submit() {
                Some(prompt) => Action::Submit(prompt),
                None => Action::None,
            },
            KeyCode::Esc if self.chat.in_flight => Action::AbortTurn,
            // -- transcript scroll (kept on the same keys as before) --------
            KeyCode::PageUp => {
                self.chat.scroll_up(SCROLL_PAGE);
                Action::None
            }
            KeyCode::PageDown => {
                self.chat.scroll_down(SCROLL_PAGE);
                Action::None
            }
            KeyCode::Home if ctrl => {
                self.chat.scroll_up(usize::MAX);
                Action::None
            }
            KeyCode::End if ctrl => {
                self.chat.follow_tail();
                Action::None
            }
            // -- cursor navigation -----------------------------------------
            KeyCode::Left if alt => {
                input.move_word_left();
                Action::None
            }
            KeyCode::Right if alt => {
                input.move_word_right();
                Action::None
            }
            KeyCode::Left => {
                input.move_left();
                Action::None
            }
            KeyCode::Right => {
                input.move_right();
                Action::None
            }
            KeyCode::Up => {
                input.move_up();
                Action::None
            }
            KeyCode::Down => {
                input.move_down();
                Action::None
            }
            KeyCode::Home => {
                input.move_line_start();
                Action::None
            }
            KeyCode::End => {
                input.move_line_end();
                Action::None
            }
            KeyCode::Char('a') if ctrl => {
                input.move_line_start();
                Action::None
            }
            KeyCode::Char('e') if ctrl => {
                input.move_line_end();
                Action::None
            }
            // -- editing ----------------------------------------------------
            KeyCode::Char('k') if ctrl => {
                input.kill_to_line_end();
                Action::None
            }
            KeyCode::Backspace => {
                input.backspace();
                Action::None
            }
            KeyCode::Delete => {
                input.delete_forward();
                Action::None
            }
            KeyCode::Char(c) if !ctrl => {
                input.insert_char(c);
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Logs tab: follow-tail scrolling plus `q` to quit (no text input here).
    fn handle_logs_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Up => {
                self.logs.scroll_up(1);
                Action::None
            }
            KeyCode::Down => {
                self.logs.scroll_down(1);
                Action::None
            }
            KeyCode::PageUp => {
                self.logs.scroll_up(SCROLL_PAGE);
                Action::None
            }
            KeyCode::PageDown => {
                self.logs.scroll_down(SCROLL_PAGE);
                Action::None
            }
            KeyCode::End => {
                self.logs.follow_tail();
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Dash tab: `q` quits, `p`/`r`/`s` name a control publish for the event
    /// loop. Nothing local is mutated — the orchestrator is the single
    /// writer of run state, so even the paused badge waits for the next
    /// retained snapshot to reflect the control.
    fn handle_dash_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('p') => Action::Control(Control::Pause),
            KeyCode::Char('r') => Action::Control(Control::Resume),
            KeyCode::Char('s') => Action::Control(Control::Stop),
            _ => Action::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Type a string into the chat input one char at a time, as the terminal
    /// would deliver it.
    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_event(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_appends_and_enter_submits() {
        let mut app = App::new();
        // 'q' types into chat input — it must NOT quit on the Chat tab.
        type_str(&mut app, "hiq");
        assert_eq!(app.handle_event(key(KeyCode::Backspace)), Action::None);
        assert_eq!(app.chat.input.text(), "hi");
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Action::Submit("hi".to_string())
        );
        assert!(app.chat.in_flight);
    }

    #[test]
    fn exact_slash_new_is_intercepted_as_a_new_session() {
        let mut app = App::new();
        type_str(&mut app, "  /new  "); // surrounding whitespace is trimmed
        assert_eq!(app.handle_event(key(KeyCode::Enter)), Action::NewSession);
        // It is consumed, never queued as a turn: input cleared, no user block.
        assert!(app.chat.input.is_empty());
        assert!(!app.chat.in_flight);
        assert!(app.chat.blocks.is_empty());
    }

    #[test]
    fn ordinary_input_beginning_with_slash_is_a_normal_turn() {
        // Only an exact `/new` is special; other slash text submits verbatim.
        for text in ["/news", "/new now", "please /new", "/help"] {
            let mut app = App::new();
            type_str(&mut app, text);
            assert_eq!(
                app.handle_event(key(KeyCode::Enter)),
                Action::Submit(text.trim().to_string()),
                "{text:?} must submit as a normal turn"
            );
        }
    }

    #[test]
    fn shift_or_alt_enter_with_slash_new_composes_a_newline_not_a_new_session() {
        for modifier in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let mut app = App::new();
            type_str(&mut app, "/new");
            assert_eq!(
                app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, modifier))),
                Action::None
            );
            assert_eq!(app.chat.input.text(), "/new\n");
        }
    }

    #[test]
    fn shift_or_alt_enter_inserts_a_newline_instead_of_sending() {
        for modifier in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let mut app = App::new();
            type_str(&mut app, "line1");
            assert_eq!(
                app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, modifier))),
                Action::None
            );
            type_str(&mut app, "line2");
            assert_eq!(app.chat.input.text(), "line1\nline2");
            assert!(!app.chat.in_flight, "newline must not submit");
            // Plain Enter sends the whole multi-line buffer.
            assert_eq!(
                app.handle_event(key(KeyCode::Enter)),
                Action::Submit("line1\nline2".to_string())
            );
        }
    }

    #[test]
    fn bracketed_paste_inserts_multiline_text_without_submitting() {
        let mut app = App::new();
        assert_eq!(
            app.handle_event(Event::Paste("a\nb\nc".to_string())),
            Action::None
        );
        assert_eq!(app.chat.input.text(), "a\nb\nc");
        assert!(!app.chat.in_flight);
    }

    #[test]
    fn cursor_navigation_and_word_jumps_insert_at_cursor() {
        let mut app = App::new();
        type_str(&mut app, "foo bar");
        // Word-jump left to the start of "bar", then insert.
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)));
        type_str(&mut app, "X");
        assert_eq!(app.chat.input.text(), "foo Xbar");
        // ctrl+a to line start, insert.
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )));
        type_str(&mut app, ">");
        assert_eq!(app.chat.input.text(), ">foo Xbar");
        // ctrl+e to line end, ctrl+k kills nothing, delete-forward at end is
        // inert; backspace removes the last char.
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
        )));
        app.handle_event(key(KeyCode::Backspace));
        assert_eq!(app.chat.input.text(), ">foo Xba");
    }

    #[test]
    fn ctrl_k_kills_to_line_end() {
        let mut app = App::new();
        type_str(&mut app, "keep this");
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )));
        for _ in 0..4 {
            app.handle_event(key(KeyCode::Right)); // after "keep"
        }
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.chat.input.text(), "keep");
    }

    #[test]
    fn enter_accepts_steering_while_in_flight() {
        let mut app = App::new();
        type_str(&mut app, "first");
        app.handle_event(key(KeyCode::Enter));
        type_str(&mut app, "second");
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Action::Submit("second".to_string())
        );
        assert!(app.chat.input.is_empty());
    }

    #[test]
    fn esc_aborts_only_while_in_flight() {
        let mut app = App::new();
        assert_eq!(app.handle_event(key(KeyCode::Esc)), Action::None);
        type_str(&mut app, "go");
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(app.handle_event(key(KeyCode::Esc)), Action::AbortTurn);
    }

    #[test]
    fn ctrl_c_quits_everywhere() {
        let mut app = App::new();
        let event = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.handle_event(event), Action::Quit);
    }

    #[test]
    fn scroll_keys_move_the_window_and_ctrl_end_refollows() {
        let mut app = App::new();
        app.handle_event(key(KeyCode::PageUp));
        app.handle_event(key(KeyCode::PageUp));
        assert_eq!(app.chat.scroll_back, 20);
        app.handle_event(key(KeyCode::PageDown));
        assert_eq!(app.chat.scroll_back, 10);
        // Plain End is cursor-to-line-end now; Ctrl+End re-follows the tail.
        app.handle_event(key(KeyCode::End));
        assert_eq!(app.chat.scroll_back, 10);
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::End,
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.chat.scroll_back, 0);
        // Ctrl+Home pins to the oldest line.
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Home,
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.chat.scroll_back, usize::MAX);
    }

    #[test]
    fn mouse_wheel_scrolls_the_active_transcript() {
        let mut app = App::new();
        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle_event(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.chat.scroll_back, SCROLL_WHEEL);
        app.handle_event(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.chat.scroll_back, 0);
        // On the Logs tab the wheel drives the logs window, not the chat one.
        app.handle_event(key(KeyCode::Tab));
        app.handle_event(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.logs.scroll_back, SCROLL_WHEEL);
        assert_eq!(app.chat.scroll_back, 0);
    }

    #[test]
    fn tab_and_shift_tab_cycle_through_the_tab_list() {
        let mut app = App::new();
        assert_eq!(app.tab, Tab::Chat);
        app.handle_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Logs);
        app.handle_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Chat);
        app.handle_event(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Logs);
        app.handle_event(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Chat);
    }

    #[test]
    fn dash_joins_the_cycle_only_when_enabled() {
        let mut app = App::new();
        // Not enabled: cycling never reaches Dash.
        app.handle_event(key(KeyCode::Tab));
        app.handle_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Chat);

        let mut app = App::new();
        app.enable_dash();
        assert_eq!(app.tabs, vec![Tab::Chat, Tab::Logs, Tab::Dash]);
        app.handle_event(key(KeyCode::Tab));
        app.handle_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Dash);
        app.handle_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Chat);
        app.handle_event(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Dash);
    }

    #[test]
    fn dash_keys_quit_and_name_the_control_publishes() {
        let mut app = App::new();
        app.enable_dash();
        app.tab = Tab::Dash;
        assert_eq!(
            app.handle_event(key(KeyCode::Char('p'))),
            Action::Control(crate::dash::Control::Pause)
        );
        assert_eq!(
            app.handle_event(key(KeyCode::Char('r'))),
            Action::Control(crate::dash::Control::Resume)
        );
        assert_eq!(
            app.handle_event(key(KeyCode::Char('s'))),
            Action::Control(crate::dash::Control::Stop)
        );
        assert_eq!(app.handle_event(key(KeyCode::Char('q'))), Action::Quit);
        // Anything else is inert — Dash has no text input.
        assert_eq!(app.handle_event(key(KeyCode::Char('x'))), Action::None);
        assert!(app.chat.input.is_empty());
    }

    #[test]
    fn q_quits_on_the_logs_tab_only() {
        let mut app = App::new();
        app.handle_event(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Logs);
        assert_eq!(app.handle_event(key(KeyCode::Char('q'))), Action::Quit);
        // Other characters do nothing on Logs (no text input there).
        assert_eq!(app.handle_event(key(KeyCode::Char('x'))), Action::None);
        assert!(app.chat.input.is_empty());
    }

    #[test]
    fn logs_scroll_keys_move_the_window_and_end_refollows() {
        let mut app = App::new();
        app.handle_event(key(KeyCode::Tab));
        app.handle_event(key(KeyCode::PageUp));
        app.handle_event(key(KeyCode::Up));
        assert_eq!(app.logs.scroll_back, 11);
        app.handle_event(key(KeyCode::Down));
        assert_eq!(app.logs.scroll_back, 10);
        app.handle_event(key(KeyCode::PageDown));
        assert_eq!(app.logs.scroll_back, 0);
        app.handle_event(key(KeyCode::PageUp));
        app.handle_event(key(KeyCode::End));
        assert_eq!(app.logs.scroll_back, 0, "End re-engages follow-tail");
        // Logs scrolling never touches the chat transcript's scroll state.
        assert_eq!(app.chat.scroll_back, 0);
    }

    #[test]
    fn key_release_events_are_ignored() {
        let mut app = App::new();
        let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(app.handle_event(Event::Key(release)), Action::None);
        assert!(app.chat.input.is_empty());
    }
}
