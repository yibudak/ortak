mod config;
mod daemon;
mod db;
mod hooks;
mod impact;
mod json;
mod line;
mod notes;
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
    /// Run the journal daemon (file watcher and shadow repository)
    Daemon {
        /// Run in the background, with output going to .ortak/daemon.log
        #[arg(long)]
        detach: bool,
        /// Stop the daemon running on this workspace
        #[arg(long, conflicts_with = "detach")]
        stop: bool,
    },
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
    /// Show which session owns the lines of a file
    Blame {
        /// File, or file and line: src/db.rs or src/db.rs:143
        target: String,
    },
    /// Record why a change was made, or read the notes on a file
    Why {
        /// Writing: ortak-N <file> <why...>. Reading: <file> or <file>:<line>
        args: Vec<String>,
    },
    /// List sessions
    Sessions,
    /// Which ortak-N this harness session is in this workspace
    Whoami {
        /// Harness session id (default: $CLAUDE_CODE_SESSION_ID)
        session_id: Option<String>,
    },
    /// Record a session's task intent
    Intent {
        /// Session reference (ortak-3)
        session: String,
        /// Task description
        text: Vec<String>,
    },
    /// Send a message to another session, or to all of them
    Tell {
        /// Recipient: ortak-3, an agent name, or "all"
        to: String,
        /// Message text
        text: Vec<String>,
        /// Sending session (default: the human session)
        #[arg(long)]
        from: Option<String>,
        /// Read the message from standard input instead of the arguments
        #[arg(long)]
        stdin: bool,
    },
    /// Show the messages a session has been sent
    Inbox {
        /// Session reference (ortak-3)
        session: String,
    },
    /// Give a file back: free its lines and drop it from this session's work
    Release {
        /// Session reference (ortak-3)
        session: String,
        /// File to release
        file: Option<String>,
        /// Release every file this session holds
        #[arg(long)]
        all: bool,
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
        /// Commit subject for this branch (default: the session's intent)
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Publish everything the session has touched, not just what is new
        #[arg(long)]
        all: bool,
        /// Rebuild the branch --branch names, instead of starting a new one
        #[arg(long, conflicts_with = "all")]
        amend: bool,
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
    /// Show what else in the workspace a session's changes may have broken
    Impact {
        /// Session reference (ortak-3)
        session: String,
    },
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
    /// Claude Code UserPromptSubmit: line status and waiting messages
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
        Command::Daemon { detach, stop } => {
            let ws = Workspace::discover_from_cwd()?;
            if stop {
                return daemon::stop(&ws);
            }
            if detach {
                return daemon::detach(&ws);
            }
            let cfg = Config::load(&ws.config_path)?;
            daemon::run(&ws, &cfg)
        }
        Command::Status { json } => status(json),
        Command::Log {
            session,
            limit,
            json,
        } => log(session.as_deref(), limit, json),
        Command::Blame { target } => blame(&target),
        Command::Why { args } => notes::run(&args),
        Command::Sessions => sessions(),
        Command::Whoami { session_id } => whoami(session_id.as_deref()),
        Command::Intent { session, text } => intent(&session, &text.join(" ")),
        Command::Tell {
            to,
            text,
            from,
            stdin,
        } => tell(&to, &text, from.as_deref(), stdin),
        Command::Inbox { session } => inbox(&session),
        Command::Release { session, file, all } => release(&session, file.as_deref(), all),
        Command::Publish {
            session,
            branch,
            exclude,
            base,
            message,
            all,
            amend,
            push,
        } => {
            let ws = Workspace::discover_from_cwd()?;
            let cfg = Config::load(&ws.config_path)?;
            // clap keeps --all and --amend apart, so the three are exclusive by
            // the time they get here.
            let scope = match (all, amend) {
                (_, true) => publish::Scope::Amend,
                (true, _) => publish::Scope::All,
                _ => publish::Scope::New,
            };
            publish::run(
                &ws,
                &cfg,
                &session,
                publish::PublishOpts {
                    branch: branch.as_deref(),
                    base: base.as_deref(),
                    exclude: &exclude,
                    scope,
                    push,
                    message: message.as_deref(),
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
        Command::Impact { session } => {
            let ws = Workspace::discover_from_cwd()?;
            let cfg = Config::load(&ws.config_path)?;
            impact::run(&ws, &cfg, &session)
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
        let base = trunk_branch(&root);
        std::fs::write(&ws.config_path, Config::default_toml(&base))?;
        println!("wrote: {}", ws.config_path.display());
        println!("publishing onto {base}; change [publish] base_branch if that is not your trunk");
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

/// The branch a new workspace publishes onto. It used to be `main` whatever the
/// repository called its trunk, so on a `master` repo the first publish its
/// owner ever ran failed on a branch name nobody had chosen.
///
/// A trunk that exists beats the branch that happens to be checked out, because
/// `ortak init` is usually run from the task branch somebody is already on.
/// Where neither name exists, HEAD is the only thing that knows, and git sets it
/// before the first commit.
fn trunk_branch(root: &std::path::Path) -> String {
    let Ok(repo) = git2::Repository::open(root) else {
        return "main".to_string();
    };
    for name in ["main", "master"] {
        if repo.find_branch(name, git2::BranchType::Local).is_ok() {
            return name.to_string();
        }
    }
    repo.find_reference("HEAD")
        .ok()
        .and_then(|head| head.symbolic_target().map(str::to_string))
        .and_then(|target| target.strip_prefix("refs/heads/").map(str::to_string))
        .unwrap_or_else(|| "main".to_string())
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
    let behind_base = commits_behind_base(&ws, &cfg);
    if as_json {
        return json::print(&json::status(
            &db,
            cfg.gate.presence_minutes * 60,
            behind_base,
        )?);
    }
    match db.heartbeat_age()? {
        Some(age) if age <= HEARTBEAT_ALIVE_SECS => {
            println!("daemon: running (last heartbeat {}s ago)", age)
        }
        Some(age) => println!("daemon: NOT RUNNING (last heartbeat {}s ago)", age),
        None => println!("daemon: never started"),
    }
    // Working out what a stopped daemon cost used to be a manual job: read the
    // heartbeat, work out the window, warn the other session in prose. Every
    // part of that is a lookup, and it only matters while the window is still
    // within reach of what someone was doing.
    if let Some(o) = db.last_outage()?.filter(|o| o.recent(db::now_ts())) {
        let recovered = match o.journaled {
            0 => "the startup scan found nothing to recover".to_string(),
            n => format!(
                "the startup scan recovered {} file(s) into the human session",
                n
            ),
        };
        println!(
            "last outage: {} to {} ({}s); {}",
            db::fmt_local(o.start, "%H:%M:%S"),
            db::fmt_local(o.end, "%H:%M:%S"),
            o.secs(),
            recovered
        );
        println!("  changes in that window the scan did not pick up are not in the journal; touch those files again to record them");
    }
    // Silent while the checkout is current, which is the normal case. Behind is
    // Silent while the checkout is current, which is the normal case. Behind is
    // worth a line of its own: publish replays a session's edits onto the base
    // branch, so in a stale checkout everyone is editing files that no longer
    // look like what their work will land on, and nothing else says so.
    if let Some((base, behind)) = behind_base.as_ref().filter(|(_, n)| *n > 0) {
        println!(
            "workspace: {} commit{} behind {}; publishes will replay onto content this \
             checkout does not have",
            behind,
            if *behind == 1 { "" } else { "s" },
            base
        );
    }
    // Silent while the journal is healthy, which is nearly always, so the
    // section is a signal rather than another line to scroll past.
    let failing = db.journal_failures()?;
    if let Some(newest) = failing.first() {
        let age = (db::now_ts() - newest.ts).max(0);
        let when = if age < 120 {
            format!("{}s ago", age)
        } else {
            format!("{} min ago", age / 60)
        };
        println!(
            "journal: NOT RECORDING {} file(s); changes to them are attributed to nobody",
            failing.len()
        );
        println!(
            "  {} ({} in a row, newest {}): {}",
            newest.file, newest.streak, when, newest.reason
        );
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

/// How far this checkout has fallen behind the branch publishes build on:
/// `git rev-list --count HEAD..base`, which is the count `publish` takes when a
/// replay fails, so the standing report and the failure agree. `None` when the
/// question has no answer here: no git repository, no such base branch in this
/// checkout, or a repository with no commits yet.
fn commits_behind_base(ws: &Workspace, cfg: &Config) -> Option<(String, i64)> {
    let repo = git2::Repository::open(&ws.root).ok()?;
    let base = repo
        .find_branch(&cfg.publish.base_branch, git2::BranchType::Local)
        .and_then(|b| b.get().peel_to_commit())
        .ok()?;
    let head = repo.head().ok()?.target()?;
    let (_, behind) = repo.graph_ahead_behind(head, base.id()).ok()?;
    Some((cfg.publish.base_branch.clone(), behind as i64))
}

/// Read line ownership back out of the journal. git history records the human
/// who committed; `regions` records the session that wrote the lines, and keeps
/// shifting them as later edits move the code around, so this is the one place
/// that can answer for an agent.
fn blame(target: &str) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    let (file, line) = split_target(target);
    let rel = relativize_arg(&ws, file);
    let owners = db.file_regions(&rel)?;
    let now = db::now_ts();

    if let Some(line) = line {
        let Some(o) = owners.iter().find(|o| o.start <= line && line <= o.end) else {
            println!(
                "no session has touched line {} of {}; it is as the base branch left it",
                line, rel
            );
            return Ok(());
        };
        println!(
            "{}:{} - ortak-{} {}, {}{}",
            rel,
            line,
            o.session_id,
            o.agent_name,
            ago(now - o.last_ts),
            owner_note(o)
        );
        println!(
            "  owns {}, intent: {}",
            range(o),
            o.intent.as_deref().unwrap_or("(not reported)")
        );
        return Ok(());
    }

    if owners.is_empty() {
        println!(
            "no session has touched {}; it is as the base branch left it",
            rel
        );
        return Ok(());
    }
    println!("{}", rel);
    for o in &owners {
        println!(
            "  {:>12}  ortak-{} {}, {}{}",
            range(o),
            o.session_id,
            o.agent_name,
            ago(now - o.last_ts),
            owner_note(o)
        );
        println!(
            "                intent: {}",
            o.intent.as_deref().unwrap_or("(not reported)")
        );
    }
    Ok(())
}

/// A session, a time and an intent read as a complete account of a line whether
/// or not anybody reported the edit behind it. `ortak log` marks the same rows
/// in the same words.
fn owner_note(o: &db::Owner) -> String {
    match db::attribution_note(o.attributed_by.as_deref()) {
        "" => String::new(),
        note => format!("   ({})", note),
    }
}

fn range(o: &db::Owner) -> String {
    if o.end >= regions::WHOLE_FILE {
        "whole file".to_string()
    } else if o.start == o.end {
        format!("{}", o.start)
    } else {
        format!("{}-{}", o.start, o.end)
    }
}

/// Split `src/db.rs:143` into its file and line. Anything after the last colon
/// that is not a line number belongs to the filename.
fn split_target(target: &str) -> (&str, Option<i64>) {
    match target.rsplit_once(':') {
        Some((file, line)) => match line.parse::<i64>() {
            Ok(n) if n > 0 => (file, Some(n)),
            _ => (target, None),
        },
        None => (target, None),
    }
}

/// The journal keys files on their workspace-relative path, so an argument
/// typed from a subdirectory or as an absolute path has to be brought back to
/// that. A path from outside the workspace is passed through and simply matches
/// nothing.
fn relativize_arg(ws: &Workspace, file: &str) -> String {
    let path = std::path::Path::new(file);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => return file.to_string(),
        }
    };
    ws.relativize(&abs).unwrap_or_else(|| file.to_string())
}

/// Rough age for someone reading a list: whichever unit keeps it short.
fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 90 => format!("{}s ago", s),
        s if s < 5400 => format!("{} min ago", s / 60),
        s if s < 172_800 => format!("{} h ago", s / 3600),
        s => format!("{} d ago", s / 86400),
    }
}

fn sessions() -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    print_sessions(&db)
}

fn print_sessions(db: &Db) -> Result<()> {
    let now = db::now_ts();
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
        // Nothing at all for a session that has published nothing, which is
        // every session in a fresh workspace: a row of empty labels reads as
        // information and is not any.
        for (i, (p, behind)) in db.published_branches(s.id)?.iter().enumerate() {
            let waiting = match (i, behind) {
                (0, 0) => ", up to date".to_string(),
                (0, n) => format!(", {n} edit{} since", if *n == 1 { "" } else { "s" }),
                _ => String::new(),
            };
            println!(
                "        {} {} (published {}{})",
                if i == 0 { "branches:" } else { "         " },
                p.branch,
                ago(now - p.ts),
                waiting
            );
        }
    }
    Ok(())
}

