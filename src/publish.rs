use crate::config::Config;
use crate::db::Db;
use crate::workspace::Workspace;
use anyhow::{bail, Context, Result};
use git2::{BranchType, IndexEntry, IndexTime, Repository, Signature};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Assemble a session's net change into a real branch on the workspace's git
/// repo: base tree + the session's files at their current workspace content.
/// The live working directory is never touched (no checkout).
pub fn run(
    ws: &Workspace,
    cfg: &Config,
    session_ref: &str,
    branch_override: Option<&str>,
    base_override: Option<&str>,
    all: bool,
    push: bool,
) -> Result<()> {
    let base = base_branch(cfg, base_override);
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;

    // One session runs several tasks, so shipping everything it ever touched
    // puts the finished work back into every later branch. Default to what came
    // after the last publish; --all rebuilds a branch holding all of it.
    let previous = db.last_publish(session.id)?;
    let after = if all {
        0
    } else {
        previous.as_ref().map_or(0, |p| p.last_edit_id)
    };
    // Read the high-water mark before the file list, never after: an edit that
    // lands in between is then republished rather than silently dropped.
    let head_edit = db.max_edit_id(session.id)?.unwrap_or(0);
    let files = db.session_files(session.id, after)?;
    if files.is_empty() {
        match previous {
            Some(p) if !all => bail!(
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
    let base_commit = match repo.find_branch(base, BranchType::Local) {
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
    repo.branch(&branch_name, &repo.find_commit(commit_oid)?, false)
        .with_context(|| {
            format!(
                "could not create branch {} (does it already exist?)",
                branch_name
            )
        })?;
    // Only now: a failed publish must not move the session's high-water mark.
    db.record_publish(session.id, &branch_name, head_edit)?;

    println!(
        "branch ready: {} ({} files, commit {})",
        branch_name,
        files.len(),
        &commit_oid.to_string()[..8]
    );
    for (f, k) in &files {
        println!("  {} {}", k, f);
    }
    // A file touched by both tasks ships whole, so a branch built on the plain
    // base carries the earlier task's changes to it as well.
    if let Some(p) = previous.filter(|p| !all && p.branch != base) {
        println!(
            "\nortak-{}'s earlier work is on {}; pass --base {} to stack this branch on it.",
            session.id, p.branch, p.branch
        );
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
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&ws.root)
            .args(["push", "-u", &cfg.publish.remote, &branch_name])
            .status()?;
        if !status.success() {
            bail!("git push failed");
        }
        println!("\ncreate the PR with:");
        println!(
            "  tea pr create --base {} --head {} --title \"{}\"",
            base, branch_name, intent
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
