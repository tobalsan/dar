//! Domain-free extension host contracts.
//!
//! ## Event Bus Delivery Semantics
//!
//! The typed event bus has two topic classes: broadcast and retained.
//!
//! Broadcast topics are bounded, best-effort fan-out channels backed by
//! `tokio::sync::broadcast`. `register_broadcast(id, capacity)` sets the per-topic
//! ring capacity; zero is normalized to one. Publishing is never backpressured by
//! slow subscribers and does not wait for delivery. If the ring overwrites values
//! before a receiver reads them, that receiver observes
//! `RecvError::Lagged(skipped)` and resumes at the oldest value still retained by
//! the ring. Values published before a subscription are not replayed. For one
//! publisher calling `publish` sequentially, each receiver observes the surviving
//! values for that topic in publish order; there is no ordering guarantee across
//! different topics.
//!
//! Retained topics are unbounded with respect to history because they keep exactly
//! one value: the current state. They are backed by `tokio::sync::watch`.
//! Publishing replaces the retained value and wakes subscribers; intermediate
//! values may be coalesced for a slow subscriber. New subscribers immediately
//! observe the latest value, and `read_retained` returns that current value without
//! subscribing. Retained topics are for state snapshots, not event history.
//!
//! Shutdown is coordinated outside the bus by [`ShutdownToken`]. The bus does not
//! drain broadcast rings or retained updates on shutdown; extensions that need a
//! graceful drain should stop producing when their shutdown token is cancelled and
//! finish any in-flight work they own before returning from `start`-spawned tasks.
//! Dropping the host-side bus closes the underlying Tokio channels and receivers
//! then observe `Closed`.
//!
//! Topic payloads are typed by the caller. A topic id may only be registered for
//! one Rust payload type and one delivery class; attempting to reuse the same id
//! with another type or class fails.

/// Build a `Vec<Arc<dyn Extension>>` from a list of extension values for the
/// composition root's plugin list.
#[macro_export]
macro_rules! plugins {
    ($($extension:expr),* $(,)?) => {
        vec![$(::std::sync::Arc::new($extension) as ::std::sync::Arc<dyn $crate::Extension>),*]
    };
}

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, bail, Context as _, Result};
use axum::Router;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, watch};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub const ENV_RELOAD_CONSUMERS_SERVICE: &str = "host.env-reload-consumers";
/// An explicitly opted-in cache that can reload agent-root environment values.
pub trait EnvReloadConsumer: Send + Sync {
    fn reload_env(&self) -> bool;
}

#[derive(Default)]
pub struct EnvReloadConsumers(Mutex<Vec<Arc<dyn EnvReloadConsumer>>>);

impl EnvReloadConsumers {
    pub fn register(&self, consumer: Arc<dyn EnvReloadConsumer>) {
        self.0
            .lock()
            .expect("env reload consumers poisoned")
            .push(consumer);
    }

    pub fn reload_all(&self) -> usize {
        let consumers = self
            .0
            .lock()
            .expect("env reload consumers poisoned")
            .clone();
        consumers
            .iter()
            .filter(|consumer| consumer.reload_env())
            .count()
    }
}
pub const APP_DONE_TOPIC: &str = "host.app-done";
pub const LOG_EVENTS_TOPIC: &str = "host.log-events";
/// Retained `Option<LogEvent>` holding the one-shot startup banner. Retained
/// (not broadcast) so a foreground that subscribes only when its `run` begins
/// still observes a banner published earlier during another extension's
/// `start` — broadcast topics do not replay values published before
/// subscription.
pub const STARTUP_BANNER_TOPIC: &str = "host.startup-banner";

pub trait Extension: Send + Sync {
    fn id(&self) -> &'static str;

    /// Whether this extension holds a singleton external connection — a
    /// scheduler's polling loop, a Telegram/IRC bridge — that must run at
    /// most once per agent identity. An agent connects to each such external
    /// surface once; running two loop processes for the same identity
    /// concurrently (e.g. via `--workflow`) must not open it twice.
    /// Extensions that return `true` here are skipped by hosts booted with
    /// `skip_agent_singletons` (non-default `--workflow` processes), so
    /// only the default-workflow process owns the connection. Defaults to
    /// `false`: most extensions, including per-process chat backends, are
    /// per-process-safe.
    fn agent_singleton(&self) -> bool {
        false
    }

