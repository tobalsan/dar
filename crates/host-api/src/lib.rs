//! Domain-free extension host contracts.
//!
//! The typed event bus has two topic classes:
//!
//! - Broadcast topics deliver each published value to currently subscribed
//!   receivers. They are best-effort fan-out: a slow receiver can lag and observe
//!   a `Lagged` error from Tokio's broadcast channel; values published before a
//!   subscription are not replayed.
//! - Retained topics are watch-like. Publishing replaces the retained value and
//!   wakes subscribers. New subscribers immediately observe the latest retained
//!   value. `read_retained` returns that current value without subscribing.
//!
//! Topic payloads are typed by the caller. A topic id may only be registered for
//! one Rust payload type; attempting to reuse the same id with another type fails.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context as _, Result};
use axum::Router;
use serde_json::Value;
use tokio::sync::{broadcast, watch};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub const APP_DONE_TOPIC: &str = "host.app-done";
pub const LOG_EVENTS_TOPIC: &str = "host.log-events";

pub trait Extension: Send + Sync {
    fn id(&self) -> &'static str;

    fn register<'a>(&'a self, _ctx: &'a mut RegisterCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn start<'a>(&'a self, _ctx: StartCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
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

pub struct ExclusiveTerminal {
    interactive: bool,
    panic_hook: Option<Arc<Mutex<Option<Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>>>>>,
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

    pub fn set_previous_panic_hook(
        &mut self,
        hook: Arc<Mutex<Option<Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>>>>,
    ) {
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
    pub fn foreground(&mut self, id: impl Into<String>, factory: ForegroundFactory) -> Result<()> {
        let id = id.into();
        if self.providers.iter().any(|provider| provider.id == id) {
            bail!("foreground provider {id} is already registered");
        }
        self.providers.push(ForegroundProvider { id, factory });
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
    pub factory: ForegroundFactory,
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
}

impl HostPaths {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .context("canonicalizing host root")?;
        let data_root = root.join("data");
        Ok(Self { root, data_root })
    }

    pub fn root(&self) -> &Path {
        &self.root
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
        self.routers.push(Router::new().nest(&prefix, mount.router));
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
}
