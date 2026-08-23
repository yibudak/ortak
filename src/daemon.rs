use crate::config::Config;
use crate::db::Db;
use crate::shadow::{self, Change};
use crate::workspace::Workspace;
use anyhow::Result;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Wait this long after the last fs event on a path before journaling it.
/// Also gives the harness hook time to land its attribution hint.
const DEBOUNCE: Duration = Duration::from_millis(400);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

pub fn run(ws: &Workspace, _cfg: &Config) -> Result<()> {
    let db = Db::open(&ws.db_path)?;
    let repo = shadow::open(ws)?;
    let human_id = db.ensure_human()?;

    let (tx, rx) = mpsc::channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(ev) = res {
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            for p in ev.paths {
                let _ = tx.send(p);
            }
        }
    })?;
    watcher.watch(&ws.root, RecursiveMode::Recursive)?;

    log(&format!("daemon started: {}", ws.root.display()));
    db.heartbeat()?;
    startup_scan(&db, &repo, ws, human_id);

    let mut pending: HashMap<String, Instant> = HashMap::new();
    let mut last_beat = Instant::now();

    loop {
        if last_beat.elapsed() >= HEARTBEAT_EVERY {
            db.heartbeat()?;
            last_beat = Instant::now();
        }

        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(abs) => {
                if let Some(rel) = filter(ws, &abs) {
                    pending.insert(rel, Instant::now());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        let due: Vec<String> = pending
            .iter()
            .filter(|(_, t)| t.elapsed() >= DEBOUNCE)
            .map(|(p, _)| p.clone())
            .collect();
        for rel in due {
            pending.remove(&rel);
            if let Err(e) = process(&db, &repo, ws, human_id, &rel) {
                log(&format!("ERROR {}: {}", rel, e));
            }
        }
    }
    Ok(())
}

/// Cheap pre-filter before ignore rules: workspace-relative path, skipping
/// the metadata directories that would otherwise feed back into the watcher.
fn filter(ws: &Workspace, abs: &Path) -> Option<String> {
    let rel = ws.relativize(abs)?;
    let first = rel.split('/').next().unwrap_or("");
    if first == ".ortak" || first == ".git" {
        return None;
    }
    Some(rel)
}

/// Journal one path change. Returns whether anything was recorded.
fn process(
    db: &Db,
    repo: &git2::Repository,
    ws: &Workspace,
    human_id: i64,
    rel: &str,
) -> Result<bool> {
    if repo.is_path_ignored(rel).unwrap_or(false) {
        return Ok(false);
    }
    let abs = ws.root.join(rel);
    if abs.is_dir() {
        return Ok(false);
    }
    let change = shadow::classify(repo, &ws.root, rel)?;
    if change == Change::None {
        return Ok(false);
    }
    // Hunks must be computed against shadow HEAD *before* the micro-commit.
    let old_blob = shadow::head_blob(repo, rel);
    let new_data = if change == Change::Delete {
        None
    } else {
        Some(std::fs::read(&abs)?)
    };
    let hunks = shadow::compute_hunks(old_blob.as_ref(), new_data.as_deref())?;
    drop(old_blob);
    let session_id = db.peek_hint(rel)?.unwrap_or(human_id);
    let session = db.get_session(session_id)?;
    let oid = shadow::commit_edit(repo, rel, change, &session.agent_name, &session.external_id)?;
    db.insert_edit(session_id, rel, change.as_str(), Some(&oid), &hunks)?;
    db.apply_edit_regions(session_id, rel, &hunks)?;
    // Only now. Everything above can fail, and a hint consumed ahead of its
    // commit is not there for the retry: the change lands on the human and the
    // regions of every session working in this file stop being shifted.
    db.clear_hints(rel)?;
    log(&format!(
        "{} {} - {} (ortak-{})",
        change.as_str(),
        rel,
        session.agent_name,
        session.id
    ));
    Ok(true)
}

/// Catch up on changes made while the daemon was down. Attribution hints are
/// long stale by now, so everything found here lands on the human session.
fn startup_scan(db: &Db, repo: &git2::Repository, ws: &Workspace, human_id: i64) {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = match repo.statuses(Some(&mut opts)) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("startup scan failed: {}", e));
            return;
        }
    };
    let mut journaled = 0u32;
    for entry in statuses.iter() {
        let Some(rel) = entry.path() else { continue };
        match process(db, repo, ws, human_id, rel) {
            Ok(true) => journaled += 1,
            Ok(false) => {}
            Err(e) => log(&format!("startup scan failed for {}: {}", rel, e)),
        }
    }
    if journaled > 0 {
        log(&format!(
            "startup scan journaled {} changes made while the daemon was stopped (human)",
            journaled
        ));
    }
}

fn log(msg: &str) {
    println!("[{}] {}", chrono::Local::now().format("%H:%M:%S"), msg);
}
