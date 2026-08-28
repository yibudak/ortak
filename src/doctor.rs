//! `ortak doctor`: can this workspace publish, asked before the work rather
//! than an hour into it.
//!
//! Round 8 shipped three fixes with one shape between them. #69 named the push
//! remote at `init`, because a fork owner's first `--push` went to an upstream
//! they cannot write to. #75 told a publish that the repository had no commits
//! instead of blaming the base branch. #77 said when the directory `init` had
//! just set up held no git repository. Every one of them fires at the moment it
//! is already too late. The checks were never what was missing; a way to ask
//! them first was.
//!
//! Each check here calls the code that already answers it, so `publish` and
//! `doctor` cannot drift into two opinions about one repository. Nothing here
//! touches the network: whether a remote will take a push is not knowable
//! without pushing, and a check that hangs on a dead DNS server is worse than
//! one that says what it did not test.

use crate::config::Config;
use crate::db::{Db, HEARTBEAT_ALIVE_SECS};
use crate::workspace::Workspace;
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;

/// How one check came out. `Skipped` is not a quiet pass: a repository with no
/// commits cannot be asked which branch it publishes onto, and answering "ok"
/// there would be a lie while answering "failed" blames the wrong thing.
#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum State {
    Ok,
    Failed,
    Skipped,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Failed => "FAILED",
            State::Skipped => "skipped",
        }
    }
}

/// One check and its answer. The wire shapes for the other read commands live
/// in `json`, declared apart from the database rows they are built from. This
/// one is not built from a row: it is computed to be read, and a second copy of
/// it over in `json` is the thing that would go stale, so the text output and
/// the JSON print the same struct.
#[derive(Serialize)]
struct Check {
    /// Machine-readable name. The text output spells it with spaces.
    check: &'static str,
    state: State,
    /// What was found, in one line.
    detail: String,
    /// What to do about it. Set on every failure, and on nothing else.
    fix: Option<String>,
}

#[derive(Serialize)]
struct Report {
    /// Nothing failed. A skipped check is not a quiet failure: the three that
    /// are skipped for a workspace whose root holds no repository are skipped
    /// because each repository under it answers them for itself, and requiring
    /// every check to be `Ok` reported that such a workspace could not publish
    /// while the report beside it said how to.
    can_publish: bool,
    checks: Vec<Check>,
}

fn ok(check: &'static str, detail: impl Into<String>) -> Check {
    Check {
        check,
        state: State::Ok,
        detail: detail.into(),
        fix: None,
    }
}

