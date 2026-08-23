use crate::config::Config;
use crate::db::Db;
use crate::workspace::Workspace;
use anyhow::{bail, Context, Result};
use git2::{BranchType, Commit, IndexEntry, IndexTime, Oid, Repository, Signature, Tree};
use std::path::Path;

/// What one publish was asked to do. A struct because every branch that adds a
/// flag adds an argument, and three of them together cross clippy's limit of
/// seven at merge time rather than in the change that caused it. A field has no
/// such limit.
pub struct PublishOpts<'a> {
    pub branch: Option<&'a str>,
    pub exclude: &'a [String],
    pub push: bool,
}

/// Assemble a session's net change into a real branch on the workspace's git
/// repo: base tree + the session's own content, replayed from its shadow
/// micro-commits so concurrent sessions' edits stay out of the branch.
/// The live working directory is never touched (no checkout).
pub fn run(ws: &Workspace, cfg: &Config, session_ref: &str, opts: PublishOpts) -> Result<()> {
    let PublishOpts {
        branch: branch_override,
        exclude,
        push,
    } = opts;
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;
    let mut files = db.session_files(session.id)?;
    if files.is_empty() {
        bail!(
            "ortak-{} has no recorded file changes; nothing to publish",
            session.id
        );
    }
    for pattern in drop_excluded(&mut files, exclude) {
        println!(
            "warning: --exclude {} matched none of ortak-{}'s files",
            pattern, session.id
        );
    }
    if files.is_empty() {
        bail!(
            "every file ortak-{} changed was excluded; nothing left to publish",
            session.id
        );
    }

    let repo = Repository::open(&ws.root).with_context(|| {
        format!(
            "publishing requires {} to be a git repository with a configured remote",
            ws.root.display()
        )
    })?;
    let base_commit = match repo.find_branch(&cfg.publish.base_branch, BranchType::Local) {
        Ok(b) => b.get().peel_to_commit()?,
        Err(_) => repo
            .head()
            .and_then(|h| h.peel_to_commit())
            .with_context(|| {
                format!(
                    "base branch '{}' does not exist and HEAD could not be resolved; the repository needs at least one commit",
                    cfg.publish.base_branch
                )
            })?,
    };
    let base_tree = base_commit.tree()?;

    // The gate lets two sessions edit distant lines of one file, so the file on
    // disk can hold another session's work. Rebuild this session's own content
    // from its shadow history instead of reading the workspace.
    let shadow = crate::shadow::open(ws)?;
    let seed = base_seed(&shadow, &repo, &base_tree, &files)?;
    // An excluded file leaves the replay too, not just the branch. Its commits
    // can still fail to apply, and a file nobody asked to publish has no
    // business failing the publish.
    let commits: Vec<String> = db
        .session_commits(session.id)?
        .into_iter()
        .filter(|(_, f)| files.iter().any(|(g, _)| g == f))
        .map(|(c, _)| c)
        .collect();
    let (session_tree, unreplayable) = session_only_tree(&shadow, &seed, &base_tree, &commits)?;

    // A file whose history cannot be replayed has no correct content to ship, so
    // it leaves the branch and the rest of the session's work still goes out.
    // One such file used to take the other four down with it.
    let skipped: Vec<String> = files
        .iter()
        .map(|(f, _)| f.clone())
        .filter(|f| unreplayable.contains(f))
        .collect();
    files.retain(|(f, _)| !skipped.contains(f));
    if files.is_empty() {
        bail!(
            "cannot publish ortak-{}: every file it changed ({}) builds on another session's work that is not on {} yet. Publish and merge that session first, or point [publish] base_branch in ortak.toml at its branch",
            session.id,
            skipped.join(", "),
            cfg.publish.base_branch
        );
    }

    // Build the branch tree in an in-memory index: base tree + session files.
    let mut index = git2::Index::new()?;
    index.read_tree(&base_tree)?;
    let mut stale: Vec<String> = Vec::new();
    for (file, kind) in &files {
        if kind == "delete" {
            let _ = index.remove_path(Path::new(file));
            continue;
        }
        let tracked = session_tree
            .get_path(Path::new(file))
            .with_context(|| format!("{} is missing from this session's replayed history", file))?;
        let data = shadow.find_blob(tracked.id())?.content().to_vec();
        if differs_from_last_write(&db, &shadow, session.id, file, &data)? {
            stale.push(file.clone());
        }
        let mode = tracked.filemode() as u32;
        // The blob lives in the shadow object database; copy it into the project repo.
        let blob_id = repo.blob(&data)?;
        index.add(&entry_for(file, blob_id, mode, data.len()))?;
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
    // After the file list, never before it: someone skimming the output has to
    // read what the branch does contain before they can judge what is missing.
    if !skipped.is_empty() {
        println!("\nleft out of this branch:");
        for f in &skipped {
            println!(
                "  {} - built on another session's work that is not on {} yet",
                f, cfg.publish.base_branch
            );
        }
        println!("the branch is incomplete; publish and merge that session, then publish again");
    }
    for f in &stale {
        println!(
            "\nwarning: {f} on this branch does not match what ortak-{id} last wrote to it.\nEdits are missing from the journal, so the branch is incomplete. Check `ortak log\n--session ortak-{id}` against `git diff --stat` before you open a PR.",
            id = session.id
        );
    }

    if push {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&ws.root)
            .args(["push", "-u", &remote_for(&repo, cfg), &branch_name])
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

fn entry_for(path: &str, id: Oid, mode: u32, size: usize) -> IndexEntry {
    IndexEntry {
        ctime: IndexTime::new(0, 0),
        mtime: IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        file_size: size as u32,
        id,
        flags: 0,
        flags_extended: 0,
        path: path.as_bytes().to_vec(),
    }
}

/// The tree the replay starts from: each touched file as the base branch has
/// it, copied into the shadow object database.
///
/// The shadow repository's own root is the workspace as `ortak init` found it,
/// which drifts further from the base branch every day the workspace lives. A
/// file another session created after init is missing there, so replaying onto
/// it turned every later edit of that file into a phantom conflict.
fn base_seed<'r>(
    shadow: &'r Repository,
    repo: &Repository,
    base_tree: &Tree,
    files: &[(String, String)],
) -> Result<Commit<'r>> {
    let mut index = git2::Index::new()?;
    for (file, _) in files {
        let Ok(entry) = base_tree.get_path(Path::new(file)) else {
            continue; // the session created it; nothing to seed
        };
        let data = repo.find_blob(entry.id())?.content().to_vec();
        let id = shadow.blob(&data)?;
        index.add(&entry_for(file, id, entry.filemode() as u32, data.len()))?;
    }
    let tree = shadow.find_tree(index.write_tree_to(shadow)?)?;
    let sig = Signature::now("ortak", "publish@ortak.local")?;
    let oid = shadow.commit(None, &sig, &sig, "publish base", &tree, &[])?;
    Ok(shadow.find_commit(oid)?)
}

