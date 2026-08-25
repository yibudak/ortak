//! Machine-readable output for the read commands.
//!
//! Agents are the main callers of `status`, `log` and `errors`, and they parse
//! whatever comes out. Prose is not a contract and reword it and every caller
//! breaks, so the shapes below are the contract instead. They are declared here
//! rather than derived on the database rows, so a column can move without the
//! wire format moving with it.

use crate::db::{Db, EditRow, ErrorRow};
use crate::regions::WHOLE_FILE;
use anyhow::Result;
use serde::Serialize;

#[derive(Serialize)]
pub struct Status {
    pub daemon: Daemon,
    pub journal: Journal,
    /// Null when the count cannot be taken: no git repository at the workspace
    /// root, or the configured base branch is not in this checkout.
    pub workspace: Option<Checkout>,
    pub sessions: Vec<Session>,
    /// Mail nobody has collected, in the same order the human output prints it.
    /// Empty while every message has reached its reader, which is the normal
    /// case.
    pub waiting_messages: Vec<WaitingMail>,
    pub regions: Vec<Region>,
}

#[derive(Serialize)]
pub struct Checkout {
    pub base_branch: String,
    /// Commits on the base branch this checkout does not have, and 0 when it is
    /// current. A publish replays onto the base branch, so anything above 0 is
    /// work the session cannot see in the files it is editing.
    pub commits_behind: i64,
}

#[derive(Serialize)]
pub struct Daemon {
    pub running: bool,
    /// Seconds since the last heartbeat, null if the daemon never started.
    pub heartbeat_age_secs: Option<i64>,
}

/// Whether the daemon is managing to record what it sees. A healthy heartbeat
/// and a failing journal look the same from `daemon` alone, so a session whose
/// edits are not landing cannot tell from there.
#[derive(Serialize)]
pub struct Journal {
    /// Files the daemon is failing on right now.
    pub failing_files: usize,
    /// The newest failure, null while the journal is healthy.
    pub newest_failure: Option<Failure>,
}

#[derive(Serialize)]
pub struct Failure {
    pub file: String,
    pub reason: String,
    /// Consecutive failed attempts on this file.
    pub streak: i64,
    pub ts: i64,
}

#[derive(Serialize)]
pub struct Session {
    /// The reference every other command takes, for example "ortak-3".
    pub session: String,
    pub agent: String,
    pub harness: Option<String>,
    pub intent: Option<String>,
    pub status: String,
    /// When a hook last spoke for this session, unix seconds. `status` is what
    /// the session last said about itself; this is when it last said anything,
    /// and a session that was killed reads active here for good. Null on a row
    /// written before the stamp existed.
    pub last_seen: Option<i64>,
    pub edits: i64,
    /// What this session has published, newest first. Empty until it publishes.
    pub branches: Vec<Branch>,
}

#[derive(Serialize)]
pub struct Branch {
    pub branch: String,
    /// When the publish went out, unix seconds.
    pub published_at: i64,
    /// Edits this session has made that none of its branches carries yet. Only
    /// the newest publish can have any: work after an older one went into the
    /// branches published since.
    pub edits_since: i64,
}

/// One session's undelivered mail. A queue nobody drains is the failure this
/// reports.
#[derive(Serialize)]
pub struct WaitingMail {
    pub session: String,
    pub agent: String,
    /// The recipient ended cleanly, so this mail goes to the next session that
    /// starts here. False for a working session and for one that stopped
    /// without saying so, whose mail is waiting for a reader who is not coming.
    pub stopped: bool,
    pub count: i64,
    /// When the oldest undelivered message was sent, unix seconds.
    pub oldest_ts: i64,
}

#[derive(Serialize)]
pub struct Region {
    pub file: String,
    pub start: i64,
    pub end: i64,
    /// The gate holds the entire file, and `end` is a sentinel.
    pub whole_file: bool,
    pub owner: String,
    pub agent: String,
    /// Seconds since the owner last edited this file.
    pub age_secs: i64,
}