/// Answer "which session am I here?" from the one handle that survives.
///
/// `ortak-N` is a row id handed out in registration order, so it is per
/// workspace and it moves: wipe `.ortak`, or start the two sessions in the
/// other order, and the numbers swap. A session resumed from a compacted
/// context, or one reading a findings file from last week, has no way to check
/// which is which. The harness session id does not move, so ask with that.
fn whoami(session_id: Option<&str>) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    // Claude Code exports its session id, which is the same string the hooks
    // are handed on stdin and the same one `sessions.external_id` stores.
    // Nothing else is assumed to, so every other harness passes it in.
    let external_id = match session_id {
        Some(id) => id.to_string(),
        None => std::env::var("CLAUDE_CODE_SESSION_ID").map_err(|_| {
            anyhow::anyhow!(
                "no harness session id in the environment; pass it: ortak whoami <session-id>. \
                 Claude Code exports it as CLAUDE_CODE_SESSION_ID"
            )
        })?,
    };
    let s = db.resolve_session(&external_id).map_err(|_| {
        anyhow::anyhow!(
            "no session in {} for harness id {}. This session has not registered here: it \
             registers at SessionStart, and only in the workspace it starts in",
            ws.root.display(),
            external_id
        )
    })?;
    println!("ortak-{} {} [{}]", s.id, s.agent_name, s.status);
    println!("  harness id: {}", s.external_id);
    println!(
        "  intent: {}",
        s.task_intent.as_deref().unwrap_or("(not reported)")
    );
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
        let t = db::fmt_local(e.ts, "%m-%d %H:%M:%S");
        let how = match db::attribution_note(e.attributed_by.as_deref()) {
            "" => String::new(),
            note => format!(", {}", note),
        };
        println!(
            "[{}] {:6} {} - {} (ortak-{}{})",
            t, e.change_kind, e.file, e.agent_name, e.session_id, how
        );
    }
    Ok(())
}

