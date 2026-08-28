use crate::config::Config;
use crate::db::{Db, PublishRow, Session};
use crate::workspace::Workspace;
use anyhow::{anyhow, bail, Context, Result};
use git2::{BranchType, Commit, IndexEntry, IndexTime, Oid, Repository, Signature, Tree};
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

/// What one publish was asked to do. A struct because every branch that adds a
/// flag adds an argument, and three of them together cross clippy's limit of
/// seven at merge time rather than in the change that caused it. A field has no
/// such limit.
pub struct PublishOpts<'a> {
    pub branch: Option<&'a str>,
    pub base: Option<&'a str>,
    pub exclude: &'a [String],
    /// Workspace-relative directory whose files this branch is limited to, for
    /// a session that has edited more than one repository in the tree.
    pub repo: Option<&'a str>,
    pub scope: Scope,
    pub push: bool,
    /// Subject line for this branch's commit, in place of the session intent.
    pub message: Option<&'a str>,
    /// Run the whole publish and stop before anything is recorded: no branch,
    /// no publish row, no freed regions, no push.
    pub dry_run: bool,
    /// Ship a file whose history cannot be replayed as its net change instead
    /// of failing the publish. Per file, and only where the replay was blocked.
    pub squash: bool,
}

/// Assemble a session's net change into a real branch on the workspace's git
/// repo: base tree + the session's own content, replayed from its shadow
/// micro-commits so concurrent sessions' edits stay out of the branch.
/// The live working directory is never touched (no checkout).
pub fn run(ws: &Workspace, cfg: &Config, session_ref: &str, opts: PublishOpts) -> Result<()> {
    let PublishOpts {
        branch: branch_override,
        base: base_override,
        exclude,
        repo: repo_only,
        scope,
        push,
        message: subject,
        dry_run,
        squash,
    } = opts;
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;
    // One session runs several tasks, so shipping everything it ever touched
    // puts the finished work back into every later branch. Default to what came
    // after the last publish; --all rebuilds a branch holding all of it.
    let history = db.publishes(session.id)?;
    let previous = history.first().cloned();
    // An amend goes back further, to where the branch it is landing on began.
    // The journal answers that much on its own; the half that reads git waits
    // until the file list has named the repository the branch lives in.
    let after = match scope {
        Scope::Amend => amend_reach(&history, branch_override)?.1,
        Scope::All => 0,
        Scope::New => previous.as_ref().map_or(0, |p| p.last_edit_id),
    };
    // Read the high-water mark before the file list, never after: an edit that
    // lands in between is then republished rather than silently dropped.
    let head_edit = db.max_edit_id(session.id)?.unwrap_or(0);
    let mut files = db.session_files(session.id, after)?;
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

    let before_exclude: Vec<String> = files.iter().map(|(f, _)| f.clone()).collect();
    for pattern in drop_excluded(&mut files, exclude) {
        println!(
            "warning: --exclude {} matched none of ortak-{}'s files",
            pattern, session.id
        );
    }
    // --repo is --exclude read the other way round, and it belongs here for the
    // same reason: `held_back` further down reads what left this list, and the
    // mark stops short of the first of them. Get that wrong and a session that
    // publishes one repository drops the other repository's edits out of every
    // publish after it.
    // `starts_with` is component-wise, so `--repo repos/alt` does not take
    // `repos/altinkaya`, and the `./` shell completion adds is trimmed the way
    // `--exclude` trims it.
    if let Some(dir) = repo_only.map(|d| d.trim_start_matches("./")) {
        files.retain(|(f, _)| Path::new(f).starts_with(dir));
        if files.is_empty() {
            bail!(
                "ortak-{} changed nothing under {}; `ortak publish ortak-{} --dry-run` lists what it did change",
                session.id,
                dir,
                session.id
            );
        }
    }
    if files.is_empty() {
        bail!(
            "every file ortak-{} changed was excluded; nothing left to publish",
            session.id
        );
    }
    // Every path the journal holds is workspace-relative, and one workspace can
    // hold sixty repositories. The branch belongs in the one its files live in,
    // and from here on `prefix` is the distance between the two: empty wherever
    // the workspace root is the repository, which is every workspace ortak had
    // before this one.
    let (prefix, repo) = owning_repo(ws, &files)?;
    let amending = match scope {
        Scope::Amend => Some(amend_target(&repo, &history, branch_override, &session)?),
        _ => None,
    };
    // An amend rebuilds on its own branch's parent and never reads the trunk,
    // so it keeps the configured name for its messages rather than resolving a
    // branch it is not going to build on.
    let chose_base = match amending {
        Some(_) => base_branch(cfg, base_override).to_string(),
        None => base_here(&repo, cfg, base_override, &prefix)?,
    };
    let base = chose_base.as_str();
    // Settled before the branch is built, not after. Nothing is lost by
    // stopping here: the branch is a function of the journal, so the same one
    // comes back once the remote is named. Stopping after the build would leave
    // a publish row and a moved high-water mark behind, and the re-run then
    // answers "already carries that work" instead of pushing, which is the trap
    // `push_advice` exists to talk people out of. It sits below the file list so
    // that a session with nothing to publish hears that first: a push that never
    // happens has no remote to be wrong about. A rehearsal reports it instead of
    // raising it, because reporting what the real run does is the whole job.
    let unsettled = push.then(|| unsettled_remote(&repo, cfg)).flatten();
    if let (Some(problem), false) = (&unsettled, dry_run) {
        bail!("{problem}");
    }

    // An amend rebuilds the branch where it already stands, so leaving --base
    // off does not quietly rebase a stacked branch onto the trunk and pull its
    // base's work into the diff. An explicit --base still wins while this
    // session's own commit is the one being rewritten, which is how a branch
    // moves. On a branch it did not publish, --base would point the branch
    // somewhere else entirely and drop what it carries, and dropping somebody
    // else's commit is the one thing an amend must never do.
    let base_commit = match (amending, base_override) {
        (Some(a), Some(b)) if !a.rewrites => bail!(
            "ortak-{} did not publish {}, so --amend puts this work on top of what that branch already carries; --base {} would move it off that. Publish this work as its own branch instead",
            session.id,
            a.branch,
            b
        ),
        (Some(a), None) => repo.find_commit(a.base)?,
        // Before the replay, not after it: a replay that succeeds is exactly
        // how a stale trunk gets away with it, and by the time anything else
        // speaks the branch is already built. An amend never lands here, and
        // rightly: it rebuilds on its own branch's parent, and there is no
        // trunk in that answer to have fallen behind anything.
        _ => {
            if let Some(warning) = base_behind_warning(&repo, &remote_for(&repo, cfg), base) {
                println!("{warning}");
            }
            base_commit_for(&repo, base)?
        }
    };
    let base_tree = base_commit.tree()?;

    // Somebody who adds a file to .gitignore means it never leaves this
    // machine. The daemon stops journaling it from that moment, but the rows it
    // wrote before the rule existed are still here, and the branch shipped the
    // file anyway, at whatever version it had when the rule landed. A secret,
    // a build artifact or a model file goes out stale, which is the worst of
    // both.
    let ignored = ignored_now(&repo, &base_tree, &files, &prefix);
    for file in &ignored {
        println!(
            "warning: {file} is ignored by this project now, so it is not going into the branch; \
             `ortak release ortak-{} {file}` drops it from the session for good",
            session.id
        );
    }
    files.retain(|(f, _)| !ignored.contains(f));
    if files.is_empty() {
        bail!(
            "every file ortak-{} changed is ignored by this project; nothing left to publish",
            session.id
        );
    }

    // The gate lets two sessions edit distant lines of one file, so the file on
    // disk can hold another session's work. Rebuild this session's own content
    // from its shadow history instead of reading the workspace.
    let shadow = crate::shadow::open(ws)?;
    let seed = base_seed(&shadow, &repo, &base_tree, &files, &prefix)?;
    // An excluded file leaves the replay too, not just the branch. Its commits
    // can still fail to apply, and a file nobody asked to publish has no
    // business failing the publish.
    let session_commits: Vec<(String, String)> = db
        .session_commits(session.id, after)?
        .into_iter()
        .filter(|(_, f)| files.iter().any(|(g, _)| g == f))
        .collect();
    // A file another session has journaled and not released is genuinely built
    // on work that has not shipped. One it has released is not, whatever the
    // shadow history still looks like. Erring towards "theirs" keeps today's
    // behaviour when the lookup fails.
    let still_theirs = |file: &str| -> bool { db.shared_file(session.id, file).unwrap_or(true) };
    // A blocked file costs the whole publish, so `--squash` takes its commits
    // out of the replay and puts its net change back afterwards. Round-trip
    // rather than one pass: the replay stops at the first file it cannot
    // apply, so a second blocked file only shows up once the first is out of
    // the way. Each turn removes a file, so this ends.
    let mut squashed: Vec<String> = Vec::new();
    let (session_tree, unreplayable) = loop {
        let commits: Vec<String> = session_commits
            .iter()
            .filter(|(_, f)| !squashed.contains(f))
            .map(|(c, _)| c.clone())
            .collect();
        let replay = match session_only_tree(
            &shadow,
            &seed,
            &base_tree,
            &commits,
            &still_theirs,
            &prefix,
        ) {
            Ok(replayed) => replayed,
            // The replay merges this session's edits into the base branch's content,
            // so a checkout that never caught up with the base fails it on every
            // file at once. That reads exactly like a collision with another
            // session, and the message below used to say so.
            // Only when the base is the one this workspace tracks. A publish that
            // names another branch with --base is deliberately stacking on work the
            // checkout does not have, so being behind it is the point rather than
            // the fault, and saying "update the checkout" there sends the reader to
            // fix the one thing that is not wrong.
            Err(e) => match commits_behind(&repo, &base_commit).filter(|_| base_override.is_none())
            {
                Some(n) => bail!(
                    "{e}\n\nthis workspace is {n} commit(s) behind {base}, which is enough on its own to fail the replay: publish rebuilds each file on {base}'s content, not on the older content you have been editing. Update the checkout and publish again; --exclude will not help while it is behind."
                ),
                None => return Err(e),
            },
        };
        match replay {
            Replay::Tree(tree, unreplayable) => break (tree, unreplayable),
            Replay::Blocked(paths) if squash && paths.iter().any(|p| !squashed.contains(p)) => {
                for p in paths {
                    if !squashed.contains(&p) {
                        squashed.push(p);
                    }
                }
            }
            // The advice a blocked replay owes the reader is which branch already
            // holds those lines, and only here is the publish history in reach.
            Replay::Blocked(paths) => bail!(blocked_message(&db, &history, session.id, &paths)?),
        }
    };
    let session_tree = squashed.iter().try_fold(session_tree, |tree, file| {
        squash_file(&shadow, &tree, &session_commits, file)
    })?;

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
            base
        );
    }
    // Everything the session changed that this branch is not carrying, either
    // because `--exclude` held it back or because the replay could not rebuild
    // it. The mark has to stop short of the first of those, or the next
    // incremental publish starts past work that has never shipped. Read after
    // the file list rather than before, unlike `head_edit`: this can only lower
    // the mark, and a lower mark republishes rather than drops.
    let held_back: Vec<String> = before_exclude
        .iter()
        .filter(|f| !files.iter().any(|(g, _)| g == *f))
        .cloned()
        .collect();
    let mark = match db.first_edit_on(session.id, after, &held_back)? {
        Some(first) => first - 1,
        None => head_edit,
    };

    // Build the branch tree in an in-memory index: base tree + session files.
    let mut index = git2::Index::new()?;
    index.read_tree(&base_tree)?;
    let mut stale: Vec<String> = Vec::new();
    for (file, kind) in &files {
        // The index being built is the repository's, so every path that reaches
        // it drops the prefix; the shadow tree beside it keeps the workspace's.
        let here = in_repo(&prefix, file);
        if kind == "delete" {
            let _ = index.remove_path(here);
            continue;
        }
        let tracked = session_tree
            .get_path(Path::new(file))
            .with_context(|| format!("{} is missing from this session's replayed history", file))?;
        let data = shadow.find_blob(tracked.id())?.content().to_vec();
        let on_base = base_tree
            .get_path(here)
            .ok()
            .and_then(|e| repo.find_blob(e.id()).ok())
            .map(|b| b.content().to_vec());
        // A squashed file is expected to differ: its net change was merged onto
        // whatever the base branch has now, and the base moving is the reason
        // it was squashed. Sending the reader after a lost journal row there
        // would point at the one thing that is not wrong.
        // What the file held when this slice began: the parent of the session's
        // first micro-commit in it. `session_commits` is in the order the replay
        // applies them, so the first row for a file is that commit.
        let started_from = session_commits
            .iter()
            .find(|(_, f)| f == file)
            .and_then(|(commit, _)| parent_blob(&shadow, commit, file));
        if !squashed.contains(file)
            && differs_from_last_write(
                &db,
                &shadow,
                session.id,
                file,
                after,
                Versions {
                    replayed: &data,
                    on_base: on_base.as_deref(),
                    started_from: started_from.as_deref(),
                },
            )?
        {
            stale.push(file.clone());
        }
        let mode = tracked.filemode() as u32;
        // The base branch may still hold a directory where this session now has
        // a file. `index.add` drops the entry silently while those paths are
        // there, so the branch shipped the old directory and none of the change
        // that replaced it. Nothing can live under a path that is about to be a
        // file, so this only ever removes what the file replaces.
        let _ = index.remove_dir(here, 0);
        // The blob lives in the shadow object database; copy it into the project repo.
        let blob_id = repo.blob(&data)?;
        index.add(&entry_for(
            &here.to_string_lossy(),
            blob_id,
            mode,
            data.len(),
        ))?;
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
    // `intent` was written when a session was one task. Since publish went
    // incremental it is several, so a session either keeps the intent broad and
    // every commit message names all three deliverables, or rewrites it before
    // each publish and forgets. --message says what this branch is and leaves
    // the intent to `status`, where the other sessions read it.
    let subject = subject.unwrap_or(&intent);
    let message = format!(
        "{}\n\nOrtak-Session: {}\nOrtak-Agent: {}\nOrtak-Files: {}\n",
        subject,
        session.external_id,
        session.agent_name,
        files.len()
    );
    // The person, not the session. Which session wrote it is already three
    // lines further up in the trailers, and the journal is the real record; the
    // author field is what a forge matches to an account, what `git shortlog`
    // counts and what a signature covers, and `ortak-3@ortak.local` matches
    // nothing, so every branch published here arrived from a ghost.
    //
    // A repository with no identity configured is the one case where the old
    // name is better than failing, since publish is not the place to teach
    // somebody `git config user.email`.
    let sig = match repo.signature() {
        Ok(sig) => sig,
        Err(_) => {
            let email = format!("ortak-{}@ortak.local", session.id);
            Signature::now(&session.agent_name, &email)?
        }
    };
    let commit_oid = repo.commit(None, &sig, &sig, &message, &tree, &[&base_commit])?;

    let branch_name = match branch_override {
        Some(b) => b.to_string(),
        None => branch_name_for(
            &cfg.publish.branch_prefix,
            session.id,
            session.task_intent.as_deref(),
        ),
    };
    let amend = amending.is_some();
    let rewrites = amending.is_some_and(|a| a.rewrites);
    // Everything above this line reads; everything below it records. A dry run
    // is the same publish with the recording left out.
    let (cooled, affected) = if dry_run {
        let (_, affected) = crate::impact::scan(ws, cfg, &db, session.id).unwrap_or_default();
        (0, affected)
    } else {
        repo.branch(&branch_name, &repo.find_commit(commit_oid)?, amend)
            .with_context(|| {
                format!(
                    "could not create branch {} (does it already exist?)",
                    branch_name
                )
            })?;
        // Only now: a failed publish must not move the session's high-water
        // mark. Rewriting this session's own commit moves that branch's mark;
        // adopting another branch records the first publish on it.
        //
        // ponytail: the row records a branch name and no repository. Two
        // repositories in one tree can hold a branch of the same name, and this
        // journal cannot tell them apart, so `--amend` and "already carries that
        // work" answer for whichever row matched the name first. A repository
        // column is the fix and it is a schema change; parked as an idea rather
        // than grown onto this branch.
        if rewrites {
            db.amend_publish(session.id, &branch_name, mark)?;
        } else {
            db.record_publish(session.id, &branch_name, mark)?;
        }

        free_lines_and_scan(ws, cfg, &db, session.id, &files)?
    };

    println!(
        "{}: {} ({} files, commit {})",
        match (dry_run, amending) {
            (true, Some(a)) if a.rewrites => "dry run, would rewrite",
            (true, Some(_)) => "dry run, would extend",
            (true, None) => "dry run, would build",
            (false, Some(a)) if a.rewrites => "branch rewritten",
            (false, Some(_)) => "branch extended",
            (false, None) => "branch ready",
        },
        branch_name,
        files.len(),
        &commit_oid.to_string()[..8]
    );
    for (f, k) in &files {
        println!("  {} {}", net_kind(&base_tree, &prefix, f, k), f);
    }
    if cooled > 0 {
        println!(
            "freed {} protected region(s) on those files; other sessions may edit them now, and `ortak blame` still names this session",
            cooled
        );
    }
    // After the file list, never before it: someone skimming the output has to
    // read what the branch does contain before they can judge what is missing.
    if !skipped.is_empty() {
        println!("\nleft out of this branch:");
        for f in &skipped {
            // The work it builds on is usually another session's, but a session
            // that created a file on an earlier branch of its own lands here
            // too, and then the branch to build on is one it already has.
            match published_branch_for(&db, &history, session.id, f)? {
                Some(b) => println!("  {} - built on {}, which is not on {} yet", f, b, base),
                None => println!(
                    "  {} - built on another session's work that is not on {} yet",
                    f, base
                ),
            }
        }
        println!("the branch is incomplete; publish and merge that work, then publish again");
    }
    // Squashing is not free and a rehearsal that squashed has to say so, or the
    // rehearsal is not one.
    if !squashed.is_empty() {
        println!("\nsquashed, so the branch carries their net change and not their history:");
        for f in &squashed {
            println!("  {}", f);
        }
        println!(
            "their replay was blocked. The merge that keeps a concurrent session's lines out of the branch is given up for those files, so read them{} before the PR",
            // A rehearsal has no branch to diff against yet, and sending
            // somebody to a command that answers "unknown revision" is the
            // rehearsal lying about what it did.
            match dry_run {
                true => String::new(),
                false => format!(" in `git diff {base}..{branch_name}`"),
            }
        );
    }
    for f in &stale {
        println!(
            "\nwarning: {f} on this branch does not match what ortak-{id} last wrote to it.\nEdits are missing from the journal, so the branch is incomplete. Check `ortak log\n--session ortak-{id}` against `git diff --stat` before you open a PR.",
            id = session.id
        );
    }

    if !affected.is_empty() {
        println!("\nother sessions are working in files that use what this branch changed:");
        crate::impact::print_refs(&affected);
        println!("names are matched as text, so read these before believing them");
    }

    if dry_run {
        // A branch name already taken is how a publish fails with everything
        // else about it right, so the rehearsal tries the name too.
        if !amend && repo.find_branch(&branch_name, BranchType::Local).is_ok() {
            println!(
                "\nbranch {branch_name} already exists, so publishing would stop there; pass --branch <name> or --amend"
            );
        }
        println!(
            "\nnothing was created: no branch, no publish record, and this session still holds its lines"
        );
        if push {
            match &unsettled {
                Some(problem) => println!("--push would stop here: {problem}"),
                None => println!(
                    "--push would push {} to {}",
                    branch_name,
                    remote_for(&repo, cfg)
                ),
            }
        }
        return Ok(());
    }

    if push {
        let remote = remote_for(&repo, cfg);
        // Every git command below runs in the repository the branch was built
        // in, which is not the workspace root once the workspace holds more than
        // one. Run from the root, `git push <remote> <branch>` resolves the
        // remote name and the branch in a repository that has neither.
        let at = repo.workdir().unwrap_or(&ws.root).to_path_buf();
        // A stacked branch is unusable on the forge until its base is there too:
        // GitHub and Forgejo both refuse a pull request whose base branch they
        // cannot find, so --base builds the stack locally and then strands it.
        // Only a branch --base named explicitly, and never the configured trunk,
        // which is nobody's to push on a publish's initiative. A remote-tracking
        // ref is skipped too: it is on a remote by definition, and pushing one
        // would put a branch called `origin/main` on the fork.
        if let Some(stack) = base_override
            .filter(|b| repo.find_branch(b, BranchType::Remote).is_err())
            .filter(|b| !on_remote(&at, &remote, b))
        {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&at)
                .args(["push", "-u", &remote, stack])
                .status()?;
            if !status.success() {
                bail!(
                    "git push of the base branch {} failed: {}",
                    stack,
                    push_advice(&repo, &remote, stack, &run_it_from(&prefix))
                );
            }
            println!(
                "pushed base branch {} first; it did not exist on {}",
                stack, remote
            );
        }
        // A rewritten branch is not a fast-forward, so the push has to say so.
        // --force-with-lease rather than --force: if the branch on the remote
        // has moved since this session last saw it, that is somebody else's
        // commit and no amend was ever meant to drop it. A commit added on top
        // of a branch this session did not publish is a fast-forward and needs
        // no lease; forcing there would be a licence to drop exactly the work
        // that path exists to keep.
        let mut args = vec!["push", "-u", remote.as_str(), branch_name.as_str()];
        if rewrites {
            args.insert(1, "--force-with-lease");
        }
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&at)
            .args(&args)
            .status()?;
        if !status.success() {
            bail!(
                "git push failed: {}",
                push_advice(&repo, &remote, &branch_name, &run_it_from(&prefix))
            );
        }
        if !amend {
            // The command has to match the forge. This printed `tea` wherever it
            // pushed, so everybody on GitHub, including this project's own
            // contributors, was handed a Gitea tool they do not have installed.
            let github = repo
                .find_remote(&remote)
                .ok()
                .and_then(|r| r.url().map(|u| u.contains("github.com")))
                .unwrap_or(false);
            let tool = if github { "gh" } else { "tea" };
            println!("\ncreate the PR with{}:", run_it_from(&prefix));
            println!(
                "  {} pr create --base {} --head {} --title \"{}\"",
                tool,
                forge_base(&repo, base),
                branch_name,
                subject
            );
        }
    } else if amend {
        let remote = remote_for(&repo, cfg);
        let from = run_it_from(&prefix);
        if rewrites {
            println!(
                "\n{} moved, so it is no longer a fast-forward. Push it with{}:\n  git push --force-with-lease {} {}",
                branch_name, from, remote, branch_name
            );
        } else {
            println!(
                "\nnot pushed; run{}: git push {} {}",
                from, remote, branch_name
            );
        }
    } else {
        println!("\nnot pushed; run: ortak publish {} --push", session_ref);
    }
    Ok(())
}

