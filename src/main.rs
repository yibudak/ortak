mod config;
mod daemon;
mod db;
mod hooks;
mod json;
mod line;
mod orchestrator;
mod publish;
mod regions;
mod shadow;
mod workspace;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;
use db::{Db, HEARTBEAT_ALIVE_SECS};
use workspace::Workspace;

#[derive(Parser)]
#[command(
    name = "ortak",
    version,
    about = "Coordination layer for concurrent agents sharing one live workspace"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize this directory as an ortak workspace
    Init,
    /// Run the journal daemon (file watcher and shadow repository) in the foreground
    Daemon,
    /// Show daemon and session status
    Status {
        /// Emit JSON for another program to read
        #[arg(long)]
        json: bool,
    },
    /// Show recent journal entries
    Log {
        /// Session reference (ortak-3, human, or agent name)
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Emit JSON for another program to read
        #[arg(long)]
        json: bool,
    },
    /// List sessions
    Sessions,
    /// Record a session's task intent
    Intent {
        /// Session reference (ortak-3)
        session: String,
        /// Task description
        text: Vec<String>,
    },
    /// Publish a session's net changes as a branch
    Publish {
        /// Session reference (ortak-3, human, or agent name)
        session: String,
        /// Branch name (default: <prefix>ortak-<id>-<slug>)
        #[arg(long)]
        branch: Option<String>,
        /// Workspace-relative path to keep out of the branch (repeatable)
        #[arg(long)]
        exclude: Vec<String>,
        /// Branch to build on, for this run only (default: [publish] base_branch)
        #[arg(long)]
        base: Option<String>,
        /// Publish everything the session has touched, not just what is new
        #[arg(long)]
        all: bool,
        /// Push the branch to the remote
        #[arg(long)]
        push: bool,
    },
    /// Report an error unrelated to your changes and stop the line
    Report {
        /// Reporting session (ortak-N)
        session: String,
        /// Command that produced the error
        #[arg(long)]
        command: Option<String>,
        /// Error output or description
        text: Vec<String>,
    },
    /// Resolve errors assigned to you and reopen the line
    Resolved {
        /// Session (ortak-N) whose error is fixed
        session: Option<String>,
        /// Resolve every open error, whoever owns it
        #[arg(long)]
        all: bool,
    },
    /// List error records
    Errors {
        /// Emit JSON for another program to read
        #[arg(long)]
        json: bool,
    },
    /// Reassign an open error
    Assign { error_id: i64, session: String },
    /// Harness hook adapters (read hook JSON from stdin)
    Hook {
        #[command(subcommand)]
        event: HookEvent,
    },
}

#[derive(Subcommand)]
enum HookEvent {
    /// Claude Code SessionStart
    SessionStart,
    /// Claude Code PreToolUse gate (Edit|Write|MultiEdit|NotebookEdit)
    PreEdit,
    /// Claude Code PostToolUse (Edit|Write|MultiEdit|NotebookEdit)
    PostEdit,
    /// Claude Code PreToolUse (Bash): claim files the command writes
    PreBash,
    /// Claude Code PostToolUse (Bash): error-reporting reminder
    PostBash,
    /// Claude Code UserPromptSubmit: hat durumu enjeksiyonu
    PromptContext,
    /// Claude Code SessionEnd
    SessionEnd,
}

/// What to do when the arguments do not parse.
///
/// The plugin's hooks.json and the installed binary update independently, so a
/// build can be handed a hook it has never heard of. Clap answers that with
/// exit code 2, which is exactly the code the harness reads as "block this tool
/// call", and the hook is registered for every session whether or not the
/// project is an ortak workspace. One stale binary therefore takes Bash away
/// everywhere until the user rebuilds, including in the shell they would
/// rebuild from. Nothing under `hook` may exit non-zero.
#[derive(Debug, PartialEq)]
enum ParseFailure {
    /// Print the message and exit non-zero, clap's usual behaviour.
    Report,
    /// Print the message, but succeed: someone asked for help.
    PrintAndSucceed,
    /// Say nothing and succeed. Hook output reaches the agent's context, so a
    /// usage error does not belong there.
    SilentSuccess,
}