fn release(session_ref: &str, file: Option<&str>, all: bool) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;
    let rel = match (file, all) {
        (Some(f), false) => Some(workspace_path(&ws, f)?),
        (None, true) => None,
        _ => anyhow::bail!("name a file to release, or pass --all to release every file"),
    };
    let (regions, edits) = db.disown(session.id, rel.as_deref())?;
    let scope = match &rel {
        Some(f) => format!(" on {}", f),
        None => String::new(),
    };
    println!(
        "released {} region(s) and {} journal row(s) held by ortak-{}{}",
        regions, edits, session.id, scope
    );
    if edits > 0 {
        println!(
            "those files are no longer part of what ortak-{} publishes",
            session.id
        );
    }
    Ok(())
}

/// A path argument as the journal stores it: relative to the workspace root,
/// whatever directory the command was run from.
fn workspace_path(ws: &Workspace, arg: &str) -> Result<String> {
    let path = std::path::Path::new(arg);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    // `sub/../f.txt` is not the path the journal stores, and without this the
    // release matches nothing and still reports that it worked.
    let abs = abs.canonicalize().unwrap_or(abs);
    ws.relativize(&abs)
        .ok_or_else(|| anyhow::anyhow!("{} is outside the workspace {}", arg, ws.root.display()))
}