/// Hand back the lines this branch just shipped, and report what it may have
/// broken elsewhere. Returns how many regions were freed and what the scan
/// found; `run` prints both, further down, where a reader expects them.
///
/// The two are one function because they are ordered, and nothing else says so.
/// Both read the same `regions` rows: the scan asks which lines this session
/// owns so it can read the names they define, and the cooling hands those very
/// lines back. Taken in the other order the scan sees a session that owns
/// nothing and reports nothing, which is what it did from the round it shipped
/// in until the row happened to survive a publish.
///
/// Once per deliverable is what makes the scan affordable here and not on every
/// prompt; `impact` says what it is for. Advisory: a scan that fails found
/// nothing, because a branch already built must not fail over what might break.
fn free_lines_and_scan(
    ws: &Workspace,
    cfg: &Config,
    db: &Db,
    session_id: i64,
    files: &[(String, String)],
) -> Result<(usize, Vec<crate::impact::Ref>)> {
    let (_, affected) = crate::impact::scan(ws, cfg, db, session_id).unwrap_or_default();
    let mut cooled = 0;
    for (file, _) in files {
        cooled += db.cool_regions(session_id, file)?;
    }
    Ok((cooled, affected))
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
    prefix: &str,
) -> Result<Commit<'r>> {
    let mut index = git2::Index::new()?;
    for (file, _) in files {
        // Read from the repository under its own path, written into the shadow
        // under the workspace's: the seed has to line up with the shadow
        // micro-commits that are about to be replayed onto it.
        let Ok(entry) = base_tree.get_path(in_repo(prefix, file)) else {
            continue; // the session created it; nothing to seed
        };
        // The base branch can hold a directory where this session now has a
        // file: flattening src/thing/mod.rs into src/thing.rs does exactly
        // that, and so does any refactor that collapses a module. Asking for a
        // blob there ended the whole publish, every file of it, with "the
        // requested type does not match the type in the ODB" and no branch.
        // What the replay needs is the directory the file replaces, or its
        // removal reads as a conflict with content the seed never had.
        let blob = match repo.find_blob(entry.id()) {
            Ok(blob) => blob,
            Err(_) => {
                if let Ok(tree) = repo.find_tree(entry.id()) {
                    seed_directory(shadow, repo, &mut index, file, &tree)?;
                }
                continue;
            }
        };
        let data = blob.content().to_vec();
        let id = shadow.blob(&data)?;
        index.add(&entry_for(file, id, entry.filemode() as u32, data.len()))?;
    }
    let tree = shadow.find_tree(index.write_tree_to(shadow)?)?;
    let sig = Signature::now("ortak", "publish@ortak.local")?;
    let oid = shadow.commit(None, &sig, &sig, "publish base", &tree, &[])?;
    Ok(shadow.find_commit(oid)?)
}

/// Copy every file the base branch keeps under `at` into the seed, so a session
/// that replaced that directory with a file starts its replay from what it
/// actually replaced.
fn seed_directory(
    shadow: &Repository,
    repo: &Repository,
    index: &mut git2::Index,
    at: &str,
    tree: &Tree,
) -> Result<()> {
    let mut files: Vec<(String, Oid, u32)> = Vec::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        if let (Some(git2::ObjectType::Blob), Some(name)) = (entry.kind(), entry.name()) {
            files.push((
                format!("{at}/{root}{name}"),
                entry.id(),
                entry.filemode() as u32,
            ));
        }
        git2::TreeWalkResult::Ok
    })?;
    for (path, id, mode) in files {
        let data = repo.find_blob(id)?.content().to_vec();
        let seeded = shadow.blob(&data)?;
        index.add(&entry_for(&path, seeded, mode, data.len()))?;
    }
    Ok(())
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
    still_theirs: &dyn Fn(&str) -> bool,
    prefix: &str,
) -> Result<Replay<'r>> {
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
            let unshipped: Vec<String> = paths
                .iter()
                .filter(|p| base_tree.get_path(in_repo(prefix, p)).is_err())
                .cloned()
                .collect();
            if !unshipped.is_empty() && unshipped.len() == paths.len() {
                // Only while somebody else still claims the file. Once they have
                // released it the conflict is this session's own history with a
                // hole in it, left by a write that was credited to the wrong
                // session, and the session's own content is the answer. Dropping
                // the commit there stranded the file for good: every later
                // commit conflicted the same way, so the whole file left the
                // branch and no release, `--all` or rewrite brought it back.
                let held: Vec<String> = unshipped
                    .iter()
                    .filter(|p| still_theirs(p))
                    .cloned()
                    .collect();
                if !held.is_empty() {
                    for p in held {
                        if !unreplayable.contains(&p) {
                            unreplayable.push(p);
                        }
                    }
                    continue;
                }
                let pick_tree = pick.tree()?;
                for p in &unshipped {
                    let path = Path::new(p);
                    merged.remove_path(path)?;
                    if let Ok(entry) = pick_tree.get_path(path) {
                        let len = shadow.find_blob(entry.id())?.content().len();
                        merged.add(&entry_for(p, entry.id(), entry.filemode() as u32, len))?;
                    }
                }
            } else {
                // Whose edits are already at those lines is not something the
                // merge reports, and the old message guessed "another session".
                // Often it is the publishing session's own work from an earlier
                // branch, which `run` can name from the journal.
                return Ok(Replay::Blocked(paths));
            }
        }
        let tree = shadow.find_tree(merged.write_tree_to(shadow)?)?;
        let next = shadow.commit(None, &sig, &sig, "replay", &tree, &[&head])?;
        head = shadow.find_commit(next)?;
    }
    Ok(Replay::Tree(head.tree()?, unreplayable))
}

/// What one replay produced: the tree and the files left out of it, or the
/// files whose change could not be separated from what is already at their
/// lines. The second is not an error here because the message a reader can act
/// on names one of the session's own branches, and reading those means reading
/// the journal.
enum Replay<'r> {
    Tree(Tree<'r>, Vec<String>),
    Blocked(Vec<String>),
}

