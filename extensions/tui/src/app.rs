//! TUI application state and key handling: a plain `Tab` enum + match.
//! Side effects (session opens, turns, aborts) stay in the event loop —
//! `handle_event` only mutates state and names the side effect to perform.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::chat::ChatState;
use crate::dash::{Control, DashState};
use crate::logs::LogsState;

/// PageUp/PageDown transcript scroll step, in lines.
const SCROLL_PAGE: usize = 10;

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
            _ => Action::None, // resize etc. — the loop redraws regardless
        }
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
    fn handle_chat_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Enter => match self.chat.submit() {
                Some(prompt) => Action::Submit(prompt),
                None => Action::None,
            },
            KeyCode::Esc if self.chat.in_flight => Action::AbortTurn,
            KeyCode::PageUp => {
                self.chat.scroll_up(SCROLL_PAGE);
                Action::None
            }
            KeyCode::PageDown => {
                self.chat.scroll_down(SCROLL_PAGE);
                Action::None
            }
            KeyCode::End => {
                self.chat.follow_tail();
                Action::None
            }
            KeyCode::Backspace => {
                self.chat.input.pop();
                Action::None
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.chat.input.push(c);
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

    #[test]
    fn typing_appends_and_enter_submits() {
        let mut app = App::new();
        for c in ['h', 'i', 'q'] {
            // 'q' types into chat input — it must NOT quit on the Chat tab.
            assert_eq!(app.handle_event(key(KeyCode::Char(c))), Action::None);
        }
        assert_eq!(app.handle_event(key(KeyCode::Backspace)), Action::None);
        assert_eq!(app.chat.input, "hi");
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Action::Submit("hi".to_string())
        );
        assert!(app.chat.in_flight);
    }

    #[test]
    fn enter_accepts_steering_while_in_flight() {
        let mut app = App::new();
        app.chat.input = "first".to_string();
        app.handle_event(key(KeyCode::Enter));
        app.chat.input = "second".to_string();
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
        app.chat.input = "go".to_string();
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
    fn scroll_keys_move_the_window_and_end_refollows() {
        let mut app = App::new();
        app.handle_event(key(KeyCode::PageUp));
        app.handle_event(key(KeyCode::PageUp));
        assert_eq!(app.chat.scroll_back, 20);
        app.handle_event(key(KeyCode::PageDown));
        assert_eq!(app.chat.scroll_back, 10);
        app.handle_event(key(KeyCode::End));
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