    fn register<'a>(&'a self, _ctx: &'a mut RegisterCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn start<'a>(&'a self, _ctx: StartCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

impl Extension for Box<dyn Extension> {
    fn id(&self) -> &'static str {
        self.as_ref().id()
    }

    fn agent_singleton(&self) -> bool {
        self.as_ref().agent_singleton()
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> BoxFuture<'a, Result<()>> {
        self.as_ref().register(ctx)
    }

    fn start<'a>(&'a self, ctx: StartCtx) -> BoxFuture<'a, Result<()>> {
        self.as_ref().start(ctx)
    }
}

pub trait Foreground: Send {
    fn run<'a>(
        &'a mut self,
        ctx: StartCtx,
        terminal: ExclusiveTerminal,
    ) -> BoxFuture<'a, Result<()>>;
}

pub type ForegroundFactory = Arc<dyn Fn() -> Box<dyn Foreground> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEvent {
    pub level: String,
    pub target: String,
    pub message: String,
}

pub type SharedPanicHook =
    Arc<Mutex<Option<Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>>>>;

pub struct ExclusiveTerminal {
    interactive: bool,
    panic_hook: Option<SharedPanicHook>,
    restore: Option<Arc<dyn Fn() + Send + Sync>>,
    stdout: Box<dyn Write + Send>,
}

impl ExclusiveTerminal {
    pub fn new(
        interactive: bool,
        stdout: Box<dyn Write + Send>,
        restore: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            interactive,
            panic_hook: None,
            restore: Some(Arc::new(restore)),
            stdout,
        }
    }

    pub fn non_interactive(stdout: Box<dyn Write + Send>) -> Self {
        Self {
            interactive: false,
            panic_hook: None,
            restore: None,
            stdout,
        }
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    pub fn writer(&mut self) -> &mut dyn Write {
        self.stdout.as_mut()
    }

    pub fn restore(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore();
        }
        if let Some(hook) = self.panic_hook.take() {
            if let Some(hook) = hook.lock().expect("panic hook mutex poisoned").take() {
                std::panic::set_hook(hook);
            }
        }
    }

    pub fn set_previous_panic_hook(&mut self, hook: SharedPanicHook) {
        self.panic_hook = Some(hook);
    }
}

impl Drop for ExclusiveTerminal {
    fn drop(&mut self) {
        self.restore();
    }
}

#[derive(Default)]
pub struct ForegroundRegistry {
    providers: Vec<ForegroundProvider>,
}

impl ForegroundRegistry {
    /// Register a cooked-mode foreground (plain line stream). The tty stays in
    /// cooked mode so Ctrl-C is delivered as SIGINT.
    pub fn foreground(&mut self, id: impl Into<String>, factory: ForegroundFactory) -> Result<()> {
        self.register(id, false, factory)
    }

    /// Register a foreground that needs an exclusive raw-mode/alt-screen
    /// terminal (e.g. a full-screen TUI that reads key events itself).
    pub fn foreground_raw_mode(
        &mut self,
        id: impl Into<String>,
        factory: ForegroundFactory,
    ) -> Result<()> {
        self.register(id, true, factory)
    }

    fn register(
        &mut self,
        id: impl Into<String>,
        raw_mode: bool,
        factory: ForegroundFactory,
    ) -> Result<()> {
        let id = id.into();
        if self.providers.iter().any(|provider| provider.id == id) {
            bail!("foreground provider {id} is already registered");
        }
        self.providers.push(ForegroundProvider {
            id,
            raw_mode,
            factory,
        });
        Ok(())
    }

    pub fn select(&self, configured: Option<&str>) -> Result<Option<ForegroundProvider>> {
        match configured {
            Some(id) => self
                .providers
                .iter()
                .find(|provider| provider.id == id)
                .cloned()
                .map(Some)
                .ok_or_else(|| anyhow!("foreground provider {id} is not registered")),
            None if self.providers.is_empty() => Ok(None),
            None if self.providers.len() == 1 => Ok(Some(self.providers[0].clone())),
            None => bail!("multiple foreground providers registered; configure foreground"),
        }
    }
}

