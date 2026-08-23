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
/// Names the daemon that owns this workspace, inside `.ortak`.
const PIDFILE: &str = "daemon.pid";
/// Where a detached daemon's output goes, inside `.ortak`.
const LOGFILE: &str = "daemon.log";
/// Set on the re-executed child so its log says which kind of daemon it is.
const DETACHED_ENV: &str = "ORTAK_DAEMON_DETACHED";

struct PidfileGuard(PathBuf);

impl Drop for PidfileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn run(ws: &Workspace, _cfg: &Config) -> Result<()> {
    let pidfile = ws.ortak_dir.join(PIDFILE);
    claim(&pidfile, process_alive)?;
    let _pidfile_guard = PidfileGuard(pidfile);
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

    let how = if std::env::var_os(DETACHED_ENV).is_some() {
        "detached"
    } else {
        "foreground"
    };
    log(&format!("daemon started ({}): {}", how, ws.root.display()));
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
            match process_snapshots(&db, &repo, &rel) {
                Ok(_) => {
                    if let Err(e) = db.clear_journal_failure(&rel) {
                        log(&format!("ERROR clearing health record for {}: {}", rel, e));
                    }
                }
                Err(e) => {
                    log(&format!("ERROR {}: {}", rel, e));
                    if let Err(e2) = db.record_journal_failure(&rel, &e.to_string()) {
                        log(&format!("ERROR recording failure for {}: {}", rel, e2));
                    }
                }
            }
        }

        let quiet: Vec<String> = pending
            .iter()
            .filter(|(_, t)| t.elapsed() >= UNATTRIBUTED_QUIET)
            .map(|(p, _)| p.clone())
            .collect();
        for rel in quiet {
            pending.remove(&rel);
            journal(&db, &repo, ws, human_id, &rel);
        }
    }
}

/// Claim this workspace for the current process. Refuses when the pidfile names
/// a daemon that is still alive: two daemons race each other on the shadow
/// repository's index and refs, and the loser of each race drops the edit it was
/// journaling. Nothing downstream can tell that happened.
///
/// A pidfile left behind by a killed daemon is replaced without complaint. That
/// is the normal way this file ends up on disk, since the daemon runs in the
/// foreground and is usually stopped with Ctrl-C.
fn claim(pidfile: &Path, alive: impl Fn(u32) -> bool) -> Result<()> {
    let me = std::process::id();
    if let Some(pid) = read_pid(pidfile) {
        if pid != me && alive(pid) {
            bail!(
                "another ortak daemon is already running on this workspace (pid {}). \
                 Two daemons drop each other's edits, so this one will not start. \
                 Stop it first, or delete {} if you know it is gone.",
                pid,
                pidfile.display()
            );
        }
    }
    if let Some(dir) = pidfile.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(pidfile, me.to_string())?;
    Ok(())
}

/// Start the daemon in the background by re-executing this binary, with its
/// output going to `.ortak/daemon.log`. The child claims the workspace itself,
/// so the pidfile ends up holding the pid that is actually watching.
pub fn detach(ws: &Workspace) -> Result<()> {
    let pidfile = ws.ortak_dir.join(PIDFILE);
    // The child would refuse too, but its complaint would land in a log file
    // nobody is watching. Answer on the terminal that asked.
    if let Some(pid) = read_pid(&pidfile) {
        if process_alive(pid) {
            bail!(
                "another ortak daemon is already running on this workspace (pid {}). \
                 Stop it with `ortak daemon --stop`.",
                pid
            );
        }
    }
    let log_path = ws.ortak_dir.join(LOGFILE);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let child = std::process::Command::new(std::env::current_exe()?)
        .arg("daemon")
        .current_dir(&ws.root)
        .env(DETACHED_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    println!("daemon detached (pid {})", child.id());
    println!("log:  {}", log_path.display());
    println!("pid:  {}", pidfile.display());
    println!("stop: ortak daemon --stop");
    Ok(())
}

/// Stop whatever daemon this workspace's pidfile names.
pub fn stop(ws: &Workspace) -> Result<()> {
    let pidfile = ws.ortak_dir.join(PIDFILE);
    match stop_action(&pidfile, process_alive) {
        StopAction::NothingRunning => {
            println!(
                "no daemon is running on this workspace ({})",
                ws.root.display()
            );
        }
        StopAction::Stale(pid) => {
            std::fs::remove_file(&pidfile)?;
            match pid {
                Some(pid) => println!(
                    "no daemon is running; removed a stale pidfile left by pid {}",
                    pid
                ),
                None => println!("no daemon is running; removed a pidfile that named no process"),
            }
        }
        StopAction::Kill(pid) => {
            let killed = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !killed {
                bail!(
                    "could not signal the daemon (pid {}); it is still running",
                    pid
                );
            }
            std::fs::remove_file(&pidfile)?;
            println!("stopped the daemon (pid {})", pid);
        }
    }
    Ok(())
}

/// What `--stop` should do about the pidfile it found.
#[derive(Debug, PartialEq)]
enum StopAction {
    NothingRunning,
    /// A pidfile naming a process that is gone, or no process at all.
    Stale(Option<u32>),
    Kill(u32),
}

fn stop_action(pidfile: &Path, alive: impl Fn(u32) -> bool) -> StopAction {
    match read_pid(pidfile) {
        None if pidfile.exists() => StopAction::Stale(None),
        None => StopAction::NothingRunning,
        Some(pid) if alive(pid) => StopAction::Kill(pid),
        Some(pid) => StopAction::Stale(Some(pid)),
    }
}

fn read_pid(pidfile: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pidfile).ok()?.trim().parse().ok()?;
    plausible_pid(pid).then_some(pid)
}

