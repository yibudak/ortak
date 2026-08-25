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
    let can_publish = checks.iter().all(|c| c.state == State::Ok);
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

/// The five checks a publish needs, in the order it needs them. A list, not a
/// framework: each one is a few lines that call what already knows the answer,
/// and every later check reads whether the earlier ones held.
fn run_checks(ws: &Workspace, cfg: &Config) -> Vec<Check> {
    let mut checks = Vec::new();
    let Ok(repo) = git2::Repository::open(&ws.root) else {
        checks.push(failed(
            "git_repository",
            format!("{} is not a git repository", ws.root.display()),
            "`git init` here, or run ortak in the checkout you meant to work in",
        ));
        // None of the three below can be asked at all without a repository, and
        // failing them would send somebody off to configure a base branch in a
        // directory git has never heard of.
        for check in ["commits", "base_branch", "push_remote"] {
            checks.push(skipped(
                check,
                "not checked: there is no git repository here",
            ));
        }
        checks.push(daemon_check(ws));
        return checks;
    };
    checks.push(ok("git_repository", ws.root.display().to_string()));

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
}