/// Rebuild the workspace as if only this session had touched it, by replaying
/// its own shadow micro-commits onto the base branch's content.
///
/// Each micro-commit changes exactly one file, so its diff against its own
/// parent is precisely this session's change at that moment, even when the
/// parent already carries another session's work. Cherry-picking gives a
/// three-way merge, which absorbs the line shifts a concurrent session causes.
/// The gate keeps sessions `margin_lines` apart, which is why those shifts stay
/// outside the patch context in practice.
///
/// Returns the replayed tree and the files that had to be left out of it.
fn session_only_tree<'r>(
    shadow: &'r Repository,
    seed: &Commit<'r>,
    base_tree: &Tree,
    commits: &[String],
) -> Result<(Tree<'r>, Vec<String>)> {
    let mut head = seed.clone();
    let mut unreplayable: Vec<String> = Vec::new();
    let sig = Signature::now("ortak", "publish@ortak.local")?;
    for id in commits {
        let oid = Oid::from_str(id).with_context(|| format!("malformed shadow commit id {id}"))?;
        let pick = shadow
            .find_commit(oid)
            .with_context(|| format!("shadow commit {id} is missing from the journal"))?;
        let mut merged = shadow.cherrypick_commit(&pick, &head, 0, None)?;
        if merged.has_conflicts() {
            let paths: Vec<String> = merged
                .conflicts()?
                .flatten()
                .filter_map(|c| c.our.or(c.their).or(c.ancestor))
                .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                .collect();
            // A file the base branch does not carry is not a content clash: this
            // session built on another session's work that has not shipped yet.
            // Drop the commit and keep replaying, so one such file costs its own
            // file and nothing else; `run` reports what was left out.
            let unshipped: Vec<&String> = paths
                .iter()
                .filter(|p| base_tree.get_path(Path::new(p)).is_err())
                .collect();
            if !unshipped.is_empty() && unshipped.len() == paths.len() {
                for p in unshipped {
                    if !unreplayable.contains(p) {
                        unreplayable.push(p.clone());
                    }
                }
                continue;
            }
            // Whose edits are already at those lines is not something the merge
            // reports, and the old message guessed "another session". Often it
            // is the publishing session's own work from an earlier branch, and
            // the guess sends the reader looking for a collaborator who is not
            // there.
            let flags = format!("--exclude {}", paths.join(" --exclude "));
            bail!(
                "cannot replay this session's change to {}: the merge could not separate it from the edits already at those lines. They may be another session's or this session's own, already published; publish cannot tell which. Run `ortak log` to see who else is in the file, or ship the rest of the session's work with {}",
                paths.join(", "),
                flags
            );
        }
        let tree = shadow.find_tree(merged.write_tree_to(shadow)?)?;
        let next = shadow.commit(None, &sig, &sig, "replay", &tree, &[&head])?;
        head = shadow.find_commit(next)?;
    }
    Ok((head.tree()?, unreplayable))
}