fn failed(check: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        check,
        state: State::Failed,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

fn skipped(check: &'static str, detail: impl Into<String>) -> Check {
    Check {
        check,
        state: State::Skipped,
        detail: detail.into(),
        fix: None,
    }
}

/// Returns whether every check passed. The caller turns that into the exit
/// code: a command shaped like this is in somebody's shell script by the end of
/// the week, and `ortak doctor && ortak publish ortak-3` only works if a failed
/// check is non-zero.
pub fn run(ws: &Workspace, cfg: &Config, as_json: bool) -> Result<bool> {
    let checks = run_checks(ws, cfg);
    let can_publish = verdict(&checks);
    if as_json {
        crate::json::print(&Report {
            can_publish,
            checks,
        })?;
        return Ok(can_publish);
    }
    println!(
        "{}",
        if can_publish {
            "this workspace can publish"
        } else {
            "this workspace cannot publish yet"
        }
    );
    for c in &checks {
        println!(
            "  {:<8}{:<15} {}",
            c.state.label(),
            c.check.replace('_', " "),
            c.detail
        );
        if let Some(fix) = &c.fix {
            println!("  {:<8}{:<15} fix: {}", "", "", fix);
        }
    }
    println!("\nno check here touched the network");
    Ok(can_publish)
}

/// Whether the report is a pass: nothing failed, rather than everything was
/// asked. A check that could not be asked is not evidence against publishing,
/// and anything genuinely broken fails a check of its own, so the two rules
/// only ever differ where something was skipped.
fn verdict(checks: &[Check]) -> bool {
    !checks.iter().any(|c| c.state == State::Failed)
}

/// The checks a publish needs, in the order it needs them. A list, not a
/// framework: each one is a few lines that call what already knows the answer,
/// and every later check reads whether the earlier ones held.
fn run_checks(ws: &Workspace, cfg: &Config) -> Vec<Check> {
    let mut checks = Vec::new();
    // Every check below this one is about the repository at the workspace root.
    // A workspace can cover a tree of them now, and each of the others answers
    // these four for itself.
    //
    // ponytail: counted, not asked. Sixty repositories are sixty commits, sixty
    // base branches and sixty push remotes, and a report with two hundred and
    // forty rows in it is not a report. `ortak doctor` inside one of them
    // answers for that one.
    let others = ws.repositories().iter().filter(|d| !d.is_empty()).count();
    let Ok(repo) = git2::Repository::open(&ws.root) else {
        // A workspace one directory inside a checkout is the common way to land
        // here, and calling that "not a git repository" is false enough to cost
        // the reader their trust in the four lines under it. A directory
        // holding a tree of repositories is the other way, and `git init` at
        // the root of one is close to the worst advice this tool could give.
        checks.push(match (others, crate::publish::repository_above(&ws.root)) {
            (0, Some(above)) => failed(
                "git_repository",
                format!(
                    "{} is inside the repository at {}, not the root of one, and publish builds from the workspace root",
                    ws.root.display(),
                    above.display()
                ),
                format!("run `ortak init` in {}, or make this directory a repository of its own with `git init`", above.display()),
            ),
            (0, None) => failed(
                "git_repository",
                format!("{} is not a git repository", ws.root.display()),
                "`git init` here, or run ortak in the checkout you meant to work in",
            ),
            // Not a failure since #105. A branch is built in the repository
            // its files live in, so a tree whose root holds no repository of
            // its own publishes perfectly well, and this used to report that
            // it could not while the same line explained how.
            (n, _) => ok(
                "git_repository",
                format!(
                    "{} holds no repository of its own, and {n} under it that this workspace journals; \
                     a branch is built in the one its files live in, so publish with `--repo <directory>` \
                     and run `ortak doctor` inside a repository to check that one",
                    ws.root.display()
                ),
            ),
        });
        // None of the three below can be asked at all without a repository, and
        // failing them would send somebody off to configure a base branch in a
        // directory git has never heard of.
        let why = match others {
            0 => "not checked: there is no git repository at the workspace root".to_string(),
            n => format!("not checked: the workspace root is not a repository, and each of the {n} under it answers this for itself"),
        };
        for check in ["commits", "base_branch", "push_remote"] {
            checks.push(skipped(check, why.clone()));
        }
        checks.push(daemon_check(ws));
        checks.push(baseline_check(ws));
        checks.push(arbiter_check(cfg));
        return checks;
    };
    checks.push(ok(
        "git_repository",
        match others {
            0 => ws.root.display().to_string(),
            n => format!(
                "{}, and {n} more repositories in this tree",
                ws.root.display()
            ),
        },
    ));

    let has_commits = !crate::publish::unborn(&repo);
    checks.push(if has_commits {
        ok("commits", head_description(&repo))
    } else {
        failed(
            "commits",
            "this repository has no commits yet, so there is nothing for a branch to build on",
            "make the first commit",
        )
    });

    let base = &cfg.publish.base_branch;
    checks.push(if !has_commits {
        skipped("base_branch", "not checked: this repository has no commits")
    } else {
        // The message a failure carries is the one `publish` would have printed
        // an hour from now, because it is the same call.
        match crate::publish::base_commit_for(&repo, base) {
            Ok(c) => ok(
                "base_branch",
                format!("{} ({})", base, &c.id().to_string()[..8]),
            ),
            Err(e) => failed("base_branch", e.to_string(), pick_a_branch(&repo)),
        }
    });

    let remote = crate::publish::remote_for(&repo, cfg);
    let url = repo
        .find_remote(&remote)
        .ok()
        .and_then(|r| r.url().map(str::to_string));
    checks.push(match url {
        Some(url) => ok(
            "push_remote",
            format!("{remote} ({url}); not tested, that takes a push"),
        ),
        None => failed(
            "push_remote",
            format!(
                "`ortak publish --push` would push to {remote}, and this clone has no remote by that name; publish still builds the branch here without it"
            ),
            name_a_remote(&repo),
        ),
    });

    checks.push(daemon_check(ws));
    checks.push(baseline_check(ws));
    checks.push(arbiter_check(cfg));
    checks
}

/// Where the checkout stands, for a repository that has commits.
fn head_description(repo: &git2::Repository) -> String {
    let Ok(head) = repo.head() else {
        return "HEAD cannot be read".to_string();
    };
    let at = head
        .target()
        .map(|oid| oid.to_string()[..8].to_string())
        .unwrap_or_else(|| "unknown".to_string());
    match head.shorthand() {
        Some(name) if head.is_branch() => format!("HEAD is on {name} ({at})"),
        _ => format!("HEAD is detached at {at}"),
    }
}

/// The branches this checkout actually has, which beats guessing at the trunk:
/// a repository whose only branch is not `main` is exactly the case that found
/// #75, and it is the reader who knows which of these their tasks merge into.
fn pick_a_branch(repo: &git2::Repository) -> String {
    let names: Vec<String> = repo
        .branches(Some(git2::BranchType::Local))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|(b, _)| b.name().ok().flatten().map(str::to_string))
        .collect();
    match names.is_empty() {
        true => "this checkout has no local branches".to_string(),
        false => format!("pick one this checkout has: {}", names.join(", ")),
    }
}

