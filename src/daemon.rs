use crate::config::Config;
use crate::db::{self, Attribution, Db};
use crate::shadow::{self, Change};
use crate::workspace::{Workspace, ORTAK_DIR};
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
        // Only while it is still ours. Once a reset has handed the workspace
        // to another daemon, the file on disk is that daemon's claim, and
        // deleting it on the way out would let a third start beside it.
        if still_ours(&self.0) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// Whether the workspace's pidfile still names this process.
///
/// `claim` reads it once at startup, so it sees a daemon that was already
/// running and nothing after. The record of who owns a workspace lives inside
/// `.ortak`, which is what a reset deletes: `rm -rf .ortak && ortak init`
/// leaves this daemon watching a workspace nothing says it owns, and the next
/// `ortak daemon` claims it without knowing this one is here. Both alive,
/// neither refusing, taking turns losing the shadow index lock and dropping
/// whichever edit they were journaling.
fn still_ours(pidfile: &Path) -> bool {
    read_pid(pidfile) == Some(std::process::id())
}

/// The binary a daemon is running, written to `meta` when it starts.
///
/// The version is the part a person reads. The part that answers the question
/// is the file's modification time and size: a version string does not move
/// between releases, and the daemon in this workspace spent all of round 12
/// running a build ten pull requests behind the hooks while both of them
/// called themselves 0.3.0.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Build {
    pub version: String,
    /// Where this process was loaded from, per `current_exe`.
    pub path: String,
    mtime: i64,
    size: u64,
}

impl Build {
    /// What this process is running, and `None` when it cannot find its own
    /// binary, which is the one case with nothing true to say.
    fn current() -> Option<Build> {
        let path = std::env::current_exe().ok()?;
        let (mtime, size) = stamp(&path);
        Some(Build {
            version: env!("CARGO_PKG_VERSION").to_string(),
            path: path.display().to_string(),
            mtime,
            size,
        })
    }
}