fn classify_parse_failure(kind: clap::error::ErrorKind, first_arg: Option<&str>) -> ParseFailure {
    use clap::error::ErrorKind as K;
    let wants_output = matches!(
        kind,
        K::DisplayHelp | K::DisplayVersion | K::DisplayHelpOnMissingArgumentOrSubcommand
    );
    match (first_arg == Some("hook"), wants_output) {
        (false, _) => ParseFailure::Report,
        (true, true) => ParseFailure::PrintAndSucceed,
        (true, false) => ParseFailure::SilentSuccess,
    }
}
/// Rust ignores SIGPIPE, so the first `println!` after a closed pipe panics with
/// "failed printing to stdout" instead of the command simply ending. `ortak log
/// | head` is an ordinary thing to type.
#[cfg(unix)]
fn restore_sigpipe() {
    // SAFETY: sets a signal disposition before any thread is spawned.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn main() {
    restore_sigpipe();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => match classify_parse_failure(e.kind(), std::env::args().nth(1).as_deref()) {
            ParseFailure::Report => e.exit(),
            ParseFailure::PrintAndSucceed => {
                let _ = e.print();
                return;
            }
            ParseFailure::SilentSuccess => return,
        },
    };
    // Hook adapters must never break the agent's session: swallow errors, exit 0.
    if let Command::Hook { event } = &cli.command {
        let res = match event {
            HookEvent::SessionStart => hooks::session_start(),
            HookEvent::PreEdit => hooks::pre_edit(),
            HookEvent::PostEdit => hooks::post_edit(),
            HookEvent::PreBash => hooks::pre_bash(),
            HookEvent::PostBash => hooks::post_bash(),
            HookEvent::PromptContext => hooks::prompt_context(),
            HookEvent::SessionEnd => hooks::session_end(),
        };
        if let Err(e) = res {
            eprintln!("ortak hook: {}", e);
        }
        return;
    }
    if let Err(e) = run(cli) {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init => init(),
        Command::Daemon => {
            let ws = Workspace::discover_from_cwd()?;
            let cfg = Config::load(&ws.config_path)?;
            daemon::run(&ws, &cfg)
        }
        Command::Status { json } => status(json),
        Command::Log {
            session,
            limit,
            json,
        } => log(session.as_deref(), limit, json),
        Command::Sessions => sessions(),
        Command::Intent { session, text } => intent(&session, &text.join(" ")),
        Command::Publish {
            session,
            branch,
            exclude,
            base,
            all,
            push,
        } => {
            let ws = Workspace::discover_from_cwd()?;
            let cfg = Config::load(&ws.config_path)?;
            publish::run(
                &ws,
                &cfg,
                &session,
                publish::PublishOpts {
                    branch: branch.as_deref(),
                    base: base.as_deref(),
                    exclude: &exclude,
                    all,
                    push,
                },
            )
        }
        Command::Report {
            session,
            command,
            text,
        } => {
            let ws = Workspace::discover_from_cwd()?;
            let cfg = Config::load(&ws.config_path)?;
            line::report(&ws, &cfg, &session, command.as_deref(), &text.join(" "))
        }
        Command::Resolved { session, all } => {
            let ws = Workspace::discover_from_cwd()?;
            line::resolved(&ws, session.as_deref(), all)
        }
        Command::Errors { json } => {
            let ws = Workspace::discover_from_cwd()?;
            line::list(&ws, json)
        }
        Command::Assign { error_id, session } => {
            let ws = Workspace::discover_from_cwd()?;
            line::assign(&ws, error_id, &session)
        }
        Command::Hook { .. } => unreachable!("hook handled in main"),
    }
}

fn init() -> Result<()> {
    let root = std::env::current_dir()?;
    let ws = Workspace::at(&root);
    let existed = ws.ortak_dir.exists();
    std::fs::create_dir_all(&ws.ortak_dir)?;
    // .ortak holds the SQLite database and the shadow repository, so it is never
    // committed. Ignoring it from the inside keeps it out of `git status` without
    // editing the project's own .gitignore. Written before the early return so an
    // existing workspace picks it up by re-running init.
    let ignore = ws.ortak_dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n")?;
    }
    if existed {
        println!("already initialized: {}", ws.ortak_dir.display());
        return Ok(());
    }
    if !ws.config_path.exists() {
        std::fs::write(&ws.config_path, Config::default_toml())?;
        println!("wrote: {}", ws.config_path.display());
    }
    let cfg = Config::load(&ws.config_path)?;
    let db = Db::open(&ws.db_path)?;
    db.ensure_human()?;
    let repo = shadow::init(&ws, &cfg)?;
    print!("capturing baseline... ");
    let oid = shadow::baseline(&repo)?;
    println!("done ({})", &oid.to_string()[..8]);
    warn_about_ignored_repos(&repo, &ws.root);
    println!("\nworkspace ready: {}", ws.root.display());
    println!("next: run `ortak daemon` in another terminal or in the background");
    Ok(())
}

/// How deep to look for hidden repositories. A repo-of-repos keeps them one or
/// two levels down; past three this is walking somebody's node_modules.
const SUBREPO_DEPTH: usize = 3;
/// How many to name before summarizing. The point is that they exist.
const SUBREPO_LISTED: usize = 10;

