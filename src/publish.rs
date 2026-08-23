use crate::config::Config;
use crate::db::{Db, PublishRow};
use crate::workspace::Workspace;
use anyhow::{bail, Context, Result};
use git2::{BranchType, IndexEntry, IndexTime, Repository, Signature};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Which of a session's edits one publish carries. The three are exclusive:
/// each names a different point to start reading the journal from.
#[derive(Clone, Copy, PartialEq)]
pub enum Scope {
    /// Everything since the session's last publish.
    New,
    /// Everything the session has ever touched.
    All,
    /// Everything the named branch already carried, plus what came after it.
    Amend,
}

/// Assemble a session's net change into a real branch on the workspace's git
/// repo: base tree + the session's files at their current workspace content.
/// The live working directory is never touched (no checkout).
pub fn run(
    ws: &Workspace,
    cfg: &Config,
    session_ref: &str,
    branch_override: Option<&str>,
    base_override: Option<&str>,
    scope: Scope,
    push: bool,
) -> Result<()> {
    let base = base_branch(cfg, base_override);
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;

    // One session runs several tasks, so shipping everything it ever touched
    // puts the finished work back into every later branch. Default to what came
    // after the last publish; --all rebuilds a branch holding all of it.
    let history = db.publishes(session.id)?;
    let previous = history.first().cloned();
    // An amend goes back further, to where the branch it is rewriting began.
    let amending = match scope {
        Scope::Amend => Some(amend_target(&history, branch_override, session.id)?),
        _ => None,
    };
    let after = match amending {
        Some((mark, _)) => mark,
        None if scope == Scope::All => 0,
        None => previous.as_ref().map_or(0, |p| p.last_edit_id),
    };
    // Read the high-water mark before the file list, never after: an edit that
    // lands in between is then republished rather than silently dropped.
    let head_edit = db.max_edit_id(session.id)?.unwrap_or(0);
    let files = db.session_files(session.id, after)?;
    if files.is_empty() {
        match previous {
            Some(p) if scope == Scope::New => bail!(
                "ortak-{} has changed nothing since its last publish; branch {} already carries that work. Pass --all to rebuild a branch with everything the session has touched",
                session.id,
                p.branch
            ),
            _ => bail!(
                "ortak-{} has no recorded file changes; nothing to publish",
                session.id
            ),
        }
    }

    // Before the work that can fail, not after the success that does not need
    // it: the publish that dies on the earlier task's lines is the one that has
    // to hear which branch to stack on.
    if let Some(p) = previous
        .as_ref()
        .filter(|p| scope == Scope::New && p.branch != base)
    {
        eprintln!(
            "ortak-{}'s earlier work is on {}; if this publish cannot separate the two, pass --base {} to stack this branch on it.",
            session.id, p.branch, p.branch
        );
    }

    // Layer 0 has no gate, so overlapping edits are possible; surface them.
    let file_names: Vec<String> = files.iter().map(|(f, _)| f.clone()).collect();
    let overlaps = db.overlapping_sessions(session.id, &file_names)?;
    if !overlaps.is_empty() {
        eprintln!("WARNING: other sessions touched these files; the branch will use their current contents:");
        for (f, other) in &overlaps {
            eprintln!("  {} - also touched by: {}", f, other);
        }
    }

    let repo = Repository::open(&ws.root).with_context(|| {
        format!(
            "publishing requires {} to be a git repository with a configured remote",
            ws.root.display()
        )
    })?;
    // An amend rebuilds the branch where it already stands: its tip's parent is
    // the commit it was published on, so leaving --base off does not quietly
    // rebase a stacked branch onto the trunk and pull its base's work into the
    // diff. An explicit --base still wins, which is how a branch moves.
    let base_commit = match (amending, base_override) {
        (Some((_, branch)), None) => repo
            .find_branch(branch, BranchType::Local)
            .with_context(|| format!("ortak published {branch}, but this repository has no such branch now"))?
            .get()
            .peel_to_commit()?
            .parent(0)
            .with_context(|| format!("{branch} has no parent commit to rebuild it on"))?,
        _ => match repo.find_branch(base, BranchType::Local) {
            Ok(b) => b.get().peel_to_commit()?,
            Err(_) => repo
                .head()
                .and_then(|h| h.peel_to_commit())
                .with_context(|| {
                    format!(
                        "base branch '{}' does not exist and HEAD could not be resolved; the repository needs at least one commit",
                        base
                    )
                })?,
        },
    };

    // Build the branch tree in an in-memory index: base tree + session files.
    let mut index = git2::Index::new()?;
    index.read_tree(&base_commit.tree()?)?;
    for (file, kind) in &files {
        if kind == "delete" {
            let _ = index.remove_path(Path::new(file));
            continue;
        }
        let abs = ws.root.join(file);
        let data = std::fs::read(&abs).with_context(|| {
            format!(
                "could not read {} (was it deleted from the workspace?)",
                file
            )
        })?;
        let mode = if abs.metadata()?.permissions().mode() & 0o111 != 0 {
            0o100755
        } else {
            0o100644
        };
        let blob_id = repo.blob(&data)?;
        let entry = IndexEntry {
            ctime: IndexTime::new(0, 0),
            mtime: IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            file_size: data.len() as u32,
            id: blob_id,
            flags: 0,
            flags_extended: 0,
            path: file.as_bytes().to_vec(),
        };
        index.add(&entry)?;
    }
    let tree_oid = index.write_tree_to(&repo)?;
    let tree = repo.find_tree(tree_oid)?;
    if tree_oid == base_commit.tree_id() {
        bail!("empty net change: the session's files match the base branch");
    }

    let intent = session
        .task_intent
        .clone()
        .unwrap_or_else(|| format!("ortak task ortak-{}", session.id));
    let message = format!(
        "{}\n\nOrtak-Session: {}\nOrtak-Agent: {}\nOrtak-Files: {}\n",
        intent,
        session.external_id,
        session.agent_name,
        files.len()
    );
    let email = format!("ortak-{}@ortak.local", session.id);
    let sig = Signature::now(&session.agent_name, &email)?;
    let commit_oid = repo.commit(None, &sig, &sig, &message, &tree, &[&base_commit])?;

    let branch_name = match branch_override {
        Some(b) => b.to_string(),
        None => format!(
            "{}ortak-{}-{}",
            cfg.publish.branch_prefix,
            session.id,
            slug(&intent)
        ),
    };
    let amend = amending.is_some();
    repo.branch(&branch_name, &repo.find_commit(commit_oid)?, amend)
        .with_context(|| {
            format!(
                "could not create branch {} (does it already exist?)",
                branch_name
            )
        })?;
    // Only now: a failed publish must not move the session's high-water mark.
    // An amend moves the branch's own mark instead of adding a second row for
    // it, so the next new deliverable still starts after everything published.
    if amend {
        db.amend_publish(session.id, &branch_name, head_edit)?;
    } else {
        db.record_publish(session.id, &branch_name, head_edit)?;
    }

    println!(
        "branch {}: {} ({} files, commit {})",
        if amend { "rewritten" } else { "ready" },
        branch_name,
        files.len(),
        &commit_oid.to_string()[..8]
    );
    for (f, k) in &files {
        println!("  {} {}", k, f);
    }

    if push {
        // A stacked branch is unusable on the forge until its base is there too:
        // GitHub and Forgejo both refuse a pull request whose base branch they
        // cannot find, so --base builds the stack locally and then strands it.
        // Only a branch --base named explicitly, and never the configured trunk,
        // which is nobody's to push on a publish's initiative.
        if let Some(stack) = base_override.filter(|b| !on_remote(ws, &cfg.publish.remote, b)) {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&ws.root)
                .args(["push", "-u", &cfg.publish.remote, stack])
                .status()?;
            if !status.success() {
                bail!("git push of the base branch {} failed", stack);
            }
            println!(
                "pushed base branch {} first; it did not exist on {}",
                stack, cfg.publish.remote
            );
        }
        // An amended branch is not a fast-forward, so the push has to say so.
        // --force-with-lease rather than --force: if the branch on the remote
        // has moved since this session last saw it, that is somebody else's
        // commit and no amend was ever meant to drop it.
        let mut args = vec!["push", "-u", cfg.publish.remote.as_str(), &branch_name];
        if amend {
            args.insert(1, "--force-with-lease");
        }
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&ws.root)
            .args(&args)
            .status()?;
        if !status.success() {
            bail!("git push failed");
        }
        if !amend {
            println!("\ncreate the PR with:");
            println!(
                "  tea pr create --base {} --head {} --title \"{}\"",
                base, branch_name, intent
            );
        }
    } else if amend {
        println!(
            "\n{} moved, so it is no longer a fast-forward. Push it with:\n  git push --force-with-lease {} {}",
            branch_name, cfg.publish.remote, branch_name
        );
    } else {
        println!("\nnot pushed; run: ortak publish {} --push", session_ref);
    }
    Ok(())
}

