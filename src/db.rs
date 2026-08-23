use crate::regions::{self, Hunk, Region};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::Duration;

/// Attribution hints, and the snapshots hanging off them, are considered stale
/// once older than this (seconds).
pub const HINT_TTL_SECS: i64 = 15;
/// Daemon heartbeat is considered alive if newer than this (seconds).
pub const HEARTBEAT_ALIVE_SECS: i64 = 15;

pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub external_id: String,
    pub agent_name: String,
    #[allow(dead_code)]
    pub kind: String,
    #[allow(dead_code)]
    pub harness: Option<String>,
    pub task_intent: Option<String>,
    pub status: String,
    #[allow(dead_code)]
    pub started_at: i64,
}

#[derive(Debug, Clone)]
pub struct EditRow {
    #[allow(dead_code)]
    pub id: i64,
    pub session_id: i64,
    pub agent_name: String,
    pub file: String,
    pub change_kind: String,
    #[allow(dead_code)]
    pub shadow_commit: Option<String>,
    pub ts: i64,
}

#[derive(Debug, Clone)]
pub struct ErrorRow {
    pub id: i64,
    pub reporter: i64,
    pub reporter_name: String,
    #[allow(dead_code)]
    pub command: Option<String>,
    pub excerpt: String,
    pub status: String,
    pub culprit: Option<i64>,
    pub culprit_name: Option<String>,
    pub fix_brief: Option<String>,
    pub ts_opened: i64,
}

impl ErrorRow {
    /// Who must act: the assigned culprit, else the reporter (untrusted
    /// "not mine" claims default the duty back onto the reporter).
    pub fn responsible(&self) -> i64 {
        self.culprit.unwrap_or(self.reporter)
    }
    pub fn responsible_name(&self) -> &str {
        self.culprit_name.as_deref().unwrap_or(&self.reporter_name)
    }
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub session_id: i64,
    pub agent_name: String,
    pub intent: Option<String>,
    pub start: i64,
    pub end: i64,
    pub last_ts: i64,
}

pub type FreshRegion = (String, i64, i64, String, i64, i64);