fn name_a_remote(repo: &git2::Repository) -> String {
    let names: Vec<String> = repo
        .remotes()
        .iter()
        .flat_map(|r| r.iter().flatten().map(str::to_string))
        .collect();
    match names.is_empty() {
        true => "add one with `git remote add <name> <url>`".to_string(),
        false => format!(
            "git config ortak.remote <name>; this clone has {}",
            names.join(", ")
        ),
    }
}

/// The daemon is not something `publish` calls, and it is the reason there is
/// anything to publish: while it is down nothing anybody edits reaches the
/// journal, and the branch that comes out is missing exactly that work.
fn daemon_check(ws: &Workspace) -> Check {
    let age = Db::open(&ws.db_path).and_then(|db| db.heartbeat_age());
    match age {
        Ok(Some(age)) if age <= HEARTBEAT_ALIVE_SECS => {
            ok("daemon", format!("running, last heartbeat {age}s ago"))
        }
        Ok(Some(age)) => failed(
            "daemon",
            format!("last heartbeat {age}s ago; edits are not reaching the journal, so a publish now ships whatever was recorded before it stopped"),
            "ortak daemon --detach",
        ),
        Ok(None) => failed(
            "daemon",
            "never started here, so nothing anybody edits is being recorded",
            "ortak daemon --detach",
        ),
        Err(e) => failed(
            "daemon",
            format!("cannot read the journal at {}: {e}", ws.db_path.display()),
            "check that .ortak is readable, or run `ortak init` in the workspace root",
        ),
    }
}

/// Whether the arbiter this workspace has asked for can run at all.
///
/// Every failure path in `orchestrator` is deliberately quiet: a command that
/// will not spawn, a non-zero exit, a timeout and unparseable output all fall
/// back to the deterministic rule, which at the gate is a denial. That is the
/// right behaviour and it means a typo in `ortak.toml` buys the deterministic
/// gate for the rest of the project, with a config file that says the arbiter
/// is on and nothing anywhere saying otherwise.
///
/// A broken arbiter fails rather than warns, and that is deliberate: this
/// feature's whole failure mode is silence, so the one command whose job is to
/// break silence must not answer `ok` and exit 0 on it.
///
/// ponytail: resolved, never run. A live probe costs nine seconds and a model
/// call on every `ortak doctor`, and it would break the promise this command
/// closes with. Whether the model answers well is not a question a health check
/// can ask. Whether the binary is there is.
fn arbiter_check(cfg: &Config) -> Check {
    let orc = &cfg.orchestrator;
    if !orc.enabled {
        return skipped(
            "arbiter",
            "[orchestrator] enabled = false; the gate and stop-the-line blame stand on their deterministic rules",
        );
    }
    match resolve_command(&orc.command) {
        Some(path) => ok(
            "arbiter",
            format!(
                "{} ({}), model {}, {}s timeout; not asked anything, that costs a ruling",
                orc.command,
                path.display(),
                orc.model,
                orc.timeout_secs
            ),
        ),
        None => failed(
            "arbiter",
            format!(
                "[orchestrator] enabled = true and `{}` is not on this PATH, so every ruling falls back to the deterministic rule and nothing says it happened",
                orc.command
            ),
            "install it, give [orchestrator] command an absolute path, or set enabled = false; this stops every ruling, not a publish",
        ),
    }
}