#[derive(Clone)]
pub struct ForegroundProvider {
    pub id: String,
    /// Whether this foreground needs an exclusive raw-mode/alt-screen terminal.
    /// Cooked foregrounds (plain line streams) leave the tty in cooked mode so
    /// the terminal driver keeps turning Ctrl-C into SIGINT.
    pub raw_mode: bool,
    pub factory: ForegroundFactory,
}

pub trait HostCommand: Send + Sync {
    fn run(&self, args: serde_json::Value) -> Result<()>;
}

#[derive(Clone)]
pub struct ShutdownToken {
    rx: watch::Receiver<bool>,
}

impl ShutdownToken {
    pub fn new(rx: watch::Receiver<bool>) -> Self {
        Self { rx }
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    pub async fn cancelled(&mut self) {
        if self.is_cancelled() {
            return;
        }
        let _ = self.rx.changed().await;
    }
}

#[derive(Clone)]
pub struct HostPaths {
    root: PathBuf,
    data_root: PathBuf,
    workflow_root: PathBuf,
    artifact_root: Option<PathBuf>,
}

impl HostPaths {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .context("canonicalizing host root")?;
        let data_root = root.join("data");
        let workflow_root = root.clone();
        Ok(Self {
            root,
            data_root,
            workflow_root,
            artifact_root: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The resolved WORKFLOW.md's directory. Defaults to [`Self::root`]; a
    /// non-default `--workflow <path>` run overrides it via
    /// [`Self::with_workflow_root`].
    pub fn workflow_root(&self) -> &Path {
        &self.workflow_root
    }

    /// Override the workflow root to something other than the agent root,
    /// for a `--workflow <path>` run whose WORKFLOW.md lives outside the
    /// agent folder.
    pub fn with_workflow_root(mut self, workflow_root: impl AsRef<Path>) -> Result<Self> {
        self.workflow_root = workflow_root
            .as_ref()
            .canonicalize()
            .context("canonicalizing workflow root")?;
        Ok(self)
    }

    /// Set host-owned storage for immutable artifacts. This must not be under
    /// agent root: chat runners can write agent files.
    pub fn with_artifact_root(mut self, artifact_root: impl AsRef<Path>) -> Result<Self> {
        std::fs::create_dir_all(artifact_root.as_ref()).context("creating artifact root")?;
        let artifact_root = artifact_root
            .as_ref()
            .canonicalize()
            .context("canonicalizing artifact root")?;
        if artifact_root.starts_with(&self.root) {
            bail!("artifact root must be outside agent root");
        }
        self.artifact_root = Some(artifact_root);
        Ok(self)
    }

    /// Per-agent private vault path. No artifact path is derived from agent input.
    pub fn artifact_dir(&self) -> Result<PathBuf> {
        let root = self
            .artifact_root
            .as_ref()
            .context("artifact root is not configured")?;
        let digest = Sha256::digest(self.root.as_os_str().as_encoded_bytes());
        let id: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(root.join(id))
    }

    pub fn assert_contained(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        assert_contained(&self.root, path)
    }

    pub fn data_dir(&self, ext_id: &str) -> Result<PathBuf> {
        validate_segment(ext_id)?;
        let path = self.data_root.join(ext_id);
        assert_contained(&self.root, &path)
    }
}

pub fn assert_contained(root: impl AsRef<Path>, path: impl AsRef<Path>) -> Result<PathBuf> {
    let root = root
        .as_ref()
        .canonicalize()
        .context("canonicalizing root")?;
    reject_parent_components(path.as_ref())?;
    let canonical = if path.as_ref().exists() {
        path.as_ref()
            .canonicalize()
            .context("canonicalizing path")?
    } else {
        let parent = path
            .as_ref()
            .parent()
            .ok_or_else(|| anyhow!("path has no parent"))?;
        reject_parent_components(parent)?;
        let parent = parent
            .canonicalize()
            .context("canonicalizing path parent")?;
        parent.join(path.as_ref().file_name().unwrap_or_default())
    };
    if !canonical.starts_with(&root) {
        bail!(
            "path {} escapes root {}",
            canonical.display(),
            root.display()
        );
    }
    Ok(canonical)
}

fn reject_parent_components(path: &Path) -> Result<()> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("path {} contains parent traversal", path.display());
    }
    Ok(())
}

fn validate_segment(segment: &str) -> Result<()> {
    if segment.is_empty() || Path::new(segment).components().count() != 1 {
        bail!("invalid path segment {segment:?}");
    }
    reject_parent_components(Path::new(segment))
}

#[derive(Default, Clone)]
pub struct ConfigStore {
    values: Arc<HashMap<String, Value>>,
}

impl ConfigStore {
    pub fn from_values(values: HashMap<String, Value>) -> Self {
        Self {
            values: Arc::new(values),
        }
    }

