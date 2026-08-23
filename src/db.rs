use crate::regions::{self, Hunk, Region};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::Duration;

/// Attribution hints, and the snapshots hanging off them, are considered stale
/// once older than this (seconds).
pub const HINT_TTL_SECS: i64 = 15;
/// Reserved `hints.file` value for a session's open Bash claim. A
/// workspace-relative path is never `*`, so it cannot collide with a real file.
pub const BASH_CLAIM: &str = "*";
/// Daemon heartbeat is considered alive if newer than this (seconds).
pub const HEARTBEAT_ALIVE_SECS: i64 = 15;

pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Render a stored timestamp on the machine's local clock.
///
/// Timestamps are stored as UTC seconds and were printed as UTC too, while the
/// daemon logs its own lines with `chrono::Local`. The same event therefore
/// read as 07:37 in `ortak log` and 10:37 in the daemon window, which is a
/// miserable thing to hit while reconstructing what another session did.
/// Machine-readable output keeps the unix seconds and stays unambiguous.
pub fn fmt_local(ts: i64, fmt: &str) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| t.with_timezone(&chrono::Local).format(fmt).to_string())
        .unwrap_or_default()
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

/// How the daemon worked out who owns an edit. An edit hook naming the file is
/// evidence; a Bash claim is an inference, and one the daemon is allowed to
/// decline. Absent on a row means neither applied and the change fell to the
/// human session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    Hook,
    Claim,
}

impl Attribution {
    pub fn as_str(self) -> &'static str {
        match self {
            Attribution::Hook => "hook",
            Attribution::Claim => "claim",
        }
    }
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
    pub attributed_by: Option<String>,
}

