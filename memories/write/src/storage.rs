//! Job state and the consolidation baseline, for the one memory service that
//! owns this store.
//!
//! Three things live here: what was extracted from each session, one row per
//! session; the jobs that produce and consume those rows; and the files of the
//! last successful consolidation. There is one worker. A job that was running
//! when the service stopped is made ready again when it starts
//! ([`Store::requeue_interrupted`]); a failed job stays failed until its source
//! changes and it is reopened.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

/// Kind of job: extraction is per session, consolidation is global.
pub const EXTRACT: &str = "extract";
pub const CONSOLIDATE: &str = "consolidate";

/// The single key of the global consolidation job.
pub const CONSOLIDATE_KEY: &str = "global";

const SCHEMA: &str = include_str!("../migrations/0001_memories.sql");
/// Idempotent, like [`JOBS`]: both run at every open.
const BASELINE: &str = include_str!("../migrations/0002_baseline.sql");
const JOBS: &str = include_str!("../migrations/0003_jobs.sql");

/// What one session was worth remembering.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stage1Output {
    pub session_id: String,
    pub source_updated_at: i64,
    pub raw_memory: String,
    pub session_summary: String,
    pub session_slug: Option<String>,
    pub generated_at: i64,
    /// How many sessions cited it. `None` means never cited, which selection
    /// treats differently from "cited, but long ago".
    pub usage_count: Option<i64>,
    pub last_usage: Option<i64>,
}

/// A job the service has taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub kind: String,
    pub job_key: String,
}

