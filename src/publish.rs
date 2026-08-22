use crate::config::Config;
use crate::db::Db;
use crate::workspace::Workspace;
use anyhow::{anyhow, bail, Context, Result};
use git2::{BranchType, Commit, IndexEntry, IndexTime, Repository, Signature};
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
    push: bool,
) -> Result<()> {
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;
    let files = db.session_files(session.id)?;
    if files.is_empty() {
        bail!(
            "ortak-{} has no recorded file changes; nothing to publish",
            session.id
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
    let base_commit = base_commit_for(&repo, &cfg.publish.base_branch)?;

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

    println!(
        "branch ready: {} ({} files, commit {})",
        branch_name,
        files.len(),
        &commit_oid.to_string()[..8]
    );
    for (f, k) in &files {
        println!("  {} {}", k, f);
    }

    if push {
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
            cfg.publish.base_branch, branch_name, intent
        );
    } else {
        println!("\nnot pushed; run: ortak publish {} --push", session_ref);
    }
    Ok(())
}

/// The commit a published branch is built on.
///
/// Falling back to HEAD when the configured branch is missing looked harmless
/// until you notice what HEAD is in a shared workspace: whatever branch the
/// tree happens to sit on, which may be another session's task branch. A repo
/// whose trunk is `master` published every task off HEAD and still printed
/// `--base main`.
fn base_commit_for<'r>(repo: &'r Repository, base: &str) -> Result<Commit<'r>> {
    repo.find_branch(base, BranchType::Local)
        .and_then(|b| b.get().peel_to_commit())
        .map_err(|_| {
            let head = repo
                .head()
                .ok()
                .and_then(|h| h.shorthand().map(String::from))
                .map(|h| format!(" (HEAD is on '{h}')"))
                .unwrap_or_default();
            anyhow!(
                "base branch '{}' does not exist in this repository{}. Set [publish] base_branch in ortak.toml to the branch these tasks merge into",
                base,
                head
            )
        })
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
    fn a_missing_base_branch_is_an_error_not_a_guess() {
        let dir = std::env::temp_dir().join(format!("ortak-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Default branch `master`, the way plenty of repositories still are.
        let repo = Repository::init_opts(
            &dir,
            git2::RepositoryInitOptions::new().initial_head("master"),
        )
        .unwrap();
        let sig = Signature::now("t", "t@t.t").unwrap();
        let tree = repo
            .find_tree(repo.index().unwrap().write_tree().unwrap())
            .unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();

        assert!(base_commit_for(&repo, "master").is_ok());
        let err = base_commit_for(&repo, "main").unwrap_err().to_string();
        assert!(err.contains("'main' does not exist"), "{err}");
        assert!(err.contains("HEAD is on 'master'"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
