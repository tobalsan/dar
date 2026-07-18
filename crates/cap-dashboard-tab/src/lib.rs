//! Dashboard tab contract.
//!
//! A general, cap-style plug point that lets any extension contribute a tab to
//! the web dashboard without the dashboard knowing anything about that
//! extension (and without the extension touching dashboard internals). Both
//! sides depend only on this crate plus `host-api`.
//!
//! ## Model: service-based discovery, dashboard-composed fragments
//!
//! Discovery is a single shared service — [`DashboardTabs`] — registered under
//! [`DASHBOARD_TABS_SERVICE`]. Each registrant adds an `Arc<dyn DashboardTab>`
//! to it during `register`; the dashboard reads the collected providers at
//! `start` to build its tab navigation.
//!
//! Rendering is *pull*, not *push*: a tab returns an HTML **fragment** (a
//! `String`) from [`DashboardTab::render`]. The dashboard owns one dynamic
//! route and dispatches `GET /tabs/{id}` to the matching provider, splicing the
//! returned fragment into the existing htmx `#content` shell. This preserves
//! the dashboard's `innerHTML`-swap pattern (no `<body>` swap) and keeps the
//! page-state (`window.__dashPage`) carried by the orchestrator run view
//! untouched: only the active tab's `#content` is replaced.
//!
//! ### Why a fragment, not a mounted endpoint
//!
//! Host HTTP routes are claimed at `register` time, before any extension
//! `start` runs, so per-extension fragment endpoints would force every
//! registrant to also reserve dashboard-shaped routes and re-implement the
//! shell. Returning a fragment through a dashboard-owned dispatch route keeps
//! the contract to one trait method and lets the dashboard remain the sole
//! owner of the htmx shell, polling cadence, and `#content` semantics.
//!
//! ### Polling inside the swap
//!
//! The dashboard's `#content` is swapped on a timer. A tab fragment that wants
//! live updates declares its own htmx polling *inside* the fragment it returns
//! (e.g. an inner element with `hx-get="/tabs/{id}" hx-trigger="every 2s"
//! hx-target="..." hx-swap="innerHTML"`), exactly as the orchestrator run view
//! does. The contract does not impose a cadence; it only guarantees the
//! fragment is composed into `#content` via `innerHTML`.
//!
//! ### Escape hatch: tabs with their own live transport
//!
//! A tab that manages its own live updates outside of htmx polling (e.g. an
//! `EventSource`/SSE stream, or any JS that must not be torn down and
//! recreated) declares [`DashboardTab::self_refreshing`] `true`. While such a
//! tab is the active one, the dashboard suspends its own `#content` timer
//! entirely instead of re-fetching and innerHTML-swapping the fragment out
//! from under the tab's live DOM/JS.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use host_api::ServiceRegistry;

/// Well-known service id under which the shared [`DashboardTabs`] registry is
/// published. Keyed by id + the `DashboardTabs` type, per `host-api` service
/// keying.
pub const DASHBOARD_TABS_SERVICE: &str = "dashboard.tabs";

/// One tab contributed to the web dashboard by an extension.
///
/// Implementations are stored as `Arc<dyn DashboardTab>` and may be called
/// concurrently, so they must be `Send + Sync`.
pub trait DashboardTab: Send + Sync {
    /// Stable, URL-safe identifier for this tab. Used as the path segment in
    /// `/tabs/{id}` and as the htmx target discriminator. Must be unique across
    /// registered tabs and contain only `[A-Za-z0-9._-]`.
    fn id(&self) -> &str;

    /// Human-readable label shown in the tab navigation.
    fn title(&self) -> &str;

    /// Render the tab body as an HTML **fragment** (no `<html>`/`<body>`). The
    /// dashboard splices this into its `#content` element via an
    /// `innerHTML`-swap. A fragment may include its own htmx attributes for
    /// in-place polling; it must not swap `<body>` or replace the dashboard
    /// shell.
    fn render(&self) -> Result<String>;

    /// Whether this tab manages its own live updates (SSE, `EventSource`, or
    /// any JS that must survive across ticks). While this tab is active the
    /// dashboard MUST NOT re-fetch/swap `#content` on its poll timer — doing
    /// so would tear down and recreate the tab's own DOM and live transport.
    /// Defaults to `false` (the tab is fine being periodically re-fetched).
    fn self_refreshing(&self) -> bool {
        false
    }

    /// Whether this tab should be the initially-active tab when the agent is
    /// **passive** (no orchestration workflow configured, so the Runs view
    /// has nothing to show). The first registered tab claiming this wins.
    /// Defaults to `false` (defer to the built-in Runs tab).
    fn passive_default(&self) -> bool {
        false
    }
}

/// Shared, append-only registry of dashboard tab providers.
///
/// Created once and shared via the host [`ServiceRegistry`]. Registrants call
/// [`DashboardTabs::shared`] (get-or-create) during their `register` and
/// [`DashboardTabs::add`] their tab; the dashboard calls
/// [`DashboardTabs::shared`] then [`DashboardTabs::snapshot`] to enumerate tabs.
///
/// Registration runs sequentially in the host's single-threaded `register`
/// loop, so get-or-create has no race; the inner `Mutex` only guards against
/// the `Arc` being shared into `start`-spawned tasks.
#[derive(Default)]
pub struct DashboardTabs {
    tabs: Mutex<Vec<Arc<dyn DashboardTab>>>,
}