/// Where this process would find the arbiter command, or nothing.
///
/// ponytail: a PATH lookup and an absolute path, which is what a config holds.
/// Two ceilings. A relative command is resolved from here while `orchestrator`
/// spawns it from the temp directory, a case Rust calls unspecified anyway. And
/// this reads the PATH of whoever ran `doctor` rather than the hook process's,
/// which is the same environment in the ordinary case and not guaranteed to be.
fn resolve_command(command: &str) -> Option<PathBuf> {
    if command.contains('/') {
        let named = PathBuf::from(command);
        return crate::update::executable_file(&named).then_some(named);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(command))
        .find(|p| crate::update::executable_file(p))
}

/// Whether the shadow repository has a baseline commit.
///
/// A workspace whose baseline never landed passes every other check here and is
/// not a workspace: with no shadow HEAD there is nothing to diff a file
/// against, so the first touch of one classifies as a create and produces a
/// whole-file hunk, and one edited line of a 4000-line model claims all 4000
/// and locks every other session out of it. `init` says `already initialized`
/// and exits 0 on the second run, so nothing else ever mentions it.
///
/// The shadow repository is opened directly rather than through `shadow::open`,
/// which rewrites the exclude file on its way past. Reading a workspace should
/// not change it.
fn baseline_check(ws: &Workspace) -> Check {
    let head = git2::Repository::open(&ws.shadow_dir)
        .and_then(|repo| repo.head()?.peel_to_commit().map(|c| c.id().to_string()));
    match head {
        Ok(id) => ok("baseline", format!("captured ({})", &id[..8])),
        // Not "run ortak init": that is what somebody already did. Since #106 a
        // second run captures the baseline it finds missing, and says so. Two
        // states reach here and both end at that one command, so they differ in
        // what they report and not in what to do about it.
        Err(_) => failed(
            "baseline",
            if ws.shadow_dir.exists() {
                "the shadow repository here has no baseline commit, so the first edit to any file is recorded as a change to the whole of it".to_string()
            } else {
                format!(
                    "there is no shadow repository at {}",
                    ws.shadow_dir.display()
                )
            },
            "`ortak init` again here: it captures a baseline it finds missing",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(tag: &str) -> Workspace {
        let root = std::env::temp_dir().join(format!("ortak-doctor-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = Workspace::at(&root);
        std::fs::create_dir_all(&ws.ortak_dir).unwrap();
        ws
    }

    fn state_of(checks: &[Check], name: &str) -> State {
        checks.iter().find(|c| c.check == name).unwrap().state
    }

    /// A directory that is not a repository fails one check, and the four that
    /// cannot be asked say so. Reporting them as failures would send somebody
    /// configuring a base branch in a directory git has never heard of.
    #[test]
    fn what_cannot_be_asked_is_not_reported_as_broken() {
        let ws = workspace("no-repo");
        let checks = run_checks(&ws, &Config::default());
        assert_eq!(state_of(&checks, "git_repository"), State::Failed);
        assert_eq!(state_of(&checks, "commits"), State::Skipped);
        assert_eq!(state_of(&checks, "base_branch"), State::Skipped);
        assert_eq!(state_of(&checks, "push_remote"), State::Skipped);
        // The journal is readable without git, so this one is a real answer.
        assert_eq!(state_of(&checks, "daemon"), State::Failed);
        assert!(checks
            .iter()
            .all(|c| c.state != State::Failed || c.fix.is_some()));
        let _ = std::fs::remove_dir_all(&ws.root);
    }

    /// The state a failed `ortak init` leaves on disk: a database, a shadow
    /// repository, and no baseline commit. Every git check passes, the daemon
    /// check answers for the daemon, and until this one nothing in the report
    /// was about the thing that is actually broken.
    #[test]
    fn a_workspace_whose_baseline_never_landed_is_told_so() {
        let ws = workspace("no-baseline");
        git2::Repository::init(&ws.root).unwrap();
        let cfg = Config::default();
        let repo = crate::shadow::init(&ws, &cfg).unwrap();
        assert_eq!(state_of(&run_checks(&ws, &cfg), "baseline"), State::Failed);

        crate::shadow::baseline(&repo, &ws, &cfg).unwrap();
        assert_eq!(state_of(&run_checks(&ws, &cfg), "baseline"), State::Ok);
        let _ = std::fs::remove_dir_all(&ws.root);
    }

    /// A directory that is not a repository and holds three, which is the
    /// workspace round 12 was for. Doctor used to fail the first check with "is
    /// not a git repository" and send the reader to `git init` here, which at
    /// the root of a tree of repositories is about the worst thing it could
    /// say. Since #105 it is not a failure at all: that workspace publishes,
    /// from inside whichever repository the files live in, and the report
    /// opened with "cannot publish yet" and then explained on the next line
    /// how to.
    #[test]
    fn a_tree_of_repositories_publishes_and_is_told_so() {
        let ws = workspace("tree");
        let cfg = Config::default();
        for sub in ["odoo-server", "repos/altinkaya", "repos/other"] {
            std::fs::create_dir_all(ws.root.join(sub)).unwrap();
            git2::Repository::init(ws.root.join(sub)).unwrap();
        }
        // The two checks that are about this workspace rather than about git.
        Db::open(&ws.db_path).unwrap().heartbeat().unwrap();
        let shadow = crate::shadow::init(&ws, &cfg).unwrap();
        crate::shadow::baseline(&shadow, &ws, &cfg).unwrap();

        let checks = run_checks(&ws, &cfg);
        let repo_check = checks.iter().find(|c| c.check == "git_repository").unwrap();
        assert_eq!(repo_check.state, State::Ok);
        assert!(
            repo_check.detail.contains("3 under it"),
            "doctor does not say what the workspace covers: {}",
            repo_check.detail
        );
        assert!(
            !repo_check.detail.contains("git init") && repo_check.detail.contains("--repo"),
            "no way out of it either: {}",
            repo_check.detail
        );
        // The three below still cannot be asked here, and now say why.
        for check in ["commits", "base_branch", "push_remote"] {
            assert_eq!(state_of(&checks, check), State::Skipped);
        }
        assert!(
            verdict(&checks),
            "told it cannot publish over: {:?}",
            checks
                .iter()
                .filter(|c| c.state == State::Failed)
                .map(|c| (c.check, &c.detail))
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&ws.root);
    }

    /// `git init` and nothing else: the repository is real, the base branch
    /// question has no answer yet, and only one line of the report is about
    /// what to do next.
    #[test]
    fn a_repository_with_no_commits_is_told_about_its_commits() {
        let ws = workspace("no-commits");
        git2::Repository::init(&ws.root).unwrap();
        let checks = run_checks(&ws, &Config::default());
        assert_eq!(state_of(&checks, "git_repository"), State::Ok);
        assert_eq!(state_of(&checks, "commits"), State::Failed);
        // Not "base branch 'main' does not exist": nothing they could write in
        // ortak.toml would help until there is a commit.
        assert_eq!(state_of(&checks, "base_branch"), State::Skipped);
        assert_eq!(state_of(&checks, "push_remote"), State::Failed);
        let _ = std::fs::remove_dir_all(&ws.root);
    }

    /// The three states of a workspace's arbiter, none of which costs a ruling
    /// to find out. Off is a legitimate state and says so; a command that
    /// resolves is named along with the model and the timeout, because
    /// otherwise haiku and sonnet are the same report; and a command that does
    /// not resolve fails, since the config asked the machine for something it
    /// cannot do and the failure is otherwise perfectly silent.
    #[test]
    fn the_arbiter_is_checked_by_looking_for_it_and_not_by_asking_it() {
        let mut cfg = Config::default();
        assert_eq!(arbiter_check(&cfg).state, State::Skipped);

        cfg.orchestrator.enabled = true;
        cfg.orchestrator.command = "sh".into();
        let found = arbiter_check(&cfg);
        assert_eq!(found.state, State::Ok);
        assert!(
            found.detail.contains(&cfg.orchestrator.model) && found.detail.contains("20s"),
            "the report cannot tell haiku from sonnet: {}",
            found.detail
        );

        cfg.orchestrator.command = "definitely-not-a-real-binary".into();
        let missing = arbiter_check(&cfg);
        assert_eq!(missing.state, State::Failed);
        // The decision this check makes about publishing: a workspace running
        // with an arbiter that can never answer is not a workspace to start
        // work in, and doctor's exit code is the only place it can say so.
        assert!(!verdict(&[missing]));
    }
}