/// Put one file's net change into the replayed tree, in place of the history
/// that could not be replayed onto it.
///
/// The replay's price is that every intermediate state has to apply, not just
/// the last one: a session that edits line 3, has the base branch change line 3
/// underneath it, and then rewrites the file without its own line-3 edit is
/// blocked on a hunk it threw away itself, while the content it means to ship
/// merges cleanly. What goes in instead is one synthetic commit carrying the
/// whole slice's change to that file, from the content the session first saw to
/// the content it ended with. It is still a cherry-pick, so a trunk change
/// elsewhere in the file is still merged rather than overwritten; what is given
/// up is the chance to resolve the session's own discarded hunks separately,
/// and any of another session's lines that its snapshots picked up.
///
/// ponytail: the overwrite is the floor, for when even the net diff collides.
/// Nothing below it would be true, so there is no third strategy to add.
fn squash_file<'r>(
    shadow: &'r Repository,
    head: &Tree<'r>,
    commits: &[(String, String)],
    file: &str,
) -> Result<Tree<'r>> {
    let ids: Vec<&str> = commits
        .iter()
        .filter(|(_, f)| f == file)
        .map(|(c, _)| c.as_str())
        .collect();
    let (Some(first), Some(last)) = (ids.first(), ids.last()) else {
        return Ok(head.clone()); // nothing of this file in the slice
    };
    let path = Path::new(file);
    let first = shadow.find_commit(Oid::from_str(first)?)?;
    let last = shadow.find_commit(Oid::from_str(last)?)?;
    let ends_at = last
        .tree()?
        .get_path(path)
        .with_context(|| format!("{file} is missing from shadow commit {}", last.id()))?;
    let ends_at = (ends_at.id(), ends_at.filemode() as u32);
    // What the session started from: the file as the parent of its first
    // micro-commit had it. A session that created the file has no such parent
    // entry, and the empty tree is the right ancestor for one.
    let starts_at = first
        .parent(0)
        .and_then(|p| p.tree())
        .ok()
        .and_then(|t| t.get_path(path).ok())
        .map(|e| (e.id(), e.filemode() as u32));

    let sig = Signature::now("ortak", "publish@ortak.local")?;
    let one_file = |entry: Option<(Oid, u32)>| -> Result<Tree<'r>> {
        let mut index = git2::Index::new()?;
        if let Some((id, mode)) = entry {
            let len = shadow.find_blob(id)?.content().len();
            index.add(&entry_for(file, id, mode, len))?;
        }
        Ok(shadow.find_tree(index.write_tree_to(shadow)?)?)
    };
    let before = shadow.commit(None, &sig, &sig, "squash from", &one_file(starts_at)?, &[])?;
    let before = shadow.find_commit(before)?;
    let net = shadow.commit(
        None,
        &sig,
        &sig,
        "squash to",
        &one_file(Some(ends_at))?,
        &[&before],
    )?;
    let net = shadow.find_commit(net)?;
    // `cherrypick_commit` reads its `ours` from a commit, and the replayed tree
    // has none of its own. Only the tree is looked at.
    let ours = shadow.commit(None, &sig, &sig, "replayed", head, &[])?;
    let mut merged = shadow.cherrypick_commit(&net, &shadow.find_commit(ours)?, 0, None)?;
    if merged.has_conflicts() {
        merged = git2::Index::new()?;
        merged.read_tree(head)?;
        let (id, mode) = ends_at;
        let len = shadow.find_blob(id)?.content().len();
        merged.add(&entry_for(file, id, mode, len))?;
    }
    Ok(shadow.find_tree(merged.write_tree_to(shadow)?)?)
}

/// The error a blocked replay ends the publish with, including which of the
/// session's own branches already carries the file it choked on.
///
/// The hint used to name the session's most recent publish, whatever that was.
/// A session with three branches whose `db.rs` work sits on the first was told
/// to stack on the third, and following that advice fails the same way an
/// hour later, which is worse than no advice at all.
fn blocked_message(
    db: &Db,
    history: &[PublishRow],
    session_id: i64,
    paths: &[String],
) -> Result<String> {
    let flags = format!("--exclude {}", paths.join(" --exclude "));
    // Nobody can know `--squash` exists before they need it: a blocked replay
    // is only reachable by trying, so this error is the one place it can be
    // learned. What it gives up belongs in the same sentence as the offer.
    let mut msg = format!(
        "cannot replay this session's change to {paths}: the merge could not separate it from the edits already at those lines. They may be another session's or this session's own, already published; publish cannot tell which. Run `ortak log` to see who else is in the file, or ship the rest of the session's work with {flags}.\n\n--squash ships {paths} as one net change instead of its history, which gets past a hunk the session has since thrown away. It gives up the merge that keeps a concurrent session's lines out of the branch, and only there: everything else still replays.",
        paths = paths.join(", "),
    );
    for file in paths {
        let Some(branch) = published_branch_for(db, history, session_id, file)? else {
            continue;
        };
        msg.push_str(&format!(
            "\n\nortak-{session_id} already published {file} on {branch}; pass --base {branch} to build this branch on that one."
        ));
    }
    Ok(msg)
}

/// The session's own branch carrying its newest published edit to a file, if
/// any of them does.
fn published_branch_for<'a>(
    db: &Db,
    history: &'a [PublishRow],
    session_id: i64,
    file: &str,
) -> Result<Option<&'a str>> {
    let Some(newest) = history.first() else {
        return Ok(None);
    };
    let Some(edit) = db.last_edit_upto(session_id, file, newest.last_edit_id)? else {
        return Ok(None);
    };
    Ok(branch_carrying(history, edit))
}

/// Which branch's slice an edit fell in. A publish carries everything between
/// the mark before it and its own, so the branch that has an edit is the oldest
/// one whose mark reaches it; `history` arrives newest first.
fn branch_carrying(history: &[PublishRow], edit: i64) -> Option<&str> {
    history
        .iter()
        .rev()
        .find(|p| p.last_edit_id >= edit)
        .map(|p| p.branch.as_str())
}

/// The three versions of one file this check reasons about: what the branch
/// carries, what the base branch has, and what the file held when the slice
/// began. A struct because the three arrive together and the fourth argument of
/// bytes is where clippy stops counting.
struct Versions<'a> {
    replayed: &'a [u8],
    on_base: Option<&'a [u8]>,
    started_from: Option<&'a [u8]>,
}

/// Whether the branch's content for a file differs from what the session last
/// wrote to it inside this publish's slice.
///
/// The replay rebuilds each file from the session's own micro-commits. Lose one
/// in the middle and the replay still succeeds, quietly producing a file the
/// session never had: round 1 shipped exactly that, and `cargo clippy` in a
/// verify worktree was the first thing to notice. The newest micro-commit of
/// the slice holds the content the session last put in the file, so the two
/// should agree.
///
/// They only agree while the base branch already carries what the session wrote
/// *before* the slice. Since publish went incremental, a second deliverable
/// ships the edits after the first one's branch and is built on `main`, which
/// does not have that branch yet, so its replay is supposed to lack those
/// lines. Comparing anyway told both sessions their branch was incomplete every
/// time they came back to a file, which is a false alarm expensive enough to
/// send someone reading `ortak log` against `git diff --stat` for a row that
/// was never lost.
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
    after: i64,
    seen: Versions,
) -> Result<bool> {
    if db.shared_file(session_id, file)? {
        return Ok(false);
    }
    let (in_slice, before_slice) = db.slice_commits(session_id, file, after)?;
    // No edit to this file inside the slice, so this branch makes no claim
    // about its content and there is nothing to check.
    let Some(commit) = in_slice else {
        return Ok(false);
    };
    // The branch is built on the base tree. Unless the base carries what the
    // session left in the file before the slice, the difference below is the
    // earlier slice and not a missing row.
    if let Some(earlier) = before_slice {
        if blob_at(shadow, &earlier, file).as_deref() != seen.on_base {
            return Ok(false);
        }
    }
    // And unless the base still holds what this slice started from. The replay
    // merges the session's edits onto the base branch, so a base that moved
    // under the session produces a file that differs from the session's last
    // write for a reason that has nothing to do with a lost row.
    //
    // This is the disagreement round 11 could not explain: a rehearsal warned
    // twice, the trunk was fast-forwarded, and the real publish a minute later
    // said nothing about a branch that was byte-identical. Two runs, two base
    // trees, one message blaming the journal for both. A moved trunk has its own
    // warning now, printed before the replay and naming the count and the
    // remote, so the quiet here loses nothing a reader needed.
    if let Some(started) = seen.started_from {
        if Some(started) != seen.on_base {
            return Ok(false);
        }
    }
    let Some(last) = blob_at(shadow, &commit, file) else {
        return Ok(false);
    };
    Ok(last != seen.replayed)
}

/// A file as it was just before this shadow commit changed it, which for the
/// first micro-commit of a slice is what the session started that slice from.
fn parent_blob(shadow: &Repository, commit: &str, file: &str) -> Option<Vec<u8>> {
    let oid = Oid::from_str(commit).ok()?;
    let parent = shadow.find_commit(oid).ok()?.parent(0).ok()?;
    blob_at(shadow, &parent.id().to_string(), file)
}

/// A file's content at a shadow commit, when both are still readable.
fn blob_at(shadow: &Repository, commit: &str, file: &str) -> Option<Vec<u8>> {
    let oid = Oid::from_str(commit).ok()?;
    let tree = shadow.find_commit(oid).ok()?.tree().ok()?;
    let entry = tree.get_path(Path::new(file)).ok()?;
    Some(shadow.find_blob(entry.id()).ok()?.content().to_vec())
}

/// The journal speaks workspace-relative paths and a repository speaks its own.
/// They are the same string wherever the workspace root is the repository,
/// which is every workspace ortak had before a tree of them, so an empty prefix
/// hands the path straight back and nothing that calls this changes there.
fn in_repo<'p>(prefix: &str, path: &'p str) -> &'p Path {
    let rel = match prefix.is_empty() {
        true => path,
        false => path
            .strip_prefix(prefix)
            .map_or(path, |r| r.trim_start_matches('/')),
    };
    Path::new(rel)
}

/// What the branch does to a file, which is not always what the journal's last
/// row about it says. A file created and then edited leaves `modify` there, and
/// a branch built on a base that has never held it is adding it. The base tree
/// is the only thing that can answer, and it is already open.
fn net_kind<'k>(base: &Tree, prefix: &str, file: &str, kind: &'k str) -> &'k str {
    match kind == "delete" || base.get_path(in_repo(prefix, file)).is_ok() {
        true => kind,
        false => "create",
    }
}

/// The repository this branch belongs in, and how far its files sit below the
/// workspace root. One workspace can hold sixty repositories and a branch
/// belongs to one of them.
///
/// `repo_of` answers `Some("")` for the workspace root, so a single-repository
/// workspace comes out of here with an empty prefix and the same repository
/// publish has always opened.
fn owning_repo(ws: &Workspace, files: &[(String, String)]) -> Result<(String, Repository)> {
    let mut tally: std::collections::BTreeMap<String, usize> = Default::default();
    let mut outside: Vec<&str> = Vec::new();
    for (f, _) in files {
        match ws.repo_of(f) {
            Some(dir) => *tally.entry(dir).or_default() += 1,
            None => outside.push(f),
        }
    }
    let counted: Vec<(String, usize)> = tally.into_iter().collect();
    let named = |dir: &str| match dir.is_empty() {
        true => "the workspace root".to_string(),
        false => dir.to_string(),
    };
    // Dropping it quietly is the one thing that must not happen: the journal
    // holds the edit, so it would leave every later publish carrying a file that
    // has never had a branch to go to.
    if !outside.is_empty() {
        bail!(
            "nothing at or above {} inside {} is a git repository, so there is no branch to build for it. Run `git init` where it belongs, or `--exclude` it out of this publish",
            outside.join(", "),
            ws.root.display()
        );
    }
    let [(dir, _)] = counted.as_slice() else {
        let each: Vec<String> = counted
            .iter()
            .map(|(d, n)| format!("{} ({n} file(s))", named(d)))
            .collect();
        bail!(
            "ortak's branch belongs to one repository and this session has edited {}: {}. Publish them one at a time with `--repo <dir>`",
            counted.len(),
            each.join(", ")
        );
    };
    let at = ws.root.join(dir);
    let repo = Repository::open(&at).with_context(|| {
        format!(
            "publishing requires {} to be a git repository",
            at.display()
        )
    })?;
    Ok((dir.clone(), repo))
}

/// The branch this publish builds on, in the repository it is building in.
///
/// `[publish] base_branch` is one value for a whole workspace, and a tree of
/// repositories does not agree on a trunk: the tree this was written for has
/// `16.0`, `main` and `master` in it. So a nested repository that does not have
/// the configured branch falls back to its own trunk and says which one it
/// took. `--base` still wins and takes a remote ref, which is the way to say
/// something other than a trunk.
///
/// The fallback is `main` then `master` and then nothing. Never HEAD: HEAD in a
/// checked-out repository is whichever branch somebody left checked out, and a
/// silent fallback across sixty repositories would eventually build a branch on
/// somebody's half-finished feature.
///
/// ponytail: two names, not a lookup. `refs/remotes/<remote>/HEAD` is the
/// authoritative answer where a clone recorded one; add it if a repository with
/// neither `main` nor `master` turns out to be common.
fn base_here(
    repo: &Repository,
    cfg: &Config,
    base_override: Option<&str>,
    prefix: &str,
) -> Result<String> {
    let configured = base_branch(cfg, base_override);
    let known = |name: &str| repo.find_branch(name, BranchType::Local).is_ok();
    if base_override.is_some() || prefix.is_empty() || known(configured) {
        return Ok(configured.to_string());
    }
    match ["main", "master"].into_iter().find(|n| known(n)) {
        Some(trunk) => {
            println!(
                "{prefix} has no branch called {configured}, so this branch is built on {trunk}, which is its trunk; `--base` says otherwise"
            );
            Ok(trunk.to_string())
        }
        None => bail!(
            "{prefix} has no branch called {configured}, and no main or master either, so there is nothing to build this branch on. Name one with `--base <branch>`; since #102 it takes a remote-tracking ref, so `--base origin/<trunk>` works without moving anything"
        ),
    }
}

/// The session's files this project's ignore rules now cover, which the branch
/// must not carry whatever the journal remembers.
///
/// Only files the base branch does not already have: a tracked file is not
/// ignored by git however well it matches a pattern, and dropping one would
/// strip a real change out of a branch without anybody asking.
fn ignored_now(
    repo: &Repository,
    base_tree: &Tree,
    files: &[(String, String)],
    prefix: &str,
) -> Vec<String> {
    files
        .iter()
        .map(|(f, _)| f.clone())
        .filter(|f| {
            let rel = in_repo(prefix, f);
            repo.is_path_ignored(rel).unwrap_or(false) && base_tree.get_path(rel).is_err()
        })
        .collect()
}