/// Whether the remote already carries this branch. Local refs go stale, and a
/// wrong answer here either strands a stack or pushes a branch nobody asked for,
/// so ask the remote.
fn on_remote(ws: &Workspace, remote: &str, branch: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(&ws.root)
        .args(["ls-remote", "--exit-code", "--heads", remote, branch])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Where an amend starts reading the journal, and the branch it rewrites.
///
/// A branch carries the edits between the publish before it and its own, so
/// rebuilding it means starting from the mark the publish before it left. That
/// is only true while the branch is the session's newest publish: rewriting an
/// older one would sweep every deliverable published since into it, which is
/// the bug `--base` was added to avoid rather than one to introduce here.
fn amend_target<'a>(
    history: &[PublishRow],
    branch: Option<&'a str>,
    session_id: i64,
) -> Result<(i64, &'a str)> {
    let Some(branch) = branch else {
        bail!("--amend rewrites one branch; name it with --branch <branch>");
    };
    let Some(newest) = history.first() else {
        bail!(
            "ortak-{} has published nothing yet, so it has no branch to amend",
            session_id
        );
    };
    if newest.branch != branch {
        if history.iter().any(|p| p.branch == branch) {
            bail!(
                "ortak-{} published {} after {}; amending {} now would sweep that later work into it. Publish the fix as its own branch instead",
                session_id, newest.branch, branch, branch
            );
        }
        bail!(
            "ortak-{} did not publish {}; its last branch was {}, and --amend only rewrites a branch this session published",
            session_id, branch, newest.branch
        );
    }
    Ok((history.get(1).map_or(0, |p| p.last_edit_id), branch))
}

