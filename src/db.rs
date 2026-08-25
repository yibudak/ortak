use crate::regions::{self, Hunk, Region};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Attribution hints, and the snapshots hanging off them, are considered stale
/// once older than this (seconds).
pub const HINT_TTL_SECS: i64 = 15;
/// Reserved `hints.file` value for a session's open Bash claim. A
/// workspace-relative path is never `*`, so it cannot collide with a real file.
pub const BASH_CLAIM: &str = "*";
/// How long a finished command's claim still speaks for what it wrote
/// (seconds). `post-bash` runs the moment the command returns, but the daemon
/// waits out its unattributed-quiet window before journaling, so a claim closed
/// on the spot is gone by the time anyone asks whose write it was. Long enough
/// to cover that wait, short enough not to catch the next thing typed, and
/// whole seconds is as fine as a stored timestamp goes.
pub const CLAIM_GRACE_SECS: i64 = 3;

/// The sessions whose Bash claim speaks for a write right now: a command still
/// running, or one that ended inside the grace. `?1` is `BASH_CLAIM` and `?2`
/// the grace cutoff, so it reads the same alone and as a subquery. One copy,
/// because two that drift hand a write to different sessions in different
/// places.
const LIVE_CLAIMANTS: &str =
    "SELECT DISTINCT h.session_id FROM hints h JOIN sessions s ON s.id = h.session_id
      WHERE h.file = ?1 AND s.status = 'active'
        AND (h.closed_at IS NULL OR h.closed_at >= ?2)";
/// The session columns `row_to_session` reads, in the order it reads them. One
/// copy, because a query that selects them in another order fills the fields
/// with each other's values and nothing complains.
const SESSION_COLS: &str =
    "id, external_id, agent_name, kind, harness, task_intent, status, started_at, last_seen";
/// Daemon heartbeat is considered alive if newer than this (seconds).
pub const HEARTBEAT_ALIVE_SECS: i64 = 15;
/// How long an outage is still worth reporting in `status` (seconds). The
/// question after one is always "what did I just miss", never "what happened
/// last Tuesday".
pub const OUTAGE_RECENT_SECS: i64 = 3600;

pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The oldest `closed_at` a Bash claim can carry and still be heard.
fn claim_grace_cutoff() -> i64 {
    now_ts() - CLAIM_GRACE_SECS
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
    /// When any hook last spoke for this session. Null on a row written before
    /// the stamp existed.
    ///
    /// ponytail: evidence, not a verdict. A session thinking for twenty minutes
    /// and one killed mid-turn look identical from here, so nothing in the tool
    /// reads this to decide anything; a person looks at the number and decides.
    pub last_seen: Option<i64>,
}

/// How the daemon worked out who owns an edit. An edit hook naming the file is
/// evidence; a Bash claim is an inference, and one the daemon is allowed to
/// decline. Absent on a row means nothing claimed the file at all and the
/// change is the human's own.
///
/// `Claimed` is the odd one out: nobody watched that write at all. A session ran
/// `ortak claim` afterwards and the journal was told. It is kept apart from
/// `Claim`, which is the daemon guessing from a command that was running at the
/// time, because a reader deciding whether to believe a row wants to know which
/// of the two happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    Hook,
    Claim,
    /// Two sessions had commands open, so the daemon declined to choose and the
    /// edit went to the human. Its own value rather than a NULL: an unclaimed
    /// write and a refused guess both land on the human session and read the
    /// same in the journal, and only one of them is worth a reader's attention.
    Contested,
    Claimed,
}

impl Attribution {
    pub fn as_str(self) -> &'static str {
        match self {
            Attribution::Hook => "hook",
            Attribution::Claim => "claim",
            Attribution::Contested => "contested",
            Attribution::Claimed => "claimed",
        }
    }
}

