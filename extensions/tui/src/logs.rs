//! Logs tab state: a bounded ring of log rows fed from the `host.log-events`
//! broadcast plus the retained one-shot startup banner, with follow-tail
//! scrolling. Row text is identical to `frontend-log`'s
//! `{time} {level} {target} {message}` line format (see [`format_event`]).

use std::collections::VecDeque;

use host_api::{EventBus, LogEvent, LOG_EVENTS_TOPIC, STARTUP_BANNER_TOPIC};
use tokio::sync::{broadcast, watch};

/// Max retained log rows; the oldest row is dropped when the ring is full.
pub const LOG_CAP: usize = 2000;

/// Render text for one event — exactly `frontend-log`'s
/// `{time} {level} {target} {message}` line format (the non-interactive
/// degrade path writes through this too, so the two outputs cannot drift
/// apart).
pub fn format_event(event: &LogEvent) -> String {
    format!(
        "{} {} {} {}",
        event.time, event.level, event.target, event.message
    )
}

/// One row in the logs ring: a real event, or the synthetic marker for rows
/// the broadcast channel dropped while the TUI lagged behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogRow {
    Event(LogEvent),
    Skipped(u64),
}

impl LogRow {
    pub fn text(&self) -> String {
        match self {
            LogRow::Event(event) => format_event(event),
            LogRow::Skipped(skipped) => format!("… {skipped} log lines skipped"),
        }
    }
}

#[derive(Default)]
pub struct LogsState {
    pub rows: VecDeque<LogRow>,
    /// Lines scrolled back from the tail; 0 = follow the newest row.
    pub scroll_back: usize,
    /// Subscribing to the log topic failed (frontend-log not linked): the
    /// pane shows a placeholder instead of rows.
    pub unavailable: bool,
}

impl LogsState {
    pub fn push_event(&mut self, event: LogEvent) {
        self.push(LogRow::Event(event));
    }

    pub fn push_skipped(&mut self, skipped: u64) {
        self.push(LogRow::Skipped(skipped));
    }

    fn push(&mut self, row: LogRow) {
        if self.rows.len() == LOG_CAP {
            self.rows.pop_front();
        }
        self.rows.push_back(row);
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_back += lines;
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_back = self.scroll_back.saturating_sub(lines);
    }

    /// End re-engages follow-tail.
    pub fn follow_tail(&mut self) {
        self.scroll_back = 0;
    }
}

/// What [`LogFeed::next`] delivered. Applied to state via [`LogFeed::apply`]
/// — split from `next` so the event loop's `select!` arm holds no borrow of
/// the app while awaiting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    Event(LogEvent),
    /// The broadcast channel dropped this many rows behind the TUI.
    Lagged(u64),
    /// Nothing more will arrive: stop pumping, keep the buffer.
    Closed,
    /// The one-shot retained startup banner.
    Banner(LogEvent),
}

/// The Logs tab's bus feed: the `host.log-events` broadcast plus the retained
/// startup banner, pumped through one cancellation-safe `next()` future.
pub struct LogFeed {
    events: Option<broadcast::Receiver<LogEvent>>,
    banner: Option<watch::Receiver<Option<LogEvent>>>,
    /// The banner has not been shown yet; cleared once it prints (it appears
    /// exactly once, mirroring `frontend-log`).
    banner_pending: bool,
}

impl LogFeed {
    /// Subscribe to both topics. A failed log subscription marks the pane
    /// unavailable (frontend-log not linked); an already-retained banner is
    /// pushed into the buffer immediately so it leads the log output.
    pub fn subscribe(bus: &EventBus, logs: &mut LogsState) -> Self {
        let events = match bus.subscribe::<LogEvent>(LOG_EVENTS_TOPIC) {
            Ok(events) => Some(events),
            Err(_) => {
                logs.unavailable = true;
                None
            }
        };
        let mut banner_pending = false;
        let banner = match bus.subscribe_retained::<Option<LogEvent>>(STARTUP_BANNER_TOPIC) {
            Ok(mut banner) => {
                match banner.borrow_and_update().clone() {
                    Some(event) => logs.push_event(event),
                    None => banner_pending = true,
                }
                Some(banner)
            }
            Err(_) => None,
        };
        Self {
            events,
            banner,
            banner_pending,
        }
    }

    /// Whether [`Self::next`] can still deliver anything. Once false, the
    /// feed's `select!` arm must stay disabled (all sources are done).
    pub fn active(&self) -> bool {
        self.events.is_some() || self.banner_pending
    }

