use crate::config::Config;
use crate::db::{Attribution, Db};
use crate::shadow::{self, Change};
use crate::workspace::Workspace;
use anyhow::{bail, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long a path must sit quiet before a change no hook claimed is journaled
/// against the human. Snapshots do not wait: they go in the moment a hook lands
/// one. The gap is generous on purpose, so a slow hook is not overtaken and its
/// session's work credited to somebody else.
const UNATTRIBUTED_QUIET: Duration = Duration::from_millis(1500);
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
            // The watcher thread is gone, so nothing will be journaled again.
            // Returning Ok here made that look like a clean shutdown, and the
            // only symptom was a heartbeat that stopped advancing.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("the filesystem watcher stopped; no further changes will be journaled")
            }
        }

        // Attributed work goes in as soon as its hook lands, one micro-commit
        // per snapshot, so two sessions writing one file inside one window stay
        // separate.
        for rel in db.snapshot_files()? {
            if let Err(e) = process_snapshots(&db, &repo, &rel) {
                log(&format!("ERROR {}: {}", rel, e));
            }
        }

        let quiet: Vec<String> = pending
            .iter()
            .filter(|(_, t)| t.elapsed() >= UNATTRIBUTED_QUIET)
            .map(|(p, _)| p.clone())
            .collect();
        for rel in quiet {
            pending.remove(&rel);
            if let Err(e) = process(&db, &repo, ws, human_id, &rel) {
                log(&format!("ERROR {}: {}", rel, e));
            }
        }
    }
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
    let recorded = process_snapshots(db, repo, rel)?;

    // Whatever is left is what no hook claimed: a Bash write, or the human.
    let change = shadow::classify(repo, &ws.root, rel)?;
    if change == Change::None {
        return Ok(recorded);
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
    // Keep how the owner was found alongside the row. Nothing else in the
    // journal separates a session naming its own file from the daemon guessing
    // off a running command, and the two are worth different amounts of trust.
    let (session_id, attributed_by) = match db.take_hint(rel)? {
        Some((id, how)) => (id, Some(how)),
        None => (human_id, None),
    };
    let session = db.get_session(session_id)?;
    let oid = shadow::commit_edit(
        repo,
        rel,
        change,
        &session.agent_name,
        &session.external_id,
        None,
    )?;
    db.insert_edit(
        session_id,
        rel,
        change.as_str(),
        Some(&oid),
        &hunks,
        attributed_by,
    )?;
    db.apply_edit_regions(session_id, rel, &hunks)?;
    log(&format!(
        "{} {} - {} (ortak-{})",
        change.as_str(),
        rel,
        session.agent_name,
        session.id
    ));
    Ok(true)
}

/// Journal every snapshot a hook recorded for this path, oldest first. Each one
/// holds one session's file as that session left it, so two sessions writing the
/// same file inside one debounce window still get one commit each.
fn process_snapshots(db: &Db, repo: &git2::Repository, rel: &str) -> Result<bool> {
    let snapshots = db.peek_snapshots(rel)?;
    if repo.is_path_ignored(rel).unwrap_or(false) {
        for (rowid, _, _) in snapshots {
            db.drop_snapshot(rowid)?;
        }
        return Ok(false);
    }
    let mut recorded = false;
    for (rowid, session_id, blob) in snapshots {
        let Some(snapshot) = git2::Oid::from_str(&blob)
            .ok()
            .and_then(|id| repo.find_blob(id).ok())
        else {
            db.drop_snapshot(rowid)?;
            continue;
        };
        let change = shadow::classify_snapshot(repo, rel, snapshot.id());
        if change == Change::None {
            db.drop_snapshot(rowid)?;
            continue;
        }
        // Hunks must be computed against shadow HEAD *before* the micro-commit.
        let old_blob = shadow::head_blob(repo, rel);
        let hunks = shadow::compute_hunks(old_blob.as_ref(), Some(snapshot.content()))?;
        drop(old_blob);
        let session = db.get_session(session_id)?;
        let oid = shadow::commit_edit(
            repo,
            rel,
            change,
            &session.agent_name,
            &session.external_id,
            Some(snapshot.id()),
        )?;
        db.insert_edit(
            session_id,
            rel,
            change.as_str(),
            Some(&oid),
            &hunks,
            Some(Attribution::Hook),
        )?;
        db.apply_edit_regions(session_id, rel, &hunks)?;
        // Only now: a hook reading between the select above and this point must
        // still find the row, or it bases its own snapshot on pre-commit content
        // and the change this commit just recorded is undone by the next one.
        db.drop_snapshot(rowid)?;
        log(&format!(
            "{} {} - {} (ortak-{})",
            change.as_str(),
            rel,
            session.agent_name,
            session.id
        ));
        recorded = true;
    }
    Ok(recorded)
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