/// Drop the `--exclude` paths from the publish, returning the ones that matched
/// nothing. A mistyped path is silent otherwise, and the file it was meant to
/// keep out of the branch ships anyway.
///
/// A pattern is a path prefix, so a directory takes everything under it.
/// `--exclude src` used to match one exact path called `src`, warn that it had
/// matched nothing, and then publish every file in the directory: a warning
/// that reads like a typo report while the flag does the opposite of what it
/// was asked.
///
/// ponytail: a prefix, not a glob. A glob is a dependency and a syntax to
/// document; if somebody needs `--exclude '*.json'` they will ask.
fn drop_excluded(files: &mut Vec<(String, String)>, exclude: &[String]) -> Vec<String> {
    let mut unmatched = Vec::new();
    for pattern in exclude {
        // Every path starts with the empty one, so `--exclude ./` would hold
        // back the whole publish while `--exclude .` warns: two spellings of
        // one intent doing opposite things. It is a typo like any other.
        let Some(path) = Some(pattern.trim_start_matches("./")).filter(|p| !p.is_empty()) else {
            unmatched.push(pattern.clone());
            continue;
        };
        // `Path::starts_with` matches whole components, which is the boundary
        // this needs: `src` takes `src/db.rs` and leaves `srcgen/main.rs`
        // alone. It ignores the trailing slash tab completion adds, too.
        let path = Path::new(path);
        let before = files.len();
        files.retain(|(f, _)| !Path::new(f).starts_with(path));
        if files.len() == before {
            unmatched.push(pattern.clone());
        }
    }
    unmatched
}

/// The remote somebody actually chose: `ortak.remote` in git config, then
/// ortak.toml. `repo.config()` reads the global and system files too, so
/// `git config --global ortak.remote fork` answers for every clone at once,
/// which is the right scope for somebody who forks everything.
fn chosen_remote(repo: &Repository, cfg: &Config) -> Option<String> {
    repo.config()
        .and_then(|c| c.get_string("ortak.remote"))
        .ok()
        .or_else(|| cfg.publish.remote.clone())
}

fn remote_names(repo: &Repository) -> Vec<String> {
    repo.remotes()
        .map(|names| names.iter().flatten().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The push remote: what somebody chose, then the only one there is, then
/// origin. One contributor pushes to a fork while another pushes to upstream, so
/// this is a per-clone setting; git config is where per-clone settings already
/// live, and it survives the `.ortak` wipes that resetting a workspace involves.
///
/// A lone remote is used whatever it is called, because there is nothing to be
/// wrong about, and a clone whose one remote is `upstream` used to be told there
/// was no remote called origin. The origin at the end is now only reached with
/// no remotes at all, where the push fails whatever this returns and
/// `push_advice` is what explains it.
pub fn remote_for(repo: &Repository, cfg: &Config) -> String {
    if let Some(chosen) = chosen_remote(repo, cfg) {
        return chosen;
    }
    match remote_names(repo).as_slice() {
        [only] => only.clone(),
        _ => "origin".to_string(),
    }
}

/// Why `--push` cannot go ahead: nobody named a remote and there is more than
/// one here, so whichever publish picked would be a guess. `origin` is the guess
/// it used to make, and in a vendored repository origin is the vendor: the push
/// either fails with a permission error that says nothing about ortak, or lands
/// somebody's task branch on somebody else's project.
///
/// It names the candidates rather than prompting, which is the call `init`
/// already made for this same question: a prompt hangs in a script, and nothing
/// on disk says which remote you can write to anyway.
fn unsettled_remote(repo: &Repository, cfg: &Config) -> Option<String> {
    if chosen_remote(repo, cfg).is_some() {
        return None;
    }
    let names = remote_names(repo);
    if names.len() < 2 {
        return None;
    }
    let at = repo.workdir().unwrap_or_else(|| repo.path());
    Some(format!(
        "--push will not guess a remote: nothing has chosen one and {} has {} ({}). `origin` in a repository you did not create is somebody else's upstream, so name yours inside that repository:\n  git config ortak.remote <name>\nNothing was built or recorded, so publishing again after that produces the same branch.",
        at.display(),
        names.len(),
        names.join(", ")
    ))
}

/// What a failed push has to say: where it went, and that the branch is already
/// built, so the way out is a push and not another publish.
///
/// A fork owner's first `--push` goes to the upstream they cannot write to,
/// because `ortak.remote` is unset on a fresh clone and the fallback is origin.
/// Git's answer is a permission error with nothing about ortak in it, and
/// re-running the publish after fixing the remote hits "changed nothing since
/// its last publish": the branch and its high-water mark are already recorded.
fn push_advice(repo: &Repository, remote: &str, branch: &str, from: &str) -> String {
    let url = repo
        .find_remote(remote)
        .ok()
        .and_then(|r| r.url().map(str::to_string))
        .unwrap_or_else(|| "no remote by that name in this clone".to_string());
    format!(
        "it went to {remote} ({url}).\n\
         The branch {branch} is built and still here, so point ortak at the remote you can\n\
         write to and push it yourself{from}:\n  \
         git config ortak.remote <name>\n  \
         git push <name> {branch}"
    )
}

/// Where a printed git command has to be run, said only when that is somewhere
/// other than where the reader is standing. Both `git config ortak.remote` and
/// `git push` answer to the repository they run in, and in a tree that is a
/// directory below the workspace root rather than the root itself.
fn run_it_from(prefix: &str) -> String {
    match prefix.is_empty() {
        true => String::new(),
        false => format!(", from {prefix}"),
    }
}

/// Generated branch name. A session that never ran `ortak intent` has no words
/// to slug, and slugging the placeholder intent instead produced branches named
/// `task/ortak-3-ortak-task-ortak-3`.
fn branch_name_for(prefix: &str, id: i64, intent: Option<&str>) -> String {
    match intent.map(str::trim).filter(|t| !t.is_empty()) {
        Some(task) => format!("{}ortak-{}-{}", prefix, id, slug(task)),
        None => format!("{}ortak-{}", prefix, id),
    }
}

/// The commit a published branch is built on.
///
/// Falling back to HEAD when the configured branch is missing looked harmless
/// until you notice what HEAD is in a shared workspace: whatever branch the
/// tree happens to sit on, which may be another session's task branch. A repo
/// whose trunk is `master` published every task off HEAD and still printed
/// `--base main`.
pub(crate) fn base_commit_for<'r>(repo: &'r Repository, base: &str) -> Result<Commit<'r>> {
    // The local branch first, so nothing about a workspace that has one changes.
    if let Ok(commit) = repo
        .find_branch(base, BranchType::Local)
        .and_then(|b| b.get().peel_to_commit())
    {
        return Ok(commit);
    }
    // Then anything else git can turn into a commit. `origin/main` is the one
    // that matters: somebody who has just been told their trunk is behind needs
    // a way to say "build on what the remote has" that does not start with
    // moving a branch by hand, which is the moment they are least sure what
    // they are allowed to touch. A tag and a raw sha come along for free.
    if let Ok(commit) = repo.revparse_single(base).and_then(|o| o.peel_to_commit()) {
        return Ok(commit);
    }
    // A repository with no commits has no branch to name either, so the advice
    // below sends the reader to ortak.toml to try other names when nothing they
    // could write there would work. Behind the fallback rather than ahead of it,
    // which costs nothing: a repository with no commits has nothing for revparse
    // to resolve either, so the fallback misses first whatever the order.
    if unborn(repo) {
        bail!(
            "this repository has no commits yet, so there is nothing for a branch to build on. Make the first commit, then publish"
        );
    }
    let head = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(String::from))
        .map(|h| format!(" (HEAD is on '{h}')"))
        .unwrap_or_default();
    bail!(
        "'{}' does not name a branch, ref or commit in this repository{}. Set [publish] base_branch in ortak.toml to the branch these tasks merge into",
        base,
        head
    )
}

/// What a forge would call this base. A pull request wants a branch on the
/// remote, and `--base origin/main` printed `gh pr create --base origin/main`,
/// which every forge rejects: the branch there is `main`.
///
/// ponytail: the remotes are asked by name rather than splitting on the first
/// slash, because a remote may have one in its name and a branch certainly may.
fn forge_base<'a>(repo: &Repository, base: &'a str) -> &'a str {
    repo.remotes()
        .ok()
        .and_then(|remotes| {
            remotes
                .iter()
                .flatten()
                .find_map(|r| base.strip_prefix(&format!("{r}/")))
        })
        .unwrap_or(base)
}

/// The git repository this path sits inside, when the path is not its root.
///
/// `Repository::open` does not walk up, and both `init` and `doctor` read its
/// failure as "not a git repository", which is what somebody working in a
/// subdirectory of a checkout was told. The verdict is right, since publish
/// builds from the workspace root and cannot work from there, but the reason
/// given was false and the rest of the report reads as unreliable after it.
pub(crate) fn repository_above(root: &Path) -> Option<std::path::PathBuf> {
    let work = Repository::discover(root).ok()?.workdir()?.to_path_buf();
    let same = work.canonicalize().ok()? == root.canonicalize().ok()?;
    (!same).then_some(work)
}

/// A repository with no commits.
///
/// An unborn HEAD is necessary and it is not sufficient, which is what this used
/// to get wrong: `git checkout --orphan` leaves HEAD unborn beside branches that
/// carry commits, and so does a `git symbolic-ref HEAD` at a name nobody has
/// written to. Both callers then tell somebody looking at their own history that
/// there is none of it, and `doctor` skips the base-branch check on top of that.
///
/// `is_empty` is not the question either: it answers false once HEAD names a
/// branch other than libgit2's own default, which any `git init -b` does.
///
/// ponytail: branches, not objects. A commit no reference can reach is nothing
/// a branch could be built on, so it is the same answer for this purpose.
pub(crate) fn unborn(repo: &Repository) -> bool {
    matches!(repo.head(), Err(e) if e.code() == git2::ErrorCode::UnbornBranch)
        && repo
            .branches(None)
            .map_or(true, |mut branches| branches.next().is_none())
}

/// How many commits the workspace's own checkout is missing from the branch a
/// publish builds on: `git rev-list --count HEAD..base`. `None` when it is level
/// or ahead, and when there is no HEAD to compare (a repository with no commits).
fn commits_behind(repo: &Repository, base: &Commit) -> Option<usize> {
    let head = repo.head().ok()?.target()?;
    let (_, behind) = repo.graph_ahead_behind(head, base.id()).ok()?;
    (behind > 0).then_some(behind)
}

/// What a publish owes the reader when the branch it is building on has itself
/// fallen behind the remote it tracks. `None` while there is nothing to say.
///
/// The session that found this seeded its replay from a trunk thirty-eight
/// commits stale and got a branch of old content with a new hunk on top, with
/// every check passing on the way. `commits_behind` asks the mirror question,
/// whether the checkout is behind the base, and it only speaks from the `Err`
/// arm of a replay that has already failed. Nothing was asking whether the base
/// was current, and a replay only fails where two contents actually collide, so
/// nothing did.
///
/// A warning and not a refusal. What comes out is a real branch that merges,
/// and a deliberately pinned older base is somebody's prerogative; publish
/// refuses only where it cannot build a correct branch at all. What failed last
/// time was that the one line hinting at it sat under the branch, so this one
/// goes out before the branch exists.
///
/// ponytail: local refs only, never `git ls-remote`. A publish that needs the
/// network to succeed is a publish that fails on a plane, and the answer here
/// is on disk from the last fetch either way.
fn base_behind_warning(repo: &Repository, remote: &str, base: &str) -> Option<String> {
    let branch = repo.find_branch(base, BranchType::Local).ok()?;
    let local = branch.get().target()?;
    // The branch's own upstream, and the push remote only where there is none.
    // Those are different questions, and in a fork workflow they are different
    // answers: with `ortak.remote` on the fork, comparing the trunk against the
    // fork's copy of it reported nothing while the real trunk ran ten commits
    // ahead. That was the workspace this was written in.
    let (upstream, from, fetch) = match branch.upstream() {
        Ok(up) => {
            let tracked = up.name().ok().flatten()?.to_string();
            let from = repo.branch_upstream_remote(branch.get().name()?).ok()?;
            let from = from.as_str()?.to_string();
            // A branch may track one under another name, so the refspec below
            // carries both halves rather than assuming they match.
            let fetch = tracked.strip_prefix(&format!("{from}/"))?.to_string();
            (up.get().target()?, from, fetch)
        }
        Err(_) => {
            let up = repo
                .find_branch(&format!("{remote}/{base}"), BranchType::Remote)
                .ok()?;
            (up.get().target()?, remote.to_string(), base.to_string())
        }
    };
    let (_, behind) = repo.graph_ahead_behind(local, upstream).ok()?;
    if behind == 0 {
        return None;
    }
    // Git refuses to fetch into the branch that is checked out, and in this
    // workspace the trunk is usually the branch checked out, so both cases are
    // ordinary and each has exactly one command that works.
    let fix = match repo
        .head()
        .ok()
        .is_some_and(|h| h.shorthand() == Some(base))
    {
        true => format!("git pull --ff-only {from} {fetch}"),
        false => format!("git fetch {from} {fetch}:{base}"),
    };
    Some(format!(
        "warning: {base} is {behind} commit(s) behind {from}/{fetch}, so this branch is being built on the older {base}: its diff will carry work that is already on the trunk, and it will not carry what landed there since.\n  {fix}\nand publish again. Nothing is wrong if you meant to build on that older base."
    ))
}

/// Whether the remote already carries this branch. Local refs go stale, and a
/// wrong answer here either strands a stack or pushes a branch nobody asked for,
/// so ask the remote.
fn on_remote(at: &Path, remote: &str, branch: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(at)
        .args(["ls-remote", "--exit-code", "--heads", remote, branch])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// What one `--amend` resolved to: where it starts reading the journal, the
/// branch it lands on, the commit it rebuilds that branch on, and whether it
/// rewrites this session's own commit or puts one on top of a commit that is
/// not this session's.
#[derive(Clone, Copy, Debug)]
struct Amend<'a> {
    branch: &'a str,
    base: Oid,
    rewrites: bool,
}

/// The half of an amend the journal can answer on its own: which branch, how
/// far back to read, and where that branch sits in this session's publish
/// history.
///
/// It is split from the rest because of the order the two need. The repository
/// an amend rebuilds in is the one holding the files, the files are whatever
/// came after this mark, and reading git for the branch tip before any of that
/// is known would mean opening a repository chosen by nothing.
fn amend_reach<'a>(
    history: &[PublishRow],
    branch: Option<&'a str>,
) -> Result<(&'a str, i64, Option<usize>)> {
    let Some(branch) = branch else {
        bail!("--amend rewrites one branch; name it with --branch <branch>");
    };
    let at = history.iter().position(|p| p.branch == branch);
    let after = match at {
        // Its own publish is the newest, so the mark before it is the floor.
        Some(0) => history.get(1).map_or(0, |p| p.last_edit_id),
        // A branch this session did not publish keeps everything on it and takes
        // this session's work since its own last publish on top.
        _ => history.first().map_or(0, |p| p.last_edit_id),
    };
    Ok((branch, after, at))
}

