use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use fs2::FileExt;
use host_api::{Extension, HttpMount, RegisterCtx};
use serde_json::json;
use tool_registry::{
    ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec, TOOL_REGISTRY_SERVICE,
};

use crate::composer;

pub enum RestartMode {
    Execv,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayOutcome {
    Accepted,
    Busy,
    NotSent,
    DeliveryUnknown,
    Rejected(u16),
}

/// CLI-owned lifecycle controller injected into a running host. The host only
/// sees its HTTP/tool adapters; compose/build/doctor/swap remain CLI concerns.
#[derive(Clone)]
pub struct RebuildCoordinator {
    agent: std::path::PathBuf,
    run_args: Vec<OsString>,
    busy: Arc<AtomicBool>,
}

impl RebuildCoordinator {
    pub fn new(agent: std::path::PathBuf, run_args: Vec<OsString>) -> Self {
        Self {
            agent,
            run_args,
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn trigger(&self) -> bool {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let this = self.clone();
        let busy = Arc::clone(&self.busy);
        tokio::spawn(async move {
            match tokio::task::spawn_blocking(move || this.rebuild_and_exec()).await {
                Ok(Err(error)) => eprintln!(
                    "dar: live self-update failed; current host remains running: {error:#}"
                ),
                Err(error) => eprintln!(
                    "dar: live self-update task failed; current host remains running: {error}"
                ),
                Ok(Ok(())) => {}
            }
            // exec never returns. Every other pipeline/task failure must permit
            // a later request instead of leaving the host permanently busy.
            busy.store(false, Ordering::Release);
        });
        true
    }

    fn rebuild_and_exec(&self) -> Result<()> {
        rebuild_with_options(
            &self.agent,
            composer::BuildOptions::default(),
            RestartMode::Skip,
        )?;
        exec_agent_run(&self.agent, &self.run_args)
    }
}

pub struct LiveRebuildExtension {
    coordinator: RebuildCoordinator,
}
impl LiveRebuildExtension {
    pub fn new(agent: std::path::PathBuf, run_args: Vec<OsString>) -> Self {
        Self {
            coordinator: RebuildCoordinator::new(agent, run_args),
        }
    }
}

impl Extension for LiveRebuildExtension {
    fn id(&self) -> &'static str {
        "live-self-update"
    }
    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let coordinator = self.coordinator.clone();
            let app = Router::new()
                .route("/self-update/rebuild", post(http_rebuild))
                .route("/health", get(|| async { "ok" }))
                .with_state(coordinator.clone());
            ctx.http.mount(HttpMount {
                namespace: "/".into(),
                router: app,
                routes: vec!["/self-update/rebuild".into(), "/health".into()],
                claim_root: false,
            })?;
            Ok(())
        })
    }
}

async fn http_rebuild(State(coordinator): State<RebuildCoordinator>) -> impl IntoResponse {
    if coordinator.trigger() {
        (StatusCode::ACCEPTED, "self-update accepted")
    } else {
        (StatusCode::CONFLICT, "self-update already in progress")
    }
}

pub struct BridgeSelfUpdateExtension {
    agent: std::path::PathBuf,
    workflow: Option<std::path::PathBuf>,
    host_addr: Option<std::net::SocketAddr>,
}

impl BridgeSelfUpdateExtension {
    pub fn new(
        agent: std::path::PathBuf,
        workflow: Option<std::path::PathBuf>,
        host_addr: Option<std::net::SocketAddr>,
    ) -> Self {
        Self {
            agent,
            workflow,
            host_addr,
        }
    }
}

impl Extension for BridgeSelfUpdateExtension {
    fn id(&self) -> &'static str {
        "bridge-self-update"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let registry = match ctx
                .services
                .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
            {
                Ok(registry) => registry,
                Err(_) => return Ok(()),
            };
            registry.register_tool(
                ToolSpec::new(
                    "self_update",
                    "Request a rebuild of this live agent host. The tool response is best-effort; confirm restart through host health and logs.",
                    json!({"type":"object","additionalProperties":false}),
                ).writes(),
                Arc::new(SelfUpdateTool {
                    agent: self.agent.clone(),
                    workflow: self.workflow.clone(),
                    host_addr: self.host_addr,
                }),
            )
        })
    }
}