/// A directory the project's git ignores is invisible to the journal, so a
/// layout that keeps other repositories behind one ignore rule gets a workspace
/// that reports success and then records nothing anybody does in them. Name them
/// once, here, while somebody is reading the output.
fn warn_about_ignored_repos(repo: &git2::Repository, root: &std::path::Path) {
    let mut found = Vec::new();
    collect_ignored_repos(repo, root, root, 0, &mut found);
    if found.is_empty() {
        return;
    }
    found.sort();
    eprintln!(
        "\nwarning: these directories are ignored by this project's .gitignore and hold their own\n\
         git repositories, so ortak will not journal anything inside them:"
    );
    for dir in found.iter().take(SUBREPO_LISTED) {
        eprintln!("  {}", dir);
    }
    if found.len() > SUBREPO_LISTED {
        eprintln!("  and {} more", found.len() - SUBREPO_LISTED);
    }
    eprintln!("Run `ortak init` inside each one you actually work in.");
}

fn collect_ignored_repos(
    repo: &git2::Repository,
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth >= SUBREPO_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name == ".git" || name == workspace::ORTAK_DIR {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if path.join(".git").exists() {
            if repo.is_path_ignored(&rel).unwrap_or(false) {
                out.push(rel);
            }
            // Whatever a nested repository holds is that repository's business.
            continue;
        }
        collect_ignored_repos(repo, root, &path, depth + 1, out);
    }
}

fn status(as_json: bool) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let cfg = Config::load(&ws.config_path)?;
    let db = Db::open(&ws.db_path)?;
    if as_json {
        return json::print(&json::status(&db, cfg.gate.presence_minutes * 60)?);
    }
    match db.heartbeat_age()? {
        Some(age) if age <= HEARTBEAT_ALIVE_SECS => {
            println!("daemon: running (last heartbeat {}s ago)", age)
        }
        Some(age) => println!("daemon: NOT RUNNING (last heartbeat {}s ago)", age),
        None => println!("daemon: never started"),
    }
    println!();
    print_sessions(&db)?;
    let hot = db.fresh_regions(cfg.gate.presence_minutes * 60)?;
    if !hot.is_empty() {
        println!("\nhot regions protected by the gate:");
        for (file, start, end, agent, sid, last_ts) in hot {
            let mins = ((db::now_ts() - last_ts) / 60).max(0);
            let range = if end >= regions::WHOLE_FILE {
                "whole file".to_string()
            } else {
                format!("{}-{}", start, end)
            };
            println!(
                "  {} {} - {} (ortak-{}), {} min ago",
                file, range, agent, sid, mins
            );
        }
    }
    Ok(())
}

fn sessions() -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    print_sessions(&db)
}

fn print_sessions(db: &Db) -> Result<()> {
    for s in db.list_sessions()? {
        let edits = db.edit_count(s.id)?;
        println!(
            "ortak-{} [{}] {} - {} edits - intent: {}",
            s.id,
            s.status,
            s.agent_name,
            edits,
            s.task_intent.as_deref().unwrap_or("(not reported)")
        );
    }
    Ok(())
}

fn log(session: Option<&str>, limit: u32, as_json: bool) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    let session_id = match session {
        Some(r) => Some(db.resolve_session(r)?.id),
        None => None,
    };
    if as_json {
        return json::print(&json::edits(db.recent_edits(session_id, limit)?));
    }
    for e in db.recent_edits(session_id, limit)? {
        let t = chrono::DateTime::from_timestamp(e.ts, 0)
            .map(|d| d.format("%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();
        let how = if e.inferred() {
            ", inferred from a running command"
        } else {
            ""
        };
        println!(
            "[{}] {:6} {} - {} (ortak-{}{})",
            t, e.change_kind, e.file, e.agent_name, e.session_id, how
        );
    }
    Ok(())
}

fn intent(session_ref: &str, text: &str) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;
    db.set_intent(session.id, text)?;
    println!("recorded intent for ortak-{}: {}", session.id, text);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind as K;

    #[test]
    fn a_hook_never_exits_non_zero() {
        use ParseFailure::*;
        // A hooks.json from a newer plugin than this binary.
        assert_eq!(
            classify_parse_failure(K::InvalidSubcommand, Some("hook")),
            SilentSuccess
        );
        // A future release adding a flag to a hook this binary already knows.
        assert_eq!(
            classify_parse_failure(K::UnknownArgument, Some("hook")),
            SilentSuccess
        );
        // `ortak hook` and `ortak hook --help` still print, and still succeed.
        assert_eq!(
            classify_parse_failure(K::DisplayHelp, Some("hook")),
            PrintAndSucceed
        );
        assert_eq!(
            classify_parse_failure(K::DisplayHelpOnMissingArgumentOrSubcommand, Some("hook")),
            PrintAndSucceed
        );
        // Someone typing at a terminal still gets told what went wrong.
        assert_eq!(
            classify_parse_failure(K::InvalidSubcommand, Some("publsh")),
            Report
        );
        assert_eq!(classify_parse_failure(K::InvalidSubcommand, None), Report);
    }
}
