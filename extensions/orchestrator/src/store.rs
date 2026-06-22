//! SQLite-backed persistence for runs, events, claims, and heartbeats.
//!
//! Replaces `logs/history.jsonl` and the in-process file logging for
//! structured lifecycle data. Uses WAL mode so the dashboard can read while
//! the orchestrator writes, with a single `Mutex<Connection>` to keep the
//! in-process footprint minimal (no connection pool needed; writes are rare).
//!
//! Memory is bounded: all historical queries are paged. The `HistoryRing` holds
//! the last N runs in-memory for the dashboard; SQLite holds everything on disk.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::state::{HistoryEntry, RunStatus};

// ─── shared constants ─────────────────────────────────────────────────────────

/// Note string passed to `record_history` when a run finishes with exit 0 but
/// the issue is still active (continuation queued).  `record_history` prefixes
/// this with the `RunStatus` debug name, so the stored lifecycle-event payload
/// becomes `"Succeeded still active after normal exit; continuation queued"`.
pub const ACTIVE_CONTINUATION_MARKER: &str = "still active after normal exit; continuation queued";

/// Full lifecycle-event `payload` value stored by `record_history` for an
/// active-continuation run.  The park-barrier SQL query matches against this
/// exact string; it is derived from `ACTIVE_CONTINUATION_MARKER` so the two
/// stay in sync automatically.
pub const ACTIVE_CONTINUATION_EVENT: &str =
    "Succeeded still active after normal exit; continuation queued";

// ─── public row/param types ───────────────────────────────────────────────────

/// Parameters for inserting a new run row at dispatch time.
pub struct NewRun<'a> {
    pub run_id: &'a str,
    pub issue_id: &'a str,
    pub issue_identifier: &'a str,
    pub workspace: &'a str,
    /// Serialized profile JSON (optional; populated by richer runner impls).
    pub profile_json: Option<&'a str>,
    pub workflow_path: Option<&'a str>,
    pub workflow_sha: Option<&'a str>,
    pub pid: u32,
    pub worker_id: Option<&'a str>,
    pub started_at: DateTime<Utc>,
    /// Runner kind this run is dispatched with (e.g. `pi`, `codex`, `fake`).
    pub runner: Option<&'a str>,
    /// Model this run is dispatched with (None = unset in config).
    pub model: Option<&'a str>,
    /// Model provider this run is dispatched with (None = none/single-provider).
    pub provider: Option<&'a str>,
}

/// Fields written when a run finishes (outcome, timing, exit code).
pub struct RunFinish {
    pub outcome: RunStatus,
    /// `Some(code)` for normal/abnormal process exit; `None` for kills/cancels
    /// that don't produce a meaningful exit code.
    pub exit_code: Option<i32>,
    pub finished_at: DateTime<Utc>,
}

/// A run row returned by list queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunRow {
    pub run_id: String,
    pub issue_id: String,
    pub issue_identifier: String,
    pub workspace: String,
    pub profile_json: Option<String>,
    pub workflow_path: Option<String>,
    pub workflow_sha: Option<String>,
    pub pid: u32,
    pub worker_id: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub outcome: Option<String>,
    pub exit_code: Option<i32>,
    pub process_alive: bool,
    pub runner: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
}

/// Parameters for inserting one event (child stdout/stderr or lifecycle event).
pub struct NewEvent<'a> {
    pub run_id: Option<&'a str>,
    pub issue_identifier: &'a str,
    /// Category: `"stdout"`, `"stderr"`, or `"lifecycle"`.
    pub kind: &'a str,
    /// Raw line or JSON payload.
    pub payload: &'a str,
    pub ts: DateTime<Utc>,
}

/// An event row returned by list queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EventRow {
    pub event_id: i64,
    pub run_id: Option<String>,
    pub issue_identifier: String,
    pub kind: String,
    pub payload: String,
    pub ts: String,
}

/// Parameters for inserting a claim record.
pub struct NewClaim<'a> {
    pub run_id: &'a str,
    pub issue_id: &'a str,
    pub issue_identifier: &'a str,
    pub worker_id: &'a str,
    pub claimed_at: DateTime<Utc>,
}

/// Parameters for inserting a heartbeat record.
pub struct NewHeartbeat<'a> {
    pub run_id: &'a str,
    pub issue_identifier: &'a str,
    pub worker_id: &'a str,
    pub ts: DateTime<Utc>,
}

// ─── Store ────────────────────────────────────────────────────────────────────