struct SelfUpdateTool {
    agent: std::path::PathBuf,
    workflow: Option<std::path::PathBuf>,
    host_addr: Option<std::net::SocketAddr>,
}
#[async_trait::async_trait]
impl ToolExecutor for SelfUpdateTool {
    async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutcome> {
        let agent = self.agent.clone();
        let workflow = self.workflow.clone();
        let host_addr = self.host_addr;
        tokio::task::spawn_blocking(move || {
            trigger_live_by_identity(&agent, workflow.as_deref(), host_addr)
        })
        .await
        .context("self_update relay task failed")?
        .map(|outcome| match outcome {
            RelayOutcome::Accepted => {
                ToolOutcome::ok("Self-update accepted; restart confirmation is best-effort.")
            }
            RelayOutcome::Busy => {
                ToolOutcome::error_code("busy", "Self-update already in progress.", None::<String>)
            }
            RelayOutcome::DeliveryUnknown => ToolOutcome::error_code(
                "delivery_unknown",
                "Request was written but the host restarted before replying.",
                None::<String>,
            ),
            RelayOutcome::NotSent => ToolOutcome::error_code(
                "not_sent",
                "Could not deliver self-update request.",
                None::<String>,
            ),
            RelayOutcome::Rejected(status) => ToolOutcome::error_code(
                "rejected",
                format!("Self-update rejected with HTTP {status}."),
                None::<String>,
            ),
        })
    }
}

pub fn rebuild(_agent: &Path, _restart: RestartMode) -> Result<()> {
    rebuild_with_options(_agent, composer::BuildOptions::default(), _restart)
}

pub fn rebuild_with_options(
    _agent: &Path,
    options: composer::BuildOptions,
    _restart: RestartMode,
) -> Result<()> {
    let agent = _agent
        .canonicalize()
        .with_context(|| format!("resolving agent folder {}", _agent.display()))?;
    ensure_self_rebuild_output_runnable(&options)?;
    with_lock(&agent, || {
        composer::compose(&agent)?;
        let new_binary = agent.join("bin/dar.new");
        if new_binary.exists() {
            fs::remove_file(&new_binary)
                .with_context(|| format!("removing stale {}", new_binary.display()))?;
        }
        composer::build_to_with_options(&agent, &new_binary, options)?;
        doctor_gate(&agent, &ProcessDoctor)?;
        atomic_swap(&agent)?;
        match _restart {
            RestartMode::Execv => exec_current_process(),
            RestartMode::Skip => Ok(()),
        }
    })
}

fn ensure_self_rebuild_output_runnable(options: &composer::BuildOptions) -> Result<()> {
    if options.universal {
        return Ok(());
    }
    let Some(target) = options.target.as_deref() else {
        return Ok(());
    };
    if target_is_host_runnable(target) {
        return Ok(());
    }
    bail!("self rebuild target `{target}` is not runnable on this host; use a host target or `build --target` for cross-built artifacts")
}

fn target_is_host_runnable(target: &str) -> bool {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => {
            matches!(
                target,
                "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl"
            )
        }
        ("linux", "aarch64") => {
            matches!(
                target,
                "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl"
            )
        }
        ("macos", "x86_64") => target == "x86_64-apple-darwin",
        ("macos", "aarch64") => target == "aarch64-apple-darwin",
        _ => false,
    }
}

trait DoctorRunner {
    fn run(&self, new_binary: &Path, agent: &Path) -> Result<()>;
}

struct ProcessDoctor;

impl DoctorRunner for ProcessDoctor {
    fn run(&self, new_binary: &Path, agent: &Path) -> Result<()> {
        let output = Command::new(new_binary)
            .args(["doctor", "--dir"])
            .arg("..")
            .current_dir(agent.join("bin"))
            .output()
            .with_context(|| format!("running doctor gate {}", new_binary.display()))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("doctor gate failed with {}:\n{stderr}", output.status);
    }
}