    pub fn get(&self, ext_id: &str) -> Option<&Value> {
        self.values.get(ext_id)
    }
}

pub struct EventBus {
    topics: HashMap<String, Topic>,
}

#[derive(Default, Clone)]
pub struct ServiceRegistry {
    services: Arc<Mutex<HashMap<ServiceKey, Box<dyn Any + Send + Sync>>>>,
}

impl ServiceRegistry {
    pub fn service<T>(&mut self, id: impl Into<String>, service: Arc<T>) -> Result<()>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let key = ServiceKey {
            id: id.into(),
            type_id: TypeId::of::<T>(),
        };
        let mut services = self.services.lock().expect("service registry poisoned");
        if services.insert(key, Box::new(service)).is_some() {
            bail!("service is already registered");
        }
        Ok(())
    }

    pub fn register<T>(&mut self, id: impl Into<String>, service: Arc<T>) -> Result<()>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.service::<T>(id, service)
    }

    pub fn get_named<T>(&self, id: &str) -> Result<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let key = ServiceKey {
            id: id.to_string(),
            type_id: TypeId::of::<T>(),
        };
        let services = self.services.lock().expect("service registry poisoned");
        services
            .get(&key)
            .ok_or_else(|| anyhow!("service {id} is not registered"))?
            .downcast_ref::<Arc<T>>()
            .cloned()
            .ok_or_else(|| anyhow!("service {id} type mismatch"))
    }

    pub fn get<T>(&self, id: &str) -> Result<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.get_named::<T>(id)
    }
}

#[derive(Hash, PartialEq, Eq, Clone)]
struct ServiceKey {
    id: String,
    type_id: TypeId,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            topics: HashMap::new(),
        }
    }

    pub fn register_broadcast<T>(&mut self, id: impl Into<String>, capacity: usize) -> Result<()>
    where
        T: Clone + Send + Sync + 'static,
    {
        let id = id.into();
        ensure_new_topic(&self.topics, &id, TypeId::of::<T>())?;
        let (tx, _) = broadcast::channel::<T>(capacity.max(1));
        self.topics.insert(
            id,
            Topic::Broadcast {
                type_id: TypeId::of::<T>(),
                sender: Box::new(tx),
            },
        );
        Ok(())
    }

    pub fn subscribe<T>(&self, id: &str) -> Result<broadcast::Receiver<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        match self.topics.get(id) {
            Some(Topic::Broadcast { type_id, sender }) if *type_id == TypeId::of::<T>() => sender
                .downcast_ref::<broadcast::Sender<T>>()
                .map(|sender| sender.subscribe())
                .ok_or_else(|| anyhow!("topic {id} sender type mismatch")),
            Some(_) => bail!("topic {id} is not a broadcast topic for requested payload type"),
            None => bail!("topic {id} is not registered"),
        }
    }

    pub fn publish<T>(&self, id: &str, value: T) -> Result<()>
    where
        T: Clone + Send + Sync + 'static,
    {
        match self.topics.get(id) {
            Some(Topic::Broadcast { type_id, sender }) if *type_id == TypeId::of::<T>() => {
                let sender = sender
                    .downcast_ref::<broadcast::Sender<T>>()
                    .ok_or_else(|| anyhow!("topic {id} sender type mismatch"))?;
                let _ = sender.send(value);
                Ok(())
            }
            Some(Topic::Retained { type_id, sender }) if *type_id == TypeId::of::<T>() => {
                let sender = sender
                    .downcast_ref::<watch::Sender<T>>()
                    .ok_or_else(|| anyhow!("topic {id} sender type mismatch"))?;
                sender.send_replace(value);
                Ok(())
            }
            Some(_) => bail!("topic {id} has a different payload type"),
            None => bail!("topic {id} is not registered"),
        }
    }

    pub fn register_retained<T>(&mut self, id: impl Into<String>, initial: T) -> Result<()>
    where
        T: Clone + Send + Sync + 'static,
    {
        let id = id.into();
        ensure_new_topic(&self.topics, &id, TypeId::of::<T>())?;
        let (tx, _) = watch::channel(initial);
        self.topics.insert(
            id,
            Topic::Retained {
                type_id: TypeId::of::<T>(),
                sender: Box::new(tx),
            },
        );
        Ok(())
    }

    pub fn subscribe_retained<T>(&self, id: &str) -> Result<watch::Receiver<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        match self.topics.get(id) {
            Some(Topic::Retained { type_id, sender }) if *type_id == TypeId::of::<T>() => sender
                .downcast_ref::<watch::Sender<T>>()
                .map(|sender| sender.subscribe())
                .ok_or_else(|| anyhow!("topic {id} sender type mismatch")),
            Some(_) => bail!("topic {id} is not a retained topic for requested payload type"),
            None => bail!("topic {id} is not registered"),
        }
    }

    pub fn read_retained<T>(&self, id: &str) -> Result<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        Ok(self.subscribe_retained::<T>(id)?.borrow().clone())
    }
}