/// A file's modification time and size, and `(0, 0)` for a path the filesystem
/// will not answer for. A binary that has been deleted therefore reads as
/// different from every real build rather than as agreement.
fn stamp(path: &Path) -> (i64, u64) {
    let Ok(meta) = path.metadata() else {
        return (0, 0);
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    (mtime, meta.len())
}

/// What the daemon recorded about itself, and whether the file it started from
/// is still that file. `None` when nothing was recorded, or when the record
/// cannot be read: a row written by another build must not be able to take
/// down the command that reports it.
pub fn running_build(db: &Db) -> Option<(Build, bool)> {
    let raw = db.daemon_build().ok().flatten()?;
    let was: Build = serde_json::from_str(&raw).ok()?;
    let current = stamp(Path::new(&was.path)) == (was.mtime, was.size);
    Some((was, current))
}

pub fn run(ws: &Workspace, cfg: &Config) -> Result<()> {
    let pidfile = ws.ortak_dir.join(PIDFILE);
    claim(&pidfile, process_alive)?;
    let _pidfile_guard = PidfileGuard(pidfile.clone());
    let db = Db::open(&ws.db_path)?;
    let repo = shadow::open(ws)?;
    // ortak's own list, read once here rather than off disk on every path: the
    // repository that owns a path has never heard of `.ortak`, so it has to be
    // told, and in this tree there may be sixty of them to tell.
    let excludes = shadow::exclude_rules(ws, cfg);
    // The gate's own window, handed to the attribution ladder so both answer
    // "is that session still working in this file" with the same number.
    let presence_secs = cfg.gate.presence_minutes * 60;
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
    // A heartbeat older than the alive threshold means nobody was watching, and
    // both ends of the gap are known: that last beat, and now. Read it before
    // this run's first heartbeat overwrites the only record of it.
    let back = db::now_ts();
    let stopped_at = db
        .last_heartbeat()?
        .filter(|t| back - t > db::HEARTBEAT_ALIVE_SECS);
    db.heartbeat()?;
    // Once, not on the heartbeat's timer: a running process cannot change the
    // file it was loaded from. What moves is the file, and `status` reads that
    // side when somebody asks.
    if let Some(build) = Build::current() {
        db.record_daemon_build(&serde_json::to_string(&build)?)?;
    }
    let journaled = startup_scan(&db, &repo, ws, &excludes, presence_secs, human_id);
    if let Some(start) = stopped_at {
        let outage = db::Outage {
            start,
            end: back,
            journaled,
        };
        log(&format!(
            "the journal has a {}s gap; `ortak status` reports it for an hour",
            outage.secs()
        ));
        db.record_outage(&outage)?;
    }

    let mut pending: HashMap<String, Instant> = HashMap::new();
    let mut last_beat = Instant::now();

    loop {
        // Before anything is written, not on the heartbeat's timer: every pass
        // through here can journal, and the seconds a heartbeat would wait are
        // the seconds two daemons spend racing.
        if !still_ours(&pidfile) {
            bail!(
                "the workspace was reset under this daemon: {} no longer names pid {}. \
                 Stopping, rather than journaling into a database and a shadow repository \
                 it no longer owns.",
                pidfile.display(),
                std::process::id()
            );
        }
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
            match process_snapshots(&db, &repo, ws, &excludes, &rel) {
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
            journal(&db, &repo, ws, &excludes, presence_secs, human_id, &rel);
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
///
/// At any depth, not just the first component. A repository does not ignore its
/// own `.git`, and it never needed to, because git does not walk in there. So
/// once the repository that owns a path is the one deciding whether ortak
/// journals it, every nested `.git/index` becomes a journalable path, and a
/// tree of sixty repositories rewrites sixty of them on every `git status`
/// anybody runs.
fn filter(ws: &Workspace, abs: &Path) -> Option<String> {
    let rel = ws.relativize(abs)?;
    if rel.split('/').any(|c| c == ORTAK_DIR || c == ".git") {
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
fn journal(
    db: &Db,
    repo: &git2::Repository,
    ws: &Workspace,
    excludes: &str,
    presence_secs: i64,
    human_id: i64,
    rel: &str,
) -> bool {
    match process(db, repo, ws, excludes, presence_secs, human_id, rel) {
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
    excludes: &str,
    presence_secs: i64,
    human_id: i64,
    rel: &str,
) -> Result<bool> {
    if shadow::ignored(ws, repo, excludes, rel) {
        return Ok(false);
    }
    let abs = ws.root.join(rel);
    if abs.is_dir() {
        return Ok(false);
    }
    let recorded = process_snapshots(db, repo, ws, excludes, rel)?;

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
    let (session_id, attributed_by) = match db.peek_hint(rel, presence_secs)? {
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
fn process_snapshots(
    db: &Db,
    repo: &git2::Repository,
    ws: &Workspace,
    excludes: &str,
    rel: &str,
) -> Result<bool> {
    let snapshots = db.peek_snapshots(rel)?;
    if shadow::ignored(ws, repo, excludes, rel) {
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
///
/// ponytail: this is the shadow repository's status walk, so it sees a file
/// inside a nested repository only once the baseline has tracked it, and a
/// tracked file is never ignored. A file *created* in one of those
/// repositories while the daemon was stopped is untracked and behind the root's
/// `.gitignore`, so this does not find it; the next write to it does. Fixing
/// that means asking each repository about itself, which is a walk per
/// repository at every daemon start.
fn startup_scan(
    db: &Db,
    repo: &git2::Repository,
    ws: &Workspace,
    excludes: &str,
    presence_secs: i64,
    human_id: i64,
) -> u32 {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = match repo.statuses(Some(&mut opts)) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("startup scan failed: {}", e));
            return 0;
        }
    };
    let mut journaled = 0u32;
    for entry in statuses.iter() {
        let Some(rel) = entry.path() else { continue };
        if journal(db, repo, ws, excludes, presence_secs, human_id, rel) {
            journaled += 1;
        }
    }
    if journaled > 0 {
        // Not "(human)": journal() takes a live hint like any other pass, so a
        // file written seconds before the daemon came back lands on the session
        // that wrote it. The count is what the scan knows; `ortak log` is where
        // the owners are.
        log(&format!(
            "startup scan journaled {} changes made while the daemon was stopped",
            journaled
        ));
    }
    journaled
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

    fn tempdb(name: &str) -> Db {
        let p = std::env::temp_dir().join(format!(
            "ortak-daemon-{}-{}.sqlite",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&p);
        Db::open(&p).unwrap()
    }

    /// The whole bug: an update replaces the binary under a running daemon and
    /// nothing notices, because both builds call themselves the same version.
    /// What is recorded is the file, so the version string is free to sit still.
    #[test]
    fn a_replaced_binary_is_not_the_build_the_daemon_is_running() {
        let db = tempdb("build");
        let exe = std::env::temp_dir().join(format!("ortak-fake-{}", std::process::id()));
        std::fs::write(&exe, b"the build it started with").unwrap();
        let (mtime, size) = stamp(&exe);
        let started = Build {
            version: "0.3.0".to_string(),
            path: exe.display().to_string(),
            mtime,
            size,
        };
        db.record_daemon_build(&serde_json::to_string(&started).unwrap())
            .unwrap();

        let (read, current) = running_build(&db).expect("a record");
        assert_eq!(read, started, "what it wrote is what it reads back");
        assert!(current, "nothing has touched the file");

        std::fs::write(&exe, b"a later build, of a different size").unwrap();
        let (_, current) = running_build(&db).expect("a record");
        assert!(!current, "the file it started from has been replaced");

        std::fs::remove_file(&exe).unwrap();
        let (_, current) = running_build(&db).expect("a record");
        assert!(
            !current,
            "a binary that is gone is not the one it is running"
        );
    }

    /// A record written by another build must not be able to take down the
    /// command that reports it: a version check that breaks `status` is worse
    /// than no version check.
    #[test]
    fn a_record_this_build_cannot_read_says_nothing() {
        let db = tempdb("junk");
        assert!(running_build(&db).is_none(), "nothing has recorded a build");
        db.record_daemon_build(r#"{"built":"by something later"}"#)
            .unwrap();
        assert!(running_build(&db).is_none());
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

    /// The reset this project has run at the start of every round, and the one
    /// shape `claim` cannot see: the pidfile is deleted with the rest of
    /// `.ortak` while its daemon is still watching, and `ortak init` builds a
    /// workspace the next daemon claims for itself.
    #[test]
    fn a_reset_workspace_stops_belonging_to_the_daemon_watching_it() {
        let f = pidfile("reset");
        claim(&f, |_| true).expect("nothing holds it");
        assert!(still_ours(&f));

        // rm -rf .ortak
        std::fs::remove_file(&f).unwrap();
        assert!(!still_ours(&f), "nothing on disk says this daemon owns it");

        // ortak init, then a second daemon claims the new workspace.
        std::fs::write(&f, "424242").unwrap();
        assert!(!still_ours(&f), "and now somebody else does");
        let _ = std::fs::remove_file(&f);
    }

    /// The other half: a daemon leaving must not take the claim of whichever
    /// daemon owns the workspace now, or a third would be free to start.
    #[test]
    fn a_leaving_daemon_clears_only_its_own_claim() {
        let mine = pidfile("guard-mine");
        claim(&mine, |_| true).unwrap();
        drop(PidfileGuard(mine.clone()));
        assert!(!mine.exists(), "its own claim goes with it");

        let theirs = pidfile("guard-theirs");
        std::fs::write(&theirs, "424242").unwrap();
        drop(PidfileGuard(theirs.clone()));
        assert_eq!(read_pid(&theirs), Some(424242), "somebody else's stays");
        let _ = std::fs::remove_file(&theirs);
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