fn intent(session_ref: &str, text: &str) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;
    db.set_intent(session.id, text)?;
    // Naming the agent as well as the number is the cheap half of catching an
    // intent recorded against the wrong session: `ortak intent ortak-3` from a
    // session that is no longer ortak-3 silently overwrites the other one's.
    println!(
        "recorded intent for ortak-{} {}: {}",
        session.id, session.agent_name, text
    );
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

fn tell(to: &str, text: &[String], from: Option<&str>, stdin: bool) -> Result<()> {
    // The messages worth sending are the ones with a signature, a path or a
    // command in them, and the shell rewrites exactly those before ortak sees
    // a word of it: backticks get substituted, quotes get eaten, newlines
    // split the argument list. --stdin takes the body as it was written.
    let text = if stdin {
        if !text.is_empty() {
            anyhow::bail!(
                "--stdin reads the message from standard input; do not also pass it as arguments"
            );
        }
        std::io::read_to_string(std::io::stdin())?
            .trim()
            .to_string()
    } else {
        text.join(" ")
    };
    if text.is_empty() {
        anyhow::bail!(
            "nothing to send; give the message as the remaining arguments, or pass --stdin"
        );
    }
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    // No sender named means a person at a terminal typed this.
    let sender = match from {
        Some(r) => db.resolve_session(r)?.id,
        None => db.ensure_human()?,
    };
    if to == "all" {
        match db.broadcast_message(sender, &text)? {
            0 => println!("no other active sessions; nothing was sent."),
            1 => println!("sent to 1 other active session."),
            n => println!("sent to {} other active sessions.", n),
        }
        return Ok(());
    }
    let recipient = db.resolve_session(to)?;
    db.send_message(sender, recipient.id, &text)?;
    println!(
        "sent to ortak-{} {}; it arrives before that session's next edit or command, or at its \
         next prompt, whichever comes first.",
        recipient.id, recipient.agent_name
    );
    Ok(())
}