/// A value that could be a process at all. Past `i32::MAX` it reaches `kill` as
/// a negative number, and on Linux `kill -1 -0` means "every process you may
/// signal" and succeeds, which would report a dead daemon as alive forever and
/// lock the workspace with nothing to diagnose.
fn plausible_pid(pid: u32) -> bool {
    pid > 0 && pid <= i32::MAX as u32
}

/// `kill -0` is the portable way to ask whether a pid is alive without adding a
/// libc dependency for one call at startup.
///
/// ponytail: this reads "permission denied" as dead. The only pid it is ever
/// asked about is another ortak daemon of the same user, so that cannot happen
/// here; if it ever can, this needs a real `kill(2)` and errno.
fn process_alive(pid: u32) -> bool {
    if !plausible_pid(pid) {
        return false;
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

/// Journal one path and keep its health record current. Returns whether
/// anything was recorded.
///
/// The daemon's own log lives in a terminal nobody is reading and no agent can
/// reach, so a journaling failure used to be invisible to everything except the
/// person watching that window. A session whose edits are not landing has to be
/// able to find that out from `ortak status`.
fn journal(db: &Db, repo: &git2::Repository, ws: &Workspace, human_id: i64, rel: &str) -> bool {
    match process(db, repo, ws, human_id, rel) {
        Ok(recorded) => {
            if let Err(e) = db.clear_journal_failure(rel) {
                log(&format!("ERROR clearing health record for {}: {}", rel, e));
            }
            recorded
        }
        Err(e) => {
            log(&format!("ERROR {}: {}", rel, e));
            if let Err(e2) = db.record_journal_failure(rel, &e.to_string()) {
                log(&format!("ERROR recording failure for {}: {}", rel, e2));
            }
            false
        }
    }
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
    let (session_id, attributed_by) = match db.peek_hint(rel)? {
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
    db.apply_edit_regions(session_id, rel, &hunks, attributed_by)?;
    // Only now. Everything above can fail, and a hint consumed ahead of its
    // commit is not there for the retry.
    db.clear_hints(rel)?;
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
        db.apply_edit_regions(session_id, rel, &hunks, Some(Attribution::Hook))?;
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
        if journal(db, repo, ws, human_id, rel) {
            journaled += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pidfile(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ortak-{}-{}.pid", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn claims_a_workspace_nobody_holds() {
        let f = pidfile("free");
        claim(&f, |_| true).expect("no pidfile, nothing to conflict with");
        assert_eq!(read_pid(&f), Some(std::process::id()));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn takes_over_from_a_dead_daemon() {
        let f = pidfile("stale");
        std::fs::write(&f, "424242").unwrap();
        claim(&f, |_| false).expect("the pid it names is gone");
        assert_eq!(read_pid(&f), Some(std::process::id()));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn refuses_while_another_daemon_lives() {
        let f = pidfile("live");
        std::fs::write(&f, "424242").unwrap();
        assert!(claim(&f, |_| true).is_err());
        assert_eq!(read_pid(&f), Some(424242), "the running daemon keeps it");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn an_unreadable_pidfile_holds_nothing() {
        let f = pidfile("junk");
        std::fs::write(&f, "not a pid").unwrap();
        claim(&f, |_| true).expect("garbage cannot hold a workspace");
        assert_eq!(read_pid(&f), Some(std::process::id()));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn a_pid_that_would_wrap_negative_holds_nothing() {
        // `kill -0 4294967295` reaches Linux as kill(-1, 0), "every process you
        // may signal", and succeeds. Such a pidfile must not lock the daemon out.
        let f = pidfile("wrap");
        std::fs::write(&f, u32::MAX.to_string()).unwrap();
        assert_eq!(read_pid(&f), None);
        claim(&f, |_| true).expect("an impossible pid holds nothing");
        assert_eq!(read_pid(&f), Some(std::process::id()));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn stop_tells_a_live_daemon_apart_from_the_file_a_dead_one_left() {
        let f = pidfile("stop");
        assert_eq!(stop_action(&f, |_| true), StopAction::NothingRunning);
        std::fs::write(&f, "424242").unwrap();
        assert_eq!(stop_action(&f, |_| true), StopAction::Kill(424242));
        assert_eq!(stop_action(&f, |_| false), StopAction::Stale(Some(424242)));
        std::fs::write(&f, "not a pid").unwrap();
        assert_eq!(stop_action(&f, |_| true), StopAction::Stale(None));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn asking_the_system_about_a_pid_works_both_ways() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(u32::MAX));
    }
}
