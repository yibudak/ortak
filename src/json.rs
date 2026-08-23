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
    pub sessions: Vec<Session>,
    pub regions: Vec<Region>,
}

#[derive(Serialize)]
pub struct Daemon {
    pub running: bool,
    /// Seconds since the last heartbeat, null if the daemon never started.
    pub heartbeat_age_secs: Option<i64>,
}

#[derive(Serialize)]
pub struct Session {
    /// The reference every other command takes, for example "ortak-3".
    pub session: String,
    pub agent: String,
    pub harness: Option<String>,
    pub intent: Option<String>,
    pub status: String,
    pub edits: i64,
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

pub fn status(db: &Db, presence_secs: i64) -> Result<Status> {
    let age = db.heartbeat_age()?;
    let mut sessions = Vec::new();
    for s in db.list_sessions()? {
        sessions.push(Session {
            session: format!("ortak-{}", s.id),
            agent: s.agent_name,
            harness: s.harness,
            intent: s.task_intent,
            status: s.status,
            edits: db.edit_count(s.id)?,
        });
    }
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
    Ok(Status {
        daemon: Daemon {
            running: age.is_some_and(|a| a <= crate::db::HEARTBEAT_ALIVE_SECS),
            heartbeat_age_secs: age,
        },
        sessions,
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
            sessions: vec![Session {
                session: "ortak-2".into(),
                agent: "claude-be11".into(),
                harness: Some("claude-code".into()),
                intent: Some("wire up the gate".into()),
                status: "active".into(),
                edits: 7,
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
            r#"{"daemon":{"running":true,"heartbeat_age_secs":3},"sessions":[{"session":"ortak-2","agent":"claude-be11","harness":"claude-code","intent":"wire up the gate","status":"active","edits":7}],"regions":[{"file":"src/db.rs","start":12,"end":40,"whole_file":false,"owner":"ortak-2","agent":"claude-be11","age_secs":90}]}"#
        );
    }
}
