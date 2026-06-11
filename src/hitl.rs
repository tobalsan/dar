//! Pluggable human-in-the-loop notifications with burst buffering.

use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

use crate::config::HitlNotifierConfig;
use crate::dotenv;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
pub struct HitlNotification {
    pub kind: String,
    pub issue_identifier: String,
    pub message: String,
}

impl HitlNotification {
    pub fn new(
        kind: impl Into<String>,
        issue_identifier: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            issue_identifier: issue_identifier.into(),
            message: message.into(),
        }
    }
}

pub trait HitlNotify: Send + Sync {
    fn notify(&self, item: HitlNotification);
    fn stop(&self);
}

pub struct NoopHitlNotifier;

impl HitlNotify for NoopHitlNotifier {
    fn notify(&self, _item: HitlNotification) {}
    fn stop(&self) {}
}

pub struct BurstHitlNotifier {
    inner: Arc<Inner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    state: Mutex<State>,
    wake: Condvar,
    sink: Sink,
    window: Duration,
    max_items: usize,
}

struct State {
    pending: Vec<HitlNotification>,
    seen: HashSet<HitlNotification>,
    window_started: Option<Instant>,
    stopping: bool,
}

enum Sink {
    Stdout,
    Webhook {
        url: String,
        client: reqwest::blocking::Client,
    },
    Cli {
        command: Vec<String>,
    },
}

#[derive(Serialize)]
struct Batch<'a> {
    items: &'a [HitlNotification],
}

impl BurstHitlNotifier {
    pub fn from_config(cfg: &HitlNotifierConfig) -> Result<Arc<dyn HitlNotify>> {
        if cfg.use_ == "none" {
            return Ok(Arc::new(NoopHitlNotifier));
        }
        let sink = match cfg.use_.as_str() {
            "stdout" => Sink::Stdout,
            "webhook" => Sink::Webhook {
                url: cfg
                    .webhook_url
                    .clone()
                    .ok_or_else(|| anyhow!("hitl.notifier.webhook_url is required"))?,
                client: reqwest::blocking::Client::new(),
            },
            "cli" => Sink::Cli {
                command: cfg.command.clone(),
            },
            other => return Err(anyhow!("unsupported HITL notifier target {other:?}")),
        };
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                pending: Vec::new(),
                seen: HashSet::new(),
                window_started: None,
                stopping: false,
            }),
            wake: Condvar::new(),
            sink,
            window: Duration::from_secs(cfg.window_secs),
            max_items: cfg.max_items,
        });
        let worker_inner = Arc::clone(&inner);
        let worker = thread::Builder::new()
            .name("hitl-notifier".to_string())
            .spawn(move || worker_loop(worker_inner))
            .context("spawning HITL notifier worker")?;
        Ok(Arc::new(Self {
            inner,
            worker: Mutex::new(Some(worker)),
        }))
    }
}

impl HitlNotify for BurstHitlNotifier {
    fn notify(&self, item: HitlNotification) {
        let mut state = self.inner.state.lock().unwrap();
        if state.stopping || state.seen.contains(&item) {
            return;
        }
        if state.pending.is_empty() {
            state.window_started = Some(Instant::now());
        }
        state.seen.insert(item.clone());
        state.pending.push(item);
        self.inner.wake.notify_one();
    }

    fn stop(&self) {
        {
            let mut state = self.inner.state.lock().unwrap();
            state.stopping = true;
            self.inner.wake.notify_one();
        }
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
    }
}