fn ensure_new_topic(topics: &HashMap<String, Topic>, id: &str, type_id: TypeId) -> Result<()> {
    match topics.get(id) {
        Some(topic) if topic.payload_type_id() == type_id => {
            bail!("topic {id} is already registered")
        }
        Some(_) => bail!("topic {id} is already registered with another payload type"),
        None => Ok(()),
    }
}

enum Topic {
    Broadcast {
        type_id: TypeId,
        sender: Box<dyn Any + Send + Sync>,
    },
    Retained {
        type_id: TypeId,
        sender: Box<dyn Any + Send + Sync>,
    },
}

impl Topic {
    fn payload_type_id(&self) -> TypeId {
        match self {
            Topic::Broadcast { type_id, .. } | Topic::Retained { type_id, .. } => *type_id,
        }
    }
}

#[derive(Default)]
pub struct HttpRegistry {
    disabled: bool,
    occupied: HashSet<String>,
    routers: Vec<Router>,
}

impl HttpRegistry {
    pub fn disabled() -> Self {
        Self {
            disabled: true,
            ..Self::default()
        }
    }

    pub fn mount(&mut self, mount: HttpMount) -> Result<()> {
        if self.disabled {
            return Ok(());
        }
        let prefix = normalize_namespace(&mount.namespace)?;
        if mount.claim_root {
            self.claim("/")?;
        }
        for route in &mount.routes {
            self.claim(&format!("{prefix}{}", normalize_route(route)?))?;
        }
        if prefix == "/" {
            self.routers.push(mount.router);
        } else {
            self.routers.push(Router::new().nest(&prefix, mount.router));
        }
        Ok(())
    }

    pub fn into_router(self) -> Router {
        self.routers
            .into_iter()
            .fold(Router::new(), |acc, router| acc.merge(router))
    }

    fn claim(&mut self, path: &str) -> Result<()> {
        if !self.occupied.insert(path.to_string()) {
            bail!("HTTP route collision at {path}");
        }
        Ok(())
    }
}

pub struct HttpMount {
    pub namespace: String,
    pub router: Router,
    pub routes: Vec<String>,
    pub claim_root: bool,
}

fn normalize_namespace(namespace: &str) -> Result<String> {
    if namespace.is_empty() || namespace == "/" {
        return Ok("/".to_string());
    }
    if !namespace.starts_with('/') || namespace.ends_with('/') {
        bail!("HTTP namespace must start with / and not end with /");
    }
    Ok(namespace.to_string())
}