impl DashboardTabs {
    /// Get the shared registry from `services`, creating and registering it on
    /// first use. Idempotent across registrants. Call this from `register`
    /// (where `services` is `&mut`); pass the same registry id everywhere.
    pub fn shared(services: &mut ServiceRegistry) -> Result<Arc<Self>> {
        if let Ok(existing) = services.get_named::<Self>(DASHBOARD_TABS_SERVICE) {
            return Ok(existing);
        }
        let registry = Arc::new(Self::default());
        services.register::<Self>(DASHBOARD_TABS_SERVICE, Arc::clone(&registry))?;
        Ok(registry)
    }

    /// Get the shared registry without creating it. For consumers reading at
    /// `start` (where the registry is frozen). Returns the registry, or an
    /// empty one when no tabs were registered — so a dashboard with zero
    /// registered tabs behaves exactly as before.
    pub fn from_services(services: &ServiceRegistry) -> Arc<Self> {
        services
            .get_named::<Self>(DASHBOARD_TABS_SERVICE)
            .unwrap_or_else(|_| Arc::new(Self::default()))
    }

    /// Append a tab provider. Ids must be unique and URL-safe.
    pub fn add(&self, tab: Arc<dyn DashboardTab>) -> Result<()> {
        let id = tab.id();
        if !is_url_safe_id(id) {
            anyhow::bail!("dashboard tab id {id:?} must contain only [A-Za-z0-9._-]");
        }
        let mut tabs = self.tabs.lock().expect("dashboard tabs registry poisoned");
        if tabs.iter().any(|existing| existing.id() == id) {
            anyhow::bail!("duplicate dashboard tab id {id:?}");
        }
        tabs.push(tab);
        Ok(())
    }

    /// Snapshot of the registered providers, in registration order.
    pub fn snapshot(&self) -> Vec<Arc<dyn DashboardTab>> {
        self.tabs
            .lock()
            .expect("dashboard tabs registry poisoned")
            .clone()
    }

    /// Look up a provider by id.
    pub fn find(&self, id: &str) -> Option<Arc<dyn DashboardTab>> {
        self.tabs
            .lock()
            .expect("dashboard tabs registry poisoned")
            .iter()
            .find(|t| t.id() == id)
            .cloned()
    }
}

fn is_url_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Escape the five HTML-unsafe characters (`& < > " '`) for safe text
/// interpolation into HTML fragments.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubTab {
        id: String,
        title: String,
        body: String,
    }

    impl DashboardTab for StubTab {
        fn id(&self) -> &str {
            &self.id
        }
        fn title(&self) -> &str {
            &self.title
        }
        fn render(&self) -> Result<String> {
            Ok(self.body.clone())
        }
    }

    fn tab(id: &str, title: &str, body: &str) -> Arc<dyn DashboardTab> {
        Arc::new(StubTab {
            id: id.to_string(),
            title: title.to_string(),
            body: body.to_string(),
        })
    }

    #[test]
    fn shared_is_get_or_create_idempotent() {
        let mut services = ServiceRegistry::default();
        let a = DashboardTabs::shared(&mut services).expect("first create");
        let b = DashboardTabs::shared(&mut services).expect("reuse existing");
        a.add(tab("one", "One", "<p>one</p>")).unwrap();
        // Both handles point at the same registry.
        assert_eq!(b.snapshot().len(), 1);
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn snapshot_preserves_registration_order_and_find_works() {
        let registry = DashboardTabs::default();
        registry.add(tab("first", "First", "<p>1</p>")).unwrap();
        registry.add(tab("second", "Second", "<p>2</p>")).unwrap();
        let ids: Vec<String> = registry
            .snapshot()
            .iter()
            .map(|t| t.id().to_string())
            .collect();
        assert_eq!(ids, vec!["first", "second"]);
        let found = registry.find("second").expect("present");
        assert_eq!(found.title(), "Second");
        assert_eq!(found.render().unwrap(), "<p>2</p>");
        assert!(registry.find("missing").is_none());
    }

    #[test]
    fn from_services_returns_empty_when_unregistered() {
        let services = ServiceRegistry::default();
        let registry = DashboardTabs::from_services(&services);
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn add_rejects_duplicate_and_invalid_ids() {
        let registry = DashboardTabs::default();
        registry.add(tab("ok-id.1", "Ok", "")).unwrap();
        assert!(registry.add(tab("ok-id.1", "Dup", "")).is_err());
        assert!(registry.add(tab("bad/id", "Bad", "")).is_err());
        assert!(registry.add(tab("", "Bad", "")).is_err());
    }

    struct LiveDefaultTab;
    impl DashboardTab for LiveDefaultTab {
        fn id(&self) -> &str {
            "live"
        }
        fn title(&self) -> &str {
            "Live"
        }
        fn render(&self) -> Result<String> {
            Ok(String::new())
        }
        fn self_refreshing(&self) -> bool {
            true
        }
        fn passive_default(&self) -> bool {
            true
        }
    }

    #[test]
    fn self_refreshing_and_passive_default_default_to_false_and_are_overridable() {
        let plain: Arc<dyn DashboardTab> = tab("plain", "Plain", "");
        assert!(!plain.self_refreshing(), "default is false");
        assert!(!plain.passive_default(), "default is false");

        let live: Arc<dyn DashboardTab> = Arc::new(LiveDefaultTab);
        assert!(live.self_refreshing(), "override honored via trait object");
        assert!(live.passive_default(), "override honored via trait object");
    }
}