/// SQLite persistence store. All writes go through a single `Mutex<Connection>`
/// so there is no pool overhead and no unbounded thread growth.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (or create) the SQLite database at `path` and initialize the schema.
    /// Creates parent directories as needed.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating data dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening SQLite store at {}", path.display()))?;
        // WAL: readers (dashboard) never block the writer (orchestrator), and
        // the writer never blocks readers. NORMAL sync is safe with WAL.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
        )
        .context("configuring SQLite pragmas")?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS runs (
                run_id          TEXT    PRIMARY KEY,
                issue_id        TEXT    NOT NULL,
                issue_identifier TEXT   NOT NULL,
                workspace       TEXT    NOT NULL,
                profile_json    TEXT,
                workflow_path   TEXT,
                workflow_sha    TEXT,
                pid             INTEGER NOT NULL DEFAULT 0,
                worker_id       TEXT,
                started_at      TEXT    NOT NULL,
                finished_at     TEXT,
                outcome         TEXT,
                exit_code       INTEGER,
                process_alive   INTEGER NOT NULL DEFAULT 1,
                runner          TEXT,
                model           TEXT,
                provider        TEXT
            );
            CREATE INDEX IF NOT EXISTS runs_identifier ON runs(issue_identifier);
            CREATE INDEX IF NOT EXISTS runs_started_at ON runs(started_at);

            CREATE TABLE IF NOT EXISTS events (
                event_id        INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id          TEXT,
                issue_identifier TEXT   NOT NULL,
                kind            TEXT    NOT NULL,
                payload         TEXT    NOT NULL,
                ts              TEXT    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS events_issue_id ON events(issue_identifier, event_id);
            CREATE INDEX IF NOT EXISTS events_run_id ON events(run_id, event_id);

            CREATE TABLE IF NOT EXISTS claims (
                claim_id        INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id          TEXT    NOT NULL,
                issue_id        TEXT    NOT NULL DEFAULT '',
                issue_identifier TEXT   NOT NULL,
                worker_id       TEXT    NOT NULL,
                claimed_at      TEXT    NOT NULL,
                released_at     TEXT
            );
            CREATE INDEX IF NOT EXISTS claims_run_id ON claims(run_id);
            CREATE TABLE IF NOT EXISTS heartbeats (
                heartbeat_id    INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id          TEXT    NOT NULL,
                issue_identifier TEXT   NOT NULL,
                worker_id       TEXT    NOT NULL,
                ts              TEXT    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS heartbeats_run_id ON heartbeats(run_id);
            ",
        )
        .context("initializing SQLite schema")?;
        let has_issue_id = conn
            .prepare("PRAGMA table_info(claims)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .any(|name| name == "issue_id");
        if !has_issue_id {
            conn.execute(
                "ALTER TABLE claims ADD COLUMN issue_id TEXT NOT NULL DEFAULT ''",
                [],
            )
            .context("adding claims.issue_id")?;
        }
        // Add per-run runner/model/provider columns on pre-existing DBs so
        // historical rows simply carry NULLs (rendered as defaults).
        let run_cols = conn
            .prepare("PRAGMA table_info(runs)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (col, sql) in [
            ("runner", "ALTER TABLE runs ADD COLUMN runner TEXT"),
            ("model", "ALTER TABLE runs ADD COLUMN model TEXT"),
            ("provider", "ALTER TABLE runs ADD COLUMN provider TEXT"),
        ] {
            if !run_cols.iter().any(|name| name == col) {
                conn.execute(sql, [])
                    .with_context(|| format!("adding runs.{col}"))?;
            }
        }
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS claims_active_issue_id
             ON claims(issue_id) WHERE released_at IS NULL AND issue_id <> ''",
            [],
        )
        .context("creating active claim unique index")?;
        Ok(())
    }

    // ── Runs ──────────────────────────────────────────────────────────────────

    /// Insert a new run row at dispatch time. `process_alive` is set to 1.
    pub fn insert_run(&self, r: &NewRun<'_>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO runs
             (run_id, issue_id, issue_identifier, workspace,
              profile_json, workflow_path, workflow_sha,
              pid, worker_id, started_at, process_alive,
              runner, model, provider)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1,?11,?12,?13)",
            params![
                r.run_id,
                r.issue_id,
                r.issue_identifier,
                r.workspace,
                r.profile_json,
                r.workflow_path,
                r.workflow_sha,
                r.pid as i64,
                r.worker_id,
                r.started_at.to_rfc3339(),
                r.runner,
                r.model,
                r.provider,
            ],
        )
        .context("insert_run")?;
        Ok(())
    }

    /// Write outcome, finished_at, exit_code, and set process_alive=0.
    pub fn finish_run(&self, run_id: &str, f: &RunFinish) -> Result<()> {
        let outcome = run_status_to_str(f.outcome);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE runs SET outcome=?1, finished_at=?2, exit_code=?3, process_alive=0
             WHERE run_id=?4",
            params![outcome, f.finished_at.to_rfc3339(), f.exit_code, run_id,],
        )
        .context("finish_run")?;
        Ok(())
    }

    /// On startup, mark any runs still flagged `process_alive=1` as
    /// `interrupted_gateway_restart`, set `finished_at` to now, and clear
    /// `process_alive`. Also prunes heartbeats for finished runs.
    /// Returns the number of rows updated.
    pub fn mark_crashed_runs(&self) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "UPDATE runs
                 SET outcome='interrupted_gateway_restart', process_alive=0, finished_at=?1
                 WHERE process_alive=1 AND finished_at IS NULL",
                params![now],
            )
            .context("mark_crashed_runs")?;
        conn.execute(
            "DELETE FROM heartbeats
             WHERE run_id IN (SELECT run_id FROM runs WHERE finished_at IS NOT NULL)",
            [],
        )
        .context("prune_finished_run_heartbeats in mark_crashed_runs")?;
        Ok(n)
    }

    /// Return PIDs for persisted runs that were still open when the previous
    /// gateway process exited. Startup uses this to terminate stale worker
    /// process groups before marking those rows interrupted.
    pub fn open_run_pids(&self) -> Result<Vec<u32>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT pid FROM runs
                 WHERE process_alive=1 AND finished_at IS NULL AND pid > 0",
            )
            .context("prepare open_run_pids")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .context("query open_run_pids")?;
        let mut out = Vec::new();
        for row in rows {
            let pid = row?;
            if pid > 0 && pid <= u32::MAX as i64 {
                out.push(pid as u32);
            }
        }
        Ok(out)
    }

    /// Load the `limit` most recent finished runs, newest-first. Used to seed
    /// the in-memory `HistoryRing` on startup (replaces history.jsonl reload).
    pub fn load_recent_runs(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT issue_identifier, outcome, pid, finished_at
             FROM runs
             WHERE finished_at IS NOT NULL
             ORDER BY finished_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let identifier: String = row.get(0)?;
            let outcome_str: Option<String> = row.get(1)?;
            let pid: i64 = row.get(2).unwrap_or(0);
            let finished_at_str: String = row.get(3)?;
            Ok((identifier, outcome_str, pid, finished_at_str))
        })?;

        let mut entries = Vec::new();
        for row in rows {
            let (identifier, outcome_str, pid, finished_at_str) = row?;
            let status = outcome_str
                .as_deref()
                .and_then(str_to_run_status)
                .unwrap_or(RunStatus::Failed);
            let ended_at = DateTime::parse_from_rfc3339(&finished_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            entries.push(HistoryEntry {
                identifier,
                status,
                pid: pid as u32,
                ended_at,
                note: outcome_str.unwrap_or_default(),
            });
        }
        Ok(entries)
    }

    /// List events for `run_id` with `event_id > since`, ascending, up to
    /// `limit`. Pass `since = 0` for the first page.
    pub fn list_events_for_run(
        &self,
        run_id: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<EventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT event_id, run_id, issue_identifier, kind, payload, ts
             FROM events
             WHERE run_id = ?1 AND event_id > ?2
             ORDER BY event_id ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![run_id, since, limit as i64], |row| {
            Ok(EventRow {
                event_id: row.get(0)?,
                run_id: row.get(1)?,
                issue_identifier: row.get(2)?,
                kind: row.get(3)?,
                payload: row.get(4)?,
                ts: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// List all runs, paged (0-based `page`), newest-first.
    pub fn list_runs_paged(&self, page: usize, page_size: usize) -> Result<Vec<RunRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT run_id, issue_id, issue_identifier, workspace,
                    profile_json, workflow_path, workflow_sha,
                    pid, worker_id, started_at, finished_at,
                    outcome, exit_code, process_alive,
                    runner, model, provider
             FROM runs
             ORDER BY started_at DESC
             LIMIT ?1 OFFSET ?2",
        )?;
        let offset = page * page_size;
        let rows = stmt.query_map(params![page_size as i64, offset as i64], |row| {
            Ok(RunRow {
                run_id: row.get(0)?,
                issue_id: row.get(1)?,
                issue_identifier: row.get(2)?,
                workspace: row.get(3)?,
                profile_json: row.get(4)?,
                workflow_path: row.get(5)?,
                workflow_sha: row.get(6)?,
                pid: row.get::<_, i64>(7).unwrap_or(0) as u32,
                worker_id: row.get(8)?,
                started_at: row.get(9)?,
                finished_at: row.get(10)?,
                outcome: row.get(11)?,
                exit_code: row.get(12)?,
                process_alive: row.get::<_, i64>(13).unwrap_or(0) != 0,
                runner: row.get(14)?,
                model: row.get(15)?,
                provider: row.get(16)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn list_runs(&self, limit: usize) -> Result<Vec<RunRow>> {
        self.list_runs_paged(0, limit)
    }

    pub fn get_run(&self, run_id: &str) -> Result<Option<RunRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT run_id, issue_id, issue_identifier, workspace,
                    profile_json, workflow_path, workflow_sha,
                    pid, worker_id, started_at, finished_at,
                    outcome, exit_code, process_alive,
                    runner, model, provider
             FROM runs
             WHERE run_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![run_id], |row| {
            Ok(RunRow {
                run_id: row.get(0)?,
                issue_id: row.get(1)?,
                issue_identifier: row.get(2)?,
                workspace: row.get(3)?,
                profile_json: row.get(4)?,
                workflow_path: row.get(5)?,
                workflow_sha: row.get(6)?,
                pid: row.get::<_, i64>(7).unwrap_or(0) as u32,
                worker_id: row.get(8)?,
                started_at: row.get(9)?,
                finished_at: row.get(10)?,
                outcome: row.get(11)?,
                exit_code: row.get(12)?,
                process_alive: row.get::<_, i64>(13).unwrap_or(0) != 0,
                runner: row.get(14)?,
                model: row.get(15)?,
                provider: row.get(16)?,
            })
        })?;
        rows.next().transpose().context("get_run")
    }

    /// Count consecutive completed runs where the worker exited cleanly while
    /// the issue remained active. Stops at any other finished outcome,
    /// including terminal/non-active successful completions.
    ///
    /// `limit` caps how many rows are fetched; callers only need to know
    /// whether the streak reached some threshold, so passing `max + 1` is enough.
    pub fn consecutive_completed_runs(&self, issue_id: &str, limit: u32) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT run_id, outcome FROM runs
                 WHERE issue_id=?1
                   AND finished_at IS NOT NULL
                   AND outcome IS NOT NULL
                 ORDER BY finished_at DESC
                 LIMIT ?2",
            )
            .context("prepare consecutive completed runs query")?;
        let rows = stmt
            .query_map(params![issue_id, limit], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query consecutive completed runs")?;

        let mut count = 0u32;
        for row in rows {
            let (run_id, outcome) = row?;
            if outcome != "completed" {
                break;
            }
            let active_completion: bool = conn
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM events
                        WHERE run_id=?1
                          AND kind='lifecycle'
                          AND payload=?2
                     )",
                    params![run_id, ACTIVE_CONTINUATION_EVENT],
                    |row| row.get(0),
                )
                .context("query active completion marker")?;
            if !active_completion {
                break;
            }
            count = count.saturating_add(1);
        }
        Ok(count)
    }

    // ── Events ────────────────────────────────────────────────────────────────

    /// Insert one event. Returns the assigned `event_id` (auto-increment),
    /// which callers can use as a `since` cursor for streaming.
    pub fn insert_event(&self, ev: &NewEvent<'_>) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO events (run_id, issue_identifier, kind, payload, ts)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                ev.run_id,
                ev.issue_identifier,
                ev.kind,
                ev.payload,
                ev.ts.to_rfc3339(),
            ],
        )
        .context("insert_event")?;
        Ok(conn.last_insert_rowid())
    }

    /// List events for `issue_identifier` with `event_id > since`, ascending,
    /// up to `limit`. Pass `since = 0` for the first page.
    pub fn list_events_since(
        &self,
        issue_identifier: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<EventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT event_id, run_id, issue_identifier, kind, payload, ts
             FROM events
             WHERE issue_identifier = ?1 AND event_id > ?2
             ORDER BY event_id ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![issue_identifier, since, limit as i64], |row| {
            Ok(EventRow {
                event_id: row.get(0)?,
                run_id: row.get(1)?,
                issue_identifier: row.get(2)?,
                kind: row.get(3)?,
                payload: row.get(4)?,
                ts: row.get(5)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn list_all_events_since(&self, since: i64, limit: usize) -> Result<Vec<EventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT event_id, run_id, issue_identifier, kind, payload, ts
             FROM events
             WHERE event_id > ?1
             ORDER BY event_id ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since, limit as i64], |row| {
            Ok(EventRow {
                event_id: row.get(0)?,
                run_id: row.get(1)?,
                issue_identifier: row.get(2)?,
                kind: row.get(3)?,
                payload: row.get(4)?,
                ts: row.get(5)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // ── Claims ────────────────────────────────────────────────────────────────

    /// Insert a claim record when a worker picks up a run. Returns `claim_id`.
    pub fn insert_claim(&self, c: &NewClaim<'_>) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO claims (run_id, issue_id, issue_identifier, worker_id, claimed_at)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                c.run_id,
                c.issue_id,
                c.issue_identifier,
                c.worker_id,
                c.claimed_at.to_rfc3339()
            ],
        )
        .context("insert_claim")?;
        Ok(conn.last_insert_rowid())
    }

    /// Mark a claim as released (run finished or cancelled).
    pub fn release_claim(&self, claim_id: i64, released_at: DateTime<Utc>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE claims SET released_at=?1 WHERE claim_id=?2",
            params![released_at.to_rfc3339(), claim_id],
        )
        .context("release_claim")?;
        Ok(())
    }

    // ── Heartbeats ────────────────────────────────────────────────────────────

    /// Record one heartbeat pulse for a running worker.
    pub fn insert_heartbeat(&self, hb: &NewHeartbeat<'_>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO heartbeats (run_id, issue_identifier, worker_id, ts)
             VALUES (?1,?2,?3,?4)",
            params![
                hb.run_id,
                hb.issue_identifier,
                hb.worker_id,
                hb.ts.to_rfc3339()
            ],
        )
        .context("insert_heartbeat")?;
        Ok(())
    }

    /// Delete heartbeat rows for runs that have already finished.
    /// The `heartbeats_run_id` index makes this a single indexed DELETE.
    /// Call this periodically to keep the heartbeats table bounded.
    pub fn prune_finished_run_heartbeats(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM heartbeats
             WHERE run_id IN (SELECT run_id FROM runs WHERE finished_at IS NOT NULL)",
            [],
        )
        .context("prune_finished_run_heartbeats")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn claim_release_count_for_run(&self, run_id: &str) -> Result<(i64, i64)> {
        let conn = self.conn.lock().unwrap();
        let total = conn.query_row(
            "SELECT count(*) FROM claims WHERE run_id=?1",
            params![run_id],
            |r| r.get(0),
        )?;
        let released = conn.query_row(
            "SELECT count(*) FROM claims WHERE run_id=?1 AND released_at IS NOT NULL",
            params![run_id],
            |r| r.get(0),
        )?;
        Ok((total, released))
    }

    #[cfg(test)]
    pub(crate) fn heartbeat_count_for_run(&self, run_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let total = conn.query_row(
            "SELECT count(*) FROM heartbeats WHERE run_id=?1",
            params![run_id],
            |r| r.get(0),
        )?;
        Ok(total)
    }
}

// ─── helpers ──────────────────────────────────────────────────────────────────

fn run_status_to_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "Running",
        RunStatus::RetryQueued => "RetryQueued",
        RunStatus::Cancelled => "interrupted",
        RunStatus::Failed => "error",
        RunStatus::Succeeded => "completed",
        RunStatus::Interrupted => "interrupted",
        RunStatus::Crashed => "interrupted_gateway_restart",
        RunStatus::NeedsHuman => "needs_human",
        RunStatus::Stalled => "stalled",
        RunStatus::Terminal => "terminal",
        RunStatus::HookFailed => "hook_failed",
        RunStatus::DispatchFailed => "dispatch_failed",
        RunStatus::Released => "released",
        RunStatus::Orphaned => "orphaned",
        RunStatus::ParkBarrier => "park_barrier",
        RunStatus::Killed => "killed",
    }
}