fn normalize_route(route: &str) -> Result<String> {
    if route.is_empty() || !route.starts_with('/') {
        bail!("HTTP route must start with /");
    }
    Ok(route.to_string())
}

pub struct RegisterCtx {
    pub bus: EventBus,
    pub http: HttpRegistry,
    pub foreground: ForegroundRegistry,
    pub services: ServiceRegistry,
    pub paths: HostPaths,
    pub config: ConfigStore,
    pub shutdown: ShutdownToken,
}

impl RegisterCtx {
    pub fn data_dir(&self, ext_id: &str) -> Result<PathBuf> {
        self.paths.data_dir(ext_id)
    }

    pub fn into_start_services(self) -> Result<StartServices> {
        Ok(StartServices {
            bus: Arc::new(self.bus),
            router: Arc::new(self.http.into_router()),
            services: self.services,
            http_addr: Arc::new(OnceLock::new()),
        })
    }
}

#[derive(Clone)]
pub struct StartCtx {
    pub shutdown: ShutdownToken,
    pub paths: HostPaths,
    pub config: ConfigStore,
    pub host: StartServices,
}

#[derive(Clone)]
pub struct StartServices {
    pub bus: Arc<EventBus>,
    pub router: Arc<Router>,
    pub services: ServiceRegistry,
    /// Socket address the host HTTP server actually bound, set once by the host
    /// after a successful synchronous bind and before any extension `start`
    /// runs. `None` when HTTP is disabled. With an OS-assigned port (`:0`) this
    /// is how an extension learns the real ephemeral port it ended up on.
    http_addr: Arc<OnceLock<std::net::SocketAddr>>,
}

impl StartServices {
    /// The address the host HTTP server bound, if HTTP is enabled and bound.
    pub fn http_addr(&self) -> Option<std::net::SocketAddr> {
        self.http_addr.get().copied()
    }

    /// Record the bound address. Called once by the host; ignored if already set.
    pub fn set_http_addr(&self, addr: std::net::SocketAddr) {
        let _ = self.http_addr.set(addr);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::{EnvReloadConsumer, EnvReloadConsumers, HostPaths};

    struct CachedSecret {
        value: std::sync::Mutex<String>,
        reloads: AtomicUsize,
    }

    impl EnvReloadConsumer for CachedSecret {
        fn reload_env(&self) -> bool {
            *self.value.lock().unwrap() = std::env::var("HOST_API_RELOAD_SECRET").unwrap();
            self.reloads.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    #[test]
    fn opted_in_consumer_is_refreshed() {
        std::env::set_var("HOST_API_RELOAD_SECRET", "before");
        let consumers = EnvReloadConsumers::default();
        let cache = Arc::new(CachedSecret {
            value: std::sync::Mutex::new(std::env::var("HOST_API_RELOAD_SECRET").unwrap()),
            reloads: AtomicUsize::new(0),
        });
        consumers.register(cache.clone());
        std::env::set_var("HOST_API_RELOAD_SECRET", "after");
        assert_eq!(consumers.reload_all(), 1);
        assert_eq!(*cache.value.lock().unwrap(), "after");
        assert_eq!(cache.reloads.load(Ordering::SeqCst), 1);
        std::env::remove_var("HOST_API_RELOAD_SECRET");
    }

    #[test]
    fn artifact_root_must_be_host_private() {
        let agent = tempfile::tempdir().unwrap();
        assert!(HostPaths::new(agent.path())
            .unwrap()
            .with_artifact_root(agent.path().join("data/artifacts"))
            .is_err());
    }

    #[test]
    fn artifact_dir_is_stable_and_outside_agent_root() {
        let agent = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        let paths = HostPaths::new(agent.path())
            .unwrap()
            .with_artifact_root(host.path().join("artifacts"))
            .unwrap();
        let artifact_dir = paths.artifact_dir().unwrap();
        assert!(artifact_dir.starts_with(host.path().canonicalize().unwrap()));
        assert!(!artifact_dir.starts_with(agent.path().canonicalize().unwrap()));
        assert_eq!(artifact_dir, paths.artifact_dir().unwrap());
    }
}