#[derive(Serialize)]
pub struct Edit {
    pub session: String,
    pub agent: String,
    pub file: String,
    pub change: String,
    pub ts: i64,
    /// Where the owner came from: "hook" when the session reported the file
    /// itself, "claim" when the daemon inferred it from a command that session
    /// was running, "settled" when nothing claimed it and the same session had
    /// written the file seconds earlier, null when nothing claimed it and the
    /// change fell to the human session. A reader deciding whether to trust
    /// `session` needs this.
    pub attribution: Option<String>,
}

#[derive(Serialize)]
pub struct Error {
    pub id: i64,
    pub status: String,
    pub reporter: String,
    /// Whoever must act: the assigned session, else the reporter.
    pub responsible: String,
    pub excerpt: String,
    pub opened_at: i64,
}

/// Emit one payload. Every JSON command goes through here so they all look
/// alike.
pub fn print<T: Serialize>(payload: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(payload)?);
    Ok(())
}

pub fn status(db: &Db, presence_secs: i64, behind_base: Option<(String, i64)>) -> Result<Status> {
    let age = db.heartbeat_age()?;
    let mut sessions = Vec::new();
    for s in db.list_sessions()? {
        let branches = db
            .published_branches(s.id)?
            .into_iter()
            .map(|(p, behind)| Branch {
                branch: p.branch,
                published_at: p.ts,
                edits_since: behind,
            })
            .collect();
        sessions.push(Session {
            session: format!("ortak-{}", s.id),
            agent: s.agent_name,
            harness: s.harness,
            intent: s.task_intent,
            status: s.status,
            last_seen: s.last_seen,
            edits: db.edit_count(s.id)?,
            branches,
        });
    }
    let waiting_messages = db
        .waiting_messages()?
        .into_iter()
        .map(|w| WaitingMail {
            session: format!("ortak-{}", w.session_id),
            agent: w.agent_name,
            stopped: w.stopped,
            count: w.count,
            oldest_ts: w.oldest,
        })
        .collect();
    let now = crate::db::now_ts();
    let regions = db
        .fresh_regions(presence_secs)?
        .into_iter()
        .map(|(file, start, end, agent, sid, last_ts)| Region {
            file,
            start,
            end,
            whole_file: end >= WHOLE_FILE,
            owner: format!("ortak-{}", sid),
            agent,
            age_secs: (now - last_ts).max(0),
        })
        .collect();
    let failing = db.journal_failures()?;
    Ok(Status {
        daemon: Daemon {
            running: age.is_some_and(|a| a <= crate::db::HEARTBEAT_ALIVE_SECS),
            heartbeat_age_secs: age,
        },
        journal: Journal {
            failing_files: failing.len(),
            newest_failure: failing.first().map(|f| Failure {
                file: f.file.clone(),
                reason: f.reason.clone(),
                streak: f.streak,
                ts: f.ts,
            }),
        },
        workspace: behind_base.map(|(base_branch, commits_behind)| Checkout {
            base_branch,
            commits_behind,
        }),
        sessions,
        waiting_messages,
        regions,
    })
}

pub fn edits(rows: Vec<EditRow>) -> Vec<Edit> {
    rows.into_iter()
        .map(|e| Edit {
            session: format!("ortak-{}", e.session_id),
            agent: e.agent_name,
            file: e.file,
            change: e.change_kind,
            ts: e.ts,
            attribution: e.attributed_by,
        })
        .collect()
}