fn str_to_run_status(s: &str) -> Option<RunStatus> {
    match s {
        "Running" => Some(RunStatus::Running),
        "RetryQueued" => Some(RunStatus::RetryQueued),
        "Cancelled" => Some(RunStatus::Cancelled),
        "Failed" | "error" => Some(RunStatus::Failed),
        "Succeeded" | "completed" => Some(RunStatus::Succeeded),
        "Interrupted" | "interrupted" => Some(RunStatus::Interrupted),
        "Crashed" | "interrupted_gateway_restart" => Some(RunStatus::Crashed),
        "NeedsHuman" | "needs_human" => Some(RunStatus::NeedsHuman),
        "stalled" => Some(RunStatus::Stalled),
        "terminal" => Some(RunStatus::Terminal),
        "hook_failed" => Some(RunStatus::HookFailed),
        "dispatch_failed" => Some(RunStatus::DispatchFailed),
        "released" => Some(RunStatus::Released),
        "orphaned" => Some(RunStatus::Orphaned),
        "park_barrier" => Some(RunStatus::ParkBarrier),
        "killed" => Some(RunStatus::Killed),
        _ => None,
    }
}

/// Generate a run ID from the issue identifier and the dispatch timestamp.
/// Unique within a single process; no UUID crate needed.
pub fn new_run_id(issue_identifier: &str, started_at: &DateTime<Utc>) -> String {
    format!("{}-{}", issue_identifier, started_at.timestamp_micros())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_tmp() -> Store {
        let dir = tempdir().unwrap();
        Store::open(&dir.path().join("store.db")).unwrap()
    }

    #[test]
    fn schema_creates_all_tables() {
        let store = open_tmp();
        let conn = store.conn.lock().unwrap();
        for tbl in ["runs", "events", "claims", "heartbeats"] {
            let count: i64 = conn
                .query_row(
                    &format!(
                        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='{tbl}'"
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table {tbl} missing");
        }
    }

    #[test]
    fn insert_run_persists_runner_model_provider() {
        let store = open_tmp();
        let now = Utc::now();
        let run_id = new_run_id("TEST-RM", &now);
        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: "rm",
                issue_identifier: "TEST-RM",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 7,
                worker_id: None,
                started_at: now,
                runner: Some("opencode"),
                model: Some("claude-opus-4-8"),
                provider: Some("anthropic"),
            })
            .unwrap();
        let row = store.get_run(&run_id).unwrap().expect("run present");
        assert_eq!(row.runner.as_deref(), Some("opencode"));
        assert_eq!(row.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(row.provider.as_deref(), Some("anthropic"));
        // Defaults to NULL when not provided.
        let run_id2 = new_run_id("TEST-RM2", &now);
        store
            .insert_run(&NewRun {
                run_id: &run_id2,
                issue_id: "rm2",
                issue_identifier: "TEST-RM2",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 8,
                worker_id: None,
                started_at: now,
                runner: Some("fake"),
                model: None,
                provider: None,
            })
            .unwrap();
        let row2 = store.get_run(&run_id2).unwrap().expect("run present");
        assert_eq!(row2.runner.as_deref(), Some("fake"));
        assert_eq!(row2.model, None);
        assert_eq!(row2.provider, None);
    }

    #[test]
    fn run_lifecycle_persists_and_loads() {
        let store = open_tmp();
        let now = Utc::now();
        let run_id = new_run_id("TEST-1", &now);

        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: "abc123",
                issue_identifier: "TEST-1",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: Some("/workflow.md"),
                workflow_sha: None,
                pid: 9999,
                worker_id: None,
                started_at: now,
                runner: None,
                model: None,
                provider: None,
            })
            .unwrap();

        store
            .finish_run(
                &run_id,
                &RunFinish {
                    outcome: RunStatus::Succeeded,
                    exit_code: Some(0),
                    finished_at: Utc::now(),
                },
            )
            .unwrap();

        let recent = store.load_recent_runs(10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].identifier, "TEST-1");
        assert!(matches!(recent[0].status, RunStatus::Succeeded));
    }

    #[test]
    fn consecutive_completed_runs_counts_only_still_active_completions() {
        let store = open_tmp();
        let issue_id = "abc123";
        for (idx, active_marker) in [(0, true), (1, true), (2, false)] {
            let started_at = Utc::now() + chrono::Duration::seconds(idx);
            let run_id = format!("TEST-1-{idx}");
            store
                .insert_run(&NewRun {
                    run_id: &run_id,
                    issue_id,
                    issue_identifier: "TEST-1",
                    workspace: "/tmp/ws",
                    profile_json: None,
                    workflow_path: None,
                    workflow_sha: None,
                    pid: 9999,
                    worker_id: None,
                    started_at,
                    runner: None,
                    model: None,
                    provider: None,
                })
                .unwrap();
            store
                .finish_run(
                    &run_id,
                    &RunFinish {
                        outcome: RunStatus::Succeeded,
                        exit_code: Some(0),
                        finished_at: started_at,
                    },
                )
                .unwrap();
            if active_marker {
                store
                    .insert_event(&NewEvent {
                        run_id: Some(&run_id),
                        issue_identifier: "TEST-1",
                        kind: "lifecycle",
                        payload: ACTIVE_CONTINUATION_EVENT,
                        ts: started_at,
                    })
                    .unwrap();
            }
        }

        assert_eq!(store.consecutive_completed_runs(issue_id, 10).unwrap(), 0);
    }

    #[test]
    fn events_since_cursor_is_paged() {
        let store = open_tmp();
        let now = Utc::now();

        for i in 0..5 {
            store
                .insert_event(&NewEvent {
                    run_id: None,
                    issue_identifier: "TEST-2",
                    kind: "stdout",
                    payload: &format!("line {i}"),
                    ts: now,
                })
                .unwrap();
        }

        let page1 = store.list_events_since("TEST-2", 0, 3).unwrap();
        assert_eq!(page1.len(), 3);
        let cursor = page1.last().unwrap().event_id;

        let page2 = store.list_events_since("TEST-2", cursor, 10).unwrap();
        assert_eq!(page2.len(), 2);
    }

    #[test]
    fn non_zero_exit_code_stored() {
        let store = open_tmp();
        let now = Utc::now();
        let run_id = new_run_id("TEST-EC", &now);

        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: "idec",
                issue_identifier: "TEST-EC",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 4242,
                worker_id: None,
                started_at: now,
                runner: None,
                model: None,
                provider: None,
            })
            .unwrap();

        store
            .finish_run(
                &run_id,
                &RunFinish {
                    outcome: RunStatus::Failed,
                    exit_code: Some(2),
                    finished_at: Utc::now(),
                },
            )
            .unwrap();

        let pages = store.list_runs_paged(0, 10).unwrap();
        let row = pages.iter().find(|r| r.run_id == run_id).unwrap();
        assert_eq!(row.exit_code, Some(2), "non-zero exit code must be stored");
        assert_eq!(row.outcome.as_deref(), Some("error"));
    }

    #[test]
    fn open_run_pids_returns_only_unfinished_alive_pids() {
        let store = open_tmp();
        let now = Utc::now();
        for (run_id, pid, finish) in [
            ("alive", 1234, false),
            ("pid-zero", 0, false),
            ("finished", 5678, true),
        ] {
            store
                .insert_run(&NewRun {
                    run_id,
                    issue_id: run_id,
                    issue_identifier: run_id,
                    workspace: "/tmp/ws",
                    profile_json: None,
                    workflow_path: None,
                    workflow_sha: None,
                    pid,
                    worker_id: None,
                    started_at: now,
                    runner: None,
                    model: None,
                    provider: None,
                })
                .unwrap();
            if finish {
                store
                    .finish_run(
                        run_id,
                        &RunFinish {
                            outcome: RunStatus::Succeeded,
                            exit_code: Some(0),
                            finished_at: now,
                        },
                    )
                    .unwrap();
            }
        }

        assert_eq!(store.open_run_pids().unwrap(), vec![1234]);
    }

    #[test]
    fn mark_crashed_runs_clears_alive_flag() {
        let store = open_tmp();
        let now = Utc::now();
        let run_id = new_run_id("TEST-3", &now);

        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: "id3",
                issue_identifier: "TEST-3",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 1234,
                worker_id: None,
                started_at: now,
                runner: None,
                model: None,
                provider: None,
            })
            .unwrap();

        // Simulate crash: run never finished.
        let n = store.mark_crashed_runs().unwrap();
        assert_eq!(n, 1, "exactly one row should be marked");

        let pages = store.list_runs_paged(0, 10).unwrap();
        let row = pages.iter().find(|r| r.run_id == run_id).unwrap();
        assert!(!row.process_alive);
        assert_eq!(row.outcome.as_deref(), Some("interrupted_gateway_restart"));
        assert!(
            row.finished_at.is_some(),
            "finished_at must be set after crash mark"
        );
    }

    #[test]
    fn crashed_runs_load_after_restart() {
        let store = open_tmp();
        let now = Utc::now();
        let run_id = new_run_id("TEST-5", &now);

        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: "id5",
                issue_identifier: "TEST-5",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 5555,
                worker_id: None,
                started_at: now,
                runner: None,
                model: None,
                provider: None,
            })
            .unwrap();

        // Mark as crashed (simulating gateway restart).
        store.mark_crashed_runs().unwrap();

        // load_recent_runs must include the crashed row.
        let recent = store.load_recent_runs(10).unwrap();
        let entry = recent.iter().find(|e| e.identifier == "TEST-5");
        assert!(
            entry.is_some(),
            "crashed run must appear in load_recent_runs"
        );
        assert!(
            matches!(entry.unwrap().status, RunStatus::Crashed),
            "status must be Crashed"
        );
    }

    #[test]
    fn list_events_for_run_paged() {
        let store = open_tmp();
        let now = Utc::now();

        let run_id = new_run_id("TEST-6", &now);
        store
            .insert_run(&NewRun {
                run_id: &run_id,
                issue_id: "id6",
                issue_identifier: "TEST-6",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 6666,
                worker_id: None,
                started_at: now,
                runner: None,
                model: None,
                provider: None,
            })
            .unwrap();

        for i in 0..5 {
            store
                .insert_event(&NewEvent {
                    run_id: Some(&run_id),
                    issue_identifier: "TEST-6",
                    kind: "stdout",
                    payload: &format!("line {i}"),
                    ts: now,
                })
                .unwrap();
        }

        let page1 = store.list_events_for_run(&run_id, 0, 3).unwrap();
        assert_eq!(page1.len(), 3);
        let cursor = page1.last().unwrap().event_id;

        let page2 = store.list_events_for_run(&run_id, cursor, 10).unwrap();
        assert_eq!(page2.len(), 2);
    }

    #[test]
    fn claims_and_heartbeats_insert() {
        let store = open_tmp();
        let now = Utc::now();

        let cid = store
            .insert_claim(&NewClaim {
                run_id: "r1",
                issue_id: "id4",
                issue_identifier: "TEST-4",
                worker_id: "w1",
                claimed_at: now,
            })
            .unwrap();
        store.release_claim(cid, Utc::now()).unwrap();

        store
            .insert_heartbeat(&NewHeartbeat {
                run_id: "r1",
                issue_identifier: "TEST-4",
                worker_id: "w1",
                ts: now,
            })
            .unwrap();
    }

    #[test]
    fn prune_heartbeats_removes_finished_runs_keeps_active() {
        let store = open_tmp();
        let now = Utc::now();

        // Insert a finished run and heartbeats for it.
        store
            .insert_run(&NewRun {
                run_id: "finished-run",
                issue_id: "issue-fin",
                issue_identifier: "TEST-FIN",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 1,
                worker_id: None,
                started_at: now,
                runner: None,
                model: None,
                provider: None,
            })
            .unwrap();
        store
            .finish_run(
                "finished-run",
                &RunFinish {
                    outcome: RunStatus::Succeeded,
                    exit_code: Some(0),
                    finished_at: now,
                },
            )
            .unwrap();
        for _ in 0..3 {
            store
                .insert_heartbeat(&NewHeartbeat {
                    run_id: "finished-run",
                    issue_identifier: "TEST-FIN",
                    worker_id: "w1",
                    ts: now,
                })
                .unwrap();
        }

        // Insert an active run and heartbeats for it.
        store
            .insert_run(&NewRun {
                run_id: "active-run",
                issue_id: "issue-act",
                issue_identifier: "TEST-ACT",
                workspace: "/tmp/ws",
                profile_json: None,
                workflow_path: None,
                workflow_sha: None,
                pid: 2,
                worker_id: None,
                started_at: now,
                runner: None,
                model: None,
                provider: None,
            })
            .unwrap();
        for _ in 0..2 {
            store
                .insert_heartbeat(&NewHeartbeat {
                    run_id: "active-run",
                    issue_identifier: "TEST-ACT",
                    worker_id: "w1",
                    ts: now,
                })
                .unwrap();
        }

        store.prune_finished_run_heartbeats().unwrap();

        // Finished-run heartbeats must be gone; active-run heartbeats must survive.
        let finished_count = store.heartbeat_count_for_run("finished-run").unwrap();
        let active_count = store.heartbeat_count_for_run("active-run").unwrap();
        assert_eq!(
            finished_count, 0,
            "heartbeats for finished run must be pruned"
        );
        assert_eq!(active_count, 2, "heartbeats for active run must survive");
    }

    #[test]
    fn active_claims_are_unique_by_issue_id_until_release() {
        let store = open_tmp();
        let now = Utc::now();
        let first = store
            .insert_claim(&NewClaim {
                run_id: "r1",
                issue_id: "issue-1",
                issue_identifier: "ALG-1",
                worker_id: "w1",
                claimed_at: now,
            })
            .unwrap();

        let duplicate = store.insert_claim(&NewClaim {
            run_id: "r2",
            issue_id: "issue-1",
            issue_identifier: "ALG-1",
            worker_id: "w2",
            claimed_at: now,
        });
        assert!(duplicate.is_err());

        store.release_claim(first, Utc::now()).unwrap();
        store
            .insert_claim(&NewClaim {
                run_id: "r3",
                issue_id: "issue-1",
                issue_identifier: "ALG-1",
                worker_id: "w3",
                claimed_at: now,
            })
            .unwrap();
    }

    #[test]
    fn legacy_claim_schema_migrates_with_multiple_active_claims() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("store.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE claims (
                    claim_id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id          TEXT    NOT NULL,
                    issue_identifier TEXT   NOT NULL,
                    worker_id       TEXT    NOT NULL,
                    claimed_at      TEXT    NOT NULL,
                    released_at     TEXT
                );
                INSERT INTO claims (run_id, issue_identifier, worker_id, claimed_at)
                VALUES ('r1', 'ALG-1', 'w1', '2026-01-01T00:00:00Z');
                INSERT INTO claims (run_id, issue_identifier, worker_id, claimed_at)
                VALUES ('r2', 'ALG-2', 'w2', '2026-01-01T00:00:01Z');
                ",
            )
            .unwrap();
        }

        let store = Store::open(&db).unwrap();
        let cid = store
            .insert_claim(&NewClaim {
                run_id: "r3",
                issue_id: "issue-3",
                issue_identifier: "ALG-3",
                worker_id: "w3",
                claimed_at: Utc::now(),
            })
            .unwrap();
        store.release_claim(cid, Utc::now()).unwrap();
    }
}