impl Drop for BurstHitlNotifier {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker_loop(inner: Arc<Inner>) {
    loop {
        let batch = {
            let mut state = inner.state.lock().unwrap();
            loop {
                if state.stopping || state.pending.len() >= inner.max_items {
                    break take_batch(&mut state);
                }
                if state.pending.is_empty() {
                    state = inner.wake.wait(state).unwrap();
                    continue;
                }
                let elapsed = state
                    .window_started
                    .map(|t| t.elapsed())
                    .unwrap_or_default();
                if elapsed >= inner.window {
                    break take_batch(&mut state);
                }
                let wait_for = inner.window - elapsed;
                let (next, timeout) = inner.wake.wait_timeout(state, wait_for).unwrap();
                state = next;
                if timeout.timed_out() {
                    break take_batch(&mut state);
                }
            }
        };
        if !batch.is_empty() {
            if let Err(e) = inner.sink.send(&batch) {
                tracing::warn!("HITL notification flush failed: {e:#}");
            }
        }
        if inner.state.lock().unwrap().stopping {
            return;
        }
    }
}

fn take_batch(state: &mut State) -> Vec<HitlNotification> {
    state.seen.clear();
    state.window_started = None;
    std::mem::take(&mut state.pending)
}

impl Sink {
    fn send(&self, items: &[HitlNotification]) -> Result<()> {
        match self {
            Sink::Stdout => {
                println!("{}", serde_json::to_string(&Batch { items })?);
                Ok(())
            }
            Sink::Webhook { url, client } => {
                let response = client.post(url).json(&Batch { items }).send()?;
                if !response.status().is_success() {
                    return Err(anyhow!("webhook returned {}", response.status()));
                }
                Ok(())
            }
            Sink::Cli { command } => {
                let (program, args) = command
                    .split_first()
                    .ok_or_else(|| anyhow!("HITL CLI command is empty"))?;
                let mut command = Command::new(program);
                dotenv::scrub_loaded_env(&mut command);
                let mut child = command
                    .args(args)
                    .stdin(Stdio::piped())
                    .spawn()
                    .with_context(|| format!("spawning HITL CLI command {program:?}"))?;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(serde_json::to_string(&Batch { items })?.as_bytes())?;
                    stdin.write_all(b"\n")?;
                }
                let status = child.wait()?;
                if !status.success() {
                    return Err(anyhow!("HITL CLI command exited with {status}"));
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use tempfile::tempdir;

    #[test]
    fn take_batch_resets_window_dedupe_set() {
        let mut state = State {
            pending: Vec::new(),
            seen: HashSet::new(),
            window_started: Some(Instant::now()),
            stopping: false,
        };
        let item = HitlNotification::new("stall", "ALG-1", "same");
        state.seen.insert(item.clone());
        state.pending.push(item.clone());

        assert_eq!(take_batch(&mut state), vec![item]);
        assert!(state.seen.is_empty());
        assert!(state.window_started.is_none());
    }

    #[test]
    fn flushes_immediately_at_max_items_and_dedupes() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("hitl.jsonl");
        let cfg = cli_config(&out, 60, 5);
        let notifier = BurstHitlNotifier::from_config(&cfg).unwrap();

        let duplicate = HitlNotification::new("stall", "ALG-1", "same");
        notifier.notify(duplicate.clone());
        notifier.notify(duplicate);
        for idx in 2..=5 {
            notifier.notify(HitlNotification::new(
                "park",
                format!("ALG-{idx}"),
                "different",
            ));
        }

        let body = wait_for_file(&out);
        notifier.stop();
        assert_eq!(body.lines().count(), 1);
        assert!(body.contains("\"ALG-1\""));
        assert!(body.contains("\"ALG-5\""));
        assert!(!body.contains("\"ALG-6\""));
    }

    #[test]
    fn stop_flushes_pending_before_window_expires() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("hitl.jsonl");
        let cfg = cli_config(&out, 60, 5);
        let notifier = BurstHitlNotifier::from_config(&cfg).unwrap();

        notifier.notify(HitlNotification::new("startup-error", "-", "boom"));
        notifier.stop();

        let body = fs::read_to_string(out).unwrap();
        assert!(body.contains("\"startup-error\""));
        assert!(body.contains("\"boom\""));
    }

    #[test]
    fn flushes_after_window_without_reaching_max_items() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("hitl.jsonl");
        let cfg = cli_config(&out, 1, 5);
        let notifier = BurstHitlNotifier::from_config(&cfg).unwrap();

        notifier.notify(HitlNotification::new("park", "ALG-1", "barrier"));

        let body = wait_for_file(&out);
        notifier.stop();
        assert!(body.contains("\"barrier\""));
    }

    fn cli_config(out: &Path, window_secs: u64, max_items: usize) -> HitlNotifierConfig {
        HitlNotifierConfig {
            use_: "cli".to_string(),
            window_secs,
            max_items,
            webhook_url: None,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "cat >> \"$1\"".to_string(),
                "hitl-test".to_string(),
                out.display().to_string(),
            ],
        }
    }

    fn wait_for_file(path: &Path) -> String {
        for _ in 0..30 {
            if let Ok(body) = fs::read_to_string(path) {
                if !body.is_empty() {
                    return body;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {}", path.display());
    }
}