/// Where an amend starts reading the journal, and what it may do to the branch.
///
/// A branch this session published carries the edits between the publish before
/// it and its own, so rebuilding it means starting from the mark the publish
/// before it left. That is only true while the branch is the session's newest
/// publish: rewriting an older one would sweep every deliverable published
/// since into it, which is the bug `--base` was added to avoid rather than one
/// to introduce here.
///
/// Most branches worth amending have no row in this journal at all: they were
/// published by a session that has since ended, or from another workspace, or
/// by hand. Refusing those is what sent every review fix into a worktree, where
/// ortak sees none of the work. There the branch itself carries the floor:
/// everything already committed on it stays, this session's edits since its own
/// last publish go on top, and nothing that was there can be lost.
fn amend_target<'a>(
    repo: &Repository,
    history: &[PublishRow],
    branch: Option<&'a str>,
    session: &Session,
) -> Result<Amend<'a>> {
    let (branch, _, at) = amend_reach(history, branch)?;
    let tip = branch_tip(repo, branch)?;
    match at {
        Some(0) => {
            // The journal says this session published the branch; git says what
            // stands on it now. Anything that is not this session's own publish
            // commit is somebody else's work, and rebuilding the branch from its
            // tip's parent would drop it without a word.
            if !published_by(&tip, &session.external_id) {
                bail!(
                    "{} has moved since ortak-{} published it: its tip {} is not a commit ortak-{} published, and rebuilding the branch would drop it. Publish this work as its own branch, or put {} back where ortak-{} left it",
                    branch,
                    session.id,
                    &tip.id().to_string()[..8],
                    session.id,
                    branch,
                    session.id
                );
            }
            // Its tip's parent is the commit it was published on, so rebuilding
            // it leaves a stacked branch stacked instead of quietly rebasing it
            // onto the trunk and pulling its base's work into the diff.
            let parent = tip
                .parent(0)
                .with_context(|| format!("{branch} has no parent commit to rebuild it on"))?;
            Ok(Amend {
                branch,
                base: parent.id(),
                rewrites: true,
            })
        }
        Some(_) => bail!(
            "ortak-{} published {} after {}; amending {} now would sweep that later work into it. Publish the fix as its own branch instead",
            session.id,
            history[0].branch,
            branch,
            branch
        ),
        // Nothing on it is this session's to rewrite, so the tip itself is the
        // floor and the new commit becomes its child.
        None => Ok(Amend {
            branch,
            base: tip.id(),
            rewrites: false,
        }),
    }
}

/// The commit a branch points at. `--amend` names a branch that is already
/// there, so a missing one is a typo or a branch that was never published,
/// rather than something to create quietly under a flag that says rebuild.
fn branch_tip<'r>(repo: &'r Repository, branch: &str) -> Result<Commit<'r>> {
    repo.find_branch(branch, BranchType::Local)
        .and_then(|b| b.get().peel_to_commit())
        .map_err(|_| {
            anyhow!(
                "--amend rebuilds a branch that already exists, and this repository has no branch {}. Publish without --amend to create it",
                branch
            )
        })
}