pub struct Db {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
  id           INTEGER PRIMARY KEY,
  external_id  TEXT NOT NULL UNIQUE,
  agent_name   TEXT NOT NULL,
  kind         TEXT NOT NULL,             -- 'llm' | 'human'
  harness      TEXT,
  task_intent  TEXT,
  status       TEXT NOT NULL DEFAULT 'active', -- 'active' | 'done'
  started_at   INTEGER NOT NULL,
  ended_at     INTEGER
);
CREATE TABLE IF NOT EXISTS edits (
  id            INTEGER PRIMARY KEY,
  session_id    INTEGER NOT NULL REFERENCES sessions(id),
  file          TEXT NOT NULL,
  change_kind   TEXT NOT NULL,            -- 'create' | 'modify' | 'delete'
  shadow_commit TEXT,
  ts            INTEGER NOT NULL,
  hunks         TEXT                      -- JSON [{old_start,old_lines,new_start,new_lines}]
);
CREATE INDEX IF NOT EXISTS idx_edits_session ON edits(session_id);
CREATE INDEX IF NOT EXISTS idx_edits_file ON edits(file);
CREATE TABLE IF NOT EXISTS hints (
  file       TEXT NOT NULL,
  session_id INTEGER NOT NULL REFERENCES sessions(id),
  ts         INTEGER NOT NULL,
  blob       TEXT                      -- shadow object id of the content the hook meant to write
);
CREATE TABLE IF NOT EXISTS regions (
  id         INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES sessions(id),
  file       TEXT NOT NULL,
  start_line INTEGER NOT NULL,
  end_line   INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_regions_file ON regions(file);
CREATE TABLE IF NOT EXISTS errors (
  id               INTEGER PRIMARY KEY,
  reporter_session INTEGER NOT NULL REFERENCES sessions(id),
  command          TEXT,
  output_excerpt   TEXT NOT NULL,
  status           TEXT NOT NULL DEFAULT 'open', -- 'open' | 'resolved'
  culprit_session  INTEGER REFERENCES sessions(id),
  fix_brief        TEXT,
  ts_opened        INTEGER NOT NULL,
  ts_resolved      INTEGER
);
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        // Migrations for older databases; harmless if the column exists.
        let _ = conn.execute("ALTER TABLE edits ADD COLUMN hunks TEXT", []);
        let _ = conn.execute("ALTER TABLE hints ADD COLUMN blob TEXT", []);
        Ok(Db { conn })
    }

    // ---- sessions -------------------------------------------------------

    pub fn upsert_session(
        &self,
        external_id: &str,
        agent_name: &str,
        kind: &str,
        harness: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions (external_id, agent_name, kind, harness, status, started_at)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5)
             ON CONFLICT(external_id) DO UPDATE SET status = 'active', ended_at = NULL",
            params![external_id, agent_name, kind, harness, now_ts()],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM sessions WHERE external_id = ?1",
            params![external_id],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn ensure_human(&self) -> Result<i64> {
        self.upsert_session("human", "human", "human", Some("fswatch"))
    }

    pub fn end_session(&self, external_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status = 'done', ended_at = ?2 WHERE external_id = ?1",
            params![external_id, now_ts()],
        )?;
        Ok(())
    }

    pub fn set_intent(&self, session_id: i64, intent: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET task_intent = ?2 WHERE id = ?1",
            params![session_id, intent],
        )?;
        Ok(())
    }

    pub fn get_session(&self, id: i64) -> Result<Session> {
        let s = self
            .conn
            .query_row(
                "SELECT id, external_id, agent_name, kind, harness, task_intent, status, started_at
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?;
        match s {
            Some(s) => Ok(s),
            None => bail!("session not found: ortak-{}", id),
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, external_id, agent_name, kind, harness, task_intent, status, started_at
             FROM sessions ORDER BY id",
        )?;
        let rows = stmt.query_map([], row_to_session)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Resolve a user-supplied session reference: "ortak-3", an external id,
    /// "human", or an agent name (latest matching session wins).
    pub fn resolve_session(&self, reference: &str) -> Result<Session> {
        if let Some(num) = reference.strip_prefix("ortak-") {
            if let Ok(id) = num.parse::<i64>() {
                return self.get_session(id);
            }
        }
        let by_ext = self
            .conn
            .query_row(
                "SELECT id, external_id, agent_name, kind, harness, task_intent, status, started_at
                 FROM sessions WHERE external_id = ?1",
                params![reference],
                row_to_session,
            )
            .optional()?;
        if let Some(s) = by_ext {
            return Ok(s);
        }
        let by_agent = self
            .conn
            .query_row(
                "SELECT id, external_id, agent_name, kind, harness, task_intent, status, started_at
                 FROM sessions WHERE agent_name = ?1 ORDER BY id DESC LIMIT 1",
                params![reference],
                row_to_session,
            )
            .optional()?;
        match by_agent {
            Some(s) => Ok(s),
            None => bail!("session not found: {}", reference),
        }
    }

    // ---- attribution hints ---------------------------------------------

    /// Record that a session wrote a file. `blob` is the shadow object holding
    /// that session's own result, rebuilt by the hook from the tool's input.
    /// Without one the daemon falls back to reading the file off disk.
    pub fn insert_hint(&self, file: &str, session_id: i64, blob: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO hints (file, session_id, ts, blob) VALUES (?1, ?2, ?3, ?4)",
            params![file, session_id, now_ts(), blob],
        )?;
        Ok(())
    }

    /// Consume the freshest non-stale blob-less hint for a file, clearing the
    /// rest. Snapshots are left for `peek_snapshots`, which journals them one
    /// commit each rather than collapsing them into a single attribution.
    pub fn take_hint(&self, file: &str) -> Result<Option<i64>> {
        let cutoff = now_ts() - HINT_TTL_SECS;
        let hit: Option<i64> = self
            .conn
            .query_row(
                "SELECT session_id FROM hints WHERE file = ?1 AND ts >= ?2 AND blob IS NULL
                 ORDER BY ts DESC LIMIT 1",
                params![file, cutoff],
                |r| r.get(0),
            )
            .optional()?;
        self.conn.execute(
            "DELETE FROM hints WHERE file = ?1 AND blob IS NULL",
            params![file],
        )?;
        // Opportunistic purge of stale hints on other files.
        self.conn
            .execute("DELETE FROM hints WHERE ts < ?1", params![cutoff])?;
        Ok(hit)
    }

    /// Files carrying a live snapshot the daemon has not journaled yet. The
    /// journal, not the file watcher, is what tells the daemon a hook has
    /// landed: the file's filesystem event may already have been flushed.
    pub fn snapshot_files(&self) -> Result<Vec<String>> {
        let cutoff = now_ts() - HINT_TTL_SECS;
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT file FROM hints WHERE blob IS NOT NULL AND ts >= ?1")?;
        let rows = stmt.query_map(params![cutoff], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Live snapshots for a file, oldest first. Insertion order decides, since
    /// `ts` only has second resolution.
    ///
    /// Reading does not consume: the next hook to run bases its own snapshot on
    /// the newest row, so a row has to outlive the commit that records it. The
    /// daemon calls `drop_snapshot` once that commit is in.
    pub fn peek_snapshots(&self, file: &str) -> Result<Vec<(i64, i64, String)>> {
        let cutoff = now_ts() - HINT_TTL_SECS;
        let mut stmt = self.conn.prepare(
            "SELECT rowid, session_id, blob FROM hints
             WHERE file = ?1 AND ts >= ?2 AND blob IS NOT NULL
             ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![file, cutoff], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The newest live snapshot for a file: what a further edit builds on.
    pub fn latest_snapshot(&self, file: &str) -> Result<Option<String>> {
        let cutoff = now_ts() - HINT_TTL_SECS;
        Ok(self
            .conn
            .query_row(
                "SELECT blob FROM hints WHERE file = ?1 AND ts >= ?2 AND blob IS NOT NULL
                 ORDER BY rowid DESC LIMIT 1",
                params![file, cutoff],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    pub fn drop_snapshot(&self, rowid: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM hints WHERE rowid = ?1", params![rowid])?;
        Ok(())
    }

    // ---- edits ----------------------------------------------------------

    pub fn insert_edit(
        &self,
        session_id: i64,
        file: &str,
        change_kind: &str,
        shadow_commit: Option<&str>,
        hunks: &[Hunk],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO edits (session_id, file, change_kind, shadow_commit, ts, hunks)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id,
                file,
                change_kind,
                shadow_commit,
                now_ts(),
                serde_json::to_string(hunks)?
            ],
        )?;
        Ok(())
    }

    pub fn recent_edits(&self, session_id: Option<i64>, limit: u32) -> Result<Vec<EditRow>> {
        let sql =
            "SELECT e.id, e.session_id, s.agent_name, e.file, e.change_kind, e.shadow_commit, e.ts
                   FROM edits e JOIN sessions s ON s.id = e.session_id
                   WHERE (?1 IS NULL OR e.session_id = ?1)
                   ORDER BY e.id DESC LIMIT ?2";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![session_id, limit], |r| {
            Ok(EditRow {
                id: r.get(0)?,
                session_id: r.get(1)?,
                agent_name: r.get(2)?,
                file: r.get(3)?,
                change_kind: r.get(4)?,
                shadow_commit: r.get(5)?,
                ts: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Files a session touched, with the last change kind per file.
    pub fn session_files(&self, session_id: i64) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT file, change_kind FROM edits WHERE session_id = ?1 AND id IN
               (SELECT MAX(id) FROM edits WHERE session_id = ?1 GROUP BY file)
             ORDER BY file",
        )?;
        let rows = stmt.query_map(params![session_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Other sessions that also touched any of the given files.
    pub fn overlapping_sessions(
        &self,
        session_id: i64,
        files: &[String],
    ) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.agent_name FROM edits e JOIN sessions s ON s.id = e.session_id
             WHERE e.file = ?1 AND e.session_id != ?2",
        )?;
        for f in files {
            let others = stmt
                .query_map(params![f, session_id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for o in others {
                out.push((f.clone(), o));
            }
        }
        Ok(out)
    }

    pub fn edit_count(&self, session_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM edits WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    // ---- regions --------------------------------------------------------

    /// After journaling an edit: shift every existing region on the file
    /// through the edit's hunks, drop regions the edit overwrote, and record
    /// the editor's own new regions (merged with what it already owned).
    pub fn apply_edit_regions(&self, session_id: i64, file: &str, hunks: &[Hunk]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let rows: Vec<(i64, i64, i64, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT id, session_id, start_line, end_line FROM regions WHERE file = ?1",
            )?;
            let iter = stmt.query_map(params![file], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            iter.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut mine: Vec<Region> = Vec::new();
        for (id, sid, start, end) in rows {
            let mapped = regions::map_region(hunks, Region { start, end });
            if sid == session_id {
                // Own regions are rebuilt below, merged with the new hunks.
                if let Some(r) = mapped {
                    mine.push(r);
                }
                tx.execute("DELETE FROM regions WHERE id = ?1", params![id])?;
            } else {
                match mapped {
                    None => {
                        tx.execute("DELETE FROM regions WHERE id = ?1", params![id])?;
                    }
                    Some(r) => {
                        tx.execute(
                            "UPDATE regions SET start_line = ?2, end_line = ?3 WHERE id = ?1",
                            params![id, r.start, r.end],
                        )?;
                    }
                }
            }
        }
        mine.extend(regions::regions_from_hunks(hunks));
        for r in regions::merge(mine) {
            tx.execute(
                "INSERT INTO regions (session_id, file, start_line, end_line, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, file, r.start, r.end, now_ts()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Hot regions of *other* active sessions overlapping any target range
    /// (with the ±margin neighborhood). Freshness = the owner's last edit on
    /// this file within the presence window.
    pub fn conflicts(
        &self,
        file: &str,
        targets: &[Region],
        me: i64,
        margin: i64,
        presence_secs: i64,
    ) -> Result<Vec<Conflict>> {
        let cutoff = now_ts() - presence_secs;
        let mut stmt = self.conn.prepare(
            "SELECT r.session_id, s.agent_name, s.task_intent, r.start_line, r.end_line,
                    (SELECT MAX(e.ts) FROM edits e
                      WHERE e.session_id = r.session_id AND e.file = r.file) AS last_ts
             FROM regions r JOIN sessions s ON s.id = r.session_id
             WHERE r.file = ?1 AND r.session_id != ?2 AND s.status = 'active'
               AND r.start_line <= ?3 AND r.end_line >= ?4
               AND (SELECT MAX(e.ts) FROM edits e
                     WHERE e.session_id = r.session_id AND e.file = r.file) >= ?5",
        )?;
        let mut out: Vec<Conflict> = Vec::new();
        for t in targets {
            let found = stmt
                .query_map(
                    params![
                        file,
                        me,
                        t.end.saturating_add(margin),
                        t.start - margin,
                        cutoff
                    ],
                    |r| {
                        Ok(Conflict {
                            session_id: r.get(0)?,
                            agent_name: r.get(1)?,
                            intent: r.get(2)?,
                            start: r.get(3)?,
                            end: r.get(4)?,
                            last_ts: r.get(5)?,
                        })
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for c in found {
                if !out
                    .iter()
                    .any(|o| o.session_id == c.session_id && o.start == c.start && o.end == c.end)
                {
                    out.push(c);
                }
            }
        }
        Ok(out)
    }

    /// All hot regions in the workspace (for `status`).
    pub fn fresh_regions(&self, presence_secs: i64) -> Result<Vec<FreshRegion>> {
        let cutoff = now_ts() - presence_secs;
        let mut stmt = self.conn.prepare(
            "SELECT r.file, r.start_line, r.end_line, s.agent_name, s.id,
                    (SELECT MAX(e.ts) FROM edits e
                      WHERE e.session_id = r.session_id AND e.file = r.file) AS last_ts
             FROM regions r JOIN sessions s ON s.id = r.session_id
             WHERE s.status = 'active'
               AND (SELECT MAX(e.ts) FROM edits e
                     WHERE e.session_id = r.session_id AND e.file = r.file) >= ?1
             ORDER BY r.file, r.start_line",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---- errors / stop-the-line ----------------------------------------

    pub fn insert_error(
        &self,
        reporter: i64,
        command: Option<&str>,
        excerpt: &str,
        culprit: Option<i64>,
        fix_brief: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO errors (reporter_session, command, output_excerpt, status, culprit_session, fix_brief, ts_opened)
             VALUES (?1, ?2, ?3, 'open', ?4, ?5, ?6)",
            params![reporter, command, excerpt, culprit, fix_brief, now_ts()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn error_rows(&self, only_open: bool, limit: u32) -> Result<Vec<ErrorRow>> {
        let sql = format!(
            "SELECT e.id, e.reporter_session, rs.agent_name, e.command, e.output_excerpt,
                    e.status, e.culprit_session, cs.agent_name, e.fix_brief, e.ts_opened
             FROM errors e
             JOIN sessions rs ON rs.id = e.reporter_session
             LEFT JOIN sessions cs ON cs.id = e.culprit_session
             {} ORDER BY e.id DESC LIMIT ?1",
            if only_open {
                "WHERE e.status = 'open'"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(ErrorRow {
                id: r.get(0)?,
                reporter: r.get(1)?,
                reporter_name: r.get(2)?,
                command: r.get(3)?,
                excerpt: r.get(4)?,
                status: r.get(5)?,
                culprit: r.get(6)?,
                culprit_name: r.get(7)?,
                fix_brief: r.get(8)?,
                ts_opened: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn open_errors(&self) -> Result<Vec<ErrorRow>> {
        self.error_rows(true, 100)
    }

    pub fn list_errors(&self, limit: u32) -> Result<Vec<ErrorRow>> {
        self.error_rows(false, limit)
    }

    /// Resolve open errors. With a session: those it is responsible for
    /// (assigned culprit, or unassigned ones it reported). Without: all.
    pub fn resolve_errors(&self, responsible: Option<i64>) -> Result<usize> {
        let n = match responsible {
            Some(id) => self.conn.execute(
                "UPDATE errors SET status = 'resolved', ts_resolved = ?2
                 WHERE status = 'open'
                   AND (culprit_session = ?1 OR (culprit_session IS NULL AND reporter_session = ?1))",
                params![id, now_ts()],
            )?,
            None => self.conn.execute(
                "UPDATE errors SET status = 'resolved', ts_resolved = ?1 WHERE status = 'open'",
                params![now_ts()],
            )?,
        };
        Ok(n)
    }

    pub fn assign_error(&self, error_id: i64, culprit: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE errors SET culprit_session = ?2 WHERE id = ?1 AND status = 'open'",
            params![error_id, culprit],
        )?;
        if n == 0 {
            bail!("open error not found: #{}", error_id);
        }
        Ok(())
    }

    /// (session_id, agent_name, file, last_ts) for edits within the lookback
    /// window. This forms the search space for assigning an owner.
    pub fn recent_session_files(
        &self,
        lookback_secs: i64,
    ) -> Result<Vec<(i64, String, String, i64)>> {
        let cutoff = now_ts() - lookback_secs;
        let mut stmt = self.conn.prepare(
            "SELECT e.session_id, s.agent_name, e.file, MAX(e.ts)
             FROM edits e JOIN sessions s ON s.id = e.session_id
             WHERE e.ts >= ?1
             GROUP BY e.session_id, e.file
             ORDER BY MAX(e.ts) DESC",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---- meta / heartbeat ----------------------------------------------

    pub fn heartbeat(&self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('daemon_heartbeat', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            params![now_ts().to_string()],
        )?;
        Ok(())
    }

    /// Seconds since the daemon's last heartbeat, if any.
    pub fn heartbeat_age(&self) -> Result<Option<i64>> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'daemon_heartbeat'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|s| s.parse::<i64>().ok()).map(|t| now_ts() - t))
    }
}

fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        external_id: r.get(1)?,
        agent_name: r.get(2)?,
        kind: r.get(3)?,
        harness: r.get(4)?,
        task_intent: r.get(5)?,
        status: r.get(6)?,
        started_at: r.get(7)?,
    })
}