/// The branch this publish builds on. `--base` is per invocation, so work that
/// sits on another session's unmerged branch can ship without every session in
/// the workspace having to agree on a new `[publish] base_branch`.
fn base_branch<'a>(cfg: &'a Config, base_override: Option<&'a str>) -> &'a str {
    base_override.unwrap_or(&cfg.publish.base_branch)
}

fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().take(48) {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "task".into()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arithmetic an amend rests on: rebuilding a branch reaches back to
    /// where its own work began, and the deliverable after it still starts from
    /// everything that has shipped. Getting the second half wrong re-publishes
    /// work that already went out, which is the bug incremental publish exists
    /// to fix.
    #[test]
    fn an_amend_rebuilds_one_branch_and_leaves_the_next_where_it_was() {
        let dir = std::env::temp_dir().join(format!("ortak-amend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let s = db
            .upsert_session("sess", "claude-sess", "llm", Some("claude-code"))
            .unwrap();
        let files = |after: i64| -> Vec<String> {
            db.session_files(s, after)
                .unwrap()
                .into_iter()
                .map(|(f, _)| f)
                .collect()
        };
        let head = || db.max_edit_id(s).unwrap().unwrap();

        // The first deliverable ships.
        db.insert_edit(s, "one.rs", "create", None, &[]).unwrap();
        db.record_publish(s, "task/one", head()).unwrap();

        // Review lands, so the fix goes into the same branch.
        db.insert_edit(s, "one.rs", "modify", None, &[]).unwrap();
        let (after, branch) = amend_target(&db.publishes(s).unwrap(), Some("task/one"), s).unwrap();
        assert_eq!((after, branch), (0, "task/one"));
        assert_eq!(files(after), vec!["one.rs"]);
        db.amend_publish(s, "task/one", head()).unwrap();
        assert_eq!(
            db.publishes(s).unwrap().len(),
            1,
            "an amend must not add a second publish row"
        );

        // The next deliverable is its own branch and carries only its own work.
        db.insert_edit(s, "two.rs", "create", None, &[]).unwrap();
        let history = db.publishes(s).unwrap();
        assert_eq!(files(history[0].last_edit_id), vec!["two.rs"]);
        db.record_publish(s, "task/two", head()).unwrap();

        // task/one is behind the newest publish now, so it is out of reach, and
        // amending task/two reaches back only as far as task/one shipped.
        let history = db.publishes(s).unwrap();
        assert!(amend_target(&history, Some("task/one"), s).is_err());
        assert!(amend_target(&history, Some("never/published"), s).is_err());
        assert!(amend_target(&history, None, s).is_err());
        assert_eq!(
            files(amend_target(&history, Some("task/two"), s).unwrap().0),
            vec!["two.rs"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn base_flag_overrides_the_configured_branch() {
        let mut cfg = Config::default();
        cfg.publish.base_branch = "main".into();
        assert_eq!(base_branch(&cfg, None), "main");
        assert_eq!(
            base_branch(&cfg, Some("task/ortak-3-parser")),
            "task/ortak-3-parser"
        );
    }

    /// The stack's base has to be pushed before the branch that stands on it,
    /// so the question is whether the remote already has it.
    #[test]
    fn a_branch_the_remote_lacks_is_reported_missing() {
        let root = std::env::temp_dir().join(format!("ortak-lsremote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bare = root.join("remote.git");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "--bare", "."]);
        git(&work, &["init", "-q", "-b", "main", "."]);
        std::fs::write(work.join("f.txt"), "x\n").unwrap();
        git(&work, &["add", "f.txt"]);
        git(
            &work,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "base",
            ],
        );
        git(&work, &["remote", "add", "up", bare.to_str().unwrap()]);
        git(&work, &["push", "-q", "up", "main"]);

        let ws = Workspace::at(&work);
        assert!(on_remote(&ws, "up", "main"));
        assert!(!on_remote(&ws, "up", "feat/never-pushed"));
        std::fs::remove_dir_all(&root).ok();
    }
}