impl EditRow {
    /// Whether the owner was guessed rather than reported. Rare once a claim
    /// only speaks for an unambiguous command, and that rarity is the point:
    /// a marked row is the one worth a second look.
    pub fn inferred(&self) -> bool {
        self.attributed_by.as_deref() == Some(Attribution::Claim.as_str())
    }
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

/// What one recorded publish carries: the branch it created, and the newest
/// edit of the session's that the branch includes.
#[derive(Debug, Clone)]
pub struct PublishRow {
    pub branch: String,
    pub last_edit_id: i64,
}

/// One message waiting for, or already handed to, a session.
#[derive(Debug, Clone)]
pub struct Message {
    #[allow(dead_code)]
    pub id: i64,
    pub from_session: i64,
    pub from_name: String,
    pub text: String,
    pub ts: i64,
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

/// A session's claim on a range of one file, as `blame` reads it back out.
#[derive(Debug, Clone)]
pub struct Owner {
    pub session_id: i64,
    pub agent_name: String,
    pub intent: Option<String>,
    pub start: i64,
    pub end: i64,
    pub last_ts: i64,
}

/// Why one session made a change, kept against the range it owned at the time.
#[derive(Debug, Clone)]
pub struct Note {
    pub session_id: i64,
    pub agent_name: String,
    pub start: i64,
    pub end: i64,
    pub text: String,
    pub ts: i64,
}

pub type FreshRegion = (String, i64, i64, String, i64, i64);

/// A file the daemon is currently failing to journal.
#[derive(Debug, Clone)]
pub struct JournalFailure {
    pub file: String,
    pub reason: String,
    pub ts: i64,
    /// Consecutive failed attempts on this file.
    pub streak: i64,
}

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
  hunks         TEXT,                     -- JSON [{old_start,old_lines,new_start,new_lines}]
  attributed_by TEXT,                     -- 'hook' | 'claim'; NULL when nothing claimed the file
  -- Set by `ortak release`: the session says the row is not its work. Every
  -- query that answers "what has this session done" skips these.
  disowned      INTEGER NOT NULL DEFAULT 0
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
CREATE TABLE IF NOT EXISTS publishes (
  id           INTEGER PRIMARY KEY,
  session_id   INTEGER NOT NULL REFERENCES sessions(id),
  branch       TEXT NOT NULL,
  last_edit_id INTEGER NOT NULL,          -- highest edits.id the branch carries
  ts           INTEGER NOT NULL
);
-- One row per recipient. A broadcast fans out when it is sent, which keeps
-- delivered_at meaningful per session and drops the message for sessions that
-- start later; a message about what is happening right now is stale by then.
CREATE TABLE IF NOT EXISTS messages (
  id           INTEGER PRIMARY KEY,
  from_session INTEGER NOT NULL REFERENCES sessions(id),
  to_session   INTEGER NOT NULL REFERENCES sessions(id),
  text         TEXT NOT NULL,
  ts           INTEGER NOT NULL,
  delivered_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_messages_to ON messages(to_session);
CREATE TABLE IF NOT EXISTS notes (
  id         INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES sessions(id),
  file       TEXT NOT NULL,
  start_line INTEGER NOT NULL,
  end_line   INTEGER NOT NULL,
  text       TEXT NOT NULL,
  ts         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_notes_file ON notes(file);
CREATE TABLE IF NOT EXISTS journal_failures (
  file   TEXT PRIMARY KEY,          -- newest failure only; the history is noise
  reason TEXT NOT NULL,
  ts     INTEGER NOT NULL,
  streak INTEGER NOT NULL
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
        let _ = conn.execute("ALTER TABLE edits ADD COLUMN attributed_by TEXT", []);
        let _ = conn.execute("ALTER TABLE hints ADD COLUMN blob TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE edits ADD COLUMN disowned INTEGER NOT NULL DEFAULT 0",
            [],
        );
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
        self.conn.execute(
            "DELETE FROM hints WHERE file = ?1 AND session_id IN
             (SELECT id FROM sessions WHERE external_id = ?2)",
            params![BASH_CLAIM, external_id],
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

    /// Read the freshest non-stale blob-less hint without consuming it.
    /// The daemon clears it only after the edit and regions are recorded.
    /// Falls back to an open Bash claim: the harness hooks cover the edit tools
    /// only, so a file written by `sed -i`, a heredoc or a codegen step reaches
    /// the daemon unattributed and would otherwise land on the human session.
    /// That fallback applies only while exactly one session has a command
    /// running; see below for why a second claim cancels the guess.
    pub fn peek_hint(&self, file: &str) -> Result<Option<(i64, Attribution)>> {
        let cutoff = now_ts() - HINT_TTL_SECS;
        let hit: Option<i64> = self
            .conn
            .query_row(
                "SELECT session_id FROM hints WHERE file = ?1 AND ts >= ?2 AND blob IS NULL
                 ORDER BY ts DESC, rowid DESC LIMIT 1",
                params![file, cutoff],
                |r| r.get(0),
            )
            .optional()?;
        // Opportunistic purge of stale hints on other files. Claims are exempt:
        // a build or test run outlives the TTL, and `post-bash` closes them.
        // Expired snapshots are deliberately purged here as well.
        self.conn.execute(
            "DELETE FROM hints WHERE ts < ?1 AND file != ?2",
            params![cutoff, BASH_CLAIM],
        )?;
        if let Some(id) = hit {
            return Ok(Some((id, Attribution::Hook)));
        }
        // A claim is not consumed: one command can write many files. Ending the
        // session retires it, so a harness that dies mid-command cannot leave
        // one standing.
        //
        // Only one, though. Two sessions running commands at the same time and
        // the daemon has no way to tell whose command produced the write; it
        // used to take the newest claim, which is how one agent's
        // `cargo fmt --all` was credited to the other, taking the region and
        // the publishable content with it. Nothing reported it. A wrong owner
        // is worse than no owner, so an ambiguous write falls through to the
        // human session, where it is at least honest.
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT h.session_id FROM hints h JOIN sessions s ON s.id = h.session_id
             WHERE h.file = ?1 AND s.status = 'active' LIMIT 2",
        )?;
        let claimants: Vec<i64> = stmt
            .query_map(params![BASH_CLAIM], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(match claimants.as_slice() {
            [only] => Some((*only, Attribution::Claim)),
            _ => None,
        })
    }

    /// Close a session's Bash claim once its command has finished.
    pub fn clear_bash_claim(&self, session_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM hints WHERE file = ?1 AND session_id = ?2",
            params![BASH_CLAIM, session_id],
        )?;
        Ok(())
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

    /// Drop blob-less hints on a file once its disk edit is recorded. Snapshot
    /// rows are committed and removed independently.
    pub fn clear_hints(&self, file: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM hints WHERE file = ?1 AND blob IS NULL",
            params![file],
        )?;
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
        attributed_by: Option<Attribution>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO edits (session_id, file, change_kind, shadow_commit, ts, hunks, attributed_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                file,
                change_kind,
                shadow_commit,
                now_ts(),
                serde_json::to_string(hunks)?,
                attributed_by.map(Attribution::as_str)
            ],
        )?;
        Ok(())
    }

    pub fn recent_edits(&self, session_id: Option<i64>, limit: u32) -> Result<Vec<EditRow>> {
        let sql = "SELECT e.id, e.session_id, s.agent_name, e.file, e.change_kind, e.shadow_commit,
                          e.ts, e.attributed_by
                   FROM edits e JOIN sessions s ON s.id = e.session_id
                   WHERE (?1 IS NULL OR e.session_id = ?1) AND e.disowned = 0
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
                attributed_by: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Files a session touched after edit `after`, with the last change kind
    /// per file. `after = 0` is every edit the session ever made.
    ///
    /// Both halves skip disowned rows, so a disowned newest row can never hide
    /// an owned older one.
    pub fn session_files(&self, session_id: i64, after: i64) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT file, change_kind FROM edits
             WHERE session_id = ?1 AND id > ?2 AND disowned = 0 AND id IN
               (SELECT MAX(id) FROM edits
                 WHERE session_id = ?1 AND id > ?2 AND disowned = 0 GROUP BY file)
             ORDER BY file",
        )?;
        let rows = stmt.query_map(params![session_id, after], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The newest edit a session has recorded, if it has recorded any.
    pub fn max_edit_id(&self, session_id: i64) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT MAX(id) FROM edits WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    /// Every shadow micro-commit this session recorded with the file it touched,
    /// oldest first after edit `after`. Publishing replays these to rebuild the session's own
    /// content, so the full history matters here where `session_files` only
    /// needs each file's final state. The file comes along so a publish can
    /// leave one out of the replay as well as out of the branch.
    pub fn session_commits(&self, session_id: i64, after: i64) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT shadow_commit, file FROM edits
             WHERE session_id = ?1 AND id > ?2 AND shadow_commit IS NOT NULL AND disowned = 0
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id, after], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The shadow micro-commit behind a session's newest edit to a file: the
    /// last content that session is known to have put there.
    pub fn last_commit_for(&self, session_id: i64, file: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT shadow_commit FROM edits
                 WHERE session_id = ?1 AND file = ?2 AND shadow_commit IS NOT NULL
                   AND disowned = 0
                 ORDER BY id DESC LIMIT 1",
                params![session_id, file],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Whether any other session has journaled an edit to this file.
    pub fn shared_file(&self, session_id: i64, file: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM edits
                 WHERE file = ?1 AND session_id != ?2 AND disowned = 0 LIMIT 1",
                params![file, session_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some())
    }

    pub fn edit_count(&self, session_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM edits WHERE session_id = ?1 AND disowned = 0",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    // ---- publishes ------------------------------------------------------

    /// Every branch a session has published, newest first. An amend needs two
    /// of these: the newest row says which branch it may rewrite, and the one
    /// before it says where that branch's work began.
    pub fn publishes(&self, session_id: i64) -> Result<Vec<PublishRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT branch, last_edit_id FROM publishes WHERE session_id = ?1
             ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(PublishRow {
                branch: r.get(0)?,
                last_edit_id: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn record_publish(&self, session_id: i64, branch: &str, last_edit_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO publishes (session_id, branch, last_edit_id, ts) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, branch, last_edit_id, now_ts()],
        )?;
        Ok(())
    }

    /// Move a branch's high-water mark instead of adding a row for it. An amend
    /// republishes one branch, so the session must end up with the same list of
    /// publishes it had before, or the next new deliverable starts from the
    /// wrong place and ships work that has already gone out.
    pub fn amend_publish(&self, session_id: i64, branch: &str, last_edit_id: i64) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE publishes SET last_edit_id = ?3, ts = ?4
             WHERE id = (SELECT MAX(id) FROM publishes WHERE session_id = ?1 AND branch = ?2)",
            params![session_id, branch, last_edit_id, now_ts()],
        )?;
        if changed == 0 {
            anyhow::bail!("ortak-{} has no publish of {} to amend", session_id, branch);
        }
        Ok(())
    }

    // ---- regions --------------------------------------------------------

    /// Give up a session's claim on a file's lines, or on every file it holds.
    /// Returns how many regions were dropped.
    ///
    /// `presence_minutes` was the only thing that ever cooled a region, and a
    /// timer cannot tell a session that walked away from one that is still
    /// working. A session that has published those lines, or says outright
    /// that it is done with them, is a signal.
    pub fn release_regions(&self, session_id: i64, file: Option<&str>) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM regions WHERE session_id = ?1 AND (?2 IS NULL OR file = ?2)",
            params![session_id, file],
        )?)
    }