    /// Wait for the next delivery. Only poll while [`Self::active`].
    pub async fn next(&mut self) -> Delivery {
        let Self {
            events,
            banner,
            banner_pending,
        } = self;
        loop {
            tokio::select! {
                event = async { events.as_mut().expect("guarded by is_some").recv().await },
                        if events.is_some() => {
                    return match event {
                        Ok(event) => Delivery::Event(event),
                        Err(broadcast::error::RecvError::Lagged(n)) => Delivery::Lagged(n),
                        Err(broadcast::error::RecvError::Closed) => Delivery::Closed,
                    };
                }
                changed = async { banner.as_mut().expect("pending implies subscribed").changed().await },
                          if *banner_pending => {
                    match changed {
                        Ok(()) => {
                            let event = banner
                                .as_mut()
                                .expect("pending implies subscribed")
                                .borrow_and_update()
                                .clone();
                            if let Some(event) = event {
                                return Delivery::Banner(event);
                            }
                        }
                        Err(_) => {
                            *banner_pending = false;
                            if events.is_none() {
                                return Delivery::Closed;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Apply one delivery to the buffer. `Closed` stops the whole feed but
    /// keeps everything received so far visible (the banner topic shares its
    /// owner with the log topic, so it can never outlive it).
    pub fn apply(&mut self, delivery: Delivery, logs: &mut LogsState) {
        match delivery {
            Delivery::Event(event) => logs.push_event(event),
            Delivery::Lagged(skipped) => logs.push_skipped(skipped),
            Delivery::Closed => {
                self.events = None;
                self.banner_pending = false;
            }
            Delivery::Banner(event) => {
                logs.push_event(event);
                self.banner_pending = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(level: &str, target: &str, message: &str) -> LogEvent {
        LogEvent {
            time: "2026-07-18 10:00:00".to_string(),
            level: level.to_string(),
            target: target.to_string(),
            message: message.to_string(),
        }
    }

    /// The topics exactly as frontend-log registers them.
    fn log_bus() -> EventBus {
        let mut bus = EventBus::new();
        bus.register_broadcast::<LogEvent>(LOG_EVENTS_TOPIC, 1024)
            .unwrap();
        bus.register_retained::<Option<LogEvent>>(STARTUP_BANNER_TOPIC, None)
            .unwrap();
        bus
    }

    #[test]
    fn row_format_matches_frontend_logs_line_format() {
        // frontend-log writes "{time} {level} {target} {message}" per line;
        // the logs tab must show byte-identical text for the same event.
        let row = LogRow::Event(event(
            "INFO",
            "issue=ISSUE-1 event=dispatched",
            "runner started",
        ));
        assert_eq!(
            row.text(),
            "2026-07-18 10:00:00 INFO issue=ISSUE-1 event=dispatched runner started"
        );
    }

    #[test]
    fn skipped_row_names_the_dropped_count() {
        assert_eq!(LogRow::Skipped(7).text(), "… 7 log lines skipped");
    }

    #[test]
    fn ring_buffer_caps_at_two_thousand_rows_dropping_the_oldest() {
        let mut logs = LogsState::default();
        for i in 0..LOG_CAP + 10 {
            logs.push_event(event("INFO", "t", &format!("row-{i}")));
        }
        assert_eq!(logs.rows.len(), LOG_CAP);
        assert_eq!(
            logs.rows.front().unwrap().text(),
            "2026-07-18 10:00:00 INFO t row-10"
        );
        assert_eq!(
            logs.rows.back().unwrap().text(),
            format!("2026-07-18 10:00:00 INFO t row-{}", LOG_CAP + 9)
        );
    }

    #[test]
    fn scroll_moves_back_and_end_refollows() {
        let mut logs = LogsState::default();
        logs.scroll_up(1);
        logs.scroll_up(10);
        assert_eq!(logs.scroll_back, 11);
        logs.scroll_down(1);
        assert_eq!(logs.scroll_back, 10);
        logs.follow_tail();
        assert_eq!(logs.scroll_back, 0);
        logs.scroll_down(5); // saturates at the tail
        assert_eq!(logs.scroll_back, 0);
    }

    #[tokio::test]
    async fn missing_topics_mark_the_pane_unavailable_and_feed_inactive() {
        let mut logs = LogsState::default();
        let feed = LogFeed::subscribe(&EventBus::new(), &mut logs);
        assert!(logs.unavailable);
        assert!(!feed.active());
    }

    #[tokio::test]
    async fn published_events_flow_into_the_buffer_in_order() {
        let bus = log_bus();
        let mut logs = LogsState::default();
        let mut feed = LogFeed::subscribe(&bus, &mut logs);
        assert!(!logs.unavailable);
        for message in ["one", "two", "three"] {
            bus.publish(LOG_EVENTS_TOPIC, event("INFO", "t", message))
                .unwrap();
        }
        for _ in 0..3 {
            let delivery = feed.next().await;
            feed.apply(delivery, &mut logs);
        }
        let texts: Vec<String> = logs.rows.iter().map(LogRow::text).collect();
        assert_eq!(
            texts,
            [
                "2026-07-18 10:00:00 INFO t one",
                "2026-07-18 10:00:00 INFO t two",
                "2026-07-18 10:00:00 INFO t three"
            ]
        );
    }

    /// The M0 startup banner (the dashboard-URL LogEvent on the retained
    /// banner topic) must appear in the logs buffer exactly once.
    #[tokio::test]
    async fn startup_banner_published_after_subscribe_lands_in_the_buffer() {
        let bus = log_bus();
        let mut logs = LogsState::default();
        let mut feed = LogFeed::subscribe(&bus, &mut logs);
        assert!(logs.rows.is_empty());

        bus.publish(
            STARTUP_BANNER_TOPIC,
            Some(event(
                "INFO",
                "issue=- event=startup",
                "dar running; dashboard on http://127.0.0.1:7878/",
            )),
        )
        .unwrap();
        let delivery = feed.next().await;
        assert!(matches!(delivery, Delivery::Banner(_)));
        feed.apply(delivery, &mut logs);
        assert_eq!(
            logs.rows.back().unwrap().text(),
            "2026-07-18 10:00:00 INFO issue=- event=startup dar running; dashboard on http://127.0.0.1:7878/"
        );
        // One-shot: the banner watch is no longer pending, so a re-publish
        // can never print it a second time.
        assert!(!feed.banner_pending);
    }

    #[tokio::test]
    async fn banner_retained_before_subscribe_leads_the_buffer() {
        let bus = log_bus();
        bus.publish(
            STARTUP_BANNER_TOPIC,
            Some(event("INFO", "issue=- event=startup", "early banner")),
        )
        .unwrap();
        let mut logs = LogsState::default();
        let feed = LogFeed::subscribe(&bus, &mut logs);
        assert_eq!(
            logs.rows.front().unwrap().text(),
            "2026-07-18 10:00:00 INFO issue=- event=startup early banner"
        );
        assert!(!feed.banner_pending);
    }

    #[tokio::test]
    async fn lag_becomes_a_synthetic_skipped_row_then_pumping_continues() {
        let mut bus = EventBus::new();
        // Tiny capacity to force the broadcast channel to drop rows.
        bus.register_broadcast::<LogEvent>(LOG_EVENTS_TOPIC, 2)
            .unwrap();
        bus.register_retained::<Option<LogEvent>>(STARTUP_BANNER_TOPIC, None)
            .unwrap();
        let mut logs = LogsState::default();
        let mut feed = LogFeed::subscribe(&bus, &mut logs);
        for i in 0..5 {
            bus.publish(LOG_EVENTS_TOPIC, event("INFO", "t", &format!("row-{i}")))
                .unwrap();
        }
        for _ in 0..3 {
            let delivery = feed.next().await;
            feed.apply(delivery, &mut logs);
        }
        let texts: Vec<String> = logs.rows.iter().map(LogRow::text).collect();
        assert_eq!(
            texts,
            [
                "… 3 log lines skipped",
                "2026-07-18 10:00:00 INFO t row-3",
                "2026-07-18 10:00:00 INFO t row-4"
            ]
        );
    }

    #[tokio::test]
    async fn closed_topic_stops_pumping_but_keeps_the_buffer() {
        let bus = log_bus();
        let mut logs = LogsState::default();
        let mut feed = LogFeed::subscribe(&bus, &mut logs);
        bus.publish(LOG_EVENTS_TOPIC, event("INFO", "t", "kept"))
            .unwrap();
        let delivery = feed.next().await;
        feed.apply(delivery, &mut logs);

        drop(bus); // both topic senders go away
        let delivery = feed.next().await;
        assert_eq!(delivery, Delivery::Closed);
        feed.apply(delivery, &mut logs);
        assert!(!feed.active(), "a dead feed must disable its select arm");
        assert_eq!(logs.rows.len(), 1, "the buffer survives the close");
        assert!(
            !logs.unavailable,
            "closed is not the unavailable placeholder"
        );
    }
}