/// Whether the branch's content for a file differs from what the session last
/// wrote to it.
///
/// The replay rebuilds each file from the session's own micro-commits. Lose one
/// in the middle and the replay still succeeds, quietly producing a file the
/// session never had: round 1 shipped exactly that, and `cargo clippy` in a
/// verify worktree was the first thing to notice. The session's newest
/// micro-commit holds the content it last put in the file, so the two should
/// agree.
///
/// ponytail: only a file this session alone has journaled edits on can be
/// checked. Where another session also touched it the replay is supposed to
/// differ, which is the whole point of it, so those are skipped in silence. A
/// base branch that has moved under the session produces the same difference on
/// a file only it touched, which is why this warns rather than refuses.
fn differs_from_last_write(
    db: &Db,
    shadow: &Repository,
    session_id: i64,
    file: &str,
    replayed: &[u8],
) -> Result<bool> {
    if db.shared_file(session_id, file)? {
        return Ok(false);
    }
    let Some(commit) = db.last_commit_for(session_id, file)? else {
        return Ok(false);
    };
    let Some(last) = blob_at(shadow, &commit, file) else {
        return Ok(false);
    };
    Ok(last != replayed)
}

/// A file's content at a shadow commit, when both are still readable.
fn blob_at(shadow: &Repository, commit: &str, file: &str) -> Option<Vec<u8>> {
    let oid = Oid::from_str(commit).ok()?;
    let tree = shadow.find_commit(oid).ok()?.tree().ok()?;
    let entry = tree.get_path(Path::new(file)).ok()?;
    Some(shadow.find_blob(entry.id()).ok()?.content().to_vec())
}