/// How a job ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Work was produced.
    Succeeded,
    /// The run was valid and there was nothing worth keeping. Not a failure:
    /// running it again would produce the same nothing.
    SucceededNoOutput,
    /// Final until the job is reopened because its source changed.
    Failed(String),
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens the store, creating it on first use.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        Self::prepare(conn)
    }

    /// An in-memory store, for tests.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(conn: Connection) -> Result<Self, rusqlite::Error> {
        // A crashed writer must not leave the store unreadable, and a reader
        // must not block a writer: the memory pipeline runs in the background
        // while sessions are being written.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let table = |name: &str| -> Result<bool, rusqlite::Error> {
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![name],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
        };
        if !table("stage1_outputs")? {
            conn.execute_batch(SCHEMA)?;
        }
        conn.execute_batch(BASELINE)?;
        conn.execute_batch(JOBS)?;
        Ok(Self { conn })
    }

    /// Records a job as waiting to be done. An existing job is left alone:
    /// one that is done comes back only through [`Store::reopen`].
    pub fn enqueue(&self, kind: &str, job_key: &str) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO jobs (kind, job_key, status) VALUES (?1, ?2, 'ready')
             ON CONFLICT (kind, job_key) DO NOTHING",
            params![kind, job_key],
        )?;
        Ok(())
    }

    /// Takes the next ready job of `kind`, if there is one.
    pub fn claim(&self, kind: &str) -> Result<Option<Claim>, rusqlite::Error> {
        let job_key: Option<String> = self
            .conn
            .query_row(
                "UPDATE jobs SET status = 'running', finished_at = NULL
                  WHERE (kind, job_key) = (
                        SELECT kind, job_key FROM jobs
                         WHERE kind = ?1 AND status = 'ready' ORDER BY job_key LIMIT 1)
                 RETURNING job_key",
                params![kind],
                |row| row.get(0),
            )
            .optional()?;
        Ok(job_key.map(|job_key| Claim { kind: kind.to_owned(), job_key }))
    }

    /// Ends a job.
    pub fn finish(&self, claim: &Claim, outcome: &Outcome, now: i64) -> Result<(), rusqlite::Error> {
        let (status, error) = match outcome {
            Outcome::Succeeded | Outcome::SucceededNoOutput => ("succeeded", None),
            Outcome::Failed(error) => ("failed", Some(error.as_str())),
        };
        self.conn.execute(
            "UPDATE jobs SET status = ?3, finished_at = ?4, last_error = ?5
              WHERE kind = ?1 AND job_key = ?2",
            params![claim.kind, claim.job_key, status, now, error],
        )?;
        Ok(())
    }

    /// Makes every job that was running when the service stopped ready again.
    /// Called once at start, before anything is claimed.
    pub fn requeue_interrupted(&self) -> Result<usize, rusqlite::Error> {
        self.conn.execute("UPDATE jobs SET status = 'ready' WHERE status = 'running'", [])
    }

    /// Stops pending work durably, keeping its records, extracts and baseline.
    /// New source activity can reopen it; restoring a project cannot.
    pub fn cancel_pending(&self, now: i64) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE jobs SET status = 'failed', finished_at = ?1, last_error = 'the project was stopped'
             WHERE status IN ('ready', 'running')",
            params![now],
        )?;
        Ok(())
    }

    /// Writes what was extracted from one session, replacing an earlier
    /// extraction of the same session.
    ///
    /// The citation counts and the consolidation baseline survive: a session
    /// re-read is the same memory seen again, and forgetting that it had been
    /// used would let a useful memory be dropped by the next selection.
    pub fn put_extraction(&self, out: &Stage1Output) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO stage1_outputs
                 (session_id, source_updated_at, raw_memory, session_summary, session_slug,
                  generated_at, usage_count, last_usage)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (session_id) DO UPDATE SET
                 source_updated_at = excluded.source_updated_at,
                 raw_memory = excluded.raw_memory,
                 session_summary = excluded.session_summary,
                 session_slug = excluded.session_slug,
                 generated_at = excluded.generated_at",
            params![
                out.session_id,
                out.source_updated_at,
                out.raw_memory,
                out.session_summary,
                out.session_slug,
                out.generated_at,
                out.usage_count,
                out.last_usage,
            ],
        )?;
        Ok(())
    }

    /// Counts one session's use of one memory, and says whether it counted.
    ///
    /// Once per pair, ever. The read path reduces repeated citations within
    /// one reply to one; this reduces repeated replies within one session to
    /// one, which is the rule the count is supposed to mean. Without it a
    /// conversation that keeps citing the same line keeps that memory alive by
    /// itself.
    pub fn record_usage(
        &self,
        memory_session: &str,
        citing_session: &str,
        now: i64,
    ) -> Result<bool, rusqlite::Error> {
        let first = self.conn.execute(
            "INSERT INTO usage_by_session (memory_session, citing_session, first_used)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (memory_session, citing_session) DO NOTHING",
            params![memory_session, citing_session, now],
        )?;
        if first == 0 {
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE stage1_outputs
                SET usage_count = coalesce(usage_count, 0) + 1, last_usage = ?2
              WHERE session_id = ?1",
            params![memory_session, now],
        )?;
        Ok(true)
    }

    /// The memories the next consolidation should work from: the most used
    /// first, then the most recently touched, bounded by `limit`.
    ///
    /// A memory nothing has cited for `max_unused_days` is not selected. One
    /// that has never been cited is judged on when it was generated instead, so
    /// that something written yesterday is not dropped for not having been
    /// needed yet.
    pub fn select_for_consolidation(
        &self,
        limit: usize,
        max_unused_days: i64,
        now: i64,
    ) -> Result<Vec<Stage1Output>, rusqlite::Error> {
        let cutoff = now - max_unused_days * 24 * 60 * 60 * 1000;
        let mut statement = self.conn.prepare(
            "SELECT session_id, source_updated_at, raw_memory, session_summary, session_slug,
                    generated_at, usage_count, last_usage
               FROM stage1_outputs
              WHERE coalesce(last_usage, generated_at) >= ?1
              ORDER BY coalesce(usage_count, 0) DESC,
                       coalesce(last_usage, generated_at) DESC,
                       session_id
              LIMIT ?2",
        )?;
        let rows = statement.query_map(params![cutoff, limit as i64], |row| {
            Ok(Stage1Output {
                session_id: row.get(0)?,
                source_updated_at: row.get(1)?,
                raw_memory: row.get(2)?,
                session_summary: row.get(3)?,
                session_slug: row.get(4)?,
                generated_at: row.get(5)?,
                usage_count: row.get(6)?,
                last_usage: row.get(7)?,
            })
        })?;
        rows.collect()
    }

    /// Marks the rows a successful consolidation consumed, with the version of
    /// each it saw. A later extraction that rewrites a row leaves the mark as
    /// it was.
    pub fn mark_consolidated(&self, sessions: &[Stage1Output]) -> Result<(), rusqlite::Error> {
        for out in sessions {
            self.conn.execute(
                "UPDATE stage1_outputs
                    SET selected_for_consolidation = 1, selected_source_updated_at = ?2
                  WHERE session_id = ?1",
                params![out.session_id, out.source_updated_at],
            )?;
        }
        Ok(())
    }

    /// Makes a job ready again because its source has changed since it last
    /// ran. False while it is running: that run is already reading the
    /// source, and the caller asks again once it is done.
    ///
    /// This is the one way a finished job comes back. `enqueue` deliberately
    /// leaves an existing job alone.
    pub fn reopen(&self, kind: &str, job_key: &str) -> Result<bool, rusqlite::Error> {
        let changed = self.conn.execute(
            "UPDATE jobs SET status = 'ready', last_error = NULL
              WHERE kind = ?1 AND job_key = ?2 AND status <> 'running'",
            params![kind, job_key],
        )?;
        Ok(changed == 1)
    }

    /// The rows a successful consolidation has consumed. Nothing clears the
    /// mark, so a row that later fell out of selection stays among them.
    pub fn selected(&self) -> Result<Vec<Stage1Output>, rusqlite::Error> {
        let mut statement = self.conn.prepare(
            "SELECT session_id, source_updated_at, raw_memory, session_summary, session_slug,
                    generated_at, usage_count, last_usage
               FROM stage1_outputs
              WHERE selected_for_consolidation = 1
              ORDER BY session_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Stage1Output {
                session_id: row.get(0)?,
                source_updated_at: row.get(1)?,
                raw_memory: row.get(2)?,
                session_summary: row.get(3)?,
                session_slug: row.get(4)?,
                generated_at: row.get(5)?,
                usage_count: row.get(6)?,
                last_usage: row.get(7)?,
            })
        })?;
        rows.collect()
    }

    /// The files of the last successful consolidation; `None` before the first
    /// baseline is recorded.
    pub fn baseline(&self) -> Result<Option<BTreeMap<String, String>>, rusqlite::Error> {
        let recorded: i64 = self.conn.query_row("SELECT count(*) FROM baseline", [], |row| row.get(0))?;
        if recorded == 0 {
            return Ok(None);
        }
        let mut statement = self.conn.prepare("SELECT path, content FROM baseline_files")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<_, _>>().map(Some)
    }

    /// Replaces the baseline in one transaction. The publication time moves
    /// only when `MEMORY.md` itself changed.
    pub fn set_baseline(&self, files: &BTreeMap<String, String>, now: i64) -> Result<(), rusqlite::Error> {
        let tx = self.conn.unchecked_transaction()?;
        let (previous, _) = Self::published_in(&tx)?;
        let index = files.get(crate::workspace::INDEX).cloned().unwrap_or_default();
        tx.execute("DELETE FROM baseline_files", [])?;
        for (path, content) in files {
            tx.execute("INSERT INTO baseline_files (path, content) VALUES (?1, ?2)", params![path, content])?;
        }
        tx.execute(
            "INSERT INTO baseline (id, published_at) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET published_at =
                 CASE WHEN ?2 THEN excluded.published_at ELSE published_at END",
            params![if index.is_empty() { 0 } else { now }, previous != index],
        )?;
        tx.commit()
    }

    /// The published index and when it last changed, in unix milliseconds.
    /// Never an in-progress workspace file.
    pub fn published(&self) -> Result<(String, i64), rusqlite::Error> {
        Self::published_in(&self.conn)
    }

    fn published_in(conn: &Connection) -> Result<(String, i64), rusqlite::Error> {
        let index: Option<String> = conn
            .query_row(
                "SELECT content FROM baseline_files WHERE path = ?1",
                params![crate::workspace::INDEX],
                |row| row.get(0),
            )
            .optional()?;
        let at: Option<i64> = conn
            .query_row("SELECT published_at FROM baseline WHERE id = 1", [], |row| row.get(0))
            .optional()?;
        Ok((index.unwrap_or_default(), at.unwrap_or(0)))
    }

    /// The status of one job, for tests.
    pub fn status(&self, kind: &str, job_key: &str) -> Result<Option<String>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT status FROM jobs WHERE kind = ?1 AND job_key = ?2",
                params![kind, job_key],
                |row| row.get(0),
            )
            .optional()
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