fn doctor_gate(agent: &Path, doctor: &dyn DoctorRunner) -> Result<()> {
    let new_binary = agent.join("bin/dar.new");
    match doctor.run(&new_binary, agent) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&new_binary);
            Err(e).context("doctor gate failed")
        }
    }
}

fn atomic_swap(agent: &Path) -> Result<()> {
    let bin = agent.join("bin/dar");
    let new = agent.join("bin/dar.new");
    let prev = agent.join("bin/dar.prev");
    fs::rename(&bin, &prev)
        .with_context(|| format!("renaming {} to {}", bin.display(), prev.display()))?;
    if let Err(e) = fs::rename(&new, &bin) {
        let rollback = fs::rename(&prev, &bin);
        if let Err(rollback_err) = rollback {
            return Err(e).with_context(|| {
                format!(
                    "renaming {} to {} failed; rollback {} to {} also failed: {}",
                    new.display(),
                    bin.display(),
                    prev.display(),
                    bin.display(),
                    rollback_err
                )
            });
        }
        return Err(e).with_context(|| format!("renaming {} to {}", new.display(), bin.display()));
    }
    Ok(())
}

fn with_lock<T>(agent: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _thread_guard = process_lock()
        .lock()
        .map_err(|_| anyhow::anyhow!("self-update in-process lock poisoned"))?;
    fs::create_dir_all(agent.join("data"))
        .with_context(|| format!("creating {}", agent.join("data").display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(agent.join("data/self-update.lock"))
        .with_context(|| format!("opening {}", agent.join("data/self-update.lock").display()))?;
    LockGuard::lock(lock)?;
    f()
}

fn process_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct LockGuard(File);

impl LockGuard {
    fn lock(file: File) -> Result<Self> {
        file.lock_exclusive().context("locking self-update lock")?;
        Ok(Self(file))
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn exec_current_process() -> Result<()> {
    let argv0 = std::env::args_os()
        .next()
        .context("current process argv[0] is missing")?;
    let args = std::env::args_os().skip(1).collect::<Vec<OsString>>();
    let err = Command::new(&argv0).args(args).exec();
    Err(err).with_context(|| format!("execv failed for {}", Path::new(&argv0).display()))
}

fn exec_agent_run(agent: &Path, args: &[OsString]) -> Result<()> {
    let executable = agent.join("bin/dar");
    let err = Command::new(&executable).args(args).exec();
    Err(err).with_context(|| format!("execv failed for {}", executable.display()))
}

pub fn run_rebuild_command(
    args: crate::cli::SelfRebuildArgs,
    options: composer::BuildOptions,
) -> Result<()> {
    match (args.agent, args.dir) {
        (None, Some(dir)) => rebuild_with_options(
            &dir.canonicalize()
                .with_context(|| format!("resolving agent folder {}", dir.display()))?,
            options,
            RestartMode::Skip,
        ),
        (Some(name), None) => {
            if options.vendor
                || options.offline
                || options.target.is_some()
                || options.static_
                || options.universal
            {
                bail!("build flags are unsupported for live rebuilds; use --dir for an offline rebuild")
            }
            trigger_live_by_name(
                &name,
                args.workflow.as_deref(),
                args.registry_dir.as_deref(),
            )
        }
        (None, None) => {
            bail!("pass an agent name for a live rebuild or --dir for an offline rebuild")
        }
        (Some(_), Some(_)) => bail!("agent name and --dir cannot be used together"),
    }
}

fn trigger_live_by_name(
    name: &str,
    workflow: Option<&Path>,
    registry_dir: Option<&Path>,
) -> Result<()> {
    let registry = dar_presence::Registry::new(
        registry_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(dar_presence::default_registry_dir),
    );
    let mut matches: Vec<_> = registry
        .read_live()
        .into_iter()
        .filter(|entry| entry.id == name)
        .collect();
    if let Some(workflow) = workflow {
        let workflow = if workflow.is_dir() {
            workflow.join("WORKFLOW.md")
        } else {
            workflow.to_path_buf()
        }
        .canonicalize()
        .with_context(|| format!("resolving --workflow {}", workflow.display()))?;
        matches.retain(|entry| Path::new(&entry.workflow) == workflow);
    }
    let entry = select_live_entry(name, matches)?;
    trigger_live_entry(&registry, &entry)
}

fn trigger_live_by_identity(
    agent: &Path,
    workflow: Option<&Path>,
    host_addr: Option<std::net::SocketAddr>,
) -> Result<RelayOutcome> {
    let agent = agent
        .canonicalize()
        .with_context(|| format!("resolving agent folder {}", agent.display()))?;
    let workflow = workflow.map(Path::canonicalize).transpose()?;
    let workflow = workflow.ok_or_else(|| anyhow::anyhow!("bridge missing workflow identity"))?;
    let addr = bridge_addr(&agent, &workflow, host_addr)?;
    Ok(post_request(&addr, "/self-update/rebuild"))
}

fn bridge_addr(
    agent: &Path,
    workflow: &Path,
    host_addr: Option<std::net::SocketAddr>,
) -> Result<String> {
    match host_addr {
        Some(addr) => Ok(addr.to_string()),
        None => {
            let (bind, port) = crate::dashboard_addr_for_root(agent, workflow)?;
            Ok(format!("{bind}:{port}"))
        }
    }
}

fn select_live_entry(
    name: &str,
    matches: Vec<dar_presence::PresenceEntry>,
) -> Result<dar_presence::PresenceEntry> {
    match matches.as_slice() {
        [] => bail!("no live dashboard presence found for agent `{name}`"),
        [entry] => Ok(entry.clone()),
        many => bail!(
            "agent `{name}` has {} live workflows; pass --workflow: {}",
            many.len(),
            many.iter()
                .map(|e| e.workflow.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn trigger_live_entry(
    registry: &dar_presence::Registry,
    entry: &dar_presence::PresenceEntry,
) -> Result<()> {
    match post_request(&entry.addr, "/self-update/rebuild") {
        RelayOutcome::Accepted | RelayOutcome::DeliveryUnknown => {}
        RelayOutcome::Busy => bail!("live rebuild already in progress"),
        RelayOutcome::NotSent => bail!("live rebuild request was not sent"),
        RelayOutcome::Rejected(status) => bail!("live rebuild request rejected with HTTP {status}"),
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(current) = registry
            .read_live()
            .into_iter()
            .find(|e| e.id == entry.id && e.folder == entry.folder && e.workflow == entry.workflow)
        {
            if restart_identity_changed(entry, &current) && get_ok(&current.addr, "/health") {
                return Ok(());
            }
        }
    }
    bail!("live rebuild accepted but restart was not confirmed within 60 seconds")
}

fn restart_identity_changed(
    before: &dar_presence::PresenceEntry,
    current: &dar_presence::PresenceEntry,
) -> bool {
    before.id == current.id
        && before.folder == current.folder
        && before.workflow == current.workflow
        && before.started_at != current.started_at
}

fn dial_addr(addr: &str) -> String {
    if let Some(port) = addr.strip_prefix("0.0.0.0:") {
        return format!("127.0.0.1:{port}");
    }
    addr.to_string()
}
fn post_request(addr: &str, path: &str) -> RelayOutcome {
    use std::io::{Read, Write};
    let addr = dial_addr(addr);
    let timeout = Duration::from_secs(3);
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(
        &addr
            .parse()
            .unwrap_or_else(|_| "127.0.0.1:9".parse().unwrap()),
        timeout,
    ) else {
        return RelayOutcome::NotSent;
    };
    if stream.set_write_timeout(Some(timeout)).is_err()
        || stream.set_read_timeout(Some(timeout)).is_err()
    {
        return RelayOutcome::NotSent;
    }
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    // Only a fully written request can have reached the host. A failed partial
    // write must not cause callers to wait for a restart that was never asked
    // for. Once every byte is written, a flush/read failure is delivery-unknown.
    if !write_request(&mut stream, request.as_bytes()) {
        return RelayOutcome::NotSent;
    }
    if stream.flush().is_err() {
        return RelayOutcome::DeliveryUnknown;
    }
    let mut response = vec![0; 8192];
    match stream.read(&mut response) {
        Ok(0) | Err(_) => RelayOutcome::DeliveryUnknown,
        Ok(n) => {
            let response = String::from_utf8_lossy(&response[..n]);
            let line = response.lines().next().unwrap_or("");
            match line.split_whitespace().nth(1).and_then(|s| s.parse().ok()) {
                Some(202) => RelayOutcome::Accepted,
                Some(409) => RelayOutcome::Busy,
                Some(status) => RelayOutcome::Rejected(status),
                None => RelayOutcome::DeliveryUnknown,
            }
        }
    }
}

fn write_request(writer: &mut impl std::io::Write, request: &[u8]) -> bool {
    let mut written = 0;
    while written < request.len() {
        match writer.write(&request[written..]) {
            Ok(0) | Err(_) => return false,
            Ok(count) => written += count,
        }
    }
    true
}
fn get_ok(addr: &str, path: &str) -> bool {
    use std::io::{Read, Write};
    let addr = dial_addr(addr);
    let timeout = Duration::from_secs(3);
    let Ok(socket) = addr.parse() else {
        return false;
    };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&socket, timeout) else {
        return false;
    };
    if stream.set_write_timeout(Some(timeout)).is_err()
        || stream.set_read_timeout(Some(timeout)).is_err()
    {
        return false;
    }
    if stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .is_err()
    {
        return false;
    }
    let mut response = [0; 8192];
    stream
        .read(&mut response)
        .is_ok_and(|n| String::from_utf8_lossy(&response[..n]).starts_with("HTTP/1.1 200"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn doctor_gate_abort_leaves_current_binary_and_removes_new() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().to_path_buf();
        write_executable(&agent.join("bin/dar"), "old");
        write_executable(&agent.join("bin/dar.new"), "new");

        let err = doctor_gate(&agent, &FakeDoctor { ok: false }).unwrap_err();

        assert!(err.to_string().contains("doctor gate failed"));
        assert_eq!(fs::read_to_string(agent.join("bin/dar")).unwrap(), "old");
        assert!(!agent.join("bin/dar.new").exists());
        assert!(!agent.join("bin/dar.prev").exists());
    }

    #[test]
    fn atomic_rename_preserves_prev() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().to_path_buf();
        write_executable(&agent.join("bin/dar"), "old");
        write_executable(&agent.join("bin/dar.new"), "new");

        atomic_swap(&agent).unwrap();

        assert_eq!(fs::read_to_string(agent.join("bin/dar")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(agent.join("bin/dar.prev")).unwrap(),
            "old"
        );
    }

    #[test]
    fn atomic_rename_restores_current_when_new_binary_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().to_path_buf();
        write_executable(&agent.join("bin/dar"), "old");
        write_executable(&agent.join("bin/dar.prev"), "older");

        let err = atomic_swap(&agent).unwrap_err();

        assert!(err.to_string().contains("renaming"));
        assert_eq!(fs::read_to_string(agent.join("bin/dar")).unwrap(), "old");
    }

    #[test]
    fn lock_serializes_concurrent_updates() {
        let _ = RestartMode::Skip;
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().to_path_buf();
        let events = Arc::new(Mutex::new(Vec::new()));
        let first_events = Arc::clone(&events);
        let second_events = Arc::clone(&events);

        let first = thread::spawn(move || {
            with_lock(&agent, || {
                first_events
                    .lock()
                    .unwrap()
                    .push(("first-start", Instant::now()));
                thread::sleep(Duration::from_millis(150));
                first_events
                    .lock()
                    .unwrap()
                    .push(("first-end", Instant::now()));
                Ok(())
            })
            .unwrap();
        });
        thread::sleep(Duration::from_millis(25));
        let agent = temp.path().to_path_buf();
        let second = thread::spawn(move || {
            with_lock(&agent, || {
                second_events
                    .lock()
                    .unwrap()
                    .push(("second-start", Instant::now()));
                Ok(())
            })
            .unwrap();
        });

        first.join().unwrap();
        second.join().unwrap();

        let events = events.lock().unwrap();
        let first_end = events
            .iter()
            .find(|(name, _)| *name == "first-end")
            .unwrap()
            .1;
        let second_start = events
            .iter()
            .find(|(name, _)| *name == "second-start")
            .unwrap()
            .1;
        assert!(second_start >= first_end);
    }

    #[test]
    fn self_rebuild_rejects_non_runnable_target() {
        let target = if cfg!(target_arch = "aarch64") {
            "x86_64-unknown-linux-musl"
        } else {
            "aarch64-unknown-linux-musl"
        };
        let err = ensure_self_rebuild_output_runnable(&composer::BuildOptions {
            target: Some(target.to_string()),
            ..composer::BuildOptions::default()
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("is not runnable on this host"),
            "{err:#}"
        );
    }

    #[test]
    fn self_rebuild_allows_host_target() {
        let target = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => "x86_64-unknown-linux-musl",
            ("linux", "aarch64") => "aarch64-unknown-linux-musl",
            ("macos", "x86_64") => "x86_64-apple-darwin",
            ("macos", "aarch64") => "aarch64-apple-darwin",
            _ => return,
        };

        ensure_self_rebuild_output_runnable(&composer::BuildOptions {
            target: Some(target.to_string()),
            ..composer::BuildOptions::default()
        })
        .unwrap();
    }

    #[test]
    fn incomplete_request_write_is_not_sent() {
        let mut writer = PartialWriter { remaining: 3 };

        assert!(!write_request(&mut writer, b"POST /self-update/rebuild"));
    }

    #[test]
    fn complete_request_write_finishes_before_response_read() {
        let mut writer = Vec::new();

        assert!(write_request(&mut writer, b"POST /self-update/rebuild"));
        assert_eq!(writer, b"POST /self-update/rebuild");
    }

    #[test]
    fn relay_maps_busy_response_after_complete_request() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        assert_eq!(
            post_request(&addr.to_string(), "/self-update/rebuild"),
            RelayOutcome::Busy
        );
        server.join().unwrap();
    }

    #[test]
    fn relay_maps_non_success_response_to_rejected() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        assert_eq!(
            post_request(&addr.to_string(), "/self-update/rebuild"),
            RelayOutcome::Rejected(503)
        );
        server.join().unwrap();
    }

    #[test]
    fn relay_treats_eof_after_complete_request_as_delivery_unknown() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            use std::io::Read;
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).unwrap() > 0);
        });

        assert_eq!(
            post_request(&addr.to_string(), "/self-update/rebuild"),
            RelayOutcome::DeliveryUnknown
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rebuild_route_returns_conflict_when_coordinator_is_busy() {
        let temp = tempfile::tempdir().unwrap();
        let coordinator = RebuildCoordinator::new(temp.path().to_path_buf(), vec![]);
        coordinator.busy.store(true, Ordering::Release);

        let response = http_rebuild(State(coordinator)).await.into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn rebuild_route_accepts_before_background_rebuild_runs() {
        let temp = tempfile::tempdir().unwrap();
        let coordinator = RebuildCoordinator::new(temp.path().to_path_buf(), vec![]);

        let response = http_rebuild(State(coordinator)).await.into_response();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[test]
    fn live_name_resolution_requires_exactly_one_workflow() {
        let one = presence("agent", "/agent", "/agent/WORKFLOW.md", 1);
        assert_eq!(select_live_entry("agent", vec![one.clone()]).unwrap(), one);

        let none = select_live_entry("agent", vec![]).unwrap_err();
        assert!(none.to_string().contains("no live dashboard presence"));

        let many = select_live_entry(
            "agent",
            vec![
                presence("agent", "/agent", "/agent/WORKFLOW.md", 1),
                presence("agent", "/agent", "/agent/workflows/other/WORKFLOW.md", 1),
            ],
        )
        .unwrap_err();
        assert!(many.to_string().contains("pass --workflow"));
    }

    #[test]
    fn restart_confirmation_uses_boot_identity_not_pid() {
        let before = presence("agent", "/agent", "/agent/WORKFLOW.md", 10);
        let mut restarted = before.clone();
        restarted.started_at = 11;
        assert_eq!(before.pid, restarted.pid);
        assert!(restart_identity_changed(&before, &restarted));
    }

    #[test]
    fn workflow_selector_targets_one_live_presence() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("agent");
        let selected = agent.join("workflows/selected");
        let other = agent.join("workflows/other");
        fs::create_dir_all(&selected).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(selected.join("WORKFLOW.md"), "").unwrap();
        fs::write(other.join("WORKFLOW.md"), "").unwrap();
        let registry = dar_presence::Registry::new(temp.path().join("registry"));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let selected_entry = presence(
            "agent",
            &agent.display().to_string(),
            &selected
                .join("WORKFLOW.md")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
            1,
        );
        let other_entry = presence(
            "agent",
            &agent.display().to_string(),
            &other
                .join("WORKFLOW.md")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
            1,
        );
        let mut selected_entry = selected_entry;
        selected_entry.addr = addr.clone();
        let mut other_entry = other_entry;
        other_entry.addr = "127.0.0.1:9".into();
        registry.write(&selected_entry).unwrap();
        registry.write(&other_entry).unwrap();
        let updated = dar_presence::PresenceEntry {
            started_at: 2,
            ..selected_entry.clone()
        };
        let registry_for_server = registry.clone();
        let server = thread::spawn(move || {
            serve_rebuild_then_health(listener, registry_for_server, updated, true)
        });

        trigger_live_by_name("agent", Some(&selected), Some(registry.dir())).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn bridge_uses_actual_ephemeral_host_address() {
        let agent = tempfile::tempdir().unwrap();
        let workflow = agent.path().join("WORKFLOW.md");
        std::fs::write(&workflow, "---\n---\n").unwrap();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 53124));

        assert_eq!(
            bridge_addr(agent.path(), &workflow, Some(addr)).unwrap(),
            "127.0.0.1:53124"
        );
    }

    #[test]
    fn delivery_unknown_still_verifies_changed_boot_and_health() {
        let temp = tempfile::tempdir().unwrap();
        let registry = dar_presence::Registry::new(temp.path().join("registry"));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let mut entry = presence("agent", "/agent", "/agent/WORKFLOW.md", 1);
        entry.addr = addr;
        registry.write(&entry).unwrap();
        let updated = dar_presence::PresenceEntry {
            started_at: 2,
            ..entry.clone()
        };
        let registry_for_server = registry.clone();
        let server = thread::spawn(move || {
            serve_rebuild_then_health(listener, registry_for_server, updated, false)
        });

        trigger_live_entry(&registry, &entry).unwrap();
        server.join().unwrap();
    }

    struct PartialWriter {
        remaining: usize,
    }

    impl std::io::Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                ));
            }
            let count = self.remaining.min(bytes.len());
            self.remaining -= count;
            Ok(count)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct FakeDoctor {
        ok: bool,
    }

    impl DoctorRunner for FakeDoctor {
        fn run(&self, _new_binary: &Path, _agent: &Path) -> Result<()> {
            if self.ok {
                Ok(())
            } else {
                anyhow::bail!("doctor gate failed")
            }
        }
    }

    fn write_executable(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn presence(
        id: &str,
        folder: &str,
        workflow: &str,
        started_at: i64,
    ) -> dar_presence::PresenceEntry {
        dar_presence::PresenceEntry {
            id: id.into(),
            folder: folder.into(),
            workflow: workflow.into(),
            addr: "127.0.0.1:1".into(),
            pid: std::process::id(),
            started_at,
        }
    }

    fn serve_rebuild_then_health(
        listener: std::net::TcpListener,
        registry: dar_presence::Registry,
        updated: dar_presence::PresenceEntry,
        reply_to_post: bool,
    ) {
        use std::io::{Read, Write};
        let (mut post, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        assert!(post.read(&mut request).unwrap() > 0);
        registry.write(&updated).unwrap();
        if reply_to_post {
            post.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        }
        drop(post);
        let (mut health, _) = listener.accept().unwrap();
        assert!(health.read(&mut request).unwrap() > 0);
        health
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
    }
}