    /// Give a file back entirely: the regions the gate defends and the journal
    /// rows `log` and `publish` read. Returns (regions dropped, edits disowned).
    ///
    /// One without the other is how the journal came to contradict itself, so
    /// they go together or not at all. The rows are marked rather than deleted,
    /// so a release by mistake has not destroyed the work and writing the file
    /// again takes it back.
    pub fn disown(&self, session_id: i64, file: Option<&str>) -> Result<(usize, usize)> {
        let tx = self.conn.unchecked_transaction()?;
        let regions = self.release_regions(session_id, file)?;
        let edits = tx.execute(
            "UPDATE edits SET disowned = 1
             WHERE session_id = ?1 AND (?2 IS NULL OR file = ?2) AND disowned = 0",
            params![session_id, file],
        )?;
        tx.commit()?;
        Ok((regions, edits))
    }

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

    /// Every region recorded on one file, ordered down the file. Unlike
    /// `fresh_regions` this ignores presence and session status: the point of
    /// blame is that a line still belongs to whoever wrote it long after that
    /// session ended.
    pub fn file_regions(&self, file: &str) -> Result<Vec<Owner>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.session_id, s.agent_name, s.task_intent, r.start_line, r.end_line,
                    COALESCE((SELECT MAX(e.ts) FROM edits e
                               WHERE e.session_id = r.session_id AND e.file = r.file), 0)
             FROM regions r JOIN sessions s ON s.id = r.session_id
             WHERE r.file = ?1
             ORDER BY r.start_line, r.end_line",
        )?;
        let rows = stmt.query_map(params![file], |r| {
            Ok(Owner {
                session_id: r.get(0)?,
                agent_name: r.get(1)?,
                intent: r.get(2)?,
                start: r.get(3)?,
                end: r.get(4)?,
                last_ts: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// A session's live regions: the ranges it currently owns, file by file.
    pub fn session_regions(&self, session_id: i64) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT file, start_line, end_line FROM regions WHERE session_id = ?1
             ORDER BY file, start_line",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The range a note written now describes: the session's newest region on
    /// the file. `None` when it owns nothing there, and the note goes against
    /// the whole file instead.
    pub fn session_region(&self, session_id: i64, file: &str) -> Result<Option<Region>> {
        Ok(self
            .conn
            .query_row(
                "SELECT start_line, end_line FROM regions
                 WHERE session_id = ?1 AND file = ?2
                 ORDER BY updated_at DESC, id DESC LIMIT 1",
                params![session_id, file],
                |r| {
                    Ok(Region {
                        start: r.get(0)?,
                        end: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    // ---- notes ----------------------------------------------------------

    pub fn insert_note(
        &self,
        session_id: i64,
        file: &str,
        start: i64,
        end: i64,
        text: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO notes (session_id, file, start_line, end_line, text, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, file, start, end, text, now_ts()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every note on a file, oldest first. Sessions are joined, not filtered:
    /// a note outlives the session that wrote it.
    pub fn file_notes(&self, file: &str) -> Result<Vec<Note>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.session_id, s.agent_name, n.start_line, n.end_line, n.text, n.ts
             FROM notes n JOIN sessions s ON s.id = n.session_id
             WHERE n.file = ?1 ORDER BY n.id",
        )?;
        let rows = stmt.query_map(params![file], |r| {
            Ok(Note {
                session_id: r.get(0)?,
                agent_name: r.get(1)?,
                start: r.get(2)?,
                end: r.get(3)?,
                text: r.get(4)?,
                ts: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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

    // ---- messages -------------------------------------------------------

    pub fn send_message(&self, from: i64, to: i64, text: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO messages (from_session, to_session, text, ts) VALUES (?1, ?2, ?3, ?4)",
            params![from, to, text, now_ts()],
        )?;
        Ok(())
    }

    /// Send to every other active agent session. The sender never gets its own
    /// broadcast, and the human session has no prompt hook to deliver through.
    pub fn broadcast_message(&self, from: i64, text: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "INSERT INTO messages (from_session, to_session, text, ts)
             SELECT ?1, id, ?2, ?3 FROM sessions
              WHERE id != ?1 AND status = 'active' AND kind != 'human'",
            params![from, text, now_ts()],
        )?)
    }

    /// Undelivered messages for a session, stamped delivered on the way out so
    /// the next prompt does not repeat them.
    pub fn take_messages(&self, session_id: i64) -> Result<Vec<Message>> {
        let tx = self.conn.unchecked_transaction()?;
        let out = {
            let mut stmt = tx.prepare(
                "SELECT m.id, m.from_session, s.agent_name, m.text, m.ts
                 FROM messages m JOIN sessions s ON s.id = m.from_session
                 WHERE m.to_session = ?1 AND m.delivered_at IS NULL
                 ORDER BY m.id",
            )?;
            let rows = stmt.query_map(params![session_id], row_to_message)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        tx.execute(
            "UPDATE messages SET delivered_at = ?2
             WHERE to_session = ?1 AND delivered_at IS NULL",
            params![session_id, now_ts()],
        )?;
        tx.commit()?;
        Ok(out)
    }

    /// Everything a session has been sent, oldest first, for a person looking.
    pub fn inbox(&self, session_id: i64) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.from_session, s.agent_name, m.text, m.ts
             FROM messages m JOIN sessions s ON s.id = m.from_session
             WHERE m.to_session = ?1 ORDER BY m.id",
        )?;
        let rows = stmt.query_map(params![session_id], row_to_message)?;
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
             WHERE e.ts >= ?1 AND e.disowned = 0
             GROUP BY e.session_id, e.file
             ORDER BY MAX(e.ts) DESC",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---- journal health -------------------------------------------------

    /// Note that a file could not be journaled.
    ///
    /// Only the newest failure per file is kept. A daemon stuck in a bad loop
    /// would otherwise add a row every 400 ms, and the history answers nothing
    /// the streak does not.
    pub fn record_journal_failure(&self, file: &str, reason: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO journal_failures (file, reason, ts, streak) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(file) DO UPDATE SET reason = ?2, ts = ?3, streak = streak + 1",
            params![file, reason, now_ts()],
        )?;
        Ok(())
    }

    /// Forget a file's failure, once it journals again.
    pub fn clear_journal_failure(&self, file: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM journal_failures WHERE file = ?1",
            params![file],
        )?;
        Ok(())
    }

    /// Files the daemon is failing on right now, newest failure first.
    pub fn journal_failures(&self) -> Result<Vec<JournalFailure>> {
        let mut stmt = self
            .conn
            .prepare("SELECT file, reason, ts, streak FROM journal_failures ORDER BY ts DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(JournalFailure {
                file: r.get(0)?,
                reason: r.get(1)?,
                ts: r.get(2)?,
                streak: r.get(3)?,
            })
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

fn row_to_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        from_session: r.get(1)?,
        from_name: r.get(2)?,
        text: r.get(3)?,
        ts: r.get(4)?,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> Db {
        let path = std::env::temp_dir().join(format!(
            "ortak-db-test-{}-{}.sqlite",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&path);
        Db::open(&path).unwrap()
    }

    #[test]
    fn a_bash_claim_attributes_an_unhinted_write() {
        let db = temp_db("claim");
        let human = db.ensure_human().unwrap();
        let agent = db
            .upsert_session("sess-a", "claude-sess", "llm", Some("claude-code"))
            .unwrap();

        // Nothing claimed: this is where the work used to fall through to human.
        assert_eq!(db.peek_hint("src/x.rs").unwrap(), None);

        db.insert_hint(BASH_CLAIM, agent, None).unwrap();
        assert_eq!(
            db.peek_hint("src/x.rs").unwrap(),
            Some((agent, Attribution::Claim))
        );
        // The claim survives, because one command can write many files.
        assert_eq!(
            db.peek_hint("src/y.rs").unwrap(),
            Some((agent, Attribution::Claim))
        );

        // A hint from the edit hooks still outranks the claim.
        db.insert_hint("src/z.rs", human, None).unwrap();
        assert_eq!(
            db.peek_hint("src/z.rs").unwrap(),
            Some((human, Attribution::Hook))
        );

        db.clear_bash_claim(agent).unwrap();
        assert_eq!(db.peek_hint("src/x.rs").unwrap(), None);
    }

    #[test]
    fn two_commands_at_once_credit_nobody() {
        let db = temp_db("two-claims");
        let a = db
            .upsert_session("sess-a", "claude-a", "llm", Some("claude-code"))
            .unwrap();
        let b = db
            .upsert_session("sess-b", "claude-b", "llm", Some("claude-code"))
            .unwrap();

        db.insert_hint(BASH_CLAIM, a, None).unwrap();
        assert_eq!(
            db.peek_hint("src/x.rs").unwrap(),
            Some((a, Attribution::Claim))
        );

        // b starts a command of its own. Whoever writes src/x.rs now, the
        // daemon cannot tell which of the two did it, so it does not guess.
        db.insert_hint(BASH_CLAIM, b, None).unwrap();
        assert_eq!(db.peek_hint("src/x.rs").unwrap(), None);

        // A hook naming the file is evidence and still outranks both claims.
        db.insert_hint("src/x.rs", b, None).unwrap();
        assert_eq!(
            db.peek_hint("src/x.rs").unwrap(),
            Some((b, Attribution::Hook))
        );
        db.clear_hints("src/x.rs").unwrap();

        // b's command finishes and a is an unambiguous guess again.
        db.clear_bash_claim(b).unwrap();
        assert_eq!(
            db.peek_hint("src/x.rs").unwrap(),
            Some((a, Attribution::Claim))
        );
    }

    #[test]
    fn the_journal_says_which_rows_were_guessed() {
        let db = temp_db("attributed-by");
        let a = db
            .upsert_session("sess-a", "claude-a", "llm", Some("claude-code"))
            .unwrap();
        db.insert_edit(a, "named.rs", "modify", None, &[], Some(Attribution::Hook))
            .unwrap();
        db.insert_edit(
            a,
            "guessed.rs",
            "modify",
            None,
            &[],
            Some(Attribution::Claim),
        )
        .unwrap();
        db.insert_edit(a, "orphan.rs", "modify", None, &[], None)
            .unwrap();

        let rows = db.recent_edits(None, 10).unwrap();
        let row = |f: &str| rows.iter().find(|r| r.file == f).unwrap();
        assert!(!row("named.rs").inferred(), "a hook named this file");
        assert!(row("guessed.rs").inferred(), "a claim guessed this one");
        // Nothing claimed it, so it went to the human by rule rather than by
        // inference. The agent column already says so; do not mark it twice.
        assert!(!row("orphan.rs").inferred());
    }

    #[test]
    fn ending_a_session_retires_its_claim() {
        let db = temp_db("end");
        let agent = db
            .upsert_session("sess-b", "claude-sess", "llm", Some("claude-code"))
            .unwrap();
        db.insert_hint(BASH_CLAIM, agent, None).unwrap();
        db.end_session("sess-b").unwrap();
        assert_eq!(db.peek_hint("src/x.rs").unwrap(), None);
    }

    fn names(files: Vec<(String, String)>) -> Vec<String> {
        files.into_iter().map(|(f, _)| f).collect()
    }

    /// One session, two tasks: the second publish must carry only the second
    /// task's file, and `--all` must still be able to rebuild everything.
    #[test]
    fn a_second_publish_ships_only_the_newer_work() {
        let db = temp_db("publishes");
        let s = db
            .upsert_session("sess-a", "claude-sess", "llm", Some("claude-code"))
            .unwrap();

        db.insert_edit(s, "first.rs", "create", None, &[], None)
            .unwrap();
        let shipped = db.max_edit_id(s).unwrap().expect("an edit was recorded");
        db.record_publish(s, "task/ortak-2-first", shipped).unwrap();

        db.insert_edit(s, "second.rs", "create", None, &[], None)
            .unwrap();

        let previous = db.publishes(s).unwrap();
        let previous = previous.first().expect("a publish was recorded");
        assert_eq!(previous.branch, "task/ortak-2-first");
        assert_eq!(
            names(db.session_files(s, previous.last_edit_id).unwrap()),
            vec!["second.rs"]
        );
        assert_eq!(
            names(db.session_files(s, 0).unwrap()),
            vec!["first.rs", "second.rs"]
        );
    }

    /// A file touched in both tasks belongs to the second branch too: the
    /// branch is built from the file's whole current content, not a patch.
    #[test]
    fn a_file_edited_again_after_a_publish_comes_back() {
        let db = temp_db("republish");
        let s = db
            .upsert_session("sess-b", "claude-sess", "llm", Some("claude-code"))
            .unwrap();

        db.insert_edit(s, "shared.rs", "create", None, &[], None)
            .unwrap();
        db.record_publish(s, "task/one", db.max_edit_id(s).unwrap().unwrap())
            .unwrap();
        db.insert_edit(s, "shared.rs", "modify", None, &[], None)
            .unwrap();

        let previous = db.publishes(s).unwrap();
        let previous = previous.first().unwrap();
        assert_eq!(
            db.session_files(s, previous.last_edit_id).unwrap(),
            vec![("shared.rs".to_string(), "modify".to_string())]
        );
    }

    fn agent(db: &Db, ext: &str) -> i64 {
        db.upsert_session(ext, ext, "llm", Some("claude-code"))
            .unwrap()
    }

    fn texts(messages: Vec<Message>) -> Vec<String> {
        messages.into_iter().map(|m| m.text).collect()
    }

    #[test]
    fn a_message_reaches_its_recipient_once() {
        let db = temp_db("messages-direct");
        let a = agent(&db, "sess-a");
        let b = agent(&db, "sess-b");

        db.send_message(a, b, "I renamed take_hint to peek_snapshots")
            .unwrap();

        assert!(texts(db.take_messages(a).unwrap()).is_empty());
        assert_eq!(
            texts(db.take_messages(b).unwrap()),
            vec!["I renamed take_hint to peek_snapshots"]
        );
        // Delivered once: the next prompt must not repeat it.
        assert!(texts(db.take_messages(b).unwrap()).is_empty());
        // The person looking still sees it afterwards.
        assert_eq!(db.inbox(b).unwrap().len(), 1);
    }

    #[test]
    fn a_broadcast_skips_its_sender_and_the_human() {
        let db = temp_db("messages-broadcast");
        let human = db.ensure_human().unwrap();
        let a = agent(&db, "sess-a");
        let b = agent(&db, "sess-b");
        let c = agent(&db, "sess-c");
        db.end_session("sess-c").unwrap();

        let sent = db
            .broadcast_message(a, "publish.rs is mid-refactor, do not start anything there")
            .unwrap();

        assert_eq!(sent, 1, "only the one other active agent session");
        assert!(texts(db.take_messages(a).unwrap()).is_empty());
        assert!(texts(db.take_messages(human).unwrap()).is_empty());
        assert!(texts(db.take_messages(c).unwrap()).is_empty());
        assert_eq!(
            texts(db.take_messages(b).unwrap()),
            vec!["publish.rs is mid-refactor, do not start anything there"]
        );
    }

    fn db() -> Db {
        Db::open(Path::new(":memory:")).expect("in-memory database")
    }

    fn session(db: &Db, name: &str) -> i64 {
        db.upsert_session(name, name, "llm", Some("claude-code"))
            .expect("session")
    }

    #[test]
    fn a_hint_outlives_a_commit_that_failed() {
        let db = db();
        let agent = session(&db, "claude-a");
        db.insert_hint("src/lib.rs", agent, None).unwrap();

        // The daemon reads the hint, its shadow commit fails, and it never
        // reaches clear_hints. The retry has to find the same owner.
        assert_eq!(
            db.peek_hint("src/lib.rs").unwrap(),
            Some((agent, Attribution::Hook))
        );
        assert_eq!(
            db.peek_hint("src/lib.rs").unwrap(),
            Some((agent, Attribution::Hook))
        );

        db.clear_hints("src/lib.rs").unwrap();
        assert_eq!(db.peek_hint("src/lib.rs").unwrap(), None);
    }

    #[test]
    fn clearing_one_file_leaves_the_rest_alone() {
        let db = db();
        let agent = session(&db, "claude-a");
        db.insert_hint("src/lib.rs", agent, None).unwrap();
        db.insert_hint("src/main.rs", agent, None).unwrap();

        db.clear_hints("src/lib.rs").unwrap();

        assert_eq!(db.peek_hint("src/lib.rs").unwrap(), None);
        assert_eq!(
            db.peek_hint("src/main.rs").unwrap(),
            Some((agent, Attribution::Hook))
        );
    }

    #[test]
    fn the_newest_hint_wins_and_a_stale_one_is_ignored() {
        let db = db();
        let first = session(&db, "claude-a");
        let second = session(&db, "claude-b");
        db.insert_hint("src/lib.rs", first, None).unwrap();
        db.insert_hint("src/lib.rs", second, None).unwrap();
        assert_eq!(
            db.peek_hint("src/lib.rs").unwrap(),
            Some((second, Attribution::Hook))
        );

        db.conn
            .execute(
                "UPDATE hints SET ts = ?1 WHERE file = 'src/lib.rs'",
                params![now_ts() - HINT_TTL_SECS - 1],
            )
            .unwrap();
        assert_eq!(db.peek_hint("src/lib.rs").unwrap(), None);
    }

    #[test]
    fn a_failing_file_is_held_until_it_journals_again() {
        let db = db();
        assert!(db.journal_failures().unwrap().is_empty());

        db.record_journal_failure("src/db.rs", "the index is locked")
            .unwrap();
        db.record_journal_failure("src/db.rs", "the index is locked")
            .unwrap();
        db.record_journal_failure("src/main.rs", "old reference value does not match")
            .unwrap();

        let failing = db.journal_failures().unwrap();
        assert_eq!(failing.len(), 2, "one row per file, not one per attempt");
        let locked = failing.iter().find(|f| f.file == "src/db.rs").unwrap();
        assert_eq!(locked.streak, 2);
        assert_eq!(locked.reason, "the index is locked");

        db.clear_journal_failure("src/db.rs").unwrap();
        let failing = db.journal_failures().unwrap();
        assert_eq!(failing.len(), 1);
        assert_eq!(failing[0].file, "src/main.rs");
    }

    #[test]
    fn a_stored_timestamp_prints_on_the_local_clock() {
        use chrono::{Offset, TimeZone};
        let ts = 1_700_000_000;
        let offset = chrono::Local
            .timestamp_opt(ts, 0)
            .unwrap()
            .offset()
            .fix()
            .local_minus_utc() as i64;
        let same_instant_shifted = chrono::DateTime::from_timestamp(ts + offset, 0)
            .unwrap()
            .format("%m-%d %H:%M:%S")
            .to_string();
        assert_eq!(fmt_local(ts, "%m-%d %H:%M:%S"), same_instant_shifted);
    }

    /// Give a session a region on `file` by journaling an edit to it, the way
    /// the daemon does.
    fn owns(db: &Db, session_id: i64, file: &str, start: i64, lines: i64) {
        let hunks = [Hunk {
            old_start: start,
            old_lines: lines,
            new_start: start,
            new_lines: lines,
        }];
        db.insert_edit(session_id, file, "modify", None, &hunks, None)
            .unwrap();
        db.apply_edit_regions(session_id, file, &hunks).unwrap();
    }

    #[test]
    fn releasing_one_session_leaves_the_other_holding_its_lines() {
        let db = db();
        let first = session(&db, "claude-a");
        let second = session(&db, "claude-b");
        let onlooker = session(&db, "claude-c");
        owns(&db, first, "src/hooks.rs", 10, 5);
        owns(&db, second, "src/hooks.rs", 100, 5);

        let at = |line: i64| Region {
            start: line,
            end: line,
        };
        let hot = |line: i64| db.conflicts("src/hooks.rs", &[at(line)], onlooker, 3, 1800);
        assert_eq!(hot(12).unwrap().len(), 1);
        assert_eq!(hot(102).unwrap().len(), 1);

        assert_eq!(db.release_regions(first, Some("src/hooks.rs")).unwrap(), 1);

        assert!(hot(12).unwrap().is_empty(), "released lines stay free");
        assert_eq!(hot(102).unwrap().len(), 1, "the other session still holds");
        assert!(db
            .fresh_regions(1800)
            .unwrap()
            .iter()
            .all(|(_, _, _, _, sid, _)| *sid == second));
    }

    #[test]
    fn a_released_file_leaves_everything_the_session_claims() {
        let db = db();
        let wrong = session(&db, "claude-a");
        let onlooker = session(&db, "claude-b");
        owns(&db, wrong, "src/impact.rs", 1, 195);
        owns(&db, wrong, "src/db.rs", 10, 4);

        assert_eq!(db.disown(wrong, Some("src/impact.rs")).unwrap(), (1, 1));

        // The gate, the publish file list and the log read three different
        // queries. Release used to quiet the first and leave the other two
        // holding a file the session had just said was not its work.
        let at = Region { start: 40, end: 40 };
        assert!(db
            .conflicts("src/impact.rs", &[at], onlooker, 3, 1800)
            .unwrap()
            .is_empty());
        let published: Vec<String> = db
            .session_files(wrong, 0)
            .unwrap()
            .into_iter()
            .map(|(f, _)| f)
            .collect();
        assert_eq!(published, ["src/db.rs"]);
        let logged: Vec<String> = db
            .recent_edits(Some(wrong), 20)
            .unwrap()
            .into_iter()
            .map(|e| e.file)
            .collect();
        assert_eq!(logged, ["src/db.rs"]);
        assert_eq!(db.edit_count(wrong).unwrap(), 1);

        // Writing the file again is how a session takes it back.
        owns(&db, wrong, "src/impact.rs", 1, 195);
        assert_eq!(db.session_files(wrong, 0).unwrap().len(), 2);
    }
}