fn inbox(session_ref: &str) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;
    let messages = db.inbox(session.id)?;
    if messages.is_empty() {
        println!("ortak-{} has no messages.", session.id);
        return Ok(());
    }
    for m in messages {
        // The daemon logs on the local clock, so an inbox on UTC reads as
        // hours old next to it and the message looks stale enough to skip.
        let t = chrono::DateTime::from_timestamp(m.ts, 0)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        println!(
            "[{}] ortak-{} {}: {}",
            t, m.from_session, m.from_name, m.text
        );
    }
    Ok(())
}

#[cfg(test)]
mod blame_tests {
    use super::*;
    use regions::Hunk;

    fn hunk(start: i64, lines: i64) -> Hunk {
        Hunk {
            old_start: start,
            old_lines: lines,
            new_start: start,
            new_lines: lines,
        }
    }

    #[test]
    fn a_line_belongs_to_the_session_whose_region_covers_it() {
        let path = std::env::temp_dir().join(format!("ortak-blame-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        let a = db
            .upsert_session("sess-a", "claude-aaaa", "llm", Some("claude-code"))
            .unwrap();
        let b = db
            .upsert_session("sess-b", "claude-bbbb", "llm", Some("claude-code"))
            .unwrap();
        db.set_intent(a, "rewrite the header").unwrap();
        db.insert_edit(a, "src/db.rs", "modify", None, &[], None)
            .unwrap();
        db.apply_edit_regions(a, "src/db.rs", &[hunk(1, 3)], None)
            .unwrap();
        db.insert_edit(b, "src/db.rs", "modify", None, &[], None)
            .unwrap();
        db.apply_edit_regions(b, "src/db.rs", &[hunk(40, 2)], None)
            .unwrap();

        let owners = db.file_regions("src/db.rs").unwrap();
        let owner_of = |line: i64| owners.iter().find(|o| o.start <= line && line <= o.end);
        assert_eq!(owners.len(), 2);
        assert_eq!(owner_of(2).map(|o| o.session_id), Some(a));
        assert_eq!(
            owner_of(2).and_then(|o| o.intent.clone()).as_deref(),
            Some("rewrite the header")
        );
        assert_eq!(owner_of(41).map(|o| o.session_id), Some(b));
        assert!(owner_of(20).is_none(), "the gap belongs to nobody");
        let _ = std::fs::remove_file(&path);
    }

    /// What `whoami` rests on: `ortak-N` is registration order, so two sessions
    /// starting in the other order swap numbers, and only the harness id still
    /// points at the same session afterwards.
    #[test]
    fn the_numbers_move_but_the_harness_id_does_not() {
        let path = std::env::temp_dir().join(format!("ortak-whoami-{}.sqlite", std::process::id()));
        let register = |order: [&str; 2]| {
            let _ = std::fs::remove_file(&path);
            let db = Db::open(&path).unwrap();
            for id in order {
                db.upsert_session(id, &format!("claude-{id}"), "llm", Some("claude-code"))
                    .unwrap();
            }
            (
                db.resolve_session("sess-a").unwrap().id,
                db.resolve_session("sess-b").unwrap().id,
            )
        };

        assert_eq!(register(["sess-a", "sess-b"]), (1, 2));
        assert_eq!(
            register(["sess-b", "sess-a"]),
            (2, 1),
            "the number follows registration order, which is why nobody should trust it"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_trailing_line_number_is_not_part_of_the_filename() {
        assert_eq!(split_target("src/db.rs:143"), ("src/db.rs", Some(143)));
        assert_eq!(split_target("src/db.rs"), ("src/db.rs", None));
        assert_eq!(split_target("odd:name.rs"), ("odd:name.rs", None));
    }
}
