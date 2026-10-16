//! Scheduler-owned, crash-durable run records and finished transcript events.
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const CAPTURE_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub job_id: String,
    pub runner: Option<String>,
    pub model: Option<String>,
    pub phase: String,
    pub pid: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub outcome: String,
    pub exit_code: Option<i32>,
    pub output_file: Option<String>,
    pub woke: bool,
    pub error: Option<String>,
    pid_started: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunEvent {
    pub id: u64,
    pub ts: DateTime<Utc>,
    pub phase: String,
    pub kind: String,
    pub text: String,
    pub detail: String,
}

#[derive(Clone)]
pub struct History {
    conn: Arc<Mutex<Connection>>,
    retention: usize,
}

#[derive(Clone)]
pub struct RunRecorder {
    history: History,
    id: String,
    storage_error: Arc<Mutex<Option<String>>>,
}

impl History {
    pub fn open(root: &Path, retention: usize) -> Result<Self> {
        let dir = root.join("cron");
        std::fs::create_dir_all(&dir).context("creating scheduler history directory")?;
        let conn = Connection::open(dir.join("history.db"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS runs (
               id TEXT PRIMARY KEY, job_id TEXT NOT NULL, data TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS runs_job ON runs(job_id);
             CREATE TABLE IF NOT EXISTS events (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
               data TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS events_run ON events(run_id, id);",
        )?;
        let history = Self {
            conn: Arc::new(Mutex::new(conn)),
            retention: retention.max(1),
        };
        Ok(history)
    }

    /// Called only by the daemon at scheduler startup, before any new fires.
    /// Opening history for tools or dashboard reads must never recover runs.
    pub fn recover(&self) -> Result<()> {
        let records = self.all()?;
        let mut stale_pids = Vec::new();
        for mut record in records {
            if record.outcome == "running" {
                if record.phase != "agent" || record.runner.as_deref() != Some("builtin") {
                    if let (Some(pid), Some(started)) = (record.pid, &record.pid_started) {
                        if pid != std::process::id()
                            && process_started(pid).as_ref() == Some(started)
                        {
                            runner_core::term_then_kill(pid, std::time::Duration::from_millis(100));
                            stale_pids.push(pid);
                        }
                    }
                }
                record.outcome = "crashed".into();
                record.finished_at = Some(Utc::now());
                record.exit_code = None;
                record.error = Some("dar stopped before this run finished".into());
                self.save(&record)?;
            }
        }
        runner_core::wait_for_pids_dead(&stale_pids, std::time::Duration::from_secs(2));
        self.prune()
    }

    pub fn start(
        &self,
        job_id: &str,
        runner: Option<&str>,
        model: Option<&str>,
        phase: &str,
    ) -> Result<RunRecorder> {
        let now = Utc::now();
        let record = RunRecord {
            id: format!(
                "{}-{}",
                now.timestamp_nanos_opt().unwrap_or_default(),
                std::process::id()
            ),
            job_id: job_id.into(),
            runner: runner.map(str::to_owned),
            model: model.map(str::to_owned),
            phase: phase.into(),
            pid: None,
            started_at: now,
            finished_at: None,
            outcome: "running".into(),
            exit_code: None,
            output_file: None,
            woke: false,
            error: None,
            pid_started: None,
        };
        self.conn.lock().expect("history mutex poisoned").execute(
            "INSERT INTO runs(id,job_id,data) VALUES (?1,?2,?3)",
            params![record.id, job_id, serde_json::to_string(&record)?],
        )?;
        Ok(RunRecorder {
            history: self.clone(),
            id: record.id,
            storage_error: Arc::new(Mutex::new(None)),
        })
    }

    fn all(&self) -> Result<Vec<RunRecord>> {
        let conn = self.conn.lock().expect("history mutex poisoned");
        let mut stmt = conn.prepare("SELECT data FROM runs ORDER BY id DESC")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn list(&self, job_id: &str) -> Result<Vec<RunRecord>> {
        let conn = self.conn.lock().expect("history mutex poisoned");
        let mut stmt = conn.prepare("SELECT data FROM runs WHERE job_id=?1 ORDER BY id DESC")?;
        let rows = stmt.query_map([job_id], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn get(&self, run_id: &str) -> Result<Option<RunRecord>> {
        let conn = self.conn.lock().expect("history mutex poisoned");
        load(&conn, run_id)
    }

    pub fn events(&self, run_id: &str, since: u64) -> Result<Vec<RunEvent>> {
        let conn = self.conn.lock().expect("history mutex poisoned");
        let mut stmt =
            conn.prepare("SELECT id,data FROM events WHERE run_id=?1 AND id>?2 ORDER BY id")?;
        let rows = stmt.query_map(params![run_id, since], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, data) = row?;
            let mut event: RunEvent = serde_json::from_str(&data)?;
            event.id = id;
            Ok(event)
        })
        .collect()
    }

    fn save(&self, record: &RunRecord) -> Result<()> {
        self.conn.lock().expect("history mutex poisoned").execute(
            "UPDATE runs SET data=?2 WHERE id=?1",
            params![record.id, serde_json::to_string(record)?],
        )?;
        Ok(())
    }

    fn prune(&self) -> Result<()> {
        let mut conn = self.conn.lock().expect("history mutex poisoned");
        let tx = conn.transaction()?;
        prune(&tx, self.retention)?;
        tx.commit()?;
        Ok(())
    }
}

fn load(conn: &Connection, id: &str) -> Result<Option<RunRecord>> {
    conn.query_row("SELECT data FROM runs WHERE id=?1", [id], |row| {
        row.get::<_, String>(0)
    })
    .optional()?
    .map(|data| Ok(serde_json::from_str(&data)?))
    .transpose()
}

fn prune(conn: &Connection, retention: usize) -> Result<()> {
    conn.execute(
        "DELETE FROM runs WHERE id IN (
           SELECT id FROM (
             SELECT id, ROW_NUMBER() OVER (PARTITION BY job_id ORDER BY id DESC) AS position
             FROM runs WHERE json_extract(data,'$.outcome') != 'running'
           ) WHERE position > ?1
         )",
        [retention as i64],
    )?;
    Ok(())
}

fn process_started(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let started = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!started.is_empty()).then_some(started)
}

impl RunRecorder {
    pub fn id(&self) -> &str {
        &self.id
    }

    fn update(&self, change: impl FnOnce(&mut RunRecord)) -> Result<()> {
        let mut conn = self.history.conn.lock().expect("history mutex poisoned");
        let tx = conn.transaction()?;
        if let Some(mut record) = load(&tx, &self.id)? {
            change(&mut record);
            tx.execute(
                "UPDATE runs SET data=?2 WHERE id=?1",
                params![self.id, serde_json::to_string(&record)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn phase(&self, phase: &str) -> Result<()> {
        self.update(|record| {
            record.phase = phase.into();
            record.pid = None;
            record.pid_started = None;
            record.exit_code = None;
        })?;
        self.event("phase", phase, "")
    }

    pub fn pid(&self, pid: u32) -> Result<()> {
        let started = process_started(pid);
        self.update(|record| {
            record.pid = Some(pid);
            record.pid_started = started;
        })
    }

    pub fn exit_code(&self, exit_code: Option<i32>) -> Result<()> {
        self.update(|record| record.exit_code = exit_code)
    }

    pub fn event(&self, kind: &str, text: &str, detail: &str) -> Result<()> {
        let conn = self.history.conn.lock().expect("history mutex poisoned");
        if let Some(record) = load(&conn, &self.id)? {
            let event = RunEvent {
                id: 0,
                ts: Utc::now(),
                phase: record.phase,
                kind: kind.into(),
                text: text.into(),
                detail: detail.into(),
            };
            conn.execute(
                "INSERT INTO events(run_id,data) VALUES (?1,?2)",
                params![self.id, serde_json::to_string(&event)?],
            )?;
        }
        Ok(())
    }

    /// Capture at most 64 KiB per stdout/stderr stream across all phases. The
    /// marker is itself durable, and further chunks are drained but discarded.
    pub fn capture(&self, stream: &str, bytes: &[u8]) -> Result<()> {
        let mut conn = self.history.conn.lock().expect("history mutex poisoned");
        let tx = conn.transaction()?;
        let Some(record) = load(&tx, &self.id)? else {
            return Ok(());
        };
        let (used, truncated): (usize, bool) = tx.query_row(
            "SELECT COALESCE(SUM(length(CAST(json_extract(data,'$.text') AS BLOB))),0),
               COALESCE(MAX(json_extract(data,'$.detail')='truncated'),0)
             FROM events WHERE run_id=?1 AND json_extract(data,'$.kind')=?2",
            params![self.id, stream],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if !truncated {
            let remaining = CAPTURE_LIMIT.saturating_sub(used);
            let mut text =
                String::from_utf8_lossy(&bytes[..bytes.len().min(remaining)]).into_owned();
            // UTF-8 replacement can grow an incomplete byte sequence.
            while text.len() > remaining {
                text.pop();
            }
            let cut = bytes.len() > remaining;
            let event = RunEvent {
                id: 0,
                ts: Utc::now(),
                phase: record.phase,
                kind: stream.into(),
                text,
                detail: if cut {
                    "truncated".into()
                } else {
                    String::new()
                },
            };
            if !event.text.is_empty() || cut {
                tx.execute(
                    "INSERT INTO events(run_id,data) VALUES (?1,?2)",
                    params![self.id, serde_json::to_string(&event)?],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Sink traits cannot return errors; callers check this before succeeding.
    pub fn remember_error(&self, error: &anyhow::Error) {
        tracing::error!(
            "[scheduler] run {} history write failed: {error:#}",
            self.id
        );
        *self
            .storage_error
            .lock()
            .expect("history error mutex poisoned") = Some(format!("{error:#}"));
    }

    pub fn storage_error(&self) -> Option<String> {
        self.storage_error
            .lock()
            .expect("history error mutex poisoned")
            .clone()
    }

    pub fn finalize(
        &self,
        outcome: &str,
        output_file: Option<&str>,
        woke: bool,
        error: Option<&str>,
        quiet_noop: bool,
    ) -> Result<()> {
        let mut conn = self.history.conn.lock().expect("history mutex poisoned");
        let tx = conn.transaction()?;
        if quiet_noop && outcome == "succeeded" && self.storage_error().is_none() {
            tx.execute("DELETE FROM runs WHERE id=?1", [&self.id])?;
        } else if let Some(mut record) = load(&tx, &self.id)? {
            record.outcome = if self.storage_error().is_some() {
                "failed"
            } else {
                outcome
            }
            .into();
            record.finished_at = Some(Utc::now());
            record.output_file = output_file.map(str::to_owned);
            record.woke = woke;
            record.error = self.storage_error().or_else(|| error.map(str::to_owned));
            tx.execute(
                "UPDATE runs SET data=?2 WHERE id=?1",
                params![self.id, serde_json::to_string(&record)?],
            )?;
            prune(&tx, self.history.retention)?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_events_cursor_and_crash_recovery() {
        let root = tempfile::tempdir().unwrap();
        let history = History::open(root.path(), 3).unwrap();
        let run = history
            .start("job", Some("pi"), Some("model"), "agent")
            .unwrap();
        run.event("tool_output", "STEP-A", "").unwrap();
        let id = run.id().to_string();
        let first = history.events(&id, 0).unwrap();
        assert_eq!(first[0].text, "STEP-A");
        let reader = Connection::open(root.path().join("cron/history.db")).unwrap();
        let persisted: String = reader
            .query_row("SELECT data FROM events WHERE run_id=?1", [&id], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(persisted.contains("STEP-A"));
        drop(reader);
        assert!(history.events(&id, first[0].id).unwrap().is_empty());
        drop(run);
        drop(history);
        let history = History::open(root.path(), 3).unwrap();
        history.recover().unwrap();
        let record = history.get(&id).unwrap().unwrap();
        assert_eq!(record.outcome, "crashed");
        assert_eq!(record.exit_code, None);
        assert_eq!(history.events(&id, 0).unwrap()[0].text, "STEP-A");
    }

    #[test]
    fn quiet_success_deletes_atomically_but_failure_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let history = History::open(root.path(), 3).unwrap();
        let quiet = history.start("job", None, None, "gate").unwrap();
        quiet.capture("stdout", b"{\"wakeAgent\":false}").unwrap();
        quiet
            .finalize("succeeded", None, false, None, true)
            .unwrap();
        assert!(history.get(quiet.id()).unwrap().is_none());
        assert!(history.events(quiet.id(), 0).unwrap().is_empty());
        let failed = history.start("job", None, None, "gate").unwrap();
        failed.capture("stderr", b"BOOM").unwrap();
        failed
            .finalize("failed", None, false, Some("failed"), true)
            .unwrap();
        assert_eq!(history.list("job").unwrap().len(), 1);
        assert_eq!(history.events(failed.id(), 0).unwrap()[0].text, "BOOM");
        drop(history);
        let history = History::open(root.path(), 3).unwrap();
        assert_eq!(history.list("job").unwrap().len(), 1);
    }

    #[test]
    fn capture_is_capped_and_truncation_is_durable() {
        let root = tempfile::tempdir().unwrap();
        let history = History::open(root.path(), 3).unwrap();
        let run = history.start("job", None, None, "script").unwrap();
        run.capture("stdout", &vec![b'x'; CAPTURE_LIMIT + 10])
            .unwrap();
        run.capture("stdout", b"discarded").unwrap();
        run.capture("stderr", b"BOOM").unwrap();
        let events = history.events(run.id(), 0).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].text.len(), CAPTURE_LIMIT);
        assert_eq!(events[0].detail, "truncated");
        assert_eq!(events[1].text, "BOOM");
    }

    #[test]
    fn retention_removes_old_events_and_survives_restart() {
        let root = tempfile::tempdir().unwrap();
        let history = History::open(root.path(), 3).unwrap();
        let mut ids = Vec::new();
        for _ in 0..5 {
            let run = history.start("job", Some("fake"), None, "agent").unwrap();
            ids.push(run.id().to_string());
            run.event("stdout", "hi", "").unwrap();
            run.finalize("succeeded", None, false, None, false).unwrap();
        }
        assert_eq!(history.list("job").unwrap().len(), 3);
        assert!(history.events(&ids[0], 0).unwrap().is_empty());
        drop(history);
        let history = History::open(root.path(), 3).unwrap();
        assert_eq!(history.list("job").unwrap().len(), 3);
        assert_eq!(history.list("job").unwrap()[0].id, ids[4]);
    }

    #[test]
    fn restart_kills_recorded_script_group_even_for_builtin_agent() {
        use std::os::unix::process::CommandExt;
        use std::os::unix::process::ExitStatusExt;

        let root = tempfile::tempdir().unwrap();
        let history = History::open(root.path(), 3).unwrap();
        let run = history.start("job", Some("builtin"), None, "gate").unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        run.pid(child.id()).unwrap();
        let id = run.id().to_string();
        drop(run);
        drop(history);
        let history = History::open(root.path(), 3).unwrap();
        history.recover().unwrap();
        let status = child.wait().unwrap();
        assert!(matches!(status.signal(), Some(15) | Some(9)));
        assert_eq!(history.get(&id).unwrap().unwrap().outcome, "crashed");
    }

    #[test]
    fn tool_history_open_does_not_recover_or_kill_live_runner() {
        use std::os::unix::process::CommandExt;

        let root = tempfile::tempdir().unwrap();
        let history = History::open(root.path(), 3).unwrap();
        let run = history.start("job", Some("pi"), None, "agent").unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        run.pid(child.id()).unwrap();
        run.event("tool_output", "STEP-A", "").unwrap();
        let tool_history = History::open(root.path(), 3).unwrap();
        assert_eq!(
            tool_history.get(run.id()).unwrap().unwrap().outcome,
            "running"
        );
        assert_eq!(tool_history.events(run.id(), 0).unwrap()[0].text, "STEP-A");
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