/// What `log` and `blame` say about a row the daemon worked out rather than was
/// told, as stored in `edits.attributed_by`. Empty for a hook and for an
/// unclaimed write: the agent column already answers those, and a marker on
/// every row is a marker nobody reads.
pub fn attribution_note(attributed_by: Option<&str>) -> &'static str {
    match attributed_by {
        Some(s) if s == Attribution::Claim.as_str() => "inferred from a running command",
        Some(s) if s == Attribution::Contested.as_str() => "two sessions had commands open",
        Some(s) if s == Attribution::Claimed.as_str() => "claimed after the fact",
        _ => "",
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

/// What one recorded publish carries: the branch it created, the newest edit of
/// the session's that the branch includes, and when it went out.
#[derive(Debug, Clone)]
pub struct PublishRow {
    pub branch: String,
    pub last_edit_id: i64,
    pub ts: i64,
}

/// One message waiting for, or already handed to, a session.
#[derive(Debug, Clone)]
pub struct Message {
    #[allow(dead_code)]
    pub id: i64,
    pub from_session: i64,
    pub from_name: String,
    /// Who it was addressed to. Usually the session reading it, and worth
    /// carrying for the case where it is not: a message handed to the next
    /// session because the one it was sent to has stopped.
    pub to_session: i64,
    pub text: String,
    pub ts: i64,
}

/// One session's undelivered mail, as `status` reports it.
#[derive(Debug, Clone)]
pub struct Waiting {
    pub session_id: i64,
    pub agent_name: String,
    pub stopped: bool,
    pub count: i64,
    pub oldest: i64,
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
    /// How the edit behind `last_ts` was attributed, as `edits.attributed_by`
    /// stores it. Blame is read to settle who wrote a line, so the two answers
    /// it must not give plainly are the guess and the refusal to guess.
    pub attributed_by: Option<String>,
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

/// A stretch with no daemon, so nothing written in it reached the journal.
/// Both ends are known: the last heartbeat before it stopped, and the moment
/// one started again.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Outage {
    pub start: i64,
    pub end: i64,
    /// Files the startup scan caught up on afterwards, against the human
    /// session. The difference between "you may have lost something" and
    /// "these three were picked up".
    pub journaled: u32,
}

impl Outage {
    pub fn secs(&self) -> i64 {
        (self.end - self.start).max(0)
    }
    pub fn recent(&self, now: i64) -> bool {
        now - self.end <= OUTAGE_RECENT_SECS
    }
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
  ended_at     INTEGER,
  last_seen    INTEGER                    -- newest hook from this session
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
  blob       TEXT,                     -- shadow object id of the content the hook meant to write
  -- When a Bash claim's command ended. NULL while it is still running; a claim
  -- closed within CLAIM_GRACE_SECS still speaks for the write its command made.
  closed_at  INTEGER
);
CREATE TABLE IF NOT EXISTS regions (
  id         INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES sessions(id),
  file       TEXT NOT NULL,
  start_line INTEGER NOT NULL,
  end_line   INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  -- The work shipped and the lines are free, but the session still wrote them.
  -- The gate skips a cooled row; blame reads it.
  cooled     INTEGER NOT NULL DEFAULT 0,
  -- How the edit that wrote these lines was attributed, as `edits` stores it.
  -- Kept here rather than read back off the session's newest edit on the file,
  -- which marked every line of a file that had seen one contested write.
  attributed_by TEXT
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
  ts_resolved      INTEGER,
  -- When the responsible session was handed its assignment, the way messages
  -- carry delivered_at. Cleared on reassignment, because the new owner has not
  -- heard it.
  owner_told_at    INTEGER
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
        let _ = conn.execute(
            "ALTER TABLE regions ADD COLUMN cooled INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute("ALTER TABLE errors ADD COLUMN owner_told_at INTEGER", []);
        let _ = conn.execute("ALTER TABLE regions ADD COLUMN attributed_by TEXT", []);
        let _ = conn.execute("ALTER TABLE hints ADD COLUMN closed_at INTEGER", []);
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN last_seen INTEGER", []);
        Ok(Db { conn })
    }

    // ---- sessions -------------------------------------------------------

    /// Register a session, or mark an existing one active again.
    ///
    /// Every hook runs this before it does anything else, which is why the
    /// `last_seen` stamp lives here rather than in the seven hook bodies: one
    /// write, in the write that was already happening, and no hook to forget.
    pub fn upsert_session(
        &self,
        external_id: &str,
        agent_name: &str,
        kind: &str,
        harness: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions
                 (external_id, agent_name, kind, harness, status, started_at, last_seen)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?5)
             ON CONFLICT(external_id) DO UPDATE
                 SET status = 'active', ended_at = NULL, last_seen = ?5",
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
                &format!("SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"),
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
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {SESSION_COLS} FROM sessions ORDER BY id"))?;
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
                &format!("SELECT {SESSION_COLS} FROM sessions WHERE external_id = ?1"),
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
                &format!(
                    "SELECT {SESSION_COLS} FROM sessions
                      WHERE agent_name = ?1 ORDER BY id DESC LIMIT 1"
                ),
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
    /// One command running makes that unambiguous. Two overlapping commands are
    /// settled by the journal instead, and only a file none of the claimants
    /// has written falls through to the human; see below.
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
        // Opportunistic purge of stale hints on other files. Claims are exempt
        // from the TTL: a build or test run outlives it, and `post-bash` closes
        // them. Expired snapshots are deliberately purged here as well.
        self.conn.execute(
            "DELETE FROM hints WHERE ts < ?1 AND file != ?2",
            params![cutoff, BASH_CLAIM],
        )?;
        // Closed claims go once their grace has run out, so one row per Bash
        // call does not accumulate for the length of a session.
        self.conn.execute(
            "DELETE FROM hints WHERE file = ?1 AND closed_at IS NOT NULL AND closed_at < ?2",
            params![BASH_CLAIM, claim_grace_cutoff()],
        )?;
        if let Some(id) = hit {
            return Ok(Some((id, Attribution::Hook)));
        }
        // A claim is not consumed: one command can write many files. Ending the
        // session retires it, so a harness that dies mid-command cannot leave
        // one standing.
        //
        // Two of them and the claims alone cannot say whose command wrote the
        // file; taking the newest is how one agent's `cargo fmt --all` was
        // credited to the other, region and publishable content with it, and
        // nothing reported it. The journal can say, though: a session that has
        // already written this file has a stake one that never touched it does
        // not. Giving up instead gives up on most writes, because in a
        // two-agent workspace somebody has a command open nearly always. A file
        // no claimant has written is the genuinely ambiguous one, and it still
        // falls through to the human with a contested marker.
        let mut stmt = self.conn.prepare(&format!("{LIVE_CLAIMANTS} LIMIT 2"))?;
        let claimants: Vec<i64> = stmt
            .query_map(params![BASH_CLAIM, claim_grace_cutoff()], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(match claimants.as_slice() {
            [] => None,
            [only] => Some((*only, Attribution::Claim)),
            _ => match self.claimant_in_file(file)? {
                Some(id) => Some((id, Attribution::Claim)),
                None => Some((self.ensure_human()?, Attribution::Contested)),
            },
        })
    }

    /// Of the sessions with a command open, the one that most recently wrote
    /// this file. `None` when none of them has: a person editing in their own
    /// editor while two agents run commands is a real case, and the honest
    /// answer there is still the human.
    ///
    /// The journal rather than `regions`, because a region is made from an edit
    /// and does not outlive it. Lines a session has already published are cool
    /// and lines it overwrote are gone, and neither means the session stopped
    /// working in the file.
    fn claimant_in_file(&self, file: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT e.session_id FROM edits e
                      WHERE e.session_id IN ({LIVE_CLAIMANTS}) AND e.file = ?3
                      ORDER BY e.id DESC LIMIT 1"
                ),
                params![BASH_CLAIM, claim_grace_cutoff(), file],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Close a session's Bash claim now its command has finished. The row stays
    /// until the grace runs out; `CLAIM_GRACE_SECS` says why deleting it here
    /// loses the writes the command itself made.
    pub fn clear_bash_claim(&self, session_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE hints SET closed_at = ?3
             WHERE file = ?1 AND session_id = ?2 AND closed_at IS NULL",
            params![BASH_CLAIM, session_id, now_ts()],
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

    /// The newest edit a session made to one file at or before a high-water
    /// mark. Publish uses it with the marks in `publishes` to work out which
    /// branch already carries a file.
    pub fn last_edit_upto(&self, session_id: i64, file: &str, upto: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT MAX(id) FROM edits
                 WHERE session_id = ?1 AND file = ?2 AND id <= ?3",
                params![session_id, file, upto],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
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

    /// The shadow micro-commits either side of a publish's slice for one file:
    /// the session's newest edit inside the slice, and its newest before it.
    /// `after` is the high-water mark the slice starts from, exclusive, so
    /// `after = 0` puts every edit inside it and leaves the second answer empty.
    ///
    /// Each is the content the session is known to have put in the file at that
    /// point. Publish needs both because it replays the slice alone: the first
    /// says what the branch should end up with, and the second says what the
    /// base branch has to already carry for that to be true.
    pub fn slice_commits(
        &self,
        session_id: i64,
        file: &str,
        after: i64,
    ) -> Result<(Option<String>, Option<String>)> {
        Ok(self.conn.query_row(
            "SELECT (SELECT shadow_commit FROM edits
                      WHERE session_id = ?1 AND file = ?2 AND shadow_commit IS NOT NULL
                        AND id > ?3 AND disowned = 0 ORDER BY id DESC LIMIT 1),
                    (SELECT shadow_commit FROM edits
                      WHERE session_id = ?1 AND file = ?2 AND shadow_commit IS NOT NULL
                        AND id <= ?3 AND disowned = 0 ORDER BY id DESC LIMIT 1)",
            params![session_id, file, after],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// The first edit this session made after `after` to any of `files`.
    ///
    /// Publish stops its high-water mark just short of this, so a file held
    /// back by `--exclude` or left out of the replay is picked up by the next
    /// publish. The mark used to advance to the session's newest edit whatever
    /// shipped, which put the held-back file behind it for good: no incremental
    /// publish would look that far back again.
    pub fn first_edit_on(
        &self,
        session_id: i64,
        after: i64,
        files: &[String],
    ) -> Result<Option<i64>> {
        if files.is_empty() {
            return Ok(None);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT id, file FROM edits WHERE session_id = ?1 AND id > ?2 ORDER BY id")?;
        let rows = stmt.query_map(params![session_id, after], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, file) = row?;
            if files.iter().any(|f| f == &file) {
                return Ok(Some(id));
            }
        }
        Ok(None)
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
            "SELECT branch, last_edit_id, ts FROM publishes WHERE session_id = ?1
             ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(PublishRow {
                branch: r.get(0)?,
                last_edit_id: r.get(1)?,
                ts: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// What a session has published, newest first, each with the number of its
    /// edits still waiting behind it.
    ///
    /// Only the newest publish can have any: work done after an older one went
    /// into the branches published since, so counting there would report
    /// shipped work as though it were still sitting in the workspace. `status`
    /// prints this and `--json` carries it, and a rule that has to hold in both
    /// belongs in one place.
    pub fn published_branches(&self, session_id: i64) -> Result<Vec<(PublishRow, i64)>> {
        let rows = self.publishes(session_id)?;
        let mut out = Vec::with_capacity(rows.len());
        for (i, p) in rows.into_iter().enumerate() {
            let behind = match i {
                0 => self.edits_since(session_id, p.last_edit_id)?,
                _ => 0,
            };
            out.push((p, behind));
        }
        Ok(out)
    }

    /// How many edits a session has recorded since edit `after`.
    fn edits_since(&self, session_id: i64, after: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM edits WHERE session_id = ?1 AND id > ?2",
            params![session_id, after],
            |r| r.get(0),
        )?)
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

    /// Free a session's lines on a file it has just published, keeping the row
    /// so blame can still answer for them. Returns how many were cooled.
    ///
    /// Publish used to delete these, which freed the lines and lost the only
    /// record of who wrote them: `ortak log` still had the edit while `blame`
    /// said the file was as the base branch left it. A release deletes because
    /// it means "this was never mine"; a publish means "this shipped", and the
    /// session did write it.
    pub fn cool_regions(&self, session_id: i64, file: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE regions SET cooled = 1
             WHERE session_id = ?1 AND file = ?2 AND cooled = 0",
            params![session_id, file],
        )?)
    }

    /// After journaling an edit: shift every existing region on the file
    /// through the edit's hunks, drop regions the edit overwrote, and record
    /// the editor's own new regions with how this edit was attributed.
    pub fn apply_edit_regions(
        &self,
        session_id: i64,
        file: &str,
        hunks: &[Hunk],
        attributed_by: Option<Attribution>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let rows: Vec<(i64, i64, i64, i64, Option<String>, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT id, session_id, start_line, end_line, attributed_by, cooled FROM regions
                 WHERE file = ?1",
            )?;
            let iter = stmt.query_map(params![file], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?;
            iter.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut mine: Vec<(Region, Option<String>, i64)> = Vec::new();
        for (id, sid, start, end, how, cooled) in rows {
            let mapped = regions::map_region(hunks, Region { start, end });
            if sid == session_id {
                // Own regions are rebuilt below, merged with the new hunks.
                // Only the lines this edit touches come back hot: the session is
                // working there again, and nowhere else. Re-hotting the whole
                // file put every published line of it back on the gate, and made
                // `cooled = 0` stop meaning "what this branch ships" for the
                // scan that reads it.
                if let Some(r) = mapped {
                    mine.push((r, how, cooled));
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
        // This edit's lines come out of whatever the session already claimed
        // there: two rows over one line would let blame answer with the older
        // of them, and the line was last written by this edit.
        let fresh = regions::regions_from_hunks(hunks);
        let how = attributed_by.map(|a| a.as_str().to_string());
        let mut claims: Vec<(Region, Option<String>, i64)> = mine
            .into_iter()
            .flat_map(|(r, was, cooled)| {
                regions::subtract(r, &fresh)
                    .into_iter()
                    .map(move |r| (r, was.clone(), cooled))
            })
            .collect();
        claims.extend(fresh.into_iter().map(|r| (r, how.clone(), 0)));
        // Merged within one attribution and no further. Coalescing a contested
        // range into the ordinary one beside it puts the marker back on lines
        // nobody contested, which is what this column exists to stop.
        let mut by_how: BTreeMap<(Option<String>, i64), Vec<Region>> = BTreeMap::new();
        for (r, was, cooled) in claims {
            by_how.entry((was, cooled)).or_default().push(r);
        }
        for ((was, cooled), ranges) in by_how {
            for r in regions::merge(ranges) {
                tx.execute(
                    "INSERT INTO regions
                       (session_id, file, start_line, end_line, updated_at, attributed_by, cooled)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![session_id, file, r.start, r.end, now_ts(), was, cooled],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The mirror of `disown`: one file's regions and journal rows become this
    /// session's, whoever the daemon credited at the time. Returns (regions
    /// moved, rows moved, the sessions they came from).
    ///
    /// Every moved row is marked `claimed`, so `blame` and `log` say the
    /// journal was told rather than that it watched. Without that this is a way
    /// to take another session's work and leave no trace, which is the opposite
    /// of what the journal is for.
    ///
    /// Disowned rows come too, and lose the mark. A file released by the wrong
    /// owner is the case this pairs with: those rows carry the shadow commits
    /// the real author's branch needs, and while they sit disowned no session
    /// can publish them.
    pub fn claim_file(&self, session_id: i64, file: &str) -> Result<(usize, usize, Vec<i64>)> {
        let tx = self.conn.unchecked_transaction()?;
        let from: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT session_id FROM edits WHERE file = ?1 AND session_id != ?2
                 UNION
                 SELECT session_id FROM regions WHERE file = ?1 AND session_id != ?2
                 ORDER BY 1",
            )?;
            let rows = stmt.query_map(params![file, session_id], |r| r.get(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let edits = tx.execute(
            "UPDATE edits SET session_id = ?2, attributed_by = ?3, disowned = 0
             WHERE file = ?1 AND (session_id != ?2 OR disowned = 1)",
            params![file, session_id, Attribution::Claimed.as_str()],
        )?;
        let regions = tx.execute(
            "UPDATE regions SET session_id = ?2, updated_at = ?3, attributed_by = ?4
             WHERE file = ?1 AND session_id != ?2",
            params![file, session_id, now_ts(), Attribution::Claimed.as_str()],
        )?;
        if edits == 0 && regions == 0 {
            let touched: i64 = tx.query_row(
                "SELECT COUNT(*) FROM edits WHERE file = ?1",
                params![file],
                |r| r.get(0),
            )?;
            if touched == 0 {
                bail!("no session has touched {}; there is nothing to claim", file);
            }
            bail!(
                "ortak-{} already holds every line and journal row on {}",
                session_id,
                file
            );
        }
        // The ranges arrive as their old owners left them, un-coalesced: they
        // are separate writes and blame naming the same session twice on two of
        // them is the truth about how the file was written.
        tx.commit()?;
        Ok((regions, edits, from))
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
               AND r.cooled = 0
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
    /// `fresh_regions` this ignores presence, session status and cooling: the
    /// point of blame is that a line still belongs to whoever wrote it long
    /// after that session ended, and publishing frees a line rather than
    /// unwriting it.
    pub fn file_regions(&self, file: &str) -> Result<Vec<Owner>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.session_id, s.agent_name, s.task_intent, r.start_line, r.end_line,
                    COALESCE((SELECT MAX(e.ts) FROM edits e
                               WHERE e.session_id = r.session_id AND e.file = r.file), 0),
                    r.attributed_by
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
                // From the region, so it speaks for these lines and no others.
                // Read off the session's newest edit on the file, it marked
                // every line the session owned there, including the ones a
                // hook had named perfectly well.
                attributed_by: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// A session's live regions: the ranges it currently owns, file by file.
    ///
    /// Cooled ones are left out, and that is what scopes the impact scan to one
    /// branch. `publish` scans before it cools, so the lines it is shipping are
    /// still hot and the lines every earlier branch shipped are not.
    pub fn session_regions(&self, session_id: i64) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT file, start_line, end_line FROM regions
             WHERE session_id = ?1 AND cooled = 0
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
             WHERE s.status = 'active' AND r.cooled = 0
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
                "SELECT m.id, m.from_session, s.agent_name, m.to_session, m.text, m.ts
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

    /// Messages still waiting for sessions that have stopped, handed to
    /// `reader` and stamped delivered.
    ///
    /// The last thing a session says is usually its handover, and it says it
    /// when the other session is least likely to be listening. Delivery hangs
    /// off the recipient doing something, so a recipient that has finished
    /// never collects: round 6 ended with the handover sitting in the table.
    /// The next session to start in this workspace is the closest thing that
    /// message has to its reader.
    ///
    /// Only a clean `SessionEnd` marks a session done. One killed mid-turn
    /// leaves its row active and its mail here, where `waiting_messages` still
    /// reports it: guessing that a quiet session is gone would hand its mail to
    /// somebody else while it was still working.
    pub fn take_orphan_messages(&self, reader: i64) -> Result<Vec<Message>> {
        let tx = self.conn.unchecked_transaction()?;
        let stopped = "delivered_at IS NULL AND to_session != ?1
                       AND to_session IN (SELECT id FROM sessions WHERE status = 'done')";
        let out = {
            let mut stmt = tx.prepare(&format!(
                "SELECT m.id, m.from_session, s.agent_name, m.to_session, m.text, m.ts
                 FROM messages m JOIN sessions s ON s.id = m.from_session
                 WHERE {stopped} ORDER BY m.id"
            ))?;
            let rows = stmt.query_map(params![reader], row_to_message)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        tx.execute(
            &format!("UPDATE messages SET delivered_at = ?2 WHERE {stopped}"),
            params![reader, now_ts()],
        )?;
        tx.commit()?;
        Ok(out)
    }

    /// Who has undelivered mail, so a message nobody ever collects is visible
    /// to the person rather than only to the table.
    pub fn waiting_messages(&self) -> Result<Vec<Waiting>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.to_session, s.agent_name, s.status, COUNT(*), MIN(m.ts)
             FROM messages m JOIN sessions s ON s.id = m.to_session
             WHERE m.delivered_at IS NULL
             GROUP BY m.to_session, s.agent_name, s.status
             ORDER BY m.to_session",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Waiting {
                session_id: r.get(0)?,
                agent_name: r.get(1)?,
                stopped: r.get::<_, String>(2)? == "done",
                count: r.get(3)?,
                oldest: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Everything a session has been sent, oldest first, for a person looking.
    pub fn inbox(&self, session_id: i64) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.from_session, s.agent_name, m.to_session, m.text, m.ts
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
        let rows = stmt.query_map(params![limit], row_to_error)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn open_errors(&self) -> Result<Vec<ErrorRow>> {
        self.error_rows(true, 100)
    }

    /// The oldest open error this session must fix and has never been told
    /// about, stamped told on the way out.
    ///
    /// The responsible session is the one party the stop-the-line never
    /// reached: the gate exempts it, because it has to edit to fix the thing,
    /// so it never sees the denial that names it. The stamp is what keeps a
    /// notice from becoming wallpaper on a line that stays stopped for an hour.
    pub fn take_owner_notice(&self, session_id: i64) -> Result<Option<ErrorRow>> {
        let tx = self.conn.unchecked_transaction()?;
        let found = {
            let mut stmt = tx.prepare(
                "SELECT e.id, e.reporter_session, rs.agent_name, e.command, e.output_excerpt,
                        e.status, e.culprit_session, cs.agent_name, e.fix_brief, e.ts_opened
                 FROM errors e
                 JOIN sessions rs ON rs.id = e.reporter_session
                 LEFT JOIN sessions cs ON cs.id = e.culprit_session
                 WHERE e.status = 'open' AND e.owner_told_at IS NULL
                   AND COALESCE(e.culprit_session, e.reporter_session) = ?1
                 ORDER BY e.id LIMIT 1",
            )?;
            stmt.query_row(params![session_id], row_to_error)
                .optional()?
        };
        if let Some(e) = &found {
            tx.execute(
                "UPDATE errors SET owner_told_at = ?2 WHERE id = ?1",
                params![e.id, now_ts()],
            )?;
        }
        tx.commit()?;
        Ok(found)
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
        // The stamp goes back to NULL with the new owner: whoever the error has
        // just landed on has not been told, whatever the last owner heard.
        let n = self.conn.execute(
            "UPDATE errors SET culprit_session = ?2, owner_told_at = NULL
             WHERE id = ?1 AND status = 'open'",
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
        Ok(self.last_heartbeat()?.map(|t| now_ts() - t))
    }

    /// When the daemon last said it was alive. A starting daemon reads this
    /// before writing its own beat, which is the only moment the previous one
    /// is still legible.
    pub fn last_heartbeat(&self) -> Result<Option<i64>> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'daemon_heartbeat'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|s| s.parse::<i64>().ok()))
    }

    /// Keep the newest outage, replacing any older one. A history would answer
    /// a question nobody asks, and the one that gets asked is about the gap
    /// that just happened.
    pub fn record_outage(&self, outage: &Outage) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('last_outage', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            params![serde_json::to_string(outage)?],
        )?;
        Ok(())
    }

    pub fn last_outage(&self) -> Result<Option<Outage>> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_outage'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|s| serde_json::from_str(&s).ok()))
    }
}

fn row_to_error(r: &rusqlite::Row<'_>) -> rusqlite::Result<ErrorRow> {
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
}

fn row_to_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        from_session: r.get(1)?,
        from_name: r.get(2)?,
        to_session: r.get(3)?,
        text: r.get(4)?,
        ts: r.get(5)?,
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
        last_seen: r.get(8)?,
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

    /// Every hook registers before it does anything, so the stamp has to move
    /// on a session that is only reading, and it has to survive the session
    /// ending: what a stopped session last said is the whole point of keeping
    /// it.
    #[test]
    fn a_session_is_stamped_whenever_a_hook_speaks_for_it() {
        let db = temp_db("last-seen");
        let id = db
            .upsert_session("sess-a", "claude-a", "llm", Some("claude-code"))
            .unwrap();
        let first = db.get_session(id).unwrap().last_seen.unwrap();

        // Registration and the second hook land in the same second, so age the
        // row rather than wait for the clock.
        db.conn
            .execute(
                "UPDATE sessions SET last_seen = ?2 WHERE id = ?1",
                params![id, first - 600],
            )
            .unwrap();
        db.upsert_session("sess-a", "claude-a", "llm", Some("claude-code"))
            .unwrap();
        let refreshed = db.get_session(id).unwrap().last_seen.unwrap();
        assert!(refreshed >= first);

        db.end_session("sess-a").unwrap();
        let ended = db.get_session(id).unwrap();
        assert_eq!(ended.status, "done");
        assert_eq!(ended.last_seen, Some(refreshed), "the last word is kept");
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

        // The command ends. Its claim is not gone yet: the daemon has not looked
        // at what the command wrote, and a write it made a moment ago is still
        // its work.
        db.clear_bash_claim(agent).unwrap();
        assert_eq!(
            db.peek_hint("src/x.rs").unwrap(),
            Some((agent, Attribution::Claim))
        );

        // Once the grace is out, the claim speaks for nothing.
        grace_expired(&db, agent);
        assert_eq!(db.peek_hint("src/x.rs").unwrap(), None);
    }

    /// Age a session's closed claim past its grace. Timestamps are whole
    /// seconds, so the alternative is a test that sleeps for three of them.
    fn grace_expired(db: &Db, session_id: i64) {
        db.conn
            .execute(
                "UPDATE hints SET closed_at = ?2
                 WHERE file = ?1 AND session_id = ?3 AND closed_at IS NOT NULL",
                params![BASH_CLAIM, now_ts() - CLAIM_GRACE_SECS - 1, session_id],
            )
            .unwrap();
    }

    /// Two commands open and a file neither of them has ever written. Nobody
    /// can be named, so nobody is.
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
        // daemon cannot tell which of the two did it, so it does not guess. It
        // says that it did not, rather than leaving a row that reads like an
        // edit the human made.
        db.insert_hint(BASH_CLAIM, b, None).unwrap();
        assert_eq!(
            db.peek_hint("src/x.rs").unwrap(),
            Some((db.ensure_human().unwrap(), Attribution::Contested))
        );

        // A hook naming the file is evidence and still outranks both claims.
        db.insert_hint("src/x.rs", b, None).unwrap();
        assert_eq!(
            db.peek_hint("src/x.rs").unwrap(),
            Some((b, Attribution::Hook))
        );
        db.clear_hints("src/x.rs").unwrap();

        // b's command finishes, and for the length of the grace it still counts:
        // the daemon may be about to journal something that command wrote.
        db.clear_bash_claim(b).unwrap();
        assert_eq!(
            db.peek_hint("src/x.rs").unwrap(),
            Some((db.ensure_human().unwrap(), Attribution::Contested))
        );

        // Grace out, and a is an unambiguous guess again.
        grace_expired(&db, b);
        assert_eq!(
            db.peek_hint("src/x.rs").unwrap(),
            Some((a, Attribution::Claim))
        );
    }

    /// The shape that cost both agents work in round 7. A session runs
    /// `cargo fmt` on a file only it has ever touched; the command returns and
    /// `post-bash` closes its claim; a second later the daemon gets round to
    /// journaling the write and finds the only open claim belongs to the other
    /// session, which was running something of its own the whole time.
    #[test]
    fn a_finished_command_still_answers_for_what_it_wrote() {
        let db = temp_db("claim-grace");
        let writer = db
            .upsert_session("sess-a", "claude-a", "llm", Some("claude-code"))
            .unwrap();
        let other = db
            .upsert_session("sess-b", "claude-b", "llm", Some("claude-code"))
            .unwrap();
        db.insert_edit(
            writer,
            "tests/end_to_end.rs",
            "create",
            None,
            &[],
            Some(Attribution::Hook),
        )
        .unwrap();

        db.insert_hint(BASH_CLAIM, writer, None).unwrap();
        db.insert_hint(BASH_CLAIM, other, None).unwrap();
        db.clear_bash_claim(writer).unwrap();
        assert_eq!(
            db.peek_hint("tests/end_to_end.rs").unwrap(),
            Some((writer, Attribution::Claim)),
            "the command that just ended is the one that wrote the file"
        );

        // Long enough after and nothing connects the write to that command any
        // more, so the one open claim answers for it, as it did before any of
        // this. The window is what the daemon waits, not a share of the file.
        grace_expired(&db, writer);
        assert_eq!(
            db.peek_hint("tests/end_to_end.rs").unwrap(),
            Some((other, Attribution::Claim))
        );
    }

    /// Two commands open at once is the resting state of a two-agent
    /// workspace, so a rule that gives up on every write made during one gives
    /// up on most of them. The file itself says who was working in it.
    #[test]
    fn two_commands_at_once_credit_the_session_already_in_the_file() {
        let db = temp_db("two-claims-stake");
        let a = db
            .upsert_session("sess-a", "claude-a", "llm", Some("claude-code"))
            .unwrap();
        let b = db
            .upsert_session("sess-b", "claude-b", "llm", Some("claude-code"))
            .unwrap();
        let quiet = db
            .upsert_session("sess-c", "claude-c", "llm", Some("claude-code"))
            .unwrap();
        db.insert_hint(BASH_CLAIM, a, None).unwrap();
        db.insert_hint(BASH_CLAIM, b, None).unwrap();
        let edited = |s: i64, f: &str| {
            db.insert_edit(s, f, "modify", None, &[], Some(Attribution::Hook))
                .unwrap()
        };

        // a has been editing publish.rs all morning and b has not, so a's
        // heredoc, codegen step or `sed -i` there is a's work.
        edited(a, "src/publish.rs");
        assert_eq!(
            db.peek_hint("src/publish.rs").unwrap(),
            Some((a, Attribution::Claim))
        );

        // b edits it too and is now the session most recently in the file.
        // Both are plausible; the newer write is the better guess, and either
        // beats a session that has never opened it.
        edited(b, "src/publish.rs");
        assert_eq!(
            db.peek_hint("src/publish.rs").unwrap(),
            Some((b, Attribution::Claim))
        );

        // Written only by a session with no command open, so neither claimant
        // has a stake in it and neither may be named: a person editing in their
        // own editor while two agents run commands is a real case, and the
        // honest answer there is the human, whatever the fall-through calls it.
        edited(quiet, "src/notes.rs");
        let ambiguous = db.peek_hint("src/notes.rs").unwrap();
        assert!(
            !matches!(ambiguous, Some((s, _)) if s == a || s == b),
            "a claimant with no stake in the file was named anyway: {ambiguous:?}"
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
        db.insert_edit(
            a,
            "contested.rs",
            "modify",
            None,
            &[],
            Some(Attribution::Contested),
        )
        .unwrap();

        let rows = db.recent_edits(None, 10).unwrap();
        let note = |f: &str| {
            let row = rows.iter().find(|r| r.file == f).unwrap();
            attribution_note(row.attributed_by.as_deref())
        };
        assert_eq!(note("named.rs"), "", "a hook named this file");
        assert_eq!(
            note("guessed.rs"),
            "inferred from a running command",
            "a claim guessed this one"
        );
        // Nothing claimed it, so it went to the human by rule rather than by
        // inference. The agent column already says so; do not mark it twice.
        assert_eq!(note("orphan.rs"), "");
        // This one also went to the human, but because the daemon refused to
        // pick between two claims. That is a decision, and it shows.
        assert_eq!(note("contested.rs"), "two sessions had commands open");
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

    /// What `status` puts under a session: the branches it has published, and
    /// the work none of them carries. Only the newest publish can have any, so
    /// counting from an older one reports work that shipped on a later branch
    /// as though it were still sitting in the workspace.
    #[test]
    fn only_the_newest_branch_has_work_waiting_behind_it() {
        let db = temp_db("branches");
        let s = db
            .upsert_session("sess-a", "claude-sess", "llm", Some("claude-code"))
            .unwrap();
        let head = || db.max_edit_id(s).unwrap().unwrap();

        db.insert_edit(s, "one.rs", "create", None, &[], None)
            .unwrap();
        db.record_publish(s, "task/one", head()).unwrap();
        db.insert_edit(s, "two.rs", "create", None, &[], None)
            .unwrap();
        db.record_publish(s, "task/two", head()).unwrap();

        let waiting = || -> Vec<i64> {
            db.published_branches(s)
                .unwrap()
                .iter()
                .map(|(_, behind)| *behind)
                .collect()
        };
        let published = db.published_branches(s).unwrap();
        assert_eq!(
            published.iter().map(|(p, _)| &p.branch).collect::<Vec<_>>(),
            vec!["task/two", "task/one"],
            "newest first, the way status reads them out"
        );
        assert!(published[0].0.ts > 0, "status has to say when it went out");
        assert_eq!(
            waiting(),
            vec![0, 0],
            "one.rs shipped on task/one and two.rs on task/two; nothing is waiting"
        );

        // A fix arrives after both, so the newest branch is behind by one and
        // the older one is still not the place to look for it.
        db.insert_edit(s, "two.rs", "modify", None, &[], None)
            .unwrap();
        assert_eq!(waiting(), vec![1, 0]);
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

    /// How round 6 ended: nine messages delivered and the tenth, the handover,
    /// addressed to a session that had finished.
    #[test]
    fn a_message_to_a_session_that_stopped_reaches_the_next_one() {
        let db = temp_db("messages-orphan");
        let a = agent(&db, "sess-a");
        let gone = agent(&db, "sess-gone");
        let live = agent(&db, "sess-live");
        db.send_message(a, gone, "round done my end: 51 stale-branch warning")
            .unwrap();
        db.send_message(a, live, "publish.rs is mid-refactor")
            .unwrap();
        db.end_session("sess-gone").unwrap();

        let next = agent(&db, "sess-next");
        assert_eq!(
            texts(db.take_orphan_messages(next).unwrap()),
            vec!["round done my end: 51 stale-branch warning"]
        );
        // Once. The session after this one inherits nothing.
        assert!(db.take_orphan_messages(next).unwrap().is_empty());
        // And a live session's mail is nobody else's to read.
        assert_eq!(
            texts(db.take_messages(live).unwrap()),
            vec!["publish.rs is mid-refactor"]
        );
    }

    /// A session killed mid-turn never fires SessionEnd, so its row stays
    /// active and its mail stays addressed to it. Handing that to somebody else
    /// would take it from a session that is still working, so status reports it
    /// instead.
    #[test]
    fn mail_for_a_session_that_never_stopped_is_reported_not_reassigned() {
        let db = temp_db("messages-waiting");
        let a = agent(&db, "sess-a");
        let killed = agent(&db, "sess-killed");
        db.send_message(a, killed, "the gate denied me on src/db.rs")
            .unwrap();

        let next = agent(&db, "sess-next");
        assert!(db.take_orphan_messages(next).unwrap().is_empty());

        let waiting = db.waiting_messages().unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].session_id, killed);
        assert_eq!(waiting[0].count, 1);
        assert!(!waiting[0].stopped);
        // Delivered mail is not waiting mail.
        db.take_messages(killed).unwrap();
        assert!(db.waiting_messages().unwrap().is_empty());
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
    fn wrote(db: &Db, who: i64, file: &str, start: i64, how: Option<Attribution>) {
        let hunks = [Hunk {
            old_start: start,
            old_lines: 6,
            new_start: start,
            new_lines: 6,
        }];
        db.insert_edit(who, file, "modify", None, &hunks, how)
            .unwrap();
        db.apply_edit_regions(who, file, &hunks, how).unwrap();
    }

    /// Round 6 from both ends: a session's own file took a write the daemon
    /// credited to whichever other session had a command open, and the file
    /// went with it.
    #[test]
    fn a_claim_takes_back_a_write_the_journal_gave_away() {
        let db = db();
        let author = session(&db, "claude-a");
        let bystander = session(&db, "claude-b");
        wrote(
            &db,
            author,
            "tests/end_to_end.rs",
            1,
            Some(Attribution::Hook),
        );
        // `cargo fmt -- tests/end_to_end.rs`, on a file only the author had
        // ever touched, landing on the session that was running something.
        wrote(
            &db,
            bystander,
            "tests/end_to_end.rs",
            20,
            Some(Attribution::Claim),
        );

        let moved = db.claim_file(author, "tests/end_to_end.rs").unwrap();
        assert_eq!(moved, (1, 1, vec![bystander]));

        // What publish reads, from both sides.
        assert_eq!(
            names(db.session_files(author, 0).unwrap()),
            vec!["tests/end_to_end.rs"]
        );
        assert!(db.session_files(bystander, 0).unwrap().is_empty());
        // And what a person reads: the file is the author's, and the journal
        // says which range was repaired rather than passing it off as a write.
        let owners = db.file_regions("tests/end_to_end.rs").unwrap();
        assert!(owners.iter().all(|o| o.session_id == author));
        assert!(owners
            .iter()
            .any(|o| { attribution_note(o.attributed_by.as_deref()) == "claimed after the fact" }));
        assert!(owners
            .iter()
            .any(|o| { attribution_note(o.attributed_by.as_deref()).is_empty() }));
        assert!(db
            .recent_edits(Some(author), 20)
            .unwrap()
            .iter()
            .any(|e| attribution_note(e.attributed_by.as_deref()) == "claimed after the fact"));
    }

    /// The other half of the round-6 repair, and the reason this stacks on
    /// release meaning "never mine": a released row is invisible to every
    /// session until something takes it back.
    #[test]
    fn a_file_the_wrong_owner_released_comes_back_whole() {
        let db = db();
        let author = session(&db, "claude-a");
        let wrong = session(&db, "claude-b");
        wrote(&db, wrong, "src/publish.rs", 40, Some(Attribution::Claim));
        assert_eq!(db.disown(wrong, Some("src/publish.rs")).unwrap(), (1, 1));

        db.claim_file(author, "src/publish.rs").unwrap();

        assert_eq!(
            names(db.session_files(author, 0).unwrap()),
            vec!["src/publish.rs"]
        );
        assert_eq!(db.edit_count(author).unwrap(), 1);
        assert_eq!(db.edit_count(wrong).unwrap(), 0);
    }

    #[test]
    fn claiming_an_untouched_file_or_one_you_hold_is_refused() {
        let db = db();
        let author = session(&db, "claude-a");
        assert!(db.claim_file(author, "src/db.rs").is_err());

        wrote(&db, author, "src/db.rs", 1, Some(Attribution::Hook));
        assert!(db.claim_file(author, "src/db.rs").is_err());
        // Refused, not half-done: the row it already held is untouched.
        assert_ne!(
            attribution_note(
                db.recent_edits(Some(author), 5).unwrap()[0]
                    .attributed_by
                    .as_deref()
            ),
            "claimed after the fact"
        );
    }

    fn owns(db: &Db, session_id: i64, file: &str, start: i64, lines: i64) {
        let hunks = [Hunk {
            old_start: start,
            old_lines: lines,
            new_start: start,
            new_lines: lines,
        }];
        db.insert_edit(session_id, file, "modify", None, &hunks, None)
            .unwrap();
        db.apply_edit_regions(session_id, file, &hunks, None)
            .unwrap();
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

    #[test]
    fn blame_says_which_owner_was_only_inferred() {
        let db = db();
        let guessed = session(&db, "claude-a");
        let reported = session(&db, "claude-b");
        let wrote = |who: i64, start: i64, how: Option<Attribution>| {
            let hunks = [Hunk {
                old_start: start,
                old_lines: 5,
                new_start: start,
                new_lines: 5,
            }];
            db.insert_edit(who, "src/impact.rs", "modify", None, &hunks, how)
                .unwrap();
            db.apply_edit_regions(who, "src/impact.rs", &hunks, how)
                .unwrap();
        };
        wrote(guessed, 1, Some(Attribution::Claim));
        wrote(reported, 40, Some(Attribution::Hook));

        let owner_of = |sid: i64| {
            db.file_regions("src/impact.rs")
                .unwrap()
                .into_iter()
                .find(|o| o.session_id == sid)
                .expect("owner")
        };
        assert_eq!(
            attribution_note(owner_of(guessed).attributed_by.as_deref()),
            "inferred from a running command",
            "nobody reported this one"
        );
        assert_eq!(
            attribution_note(owner_of(reported).attributed_by.as_deref()),
            ""
        );

        // Editing it again with a hook behind the change settles the question,
        // and the mark goes with the row the printed age belongs to.
        wrote(guessed, 1, Some(Attribution::Hook));
        assert_eq!(
            attribution_note(owner_of(guessed).attributed_by.as_deref()),
            ""
        );
    }

    #[test]
    fn only_the_newest_outage_is_kept() {
        let db = db();
        assert!(db.last_outage().unwrap().is_none(), "nothing has stopped");

        db.record_outage(&Outage {
            start: 100,
            end: 190,
            journaled: 3,
        })
        .unwrap();
        db.record_outage(&Outage {
            start: 400,
            end: 402,
            journaled: 0,
        })
        .unwrap();

        let o = db.last_outage().unwrap().expect("an outage");
        assert_eq!((o.start, o.end, o.journaled), (400, 402, 0));
        assert_eq!(o.secs(), 2);
        assert!(o.recent(o.end + 60), "this is the one being asked about");
        assert!(!o.recent(o.end + OUTAGE_RECENT_SECS + 1));
    }

    /// B measured this in round 8 and A wrote it up: a published file that the
    /// session comes back to had every one of its lines put back on the gate,
    /// not just the ones edited, so the scan that reads `cooled = 0` to find out
    /// what a branch is shipping was told the whole file.
    #[test]
    fn editing_a_published_file_re_hots_only_the_lines_edited() {
        let db = db();
        let me = session(&db, "claude-a");
        // Two separate stretches of one file, both shipped.
        owns(&db, me, "publish.rs", 10, 5);
        owns(&db, me, "publish.rs", 80, 5);
        assert_eq!(db.cool_regions(me, "publish.rs").unwrap(), 2);

        // Back for one of them, in a different part of the file.
        owns(&db, me, "publish.rs", 80, 5);

        let state = |cooled: i64| -> Vec<(i64, i64)> {
            let mut stmt = db
                .conn
                .prepare(
                    "SELECT start_line, end_line FROM regions
                      WHERE session_id = ?1 AND file = ?2 AND cooled = ?3
                      ORDER BY start_line",
                )
                .unwrap();
            let rows = stmt
                .query_map(params![me, "publish.rs", cooled], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(
            state(0),
            vec![(80, 84)],
            "only what was written again is hot"
        );
        assert_eq!(state(1), vec![(10, 14)], "the rest is still shipped");

        // And the gate agrees: the cooled stretch is still free.
        let onlooker = session(&db, "claude-b");
        assert!(db
            .conflicts(
                "publish.rs",
                &[Region { start: 12, end: 12 }],
                onlooker,
                3,
                1800
            )
            .unwrap()
            .is_empty());
        assert_eq!(
            db.conflicts(
                "publish.rs",
                &[Region { start: 82, end: 82 }],
                onlooker,
                3,
                1800
            )
            .unwrap()
            .len(),
            1,
            "and defends the lines the session came back to"
        );
    }

    /// After a publish, the three answers must agree: the gate lets the lines
    /// go, blame still names who wrote them, and the branch is not rebuilt from
    /// them a second time.
    #[test]
    fn a_published_file_stays_answerable_after_its_lines_go_free() {
        let db = db();
        let author = session(&db, "claude-a");
        let onlooker = session(&db, "claude-b");
        owns(&db, author, "src/publish.rs", 10, 5);
        let hot = || {
            db.conflicts(
                "src/publish.rs",
                &[Region { start: 12, end: 12 }],
                onlooker,
                3,
                1800,
            )
            .unwrap()
        };
        assert_eq!(hot().len(), 1);

        let head = db.max_edit_id(author).unwrap().unwrap();
        assert_eq!(db.cool_regions(author, "src/publish.rs").unwrap(), 1);

        assert!(hot().is_empty(), "published lines are free");
        assert!(
            db.fresh_regions(1800).unwrap().is_empty(),
            "and the gate stops advertising them"
        );
        let owners = db.file_regions("src/publish.rs").unwrap();
        assert_eq!(owners.len(), 1, "blame still answers for them");
        assert_eq!(owners[0].session_id, author);
        assert_eq!(
            db.session_files(author, 0).unwrap().len(),
            1,
            "and the edit is still the session's work"
        );
        assert!(
            db.session_files(author, head).unwrap().is_empty(),
            "the next publish does not ship it again"
        );

        // Cooling twice, as a second publish of the same file would, is not a
        // second freeing.
        assert_eq!(db.cool_regions(author, "src/publish.rs").unwrap(), 0);

        // A release is the other case and stays the other case: the session
        // says the lines were never its work, so blame goes quiet too.
        assert_eq!(
            db.release_regions(author, Some("src/publish.rs")).unwrap(),
            1
        );
        assert!(db.file_regions("src/publish.rs").unwrap().is_empty());
    }

    /// Blame answers for a line, so it has to carry the same warning `log` does
    /// about how its answer was arrived at.
    #[test]
    fn blame_carries_how_the_owner_was_worked_out() {
        let db = db();
        let human = db.ensure_human().unwrap();
        let agent = session(&db, "claude-a");
        let wrote = |who: i64, start: i64, how: Option<Attribution>| {
            let hunks = [Hunk {
                old_start: start,
                old_lines: 5,
                new_start: start,
                new_lines: 5,
            }];
            db.insert_edit(who, "src/impact.rs", "modify", None, &hunks, how)
                .unwrap();
            db.apply_edit_regions(who, "src/impact.rs", &hunks, how)
                .unwrap();
        };
        wrote(agent, 1, Some(Attribution::Hook));
        wrote(human, 40, Some(Attribution::Contested));

        let note_for = |sid: i64| {
            let owners = db.file_regions("src/impact.rs").unwrap();
            let o = owners.iter().find(|o| o.session_id == sid).expect("owner");
            attribution_note(o.attributed_by.as_deref())
        };
        assert_eq!(note_for(agent), "", "a hook reported this one");
        assert_eq!(note_for(human), "two sessions had commands open");
    }

    /// The owner of a stopped line hears about it once. Once, because the gate
    /// exempts that session and a notice before every edit for as long as the
    /// line is down stops being read; and again on reassignment, because the
    /// session the error has just landed on has heard nothing.
    #[test]
    fn the_line_reaches_its_owner_once_per_assignment() {
        let db = temp_db("owner-notice");
        let reporter = db.upsert_session("a", "claude-a", "llm", None).unwrap();
        let culprit = db.upsert_session("b", "claude-b", "llm", None).unwrap();
        let id = db
            .insert_error(
                reporter,
                Some("cargo test"),
                "no method named `inferred` found for struct `EditRow`",
                Some(culprit),
                None,
            )
            .unwrap();

        // The reporter is not the owner: it is already being told by the gate.
        assert!(db.take_owner_notice(reporter).unwrap().is_none());

        let told = db.take_owner_notice(culprit).unwrap().expect("owner hears");
        assert_eq!(told.id, id);
        assert!(
            db.take_owner_notice(culprit).unwrap().is_none(),
            "and hears it once, not before every edit"
        );

        db.assign_error(id, reporter).unwrap();
        assert_eq!(
            db.take_owner_notice(reporter).unwrap().map(|e| e.id),
            Some(id),
            "a reassignment is news to whoever it lands on"
        );
        assert!(
            db.take_owner_notice(culprit).unwrap().is_none(),
            "and the session it left owes nothing"
        );

        // An unassigned error is the reporter's own, and it must hear that too.
        let ambiguous = db
            .insert_error(reporter, None, "cannot find -lsqlite3", None, None)
            .unwrap();
        assert_eq!(
            db.take_owner_notice(reporter).unwrap().map(|e| e.id),
            Some(ambiguous)
        );

        // A resolved line is nobody's news, told or not.
        let unread = db
            .insert_error(reporter, None, "linker crashed", Some(culprit), None)
            .unwrap();
        assert_eq!(db.resolve_errors(None).unwrap(), 3);
        assert!(db.take_owner_notice(culprit).unwrap().is_none());
        assert!(db.open_errors().unwrap().is_empty(), "#{} closed", unread);
    }

    /// One session, one file, two edits: an ordinary one and one the daemon
    /// refused to attribute. The marker came from the session's newest edit on
    /// the file, so both lines wore it, and a marker that fires on innocent
    /// lines is one nobody reads by the third time they see it.
    #[test]
    fn only_the_contested_lines_carry_the_marker() {
        let db = db();
        let me = session(&db, "claude-a");
        let wrote = |start: i64, lines: i64, how: Option<Attribution>| {
            let hunks = [Hunk {
                old_start: start,
                old_lines: lines,
                new_start: start,
                new_lines: lines,
            }];
            db.insert_edit(me, "src/app.rs", "modify", None, &hunks, how)
                .unwrap();
            db.apply_edit_regions(me, "src/app.rs", &hunks, how)
                .unwrap();
        };
        let marker_at = |line: i64| {
            let owners = db.file_regions("src/app.rs").unwrap();
            let o = owners
                .iter()
                .find(|o| o.start <= line && line <= o.end)
                .expect("a line this session wrote");
            attribution_note(o.attributed_by.as_deref())
        };

        wrote(1, 10, Some(Attribution::Hook));
        wrote(40, 2, Some(Attribution::Contested));
        assert_eq!(marker_at(5), "", "an ordinary line of the same file");
        assert_eq!(marker_at(40), "two sessions had commands open");

        // A contested write inside lines the session already owned takes them
        // off the older claim. Left there, the one line that really was
        // contested would be the one line reading as ordinary.
        wrote(5, 1, Some(Attribution::Contested));
        assert_eq!(marker_at(5), "two sessions had commands open");
        assert_eq!(marker_at(4), "");
        assert_eq!(marker_at(6), "");
    }
}