/// Drop the `--exclude` paths from the publish, returning the ones that matched
/// nothing. A mistyped path is silent otherwise, and the file it was meant to
/// keep out of the branch ships anyway.
fn drop_excluded(files: &mut Vec<(String, String)>, exclude: &[String]) -> Vec<String> {
    let mut unmatched = Vec::new();
    for pattern in exclude {
        let path = pattern.trim_start_matches("./");
        let before = files.len();
        files.retain(|(f, _)| f != path);
        if files.len() == before {
            unmatched.push(pattern.clone());
        }
    }
    unmatched
/// The push remote: `ortak.remote` in git config, then ortak.toml, then origin.
/// One contributor pushes to a fork while another pushes to upstream, so this is
/// a per-clone setting; git config is where per-clone settings already live, and
/// it survives the `.ortak` wipes that resetting a workspace involves.
fn remote_for(repo: &Repository, cfg: &Config) -> String {
    repo.config()
        .and_then(|c| c.get_string("ortak.remote"))
        .ok()
        .or_else(|| cfg.publish.remote.clone())
        .unwrap_or_else(|| "origin".to_string())
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

    fn lines(edits: &[(usize, &str)]) -> String {
        let mut v: Vec<String> = (1..=40).map(|i| format!("line_{i}")).collect();
        for (n, text) in edits {
            v[n - 1] = (*text).to_string();
        }
        v.join("\n") + "\n"
    }

    fn tree_with<'r>(repo: &'r Repository, content: Option<&str>) -> Tree<'r> {
        let mut tb = repo.treebuilder(None).unwrap();
        if let Some(c) = content {
            let blob = repo.blob(c.as_bytes()).unwrap();
            tb.insert("app.py", blob, 0o100644).unwrap();
        }
        repo.find_tree(tb.write().unwrap()).unwrap()
    }

    /// One shadow micro-commit: `content` as the only file, on top of `parent`.
    fn commit(repo: &Repository, parent: Option<&Commit>, content: Option<&str>) -> Oid {
        let tree = tree_with(repo, content);
        let sig = Signature::now("t", "t@t.t").unwrap();
        let parents: Vec<&Commit> = parent.into_iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, "e", &tree, &parents)
            .unwrap()
    }

    fn parentless<'r>(repo: &'r Repository, tree: &Tree) -> Commit<'r> {
        let sig = Signature::now("t", "t@t.t").unwrap();
        let oid = repo.commit(None, &sig, &sig, "base", tree, &[]).unwrap();
        repo.find_commit(oid).unwrap()
    }

    fn file_in(tree: &Tree, repo: &Repository) -> String {
        named_file_in(tree, repo, "app.py")
    }

    fn named_file_in(tree: &Tree, repo: &Repository, name: &str) -> String {
        let entry = tree.get_path(Path::new(name)).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        String::from_utf8_lossy(blob.content()).into_owned()
    }

    /// A micro-commit touching one named file, on top of whatever `parent` has.
    fn commit_file(repo: &Repository, parent: Option<&Commit>, name: &str, content: &str) -> Oid {
        let mut tb = match parent {
            Some(p) => repo.treebuilder(Some(&p.tree().unwrap())).unwrap(),
            None => repo.treebuilder(None).unwrap(),
        };
        let blob = repo.blob(content.as_bytes()).unwrap();
        tb.insert(name, blob, 0o100644).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let sig = Signature::now("t", "t@t.t").unwrap();
        let parents: Vec<&Commit> = parent.into_iter().collect();
        repo.commit(None, &sig, &sig, "e", &tree, &parents).unwrap()
    }

    fn scratch(name: &str) -> (std::path::PathBuf, Repository) {
        let dir = std::env::temp_dir().join(format!("ortak-replay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = Repository::init(&dir).unwrap();
        (dir, repo)
    }

    /// Two sessions edit distant lines of one file, which the gate permits, and
    /// their micro-commits interleave. Replaying session A must reproduce only
    /// A's lines: publishing A's branch may not carry B's work.
    #[test]
    fn replay_excludes_a_concurrent_session() {
        let (dir, repo) = scratch("concurrent");
        let root = commit(&repo, None, Some(&lines(&[])));
        let root = repo.find_commit(root).unwrap();
        let a1 = commit(&repo, Some(&root), Some(&lines(&[(3, "AAA-first")])));
        let a1c = repo.find_commit(a1).unwrap();
        let b1 = commit(
            &repo,
            Some(&a1c),
            Some(&lines(&[(3, "AAA-first"), (30, "BBB")])),
        );
        let b1c = repo.find_commit(b1).unwrap();
        // A edits again, on top of a tree that already contains B's line.
        let a2 = commit(
            &repo,
            Some(&b1c),
            Some(&lines(&[(3, "AAA-first"), (30, "BBB"), (5, "AAA-second")])),
        );

        let base = tree_with(&repo, Some(&lines(&[])));
        let seed = parentless(&repo, &base);
        let picks = vec![a1.to_string(), a2.to_string()];
        let (tree, skipped) = session_only_tree(&repo, &seed, &base, &picks).unwrap();
        assert!(skipped.is_empty());
        let out = file_in(&tree, &repo);

        assert!(out.contains("AAA-first"), "A's first edit is missing");
        assert!(out.contains("AAA-second"), "A's second edit is missing");
        assert!(
            !out.contains("BBB"),
            "session B's edit leaked into A's branch:\n{out}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replay_of_nothing_is_the_base_branch() {
        let (dir, repo) = scratch("empty");
        let base = tree_with(&repo, Some(&lines(&[])));
        let seed = parentless(&repo, &base);
        let (tree, _) = session_only_tree(&repo, &seed, &base, &[]).unwrap();
        assert_eq!(file_in(&tree, &repo), lines(&[]));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One session creates a file, it ships, and a later session edits it. The
    /// shadow root predates the file entirely, so replaying onto the root used
    /// to fail; the base branch is what the branch is built on, so that is what
    /// the replay has to start from.
    #[test]
    fn replay_handles_a_file_created_after_init() {
        let (dir, repo) = scratch("after-init");
        let root = commit(&repo, None, None); // app.py does not exist yet
        let root = repo.find_commit(root).unwrap();
        let created = commit(&repo, Some(&root), Some(&lines(&[(3, "AAA-created")])));
        let created_c = repo.find_commit(created).unwrap();
        let edited = commit(
            &repo,
            Some(&created_c),
            Some(&lines(&[(3, "AAA-created"), (30, "BBB-edit")])),
        );

        // Session A's file has since been merged, so the base branch carries it.
        let base = tree_with(&repo, Some(&lines(&[(3, "AAA-created")])));
        let seed = parentless(&repo, &base);
        let (tree, _) = session_only_tree(&repo, &seed, &base, &[edited.to_string()]).unwrap();
        assert!(file_in(&tree, &repo).contains("BBB-edit"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same shape, except the file has not shipped, and the session also
    /// touched a file of its own. One unreplayable file used to take the whole
    /// session's work down with it; now it costs only itself.
    #[test]
    fn replay_leaves_out_a_dependency_on_unshipped_work() {
        let (dir, repo) = scratch("unshipped");
        // Another session created dep.py, and it is on no branch yet.
        let theirs = commit_file(&repo, None, "dep.py", &lines(&[(3, "THEIRS")]));
        let theirs = repo.find_commit(theirs).unwrap();
        let touched = commit_file(
            &repo,
            Some(&theirs),
            "dep.py",
            &lines(&[(3, "THEIRS"), (30, "MINE-edit")]),
        );
        let touched_c = repo.find_commit(touched).unwrap();
        let own = commit_file(
            &repo,
            Some(&touched_c),
            "mine.py",
            &lines(&[(1, "MINE-new")]),
        );

        let base = tree_with(&repo, None); // nothing shipped
        let seed = parentless(&repo, &base);
        let picks = vec![touched.to_string(), own.to_string()];
        let (tree, skipped) = session_only_tree(&repo, &seed, &base, &picks).unwrap();

        assert_eq!(skipped, vec!["dep.py".to_string()]);
        assert!(named_file_in(&tree, &repo, "mine.py").contains("MINE-new"));
        assert!(
            tree.get_path(Path::new("dep.py")).is_err(),
            "an unshipped dependency leaked into the branch"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A genuine content clash, which the replay cannot resolve and cannot
    /// attribute either. The message has to name the file and stop there.
    #[test]
    fn a_conflict_names_the_file_and_not_a_culprit() {
        let (dir, repo) = scratch("conflict");
        let root = commit(&repo, None, Some(&lines(&[(3, "EARLIER")])));
        let root = repo.find_commit(root).unwrap();
        let mine = commit(&repo, Some(&root), Some(&lines(&[(3, "MINE")])));

        // The base branch has its own line 3, so the pick has nowhere to land.
        let base = tree_with(&repo, Some(&lines(&[(3, "ALREADY-SHIPPED")])));
        let seed = parentless(&repo, &base);
        let err = session_only_tree(&repo, &seed, &base, &[mine.to_string()])
            .unwrap_err()
            .to_string();

        assert!(err.contains("app.py"), "{err}");
        assert!(err.contains("--exclude app.py"), "{err}");
        assert!(!err.contains("another session's edits"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A journal that lost a row in the middle replays into a file the session
    /// never had. Nothing else notices, so publish has to.
    #[test]
    fn a_branch_missing_a_journal_row_is_flagged() {
        let (dir, repo) = scratch("stale");
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let mine = db
            .upsert_session("claude-a", "claude-a", "llm", None)
            .unwrap();

        let first = commit_file(&repo, None, "app.py", &lines(&[(3, "ONE")]));
        let first_c = repo.find_commit(first).unwrap();
        let second = commit_file(
            &repo,
            Some(&first_c),
            "app.py",
            &lines(&[(3, "ONE"), (9, "TWO")]),
        );
        db.insert_edit(mine, "app.py", "modify", Some(&first.to_string()), &[])
            .unwrap();
        db.insert_edit(mine, "app.py", "modify", Some(&second.to_string()), &[])
            .unwrap();

        let short = lines(&[(3, "ONE")]);
        let whole = lines(&[(3, "ONE"), (9, "TWO")]);
        assert!(differs_from_last_write(&db, &repo, mine, "app.py", short.as_bytes()).unwrap());
        assert!(!differs_from_last_write(&db, &repo, mine, "app.py", whole.as_bytes()).unwrap());

        // Once another session is in the file too, the replay is meant to
        // differ and there is nothing left to assert.
        let theirs = db
            .upsert_session("claude-b", "claude-b", "llm", None)
            .unwrap();
        db.insert_edit(theirs, "app.py", "modify", None, &[])
            .unwrap();
        assert!(!differs_from_last_write(&db, &repo, mine, "app.py", short.as_bytes()).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exclude_drops_the_named_file_and_reports_a_miss() {
        let mut files = vec![
            ("src/db.rs".to_string(), "modify".to_string()),
            ("notes.md".to_string(), "create".to_string()),
        ];
        let unmatched = drop_excluded(&mut files, &["./notes.md".into(), "src/gone.rs".into()]);
        assert_eq!(files, vec![("src/db.rs".to_string(), "modify".to_string())]);
        assert_eq!(unmatched, vec!["src/gone.rs".to_string()]);
    }

    /// Excluding a file has to take its commits out of the replay as well.
    /// Leaving them in let a file nobody was publishing fail the publish.
    #[test]
    fn an_excluded_file_is_not_replayed_either() {
        let (dir, repo) = scratch("excluded-replay");
        let root = commit_file(&repo, None, "app.py", &lines(&[]));
        let root_c = repo.find_commit(root).unwrap();
        // A commit on a file that will be excluded, in between two that are kept.
        let dropped = commit_file(&repo, Some(&root_c), "drop.py", "unrelated\n");
        let dropped_c = repo.find_commit(dropped).unwrap();
        let kept = commit_file(
            &repo,
            Some(&dropped_c),
            "app.py",
            &lines(&[(4, "KEPT-edit")]),
        );

        let mut files = vec![
            ("app.py".to_string(), "modify".to_string()),
            ("drop.py".to_string(), "create".to_string()),
        ];
        drop_excluded(&mut files, &["drop.py".into()]);
        let all = vec![
            (dropped.to_string(), "drop.py".to_string()),
            (kept.to_string(), "app.py".to_string()),
        ];
        let commits: Vec<String> = all
            .into_iter()
            .filter(|(_, f)| files.iter().any(|(g, _)| g == f))
            .map(|(c, _)| c)
            .collect();
        assert_eq!(commits, vec![kept.to_string()]);

        let base = tree_with(&repo, Some(&lines(&[])));
        let seed = parentless(&repo, &base);
        let (tree, skipped) = session_only_tree(&repo, &seed, &base, &commits).unwrap();
        assert!(skipped.is_empty());
        assert!(file_in(&tree, &repo).contains("KEPT-edit"));
        assert!(tree.get_path(Path::new("drop.py")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    #[test]
    fn remote_resolution_order() {
        let dir = std::env::temp_dir().join(format!("ortak-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = Repository::init(&dir).unwrap();
        let mut cfg = Config::default();

        // Nothing configured anywhere.
        assert_eq!(remote_for(&repo, &cfg), "origin");

        // ortak.toml alone still works, for anyone already relying on it.
        cfg.publish.remote = Some("upstream".into());
        assert_eq!(remote_for(&repo, &cfg), "upstream");

        // git config wins: it is the per-clone setting.
        repo.config()
            .unwrap()
            .set_str("ortak.remote", "fork")
            .unwrap();
        assert_eq!(remote_for(&repo, &cfg), "fork");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