/// Whether a commit is one this session published. Every publish stamps the
/// session's external id into the commit message, so git carries the answer:
/// the journal can be wiped and the workspace re-initialised, and the branch
/// still says who left the commit on it.
fn published_by(commit: &Commit, external_id: &str) -> bool {
    commit.message().is_some_and(|m| {
        m.lines()
            .filter_map(|l| l.strip_prefix("Ortak-Session:"))
            .any(|id| id.trim() == external_id)
    })
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

    /// The replay's question about each file: does another session still claim
    /// it? In every test but one, nobody does.
    fn shared_none(_: &str) -> bool {
        false
    }

    /// The replay, for the tests that expect it to get through. It deliberately
    /// shadows the real `session_only_tree` so those tests keep reading as the
    /// two things they are about, the tree and what was left out of it, and
    /// only the tests about a blocked replay have to mention `Replay`.
    fn session_only_tree<'r>(
        shadow: &'r Repository,
        seed: &Commit<'r>,
        base_tree: &Tree,
        commits: &[String],
    ) -> Result<(Tree<'r>, Vec<String>)> {
        match super::session_only_tree(shadow, seed, base_tree, commits, &shared_none, "")? {
            Replay::Tree(tree, skipped) => Ok((tree, skipped)),
            Replay::Blocked(paths) => panic!("the replay was blocked on {}", paths.join(", ")),
        }
    }

    /// `--exclude` means "not on this branch", never "not on any branch". The
    /// mark used to jump to the session's newest edit whatever shipped, so the
    /// held-back file sat behind it and no later incremental publish looked
    /// that far back again.
    #[test]
    fn the_mark_stops_at_the_first_file_held_back() {
        let dir = std::env::temp_dir().join(format!("ortak-mark-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let me = db.upsert_session("a", "claude-a", "llm", None).unwrap();
        let hunk = [crate::regions::Hunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
        }];
        for f in ["keep.txt", "later.txt", "keep.txt"] {
            db.insert_edit(me, f, "modify", None, &hunk, None).unwrap();
        }
        let ids: Vec<i64> = (1..=3).collect();

        // Nothing held back: the mark is the session's newest edit.
        assert_eq!(db.first_edit_on(me, 0, &[]).unwrap(), None);
        // later.txt held back: the mark stops just before its only edit, so the
        // next publish sees it again, and keep.txt's later edit comes with it.
        assert_eq!(
            db.first_edit_on(me, 0, &["later.txt".to_string()]).unwrap(),
            Some(ids[1])
        );
        // Already past it: a second publish does not rewind to work it shipped.
        assert_eq!(
            db.first_edit_on(me, ids[1], &["later.txt".to_string()])
                .unwrap(),
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every branch this tool published arrived as `claude-75c62662
    /// <ortak-3@ortak.local>`, which no forge can match to an account and no
    /// signature can cover. Which session did the work is in the trailers.
    #[test]
    fn a_published_commit_is_authored_by_the_person() {
        let dir = std::env::temp_dir().join(format!("ortak-author-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = Repository::init(&dir).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "A Person").unwrap();
            cfg.set_str("user.email", "person@example.com").unwrap();
        }
        let sig = repo.signature().unwrap();
        assert_eq!(sig.name(), Some("A Person"));
        assert_eq!(sig.email(), Some("person@example.com"));

        // And the fallback, for a repository that has never been told who it
        // belongs to: better a name than a failed publish.
        let bare = std::env::temp_dir().join(format!("ortak-author-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bare);
        let plain = Repository::init(&bare).unwrap();
        {
            let mut cfg = plain.config().unwrap();
            let _ = cfg.remove("user.name");
            let _ = cfg.remove("user.email");
        }
        if plain.signature().is_err() {
            assert!(Signature::now("claude-abcd", "ortak-3@ortak.local").is_ok());
        }
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

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

    /// A file journaled before .gitignore learned about it. The rows are real
    /// and the branch is still the wrong place for it, and it went out at the
    /// version it had when the rule landed, since nothing has journaled it
    /// since.
    #[test]
    fn a_file_the_project_ignores_now_stays_out_of_the_branch() {
        let (dir, repo) = scratch("ignored");
        std::fs::write(dir.join(".gitignore"), "secret.env\napp.py\n").unwrap();
        // app.py is on the base branch, so git does not ignore it whatever the
        // pattern says, and neither may this.
        let base = tree_with(&repo, Some("tracked\n"));
        let files = vec![
            ("secret.env".to_string(), "create".to_string()),
            ("app.py".to_string(), "modify".to_string()),
            ("src/main.rs".to_string(), "modify".to_string()),
        ];

        assert_eq!(ignored_now(&repo, &base, &files, ""), vec!["secret.env"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The base branch keeps a directory where the session now has a file, the
    /// shape of any module flattened into one file. Reading a blob out of that
    /// tree entry ended the whole publish with "the requested type does not
    /// match the type in the ODB", and skipping the entry left the replay
    /// merging a deletion against content the seed had never been given.
    #[test]
    fn a_directory_the_session_replaced_with_a_file_replays() {
        let (dir, repo) = scratch("dir-to-file");
        let inner = {
            let mut sub = repo.treebuilder(None).unwrap();
            let blob = repo.blob(b"pub fn go() {}\n").unwrap();
            sub.insert("mod.rs", blob, 0o100644).unwrap();
            sub.write().unwrap()
        };
        let mut tb = repo.treebuilder(None).unwrap();
        tb.insert("thing", inner, 0o040000).unwrap();
        let base = repo.find_tree(tb.write().unwrap()).unwrap();
        let before = parentless(&repo, &base);
        // The session's own micro-commit, with thing as a file.
        let flat = commit_file(&repo, Some(&before), "thing", "pub fn walk() {}\n");

        let files = vec![("thing".to_string(), "modify".to_string())];
        let seed = base_seed(&repo, &repo, &base, &files, "").unwrap();
        let (tree, skipped) = session_only_tree(&repo, &seed, &base, &[flat.to_string()]).unwrap();

        assert!(skipped.is_empty(), "left out: {skipped:?}");
        assert_eq!(named_file_in(&tree, &repo, "thing"), "pub fn walk() {}\n");
        assert!(
            tree.get_path(Path::new("thing/mod.rs")).is_err(),
            "the branch still carries the directory the file replaced"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `ortak init` in a subdirectory of a checkout, which both init and doctor
    /// used to call a directory with no git repository at all. Publish still
    /// cannot work from there; the reason given was the false part.
    #[test]
    fn a_workspace_inside_a_checkout_knows_where_the_repository_is() {
        let (dir, _repo) = scratch("inside");
        let sub = dir.join("pkg");
        std::fs::create_dir_all(&sub).unwrap();

        let found = repository_above(&sub).expect("the repository one directory up");
        assert_eq!(found.canonicalize().unwrap(), dir.canonicalize().unwrap());
        // The root of a repository is not inside another one, and a directory
        // with no repository above it has nothing to name.
        assert_eq!(repository_above(&dir), None);
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

    /// A write credited to the wrong session leaves a foreign commit in the
    /// middle of a file's shadow history, and every later commit of the real
    /// author's then conflicts against it. While the other session still claims
    /// the file that is honest; once it has released it, dropping the author's
    /// own work is not, and used to be permanent.
    #[test]
    fn a_released_file_replays_again() {
        let (dir, repo) = scratch("released");
        let mine1 = commit_file(&repo, None, "mine.py", &lines(&[(1, "MINE-1")]));
        let mine1_c = repo.find_commit(mine1).unwrap();
        // Somebody else's write lands in the middle: a formatter run, credited
        // to whichever session had a command open.
        let theirs = commit_file(
            &repo,
            Some(&mine1_c),
            "mine.py",
            &lines(&[(1, "MINE-1"), (2, "REFORMATTED")]),
        );
        let theirs_c = repo.find_commit(theirs).unwrap();
        let mine2 = commit_file(
            &repo,
            Some(&theirs_c),
            "mine.py",
            &lines(&[(1, "MINE-2"), (2, "REFORMATTED")]),
        );

        let base = tree_with(&repo, None); // mine.py is on no branch
        let seed = parentless(&repo, &base);
        let picks = vec![mine1.to_string(), mine2.to_string()];

        // Still theirs: the old answer, and the right one while it is true.
        let skipped = match super::session_only_tree(
            &repo,
            &seed,
            &base,
            &picks,
            &|f: &str| f == "mine.py",
            "",
        )
        .unwrap()
        {
            Replay::Tree(_, skipped) => skipped,
            Replay::Blocked(p) => panic!("blocked on {}", p.join(", ")),
        };
        assert_eq!(skipped, vec!["mine.py".to_string()]);

        // Released: the file is the author's own history with a hole in it.
        let (tree, skipped) = session_only_tree(&repo, &seed, &base, &picks).unwrap();
        assert!(skipped.is_empty(), "a released file is publishable again");
        assert!(named_file_in(&tree, &repo, "mine.py").contains("MINE-2"));
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
        let theirs_still = |f: &str| f == "dep.py";
        let (tree, skipped) =
            match super::session_only_tree(&repo, &seed, &base, &picks, &theirs_still, "").unwrap()
            {
                Replay::Tree(tree, skipped) => (tree, skipped),
                Replay::Blocked(p) => panic!("blocked on {}", p.join(", ")),
            };

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
        match super::session_only_tree(&repo, &seed, &base, &[mine.to_string()], &shared_none, "")
            .unwrap()
        {
            Replay::Blocked(paths) => assert_eq!(paths, vec!["app.py".to_string()]),
            Replay::Tree(..) => panic!("a clash on line 3 replayed as if it were clean"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The history round 9 found: the session edits line 3, the base branch
    /// moves line 3 underneath it, and the session then rewrites the file
    /// without its own line-3 edit. The replay dies on a hunk the session threw
    /// away itself, while the content it means to ship merges cleanly.
    #[test]
    fn a_squash_ships_a_history_that_cannot_replay() {
        let (dir, repo) = scratch("squash");
        let start = commit(&repo, None, Some(&lines(&[])));
        let start = repo.find_commit(start).unwrap();
        let edit = commit(&repo, Some(&start), Some(&lines(&[(3, "MINE")])));
        let rewrite = repo.find_commit(edit).unwrap();
        // The rewrite drops line 3 back and leaves the work this branch is for.
        let rewrite = commit(&repo, Some(&rewrite), Some(&lines(&[(20, "REAL-WORK")])));

        let base = tree_with(&repo, Some(&lines(&[(3, "ALREADY-SHIPPED")])));
        let seed = parentless(&repo, &base);
        let commits = vec![edit.to_string(), rewrite.to_string()];
        match super::session_only_tree(&repo, &seed, &base, &commits, &shared_none, "").unwrap() {
            Replay::Blocked(paths) => assert_eq!(paths, vec!["app.py".to_string()]),
            Replay::Tree(..) => panic!("the discarded line-3 edit replayed as if it were clean"),
        }

        // What --squash does instead: the file's commits leave the replay, and
        // its net change goes on the base once.
        let pairs: Vec<(String, String)> = commits
            .iter()
            .map(|c| (c.clone(), "app.py".to_string()))
            .collect();
        let (replayed, _) = session_only_tree(&repo, &seed, &base, &[]).unwrap();
        let squashed = squash_file(&repo, &replayed, &pairs, "app.py").unwrap();
        let out = file_in(&squashed, &repo);
        assert!(
            out.contains("REAL-WORK"),
            "the session's work is missing:\n{out}"
        );
        assert!(
            out.contains("ALREADY-SHIPPED"),
            "the base's line 3 was overwritten:\n{out}"
        );
        assert!(
            !out.contains("MINE"),
            "the discarded edit came back:\n{out}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The floor under the net change: when that collides too, the content the
    /// session ended with is the only answer left, and it replaces the file.
    /// It takes the base branch's line with it, which is why the publish says
    /// which files it squashed and where to read them.
    #[test]
    fn a_squash_that_still_collides_takes_the_sessions_own_content() {
        let (dir, repo) = scratch("squash-collide");
        let start = commit(&repo, None, Some(&lines(&[])));
        let start = repo.find_commit(start).unwrap();
        let mine = commit(&repo, Some(&start), Some(&lines(&[(3, "MINE")])));

        let base = tree_with(&repo, Some(&lines(&[(3, "ALREADY-SHIPPED")])));
        let seed = parentless(&repo, &base);
        let (replayed, _) = session_only_tree(&repo, &seed, &base, &[]).unwrap();
        let pairs = vec![(mine.to_string(), "app.py".to_string())];
        let out = file_in(
            &squash_file(&repo, &replayed, &pairs, "app.py").unwrap(),
            &repo,
        );
        assert!(out.contains("MINE"), "{out}");
        assert!(!out.contains("ALREADY-SHIPPED"), "{out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stacking on another branch means the checkout is behind it by design, so
    /// the count is real and the explanation is wrong: it sends the reader to
    /// update a checkout that is exactly where it should be, and says nothing
    /// about the clash that actually stopped the replay.
    #[test]
    fn a_deliberate_stack_is_not_a_stale_checkout() {
        let (dir, repo) = scratch("stacked");
        let here = commit(&repo, None, Some(&lines(&[])));
        let here = repo.find_commit(here).unwrap();
        let stack_onto = commit_file(&repo, Some(&here), "dep.py", &lines(&[(3, "THEIRS")]));
        let stack_onto = repo.find_commit(stack_onto).unwrap();

        // The count itself is honest either way.
        assert_eq!(commits_behind(&repo, &stack_onto), Some(1));

        // What changes is whether it is worth saying. `--base` present means no.
        let explained = |override_: Option<&str>| {
            commits_behind(&repo, &stack_onto).filter(|_| override_.is_none())
        };
        assert_eq!(explained(None), Some(1), "no --base: the checkout is stale");
        assert_eq!(
            explained(Some("feat/other")),
            None,
            "--base: behind is the point of stacking"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The other reason that conflict happens, and the one `--exclude` cannot
    /// help with: the checkout the session has been editing is older than the
    /// branch its work is being rebuilt on.
    #[test]
    fn a_workspace_behind_its_base_is_counted() {
        let (dir, repo) = scratch("behind");
        let checked_out = commit(&repo, None, Some(&lines(&[]))); // moves HEAD
        let checked_out = repo.find_commit(checked_out).unwrap();
        assert_eq!(
            commits_behind(&repo, &checked_out),
            None,
            "a checkout is not behind itself"
        );

        // The base branch moves on while the workspace stays where it was.
        let moved = commit_file(&repo, Some(&checked_out), "app.py", &lines(&[(3, "NEWER")]));
        let moved = repo.find_commit(moved).unwrap();
        assert_eq!(commits_behind(&repo, &moved), Some(1));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Three deliverables, and the file the replay chokes on belongs to the
    /// second. The hint named the session's most recent publish instead, so
    /// following it cost another publish to learn the advice was wrong.
    #[test]
    fn the_stack_hint_names_the_branch_that_carries_the_file() {
        let (dir, _repo) = scratch("stack-hint");
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let mine = db
            .upsert_session("claude-a", "claude-a", "llm", None)
            .unwrap();
        for (file, branch) in [
            ("one.rs", "first"),
            ("db.rs", "second"),
            ("three.rs", "third"),
        ] {
            db.insert_edit(mine, file, "modify", None, &[], None)
                .unwrap();
            let mark = db.max_edit_id(mine).unwrap().unwrap();
            db.record_publish(mine, branch, mark).unwrap();
        }
        // The publish that fails is a fourth edit, to the second branch's file,
        // and it is the one edit no branch carries yet.
        db.insert_edit(mine, "db.rs", "modify", None, &[], None)
            .unwrap();
        let history = db.publishes(mine).unwrap();

        let msg = blocked_message(&db, &history, mine, &["db.rs".to_string()]).unwrap();
        assert!(msg.contains("--exclude db.rs"), "{msg}");
        assert!(msg.contains("--base second"), "{msg}");
        assert!(!msg.contains("third"), "{msg}");

        // A file this session has published nowhere gets the clash and no
        // advice, which is better than advice that cannot work.
        let theirs = blocked_message(&db, &history, mine, &["theirs.rs".to_string()]).unwrap();
        assert!(!theirs.contains("--base"), "{theirs}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_edit_belongs_to_the_branch_whose_slice_reaches_it() {
        // Newest first, as `publishes` returns them.
        let h: Vec<PublishRow> = [("third", 30), ("second", 20), ("first", 10)]
            .into_iter()
            .map(|(branch, last_edit_id)| PublishRow {
                branch: branch.to_string(),
                last_edit_id,
                ts: 0,
            })
            .collect();
        assert_eq!(branch_carrying(&h, 5), Some("first"));
        assert_eq!(branch_carrying(&h, 10), Some("first"));
        assert_eq!(branch_carrying(&h, 11), Some("second"));
        assert_eq!(branch_carrying(&h, 30), Some("third"));
        assert_eq!(branch_carrying(&h, 31), None);
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
        db.insert_edit(
            mine,
            "app.py",
            "modify",
            Some(&first.to_string()),
            &[],
            None,
        )
        .unwrap();
        db.insert_edit(
            mine,
            "app.py",
            "modify",
            Some(&second.to_string()),
            &[],
            None,
        )
        .unwrap();

        let short = lines(&[(3, "ONE")]);
        let whole = lines(&[(3, "ONE"), (9, "TWO")]);
        let base = lines(&[]);
        // This session's first micro-commit here is the root of the shadow, so
        // there is no parent to say what it started from, and the check runs on
        // what it does know.
        let check = |replayed: &str| {
            differs_from_last_write(
                &db,
                &repo,
                mine,
                "app.py",
                0,
                Versions {
                    replayed: replayed.as_bytes(),
                    on_base: Some(base.as_bytes()),
                    started_from: parent_blob(&repo, &first.to_string(), "app.py").as_deref(),
                },
            )
            .unwrap()
        };
        assert!(check(&short));
        assert!(!check(&whole));

        // Once another session is in the file too, the replay is meant to
        // differ and there is nothing left to assert.
        let theirs = db
            .upsert_session("claude-b", "claude-b", "llm", None)
            .unwrap();
        db.insert_edit(theirs, "app.py", "modify", None, &[], None)
            .unwrap();
        assert!(!check(&short));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A second deliverable that comes back to a file ships only the edits made
    /// since the first one's branch, so its replay lacks what that branch
    /// carries. That is the design, and the missing-row warning called it a
    /// broken journal every time until the check was given the slice.
    #[test]
    fn a_later_slice_is_not_a_missing_row() {
        let (dir, repo) = scratch("slice");
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let mine = db
            .upsert_session("claude-a", "claude-a", "llm", None)
            .unwrap();

        // Three edits to one file: the first shipped on its own branch, the
        // other two are this publish's slice.
        let mut parent = None;
        let mut published = 0;
        let mut written: Vec<Oid> = Vec::new();
        for content in [
            lines(&[(3, "ONE")]),
            lines(&[(3, "ONE"), (9, "TWO")]),
            lines(&[(3, "ONE"), (9, "TWO"), (20, "THREE")]),
        ] {
            let oid = commit_file(&repo, parent.as_ref(), "app.py", &content);
            db.insert_edit(mine, "app.py", "modify", Some(&oid.to_string()), &[], None)
                .unwrap();
            // The first of the three shipped on its own branch, so the slice
            // this publish carries starts after it.
            if published == 0 {
                published = db.max_edit_id(mine).unwrap().unwrap();
            }
            written.push(oid);
            parent = Some(repo.find_commit(oid).unwrap());
        }
        // The slice begins at the second write, so what it started from is what
        // the first one left.
        let started = parent_blob(&repo, &written[1].to_string(), "app.py");
        let check = |replayed: &str, on_base: &str| {
            differs_from_last_write(
                &db,
                &repo,
                mine,
                "app.py",
                published,
                Versions {
                    replayed: replayed.as_bytes(),
                    on_base: Some(on_base.as_bytes()),
                    started_from: started.as_deref(),
                },
            )
            .unwrap()
        };

        // Built on main, which has neither the first branch nor the file's
        // first edit. The replay carries the slice alone and says nothing.
        let trunk = lines(&[]);
        assert!(!check(&lines(&[(9, "TWO"), (20, "THREE")]), &trunk));

        // Stacked on the first branch, the check works again: the base has the
        // earlier write, so the replay owes the slice's last write exactly.
        let stacked = lines(&[(3, "ONE")]);
        assert!(!check(
            &lines(&[(3, "ONE"), (9, "TWO"), (20, "THREE")]),
            &stacked
        ));
        // The row behind "TWO" went missing, so the replay skipped it.
        assert!(check(&lines(&[(3, "ONE"), (20, "THREE")]), &stacked));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The disagreement round 11 wrote down and could not explain: a rehearsal
    /// warned that edits were missing from the journal, the trunk was
    /// fast-forwarded, and the real publish a minute later said nothing about a
    /// branch that came out byte-identical. Two runs, two base trees. The trunk
    /// moving in a file the session is editing is enough on its own, and it is
    /// not what the message claims.
    #[test]
    fn a_trunk_that_moved_is_not_a_missing_row() {
        let (dir, repo) = scratch("moved-base");
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let mine = db
            .upsert_session("claude-a", "claude-a", "llm", None)
            .unwrap();

        // The workspace as the session found it, then one edit of its own.
        let start = lines(&[]);
        let began = commit_file(&repo, None, "app.py", &start);
        let mine_now = lines(&[(3, "MINE")]);
        let wrote = commit_file(
            &repo,
            Some(&repo.find_commit(began).unwrap()),
            "app.py",
            &mine_now,
        );
        db.insert_edit(
            mine,
            "app.py",
            "modify",
            Some(&wrote.to_string()),
            &[],
            None,
        )
        .unwrap();
        let started = parent_blob(&repo, &wrote.to_string(), "app.py");
        let check = |replayed: &str, on_base: &str| {
            differs_from_last_write(
                &db,
                &repo,
                mine,
                "app.py",
                0,
                Versions {
                    replayed: replayed.as_bytes(),
                    on_base: Some(on_base.as_bytes()),
                    started_from: started.as_deref(),
                },
            )
            .unwrap()
        };

        // Built on the trunk the session started from, a replay that is not
        // this session's last write is still worth saying out loud.
        assert!(check(&lines(&[]), &start));
        assert!(!check(&mine_now, &start));

        // Somebody else lands a commit on the trunk, in the same file and far
        // from this session's line, and the replay merges onto it. The result
        // differs from the session's last write by exactly that commit.
        let moved = lines(&[(20, "THEIRS")]);
        assert!(
            !check(&lines(&[(3, "MINE"), (20, "THEIRS")]), &moved),
            "the trunk moving under the session is not a lost journal row"
        );
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

    /// `--exclude src` warned that it had matched nothing and then published
    /// every file under `src/`. It is a path prefix now, and the separator is
    /// what keeps `srcgen/` out of it.
    #[test]
    fn exclude_takes_a_directory_and_stops_at_the_separator() {
        let touched = || {
            vec![
                ("src/db.rs".to_string(), "modify".to_string()),
                ("src/publish.rs".to_string(), "modify".to_string()),
                ("srcgen/main.rs".to_string(), "create".to_string()),
            ]
        };
        let kept = vec![("srcgen/main.rs".to_string(), "create".to_string())];

        let mut files = touched();
        let unmatched = drop_excluded(&mut files, &["src".into()]);
        assert_eq!(files, kept);
        assert!(unmatched.is_empty(), "{unmatched:?}");

        let mut files = touched();
        drop_excluded(&mut files, &["./src/".into()]);
        assert_eq!(files, kept);

        // A directory the session did not touch is still a typo, and the
        // warning is the only thing that says so. `./` trims to the empty
        // string, which every path starts with, so it holds nothing back
        // rather than everything.
        for typo in ["doc", "./"] {
            let mut files = touched();
            assert_eq!(drop_excluded(&mut files, &[typo.into()]), vec![typo]);
            assert_eq!(files, touched(), "{typo} held files back");
        }
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
    }

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

    #[test]
    fn a_push_only_guesses_where_there_is_nothing_to_guess_between() {
        let dir = std::env::temp_dir().join(format!("ortak-unsettled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = Repository::init(&dir).unwrap();
        let mut cfg = Config::default();

        // One remote, whatever it is called. A clone whose only remote is
        // `upstream` was told there was no remote called origin.
        repo.remote("upstream", "https://example.invalid/a.git")
            .unwrap();
        assert_eq!(remote_for(&repo, &cfg), "upstream");
        assert!(unsettled_remote(&repo, &cfg).is_none());

        // A second one, and now nothing in the repository picks between them.
        repo.remote("fork", "https://example.invalid/b.git")
            .unwrap();
        let problem = unsettled_remote(&repo, &cfg).expect("two remotes and no answer");
        for expected in [
            "upstream",
            "fork",
            "git config ortak.remote",
            dir.file_name().unwrap().to_str().unwrap(),
        ] {
            assert!(
                problem.contains(expected),
                "{expected} missing from: {problem}"
            );
        }
        assert!(!problem.contains("ortak.toml"), "{problem}");

        // ortak.toml is still an answer, for anyone already relying on it.
        cfg.publish.remote = Some("fork".into());
        assert!(unsettled_remote(&repo, &cfg).is_none());
        cfg.publish.remote = None;

        // A chosen remote that is not in this clone is still chosen: that push
        // reaches git and push_advice names what it looked for, which is a
        // better answer than a second guess on top of the first.
        repo.config()
            .unwrap()
            .set_str("ortak.remote", "gone")
            .unwrap();
        assert!(unsettled_remote(&repo, &cfg).is_none());
        assert_eq!(remote_for(&repo, &cfg), "gone");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_without_intent_gets_a_plain_branch_name() {
        assert_eq!(branch_name_for("task/", 3, None), "task/ortak-3");
        assert_eq!(branch_name_for("task/", 3, Some("  ")), "task/ortak-3");
        assert_eq!(
            branch_name_for("task/", 3, Some("Implement the login page")),
            "task/ortak-3-implement-the-login-page"
        );
    }

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
        assert!(err.contains("'main' does not name a branch"), "{err}");
        assert!(err.contains("HEAD is on 'master'"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--base origin/main` used to come back as "does not exist in this
    /// repository", which was wrong twice over: the ref is right there, and the
    /// advice pointed at `ortak.toml`, which is not where the problem was.
    /// Somebody whose trunk has just been reported behind needs a way to build
    /// on what the remote has without moving a branch by hand first.
    #[test]
    fn base_takes_a_remote_ref_a_tag_or_a_sha() {
        let (dir, repo) = scratch("base-revparse");
        let first = commit_on(&repo, "main", None, "first", "one\n");
        let second = commit_on(&repo, "trunk", Some(&first), "second", "two\n");
        // A checkout standing on its trunk, the way a workspace is.
        repo.set_head("refs/heads/main").unwrap();
        repo.reference("refs/remotes/origin/main", second.id(), true, "test")
            .unwrap();
        repo.tag_lightweight("v1", second.as_object(), true)
            .unwrap();

        assert_eq!(base_commit_for(&repo, "main").unwrap().id(), first.id());
        assert_eq!(
            base_commit_for(&repo, "origin/main").unwrap().id(),
            second.id()
        );
        assert_eq!(base_commit_for(&repo, "v1").unwrap().id(), second.id());
        assert_eq!(
            base_commit_for(&repo, &second.id().to_string())
                .unwrap()
                .id(),
            second.id()
        );

        // A local branch of that name still wins, so no workspace that has one
        // changes behaviour because a remote grew a ref beside it.
        repo.branch("origin/main", &first, true).unwrap();
        assert_eq!(
            base_commit_for(&repo, "origin/main").unwrap().id(),
            first.id()
        );

        // And a name that is nothing at all still reads like a typo report.
        let err = base_commit_for(&repo, "no-such-thing")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not name a branch, ref or commit"),
            "{err}"
        );
        assert!(err.contains("ortak.toml"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A pull request wants a branch on the forge, and `origin/main` is not one.
    #[test]
    fn a_remote_ref_is_named_to_the_forge_without_its_remote() {
        let (dir, repo) = scratch("forge-base");
        repo.remote("origin", "https://example.invalid/x.git")
            .unwrap();
        assert_eq!(forge_base(&repo, "origin/main"), "main");
        assert_eq!(forge_base(&repo, "main"), "main");
        // Not a remote in this clone, so it is a branch name with a slash in it.
        assert_eq!(forge_base(&repo, "upstream/main"), "upstream/main");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A trunk that has fallen behind its remote publishes a branch of old
    /// content and passes every check on the way, which is how thirty-eight
    /// commits went missing under a round-10 branch. The three silences matter
    /// more than the count: a purely local trunk is ordinary, a trunk level with
    /// its remote has nothing to report, and a trunk *ahead* of its remote is
    /// the normal state of anybody who has just published.
    #[test]
    fn a_base_branch_behind_its_remote_says_so_and_says_nothing_otherwise() {
        let (dir, repo) = scratch("base-behind");
        let first = commit_on(&repo, "main", None, "first", "one\n");

        // No remote-tracking ref yet: a trunk nobody has ever pushed.
        assert_eq!(base_behind_warning(&repo, "origin", "main"), None);

        // Level with the remote.
        repo.reference("refs/remotes/origin/main", first.id(), true, "test")
            .unwrap();
        assert_eq!(base_behind_warning(&repo, "origin", "main"), None);

        // The remote moves on, twice, and the local branch stays put.
        let second = commit_on(&repo, "upstream", Some(&first), "second", "two\n");
        let third = commit_on(&repo, "upstream", Some(&second), "third", "three\n");
        repo.reference("refs/remotes/origin/main", third.id(), true, "test")
            .unwrap();
        let warning = base_behind_warning(&repo, "origin", "main").expect("no warning");
        assert!(
            warning.contains("2 commit(s) behind origin/main"),
            "{warning}"
        );
        assert!(warning.contains("git fetch origin main:main"), "{warning}");

        // Ahead of the remote, which is every session that has just published.
        // Reading `graph_ahead_behind` the other way round would warn here.
        commit_on(&repo, "main", Some(&third), "fourth", "four\n");
        assert_eq!(base_behind_warning(&repo, "origin", "main"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The push remote is not the question. This is the fork workflow the check
    /// was written in: `ortak.remote` names the fork, whose copy of the trunk is
    /// level or unrelated, while the trunk everybody merges into has run ahead.
    /// Asking the push remote, as the obvious reading would, says nothing at all
    /// in the one workspace where the bug had already happened.
    #[test]
    fn the_branch_upstream_beats_the_push_remote() {
        let (dir, repo) = scratch("base-upstream");
        let first = commit_on(&repo, "main", None, "first", "one\n");
        let second = commit_on(&repo, "trunk", Some(&first), "second", "two\n");
        repo.remote("origin", "https://example.invalid/trunk.git")
            .unwrap();
        repo.remote("fork", "https://example.invalid/fork.git")
            .unwrap();
        repo.reference("refs/remotes/fork/main", first.id(), true, "test")
            .unwrap();
        repo.reference("refs/remotes/origin/main", second.id(), true, "test")
            .unwrap();
        repo.find_branch("main", BranchType::Local)
            .unwrap()
            .set_upstream(Some("origin/main"))
            .unwrap();

        let warning =
            base_behind_warning(&repo, "fork", "main").expect("the fork's copy silenced it");
        assert!(
            warning.contains("1 commit(s) behind origin/main"),
            "{warning}"
        );
        assert!(warning.contains("git fetch origin main:main"), "{warning}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A commit on a branch, moving the branch to it. `msg` is what tells a
    /// session's own publish from a commit somebody else left there.
    fn commit_on<'r>(
        repo: &'r Repository,
        branch: &str,
        parent: Option<&Commit>,
        msg: &str,
        content: &str,
    ) -> Commit<'r> {
        let mut tb = repo.treebuilder(None).unwrap();
        let blob = repo.blob(content.as_bytes()).unwrap();
        tb.insert("f.txt", blob, 0o100644).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let sig = Signature::now("t", "t@t.t").unwrap();
        let parents: Vec<&Commit> = parent.into_iter().collect();
        let oid = repo.commit(None, &sig, &sig, msg, &tree, &parents).unwrap();
        let commit = repo.find_commit(oid).unwrap();
        repo.branch(branch, &commit, true).unwrap();
        commit
    }

    /// `git init`, then `ortak init`, then a day's work: the first publish told
    /// them to set base_branch to the branch these tasks merge into, and there
    /// is no name they could have written there.
    #[test]
    fn a_repository_with_no_commits_says_so_instead_of_blaming_the_config() {
        let dir = std::env::temp_dir().join(format!("ortak-unborn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = Repository::init(&dir).unwrap();
        assert!(unborn(&repo));
        let err = base_commit_for(&repo, "main").unwrap_err().to_string();
        assert!(err.contains("no commits yet"), "{err}");
        assert!(!err.contains("ortak.toml"), "{err}");

        // A commit and a branch, and HEAD left pointing at a name nothing has
        // been written to, which is what `git checkout --orphan` leaves behind.
        // The history is right there, so saying it has none is a lie that also
        // stops `doctor` checking the base branch.
        commit_on(&repo, "main", None, "base\n", "base\n");
        repo.set_head("refs/heads/nothing-here").unwrap();
        assert!(
            !unborn(&repo),
            "an unborn HEAD beside branches that have commits is not an empty repository"
        );
        let err = base_commit_for(&repo, "no-such-branch")
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not name a branch"), "{err}");
        assert!(base_commit_for(&repo, "main").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The arithmetic an amend rests on: rebuilding a branch reaches back to
    /// where its own work began, and the deliverable after it still starts from
    /// everything that has shipped. Getting the second half wrong re-publishes
    /// work that already went out, which is the bug incremental publish exists
    /// to fix.
    ///
    /// Then the two questions the journal cannot answer alone: a branch it has
    /// no row for is not a branch to refuse, and a branch that has moved since
    /// this session published it is not one to rebuild.
    #[test]
    fn an_amend_rebuilds_its_own_branch_and_adopts_one_it_did_not_publish() {
        let dir = std::env::temp_dir().join(format!("ortak-amend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        db.upsert_session("sess", "claude-sess", "llm", Some("claude-code"))
            .unwrap();
        let session = db.resolve_session("sess").unwrap();
        let s = session.id;
        let files = |after: i64| -> Vec<String> {
            db.session_files(s, after)
                .unwrap()
                .into_iter()
                .map(|(f, _)| f)
                .collect()
        };
        let head = || db.max_edit_id(s).unwrap().unwrap();
        let repo = Repository::init(dir.join("repo")).unwrap();
        let mine = "shipped\n\nOrtak-Session: sess\n";
        let base = commit_on(&repo, "main", None, "base\n", "base\n");

        // The first deliverable ships.
        db.insert_edit(s, "one.rs", "create", None, &[], None)
            .unwrap();
        commit_on(&repo, "task/one", Some(&base), mine, "one\n");
        db.record_publish(s, "task/one", head()).unwrap();

        // Review lands, so the fix goes into the same branch.
        db.insert_edit(s, "one.rs", "modify", None, &[], None)
            .unwrap();
        let a = amend_target(&repo, &db.publishes(s).unwrap(), Some("task/one"), &session).unwrap();
        // How far back it reaches comes from the journal half, which runs before
        // the repository is known; the rest of the answer is this one.
        let reach = amend_reach(&db.publishes(s).unwrap(), Some("task/one"))
            .unwrap()
            .1;
        assert_eq!((reach, a.branch, a.rewrites), (0, "task/one", true));
        assert_eq!(files(reach), vec!["one.rs"]);
        db.amend_publish(s, "task/one", head()).unwrap();
        assert_eq!(
            db.publishes(s).unwrap().len(),
            1,
            "an amend must not add a second publish row"
        );

        // The next deliverable is its own branch and carries only its own work.
        db.insert_edit(s, "two.rs", "create", None, &[], None)
            .unwrap();
        let history = db.publishes(s).unwrap();
        assert_eq!(files(history[0].last_edit_id), vec!["two.rs"]);
        commit_on(&repo, "task/two", Some(&base), mine, "two\n");
        db.record_publish(s, "task/two", head()).unwrap();

        // task/one is behind the newest publish now, so it is out of reach, and
        // amending task/two reaches back only as far as task/one shipped.
        let history = db.publishes(s).unwrap();
        assert!(amend_target(&repo, &history, Some("task/one"), &session).is_err());
        assert!(amend_target(&repo, &history, None, &session).is_err());
        assert!(
            amend_target(&repo, &history, Some("never/published"), &session).is_err(),
            "a branch that is not in the repository is a typo, not one to create"
        );
        assert!(amend_target(&repo, &history, Some("task/two"), &session).is_ok());
        assert_eq!(
            files(amend_reach(&history, Some("task/two")).unwrap().1),
            vec!["two.rs"]
        );

        // A branch this session never published: nothing on it is this
        // session's to rewrite, so the tip becomes the floor and the work since
        // the last publish goes on top of it.
        let theirs = commit_on(
            &repo,
            "feat/theirs",
            Some(&base),
            "their work\n",
            "theirs\n",
        );
        db.insert_edit(s, "three.rs", "create", None, &[], None)
            .unwrap();
        let history = db.publishes(s).unwrap();
        let adopted = amend_target(&repo, &history, Some("feat/theirs"), &session).unwrap();
        assert!(
            !adopted.rewrites,
            "a commit this session did not publish must not be rewritten"
        );
        let adopted_reach = amend_reach(&history, Some("feat/theirs")).unwrap().1;
        assert_eq!(adopted_reach, history[0].last_edit_id);
        assert_eq!(files(adopted_reach), vec!["three.rs"]);

        // And the branch it did publish, once somebody else has put a commit on
        // top: rebuilding it from the tip's parent would drop that commit.
        commit_on(
            &repo,
            "task/two",
            Some(&theirs),
            "a fix by hand\n",
            "moved\n",
        );
        let err = amend_target(&repo, &db.publishes(s).unwrap(), Some("task/two"), &session)
            .unwrap_err()
            .to_string();
        assert!(err.contains("would drop it"), "{err}");
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
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-qm",
                "base",
            ],
        );
        git(&work, &["remote", "add", "up", bare.to_str().unwrap()]);
        git(&work, &["push", "-q", "up", "main"]);

        assert!(on_remote(&work, "up", "main"));
        assert!(!on_remote(&work, "up", "feat/never-pushed"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// One session changes a function, another is working in the file that
    /// calls it: the break the gate cannot see, and the only thing the scan is
    /// for. Publishing the first session has to name the second.
    ///
    /// The scan and the release read the same rows, so this fails the moment
    /// they run in the other order, which is how the scan spent two rounds
    /// printing nothing while `ortak impact` answered the same question fine.
    #[test]
    fn a_published_branch_still_names_what_it_may_have_broken() {
        let dir = std::env::temp_dir().join(format!("ortak-shipped-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.py"), "def remote_for(cfg):\n    return cfg\n").unwrap();
        std::fs::write(dir.join("caller.py"), "value = remote_for(config)\n").unwrap();

        let ws = Workspace::at(&dir);
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let mine = db.upsert_session("a", "claude-a", "llm", None).unwrap();
        let theirs = db.upsert_session("b", "claude-b", "llm", None).unwrap();
        db.apply_edit_regions(
            mine,
            "lib.py",
            &[crate::regions::Hunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
            }],
            None,
        )
        .unwrap();
        db.insert_edit(theirs, "caller.py", "modify", None, &[], None)
            .unwrap();

        let files = vec![("lib.py".to_string(), "modify".to_string())];
        let (freed, affected) =
            free_lines_and_scan(&ws, &Config::default(), &db, mine, &files).unwrap();

        assert_eq!(freed, 1, "the work is out, so the lines are free");
        assert_eq!(affected.len(), 1, "and the caller is still named");
        assert_eq!(affected[0].name, "remote_for");
        assert_eq!(affected[0].file, "caller.py");
        assert_eq!(affected[0].session, theirs);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same session's second deliverable, on a file nobody else references.
    /// The first branch's lines are cooled by then, and a scan that still reads
    /// them reports the first branch's blast radius against the second.
    ///
    /// A session four deliverables in would get four deliverables' worth of
    /// names that way, and a report wrong three times is not read a fourth.
    #[test]
    fn a_second_publish_scans_only_what_it_is_shipping() {
        let dir = std::env::temp_dir().join(format!("ortak-second-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.py"), "def remote_for(cfg):\n    return cfg\n").unwrap();
        // other.py has to define something, or the second scan finds nothing
        // because there was nothing to find and the test passes for free.
        std::fs::write(dir.join("other.py"), "def nobody_calls_this():\n    pass\n").unwrap();
        std::fs::write(dir.join("caller.py"), "value = remote_for(config)\n").unwrap();

        let ws = Workspace::at(&dir);
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let mine = db.upsert_session("a", "claude-a", "llm", None).unwrap();
        let theirs = db.upsert_session("b", "claude-b", "llm", None).unwrap();
        db.insert_edit(theirs, "caller.py", "modify", None, &[], None)
            .unwrap();
        let first_line = [crate::regions::Hunk {
            old_start: 1,
            old_lines: 0,
            new_start: 1,
            new_lines: 1,
        }];
        let publish = |file: &str| {
            let files = vec![(file.to_string(), "modify".to_string())];
            free_lines_and_scan(&ws, &Config::default(), &db, mine, &files).unwrap()
        };

        db.apply_edit_regions(mine, "lib.py", &first_line, None)
            .unwrap();
        let (cooled, affected) = publish("lib.py");
        assert_eq!(cooled, 1);
        assert_eq!(affected.len(), 1, "the first branch does touch remote_for");

        db.apply_edit_regions(mine, "other.py", &first_line, None)
            .unwrap();
        let (cooled, affected) = publish("other.py");
        assert_eq!(cooled, 1);
        let named: Vec<&str> = affected.iter().map(|r| r.name.as_str()).collect();
        assert!(
            named.is_empty(),
            "remote_for shipped on the first branch, not this one: {named:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// What a fork owner's first push has to come back with: the remote it went
    /// to, the other one in the clone, and a way out that is not another
    /// publish, since the publish is already recorded by the time git refuses.
    #[test]
    fn a_failed_push_names_the_remote_and_the_way_out() {
        let root = std::env::temp_dir().join(format!("ortak-advice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = Repository::init(&root).unwrap();
        repo.remote("origin", "https://example.com/upstream.git")
            .unwrap();

        let advice = push_advice(&repo, "origin", "task/ortak-2-login", "");
        assert!(
            advice.contains("origin (https://example.com/upstream.git)"),
            "{advice}"
        );
        // In a tree the two commands answer to the repository they run in, so
        // the advice has to say which one that is.
        let nested = push_advice(
            &repo,
            "origin",
            "task/ortak-2-login",
            &run_it_from("repos/x"),
        );
        assert!(nested.contains("from repos/x"), "{nested}");
        assert!(
            advice.contains("git config ortak.remote <name>"),
            "{advice}"
        );
        assert!(
            advice.contains("git push <name> task/ortak-2-login"),
            "{advice}"
        );
        // The other half of the same mistake: ortak.remote naming a remote the
        // clone does not have. The push fails the same way and the advice still
        // has to say something true.
        let advice = push_advice(&repo, "kaan", "task/ortak-2-login", "");
        assert!(advice.contains("no remote by that name"), "{advice}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A rehearsal has to run the real thing: the same replay, the same
    /// failures, and none of the recording. The three things publish leaves
    /// behind are a branch, a publish row and freed lines, so a dry run is
    /// exactly the run that leaves none of them.
    #[test]
    fn a_dry_run_builds_the_branch_and_records_none_of_it() {
        let root = std::env::temp_dir().join(format!("ortak-dryrun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut opts = git2::RepositoryInitOptions::new();
        opts.initial_head("main");
        let repo = Repository::init_opts(&root, &opts).unwrap();
        std::fs::write(root.join("f.txt"), "one\ntwo\n").unwrap();
        let sig = Signature::now("t", "t@t").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();

        let ws = Workspace::at(&root);
        let cfg = Config::default();
        std::fs::create_dir_all(&ws.ortak_dir).unwrap();
        let shadow = crate::shadow::init(&ws, &cfg).unwrap();
        crate::shadow::baseline(&shadow, &ws, &cfg).unwrap();
        let db = Db::open(&ws.db_path).unwrap();
        let me = db
            .upsert_session("ext", "claude-a", "llm", Some("rehearse a publish"))
            .unwrap();

        // One journaled edit, the way the daemon records one.
        std::fs::write(root.join("f.txt"), "one\nTWO\n").unwrap();
        let commit = crate::shadow::commit_edit(
            &shadow,
            "f.txt",
            crate::shadow::Change::Modify,
            "claude-a",
            "ext",
            None,
        )
        .unwrap();
        let hunk = crate::regions::Hunk {
            old_start: 2,
            old_lines: 1,
            new_start: 2,
            new_lines: 1,
        };
        db.insert_edit(me, "f.txt", "modify", Some(&commit), &[hunk], None)
            .unwrap();
        db.apply_edit_regions(me, "f.txt", &[hunk], None).unwrap();

        let opts = |dry_run| PublishOpts {
            branch: Some("task/rehearsal"),
            base: None,
            exclude: &[],
            repo: None,
            scope: Scope::New,
            push: false,
            message: None,
            dry_run,
            squash: false,
        };
        let session = format!("ortak-{me}");
        run(&ws, &cfg, &session, opts(true)).unwrap();
        assert!(
            repo.find_branch("task/rehearsal", BranchType::Local)
                .is_err(),
            "the dry run created the branch"
        );
        assert!(
            db.publishes(me).unwrap().is_empty(),
            "the dry run moved the session's high-water mark"
        );
        assert_eq!(
            db.session_regions(me).unwrap().len(),
            1,
            "the dry run freed lines the session is still holding"
        );

        // The same command without it does all three, and the branch it builds
        // is the one the rehearsal described.
        run(&ws, &cfg, &session, opts(false)).unwrap();
        let branch = repo
            .find_branch("task/rehearsal", BranchType::Local)
            .unwrap();
        let published = branch.get().peel_to_tree().unwrap();
        assert_eq!(named_file_in(&published, &repo, "f.txt"), "one\nTWO\n");
        assert_eq!(db.publishes(me).unwrap().len(), 1);
        assert!(db.session_regions(me).unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// The summary a reviewer reads, against the branch's own base. A file the
    /// session created and then edited leaves `modify` as the journal's last
    /// word on it, and saying that about a file the branch is adding is the
    /// summary describing somebody else's change.
    #[test]
    fn a_file_the_branch_adds_is_not_summarised_as_a_modification() {
        let root = std::env::temp_dir().join(format!("ortak-netkind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("repos/inner")).unwrap();
        let repo = Repository::init(&root).unwrap();
        std::fs::write(root.join(".gitignore"), "repos/\n").unwrap();
        std::fs::write(root.join("there.rs"), "old\n").unwrap();
        commit_everything(&repo);
        let base = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();

        assert_eq!(net_kind(&base, "", "there.rs", "modify"), "modify");
        assert_eq!(net_kind(&base, "", "new.rs", "modify"), "create");
        assert_eq!(
            net_kind(&base, "", "gone.rs", "delete"),
            "delete",
            "a file the base never had cannot be deleted into existence"
        );

        // A nested repository's own base holds its own paths, so the prefix has
        // to come off first or every file it publishes reads as new.
        let inner = Repository::init(root.join("repos/inner")).unwrap();
        std::fs::write(root.join("repos/inner/there.rs"), "old\n").unwrap();
        commit_everything(&inner);
        let nested = inner
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        assert!(nested.get_path(Path::new("repos/inner/there.rs")).is_err());
        assert_eq!(
            net_kind(&nested, "repos/inner", "repos/inner/there.rs", "modify"),
            "modify"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn commit_everything(repo: &Repository) {
        let sig = Signature::now("t", "t@t").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["."], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();
    }

    /// A workspace shaped like the tree this round is for: a root repository on
    /// `16.0` whose `.gitignore` hides `repos/`, and a repository inside it with
    /// a trunk of its own.
    fn nested_workspace(name: &str) -> (std::path::PathBuf, Repository, Repository) {
        let root = std::env::temp_dir().join(format!("ortak-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("repos/inner/models")).unwrap();
        let mut opts = git2::RepositoryInitOptions::new();
        let outer = Repository::init_opts(&root, opts.initial_head("16.0")).unwrap();
        std::fs::write(root.join(".gitignore"), "repos/\n").unwrap();
        std::fs::write(root.join("README.md"), "root\n").unwrap();
        commit_everything(&outer);
        let inner =
            Repository::init_opts(root.join("repos/inner"), opts.initial_head("main")).unwrap();
        std::fs::write(root.join("repos/inner/models/sale.py"), "PRICE = 1\n").unwrap();
        commit_everything(&inner);
        (root, outer, inner)
    }

    /// One edit as the daemon records one: a shadow micro-commit and the
    /// journal row that points at it.
    fn journal_edit(shadow: &Repository, db: &Db, session: i64, rel: &str) {
        let commit = crate::shadow::commit_edit(
            shadow,
            rel,
            crate::shadow::Change::Modify,
            "b",
            "ext",
            None,
        )
        .unwrap();
        let hunk = crate::regions::Hunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
        };
        db.insert_edit(session, rel, "modify", Some(&commit), &[hunk], None)
            .unwrap();
    }

    #[test]
    fn a_branch_is_built_in_the_repository_its_files_live_in() {
        let (root, outer, inner) = nested_workspace("nested");
        let ws = Workspace::at(&root);
        let cfg = Config::default();
        std::fs::create_dir_all(&ws.ortak_dir).unwrap();
        let shadow = crate::shadow::init(&ws, &cfg).unwrap();
        crate::shadow::baseline(&shadow, &ws, &cfg).unwrap();
        let db = Db::open(&ws.db_path).unwrap();
        let me = db
            .upsert_session("ext", "b", "llm", Some("edit the nested repo"))
            .unwrap();

        std::fs::write(root.join("repos/inner/models/sale.py"), "PRICE = 2\n").unwrap();
        journal_edit(&shadow, &db, me, "repos/inner/models/sale.py");

        run(
            &ws,
            &cfg,
            &format!("ortak-{me}"),
            PublishOpts {
                branch: Some("task/nested"),
                base: None,
                exclude: &[],
                repo: None,
                scope: Scope::New,
                push: false,
                message: None,
                dry_run: false,
                squash: false,
            },
        )
        .unwrap();

        // In the repository that holds the file, not the one at the top of the
        // workspace, and carrying the path that repository knows it by.
        let built = inner
            .find_branch("task/nested", BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = built.tree().unwrap();
        assert_eq!(
            named_file_in(&tree, &inner, "models/sale.py"),
            "PRICE = 2\n"
        );
        assert!(
            tree.get_path(Path::new("repos/inner/models/sale.py"))
                .is_err(),
            "the journal's path reached the branch"
        );
        assert!(
            outer.find_branch("task/nested", BranchType::Local).is_err(),
            "the branch was built in the workspace root instead"
        );
        // Its own trunk, and its own `.gitignore` decision: the root hides
        // `repos/`, which says nothing about what the repository inside it
        // tracks.
        let trunk = inner
            .find_branch("main", BranchType::Local)
            .unwrap()
            .get()
            .target()
            .unwrap();
        assert_eq!(built.parent(0).unwrap().id(), trunk);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_session_across_two_repositories_is_refused_and_repo_takes_one() {
        let (root, _outer, inner) = nested_workspace("spanning");
        let ws = Workspace::at(&root);
        let cfg = Config::default();
        std::fs::create_dir_all(&ws.ortak_dir).unwrap();
        let shadow = crate::shadow::init(&ws, &cfg).unwrap();
        crate::shadow::baseline(&shadow, &ws, &cfg).unwrap();
        let db = Db::open(&ws.db_path).unwrap();
        let me = db.upsert_session("ext", "b", "llm", Some("both")).unwrap();

        // The root repository's file is journaled first, so its edit id is the
        // lower one and the mark below has something to stop short of.
        std::fs::write(root.join("README.md"), "root two\n").unwrap();
        journal_edit(&shadow, &db, me, "README.md");
        std::fs::write(root.join("repos/inner/models/sale.py"), "PRICE = 2\n").unwrap();
        journal_edit(&shadow, &db, me, "repos/inner/models/sale.py");

        let opts = |repo| PublishOpts {
            branch: Some("task/both"),
            base: None,
            exclude: &[],
            repo,
            scope: Scope::New,
            push: false,
            message: None,
            dry_run: false,
            squash: false,
        };
        let session = format!("ortak-{me}");
        let refused = run(&ws, &cfg, &session, opts(None))
            .unwrap_err()
            .to_string();
        for expected in ["repos/inner", "the workspace root", "--repo"] {
            assert!(refused.contains(expected), "{expected} missing: {refused}");
        }

        run(&ws, &cfg, &session, opts(Some("repos/inner"))).unwrap();
        assert!(inner.find_branch("task/both", BranchType::Local).is_ok());
        // The mark stops short of the edit this branch did not carry, so the
        // other repository's work is still waiting for its own publish rather
        // than gone.
        let mark = db.publishes(me).unwrap()[0].last_edit_id;
        let waiting: Vec<String> = db
            .session_files(me, mark)
            .unwrap()
            .into_iter()
            .map(|(f, _)| f)
            .collect();
        assert!(
            waiting.contains(&"README.md".to_string()),
            "the root repository's edit left the journal: {waiting:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_file_no_repository_holds_is_named_rather_than_dropped() {
        let root = std::env::temp_dir().join(format!("ortak-homeless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let ws = Workspace::at(&root);
        let files = vec![("notes.txt".to_string(), "modify".to_string())];
        let refused = match owning_repo(&ws, &files) {
            Err(e) => e.to_string(),
            Ok((dir, _)) => panic!("a directory with no git in it published as {dir:?}"),
        };
        assert!(refused.contains("notes.txt"), "{refused}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_nested_repository_without_the_configured_trunk_uses_its_own() {
        let dir = std::env::temp_dir().join(format!("ortak-trunk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut opts = git2::RepositoryInitOptions::new();
        let repo = Repository::init_opts(&dir, opts.initial_head("master")).unwrap();
        commit_everything(&repo);
        let mut cfg = Config::default();
        cfg.publish.base_branch = "16.0".into();

        assert_eq!(base_here(&repo, &cfg, None, "repos/x").unwrap(), "master");
        assert_eq!(
            base_here(&repo, &cfg, Some("origin/16.0"), "repos/x").unwrap(),
            "origin/16.0",
            "--base still wins"
        );
        // At the workspace root nothing falls back: that is the single
        // repository publish has always had, and its missing branch reaches the
        // error it has always reached.
        assert_eq!(base_here(&repo, &cfg, None, "").unwrap(), "16.0");

        // Neither name is there to fall back to, and HEAD is not an answer.
        let other = std::env::temp_dir().join(format!("ortak-trunk2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other);
        let repo = Repository::init_opts(&other, opts.initial_head("feature/x")).unwrap();
        commit_everything(&repo);
        let refused = base_here(&repo, &cfg, None, "repos/x")
            .unwrap_err()
            .to_string();
        assert!(refused.contains("--base"), "{refused}");
        assert!(
            !refused.contains("feature/x"),
            "HEAD became the base: {refused}"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }
}