pub fn errors(rows: &[ErrorRow]) -> Vec<Error> {
    rows.iter()
        .map(|e| Error {
            id: e.id,
            status: e.status.clone(),
            reporter: format!("ortak-{}", e.reporter),
            responsible: format!("ortak-{}", e.responsible()),
            excerpt: e.excerpt.clone(),
            opened_at: e.ts_opened,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_payload_keeps_its_shape() {
        let payload = Status {
            daemon: Daemon {
                running: true,
                heartbeat_age_secs: Some(3),
            },
            journal: Journal {
                failing_files: 1,
                newest_failure: Some(Failure {
                    file: "src/db.rs".into(),
                    reason: "the index is locked".into(),
                    streak: 4,
                    ts: 1_700_000_000,
                }),
            },
            workspace: Some(Checkout {
                base_branch: "main".into(),
                commits_behind: 83,
            }),
            sessions: vec![Session {
                session: "ortak-2".into(),
                agent: "claude-be11".into(),
                harness: Some("claude-code".into()),
                intent: Some("wire up the gate".into()),
                status: "active".into(),
                last_seen: Some(1_700_000_000),
                edits: 7,
                branches: vec![Branch {
                    branch: "feat/the-gate".into(),
                    published_at: 1_700_000_000,
                    edits_since: 2,
                }],
            }],
            waiting_messages: vec![WaitingMail {
                session: "ortak-3".into(),
                agent: "claude-75c6".into(),
                stopped: false,
                count: 2,
                oldest_ts: 1_700_000_000,
            }],
            regions: vec![Region {
                file: "src/db.rs".into(),
                start: 12,
                end: 40,
                whole_file: false,
                owner: "ortak-2".into(),
                agent: "claude-be11".into(),
                age_secs: 90,
            }],
        };
        assert_eq!(
            serde_json::to_string(&payload).unwrap(),
            r#"{"daemon":{"running":true,"heartbeat_age_secs":3},"journal":{"failing_files":1,"newest_failure":{"file":"src/db.rs","reason":"the index is locked","streak":4,"ts":1700000000}},"workspace":{"base_branch":"main","commits_behind":83},"sessions":[{"session":"ortak-2","agent":"claude-be11","harness":"claude-code","intent":"wire up the gate","status":"active","last_seen":1700000000,"edits":7,"branches":[{"branch":"feat/the-gate","published_at":1700000000,"edits_since":2}]}],"waiting_messages":[{"session":"ortak-3","agent":"claude-75c6","stopped":false,"count":2,"oldest_ts":1700000000}],"regions":[{"file":"src/db.rs","start":12,"end":40,"whole_file":false,"owner":"ortak-2","agent":"claude-be11","age_secs":90}]}"#
        );
    }

    /// The count comes off the same query the printed line uses, so the two
    /// cannot drift. An empty list is the answer for a workspace where every
    /// message arrived, and it is a different fact from the field being absent.
    #[test]
    fn status_carries_the_mail_the_printed_line_carries() {
        let path =
            std::env::temp_dir().join(format!("ortak-json-mail-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        assert!(status(&db, 600, None).unwrap().waiting_messages.is_empty());

        let from = db.upsert_session("a", "claude-a", "llm", None).unwrap();
        let to = db.upsert_session("b", "claude-b", "llm", None).unwrap();
        db.send_message(from, to, "db.rs is mid-refactor").unwrap();
        db.send_message(from, to, "and now it is not").unwrap();

        let waiting = status(&db, 600, None).unwrap().waiting_messages;
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].session, format!("ortak-{}", to));
        assert_eq!(waiting[0].agent, "claude-b");
        assert_eq!(waiting[0].count, 2);
        assert!(!waiting[0].stopped);

        db.take_messages(to).unwrap();
        assert!(status(&db, 600, None).unwrap().waiting_messages.is_empty());
    }

    #[test]
    fn an_edit_payload_says_where_its_owner_came_from() {
        let row = |attributed_by: Option<&str>| EditRow {
            id: 1,
            session_id: 2,
            agent_name: "claude-be11".into(),
            file: "src/db.rs".into(),
            change_kind: "modify".into(),
            shadow_commit: None,
            ts: 1_700_000_000,
            attributed_by: attributed_by.map(str::to_string),
        };
        let of = |a| edits(vec![row(a)]).remove(0).attribution;
        assert_eq!(of(Some("claim")).as_deref(), Some("claim"));
        assert_eq!(of(Some("hook")).as_deref(), Some("hook"));
        // Nothing claimed the file and the change fell to the human session,
        // which is a different fact from a session having reported it.
        assert_eq!(of(None), None);
    }
}
