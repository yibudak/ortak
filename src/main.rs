mod config;
mod daemon;
mod db;
mod doctor;
mod hooks;
mod impact;
mod json;
mod line;
mod notes;
mod opencode;
mod orchestrator;
mod publish;
mod regions;
mod shadow;
mod uninstall;
mod update;
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
    /// Update the current binary and installed agent plugins
    Update,
    /// Remove the binary, agent plugins, bundled skills, and marketplaces
    Uninstall,
    /// Install integrations for OpenCode
    #[command(name = "opencode")]
    OpenCode {
        #[command(subcommand)]
        command: OpenCodeCommand,
    },
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
    /// Check whether this workspace can publish, and say what to fix if not.
    /// Exits non-zero when a check fails
    Doctor {
        /// Emit JSON for another program to read
        #[arg(long)]
        json: bool,
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
        /// Sending session (default: the session $CLAUDE_CODE_SESSION_ID names, else the human)
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
    /// Take a file's lines and journal rows back from whoever they were credited to
    Claim {
        /// Session reference (ortak-3)
        session: String,
        /// File to claim
        file: String,
    },
    /// Publish a session's net changes as a branch
    Publish {
        /// Session reference (ortak-3, human, or agent name)
        session: String,
        /// Branch name (default: <prefix>ortak-<id>-<slug>)
        #[arg(long)]
        branch: Option<String>,
        /// Workspace-relative file or directory to keep out of the branch (repeatable)
        #[arg(long)]
        exclude: Vec<String>,
        /// Publish only the files under this workspace-relative directory
        ///
        /// A workspace can hold more than one git repository, and a branch
        /// belongs to one of them. Everything held back keeps its place in the
        /// journal and goes out in its own publish.
        #[arg(long)]
        repo: Option<String>,
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
        /// Say what the branch would carry and record nothing
        #[arg(long)]
        dry_run: bool,
        /// Ship a file whose history cannot be replayed as one net change
        ///
        /// Only the files the replay was blocked on, and it gives up the merge
        /// that keeps a concurrent session's edits out of the branch.
        #[arg(long)]
        squash: bool,
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
enum OpenCodeCommand {
    /// Install the global Ortak plugin and workflow skill
    Install,
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
        Command::Update => update::run(),
        Command::Uninstall => uninstall::run(),
        Command::OpenCode {
            command: OpenCodeCommand::Install,
        } => opencode::install(),
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
        Command::Doctor { json } => {
            let ws = Workspace::discover_from_cwd()?;
            let cfg = Config::load(&ws.config_path)?;
            // The report is the output; a failed check is not an error to print
            // a second time, it is an exit code for whatever ran this.
            if !doctor::run(&ws, &cfg, json)? {
                std::process::exit(1);
            }
            Ok(())
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
        Command::Claim { session, file } => claim(&session, &file),
        Command::Publish {
            session,
            branch,
            exclude,
            repo,
            base,
            message,
            all,
            amend,
            push,
            dry_run,
            squash,
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
                    repo: repo.as_deref(),
                    scope,
                    push,
                    message: message.as_deref(),
                    dry_run,
                    squash,
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
        // A workspace whose baseline never landed looks exactly like a healthy
        // one from out here and behaves nothing like it: with no shadow HEAD
        // the first touch of any file classifies as a create and claims the
        // file's whole length, so one edited line locks everybody out of a
        // 4000-line model. `ortak init` said `already initialized` and exited
        // 0, and nothing ever mentioned it again. Capturing it now is what
        // should have happened the first time, and there is nothing to undo:
        // an empty shadow history is what makes this reachable.
        if let Ok(repo) = shadow::open(&ws) {
            if repo.head().is_err() {
                let cfg = Config::load(&ws.config_path)?;
                print!("the baseline here was never captured; capturing it now... ");
                let (oid, unread) = shadow::baseline(&repo, &ws, &cfg)?;
                println!("done ({})", &oid.to_string()[..8]);
                name_what_could_not_be_read(&unread);
            }
        }
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
    let (oid, unread) = shadow::baseline(&repo, &ws, &cfg)?;
    println!("done ({})", &oid.to_string()[..8]);
    name_what_could_not_be_read(&unread);
    name_the_repositories(&ws);
    name_the_push_remote(&ws, &cfg);
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

/// Where `ortak publish --push` will push, said once while somebody is reading
/// the output of `init`.
///
/// The remote is per clone and lives in git config, so on a fresh clone it is
/// unset and the fallback is origin: a fork owner's first push goes to the
/// upstream they cannot write to. Contributing to a public repository through a
/// fork is the normal shape of this, not an exotic one, and what git returns is
/// a permission error with nothing about ortak in it.
///
/// It looks rather than asks. A prompt hangs in a script, and nothing on disk
/// says which remote you can write to anyway, so naming the target and the
/// one-line change beats guessing at one.
fn name_the_push_remote(ws: &Workspace, cfg: &Config) {
    let root = &ws.root;
    let nested = ws.repositories().iter().filter(|d| !d.is_empty()).count();
    let Ok(repo) = git2::Repository::open(root) else {
        // Not an error: the journal and the gate work fine without git. But
        // "workspace ready" is the only thing init said, and publishing is the
        // reason the workspace exists, so say the one thing that will not work.
        // A directory inside a checkout is not a directory without one, and
        // saying so sends somebody to `git init` in a repository they already
        // have. A directory holding a tree of them is neither.
        match (nested, publish::repository_above(root)) {
            (0, Some(above)) => println!(
                "\nthis directory is inside the git repository at {}, rather than the root of one, so `ortak publish` has nowhere to build a branch; run `ortak init` there instead",
                above.display()
            ),
            (0, None) => println!(
                "\nthis directory is not a git repository, so `ortak publish` has nowhere to build a branch"
            ),
            (n, _) => println!(
                "\nthis directory is not a git repository itself, so publish one of the {n} under it: `ortak publish <session> --repo <directory>`"
            ),
        }
        return;
    };
    let Ok(names) = repo.remotes() else {
        return;
    };
    let remotes: Vec<String> = names.iter().flatten().map(str::to_string).collect();
    if remotes.is_empty() {
        println!("\nthis clone has no git remote, so `ortak publish --push` has nowhere to push");
        return;
    }
    let chosen = publish::remote_for(&repo, cfg);
    let url = repo
        .find_remote(&chosen)
        .ok()
        .and_then(|r| r.url().map(str::to_string));
    match &url {
        Some(url) => println!("\n`ortak publish --push` pushes to {chosen} ({url})"),
        None => println!(
            "\n`ortak publish --push` pushes to {chosen}, and this clone has no remote by that name"
        ),
    }
    // One remote, for one repository, and in a tree that is one of sixty.
    // `publish` picks the remote of the repository it is building in, so
    // whatever is said here is about the root and nothing else.
    if nested > 0 {
        println!(
            "  that is the root's remote; each of the {nested} repositories under it has its own"
        );
    }
    // Somebody who has already answered this does not need to be asked again,
    // unless what they answered names a remote that is not here.
    let answered = repo
        .config()
        .and_then(|c| c.get_string("ortak.remote"))
        .is_ok()
        || cfg.publish.remote.is_some();
    if answered && url.is_some() {
        return;
    }
    let others: Vec<&str> = remotes
        .iter()
        .map(String::as_str)
        .filter(|r| *r != chosen)
        .collect();
    if !others.is_empty() {
        println!(
            "  other remotes here: {}. Working through a fork? `git config ortak.remote <name>`",
            others.join(", ")
        );
    }
}

/// How many repositories to name before summarizing. Sixty is a wall of text
/// and the point is what the workspace covers, not an inventory of it.
const SUBREPO_LISTED: usize = 10;

/// What the baseline walk could not open. These files are not in it, so the
/// first edit to one of them classifies as a create and claims the whole file,
/// and somebody who has just watched `init` print `done` has no other way to
/// learn that. A tree of sixty repositories holds logs, backups and filestores,
/// and some of them belong to another user.
fn name_what_could_not_be_read(paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    eprintln!(
        "\nwarning: {} path(s) could not be read, so they are not in the baseline:",
        paths.len()
    );
    for p in paths.iter().take(SUBREPO_LISTED) {
        eprintln!("  {}", p);
    }
    if paths.len() > SUBREPO_LISTED {
        eprintln!("  and {} more", paths.len() - SUBREPO_LISTED);
    }
    eprintln!("The first edit to any of those is recorded as a whole-file change.");
}

/// What this workspace covers, said once while somebody is reading the output of
/// `init`.
///
/// This used to be a warning: these directories are ignored by your
/// `.gitignore` and hold their own repositories, so ortak will not journal
/// anything in them, run `ortak init` inside each one you actually work in.
/// Every clause of that is now false, and the advice was sixty workspaces and
/// sixty daemons, which is the arrangement one workspace over a tree replaces.
fn name_the_repositories(ws: &Workspace) {
    let found = ws.repositories();
    // An ordinary checkout is one repository and has nothing to say here.
    if found.len() < 2 {
        return;
    }
    println!(
        "\nthis workspace covers {} git repositories, whatever the root's .gitignore says\n\
         about the directories they sit in:",
        found.len()
    );
    for dir in found.iter().take(SUBREPO_LISTED) {
        println!("  {}", if dir.is_empty() { "(the root)" } else { dir });
    }
    if found.len() > SUBREPO_LISTED {
        println!("  and {} more", found.len() - SUBREPO_LISTED);
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
    let age = db.heartbeat_age()?;
    match age {
        Some(age) if age <= HEARTBEAT_ALIVE_SECS => {
            println!("daemon: running (last heartbeat {}s ago)", age)
        }
        Some(age) => println!("daemon: NOT RUNNING (last heartbeat {}s ago)", age),
        None => println!("daemon: never started"),
    }
    // A daemon holds the binary it was started from for as long as it runs,
    // while the hooks resolve ortak from PATH afresh on every call, so an
    // update mid-session leaves two programs writing one journal. Only worth
    // saying while a daemon is alive: once it stops, the record it left is
    // about a process that is gone too.
    if age.is_some_and(|a| a <= HEARTBEAT_ALIVE_SECS) {
        if let Some((build, false)) = daemon::running_build(&db) {
            println!(
                "daemon build: it started from {} (ortak {}), and that is not the file \
                 there now; the hooks read ortak from PATH on every call, so two builds \
                 are writing this journal",
                build.path, build.version
            );
            println!(
                "  restart it to catch up: `ortak daemon --stop`, then `ortak daemon --detach`"
            );
        }
    }
    // Working out what a stopped daemon cost used to be a manual job: read the
    // heartbeat, work out the window, warn the other session in prose. Every
    // part of that is a lookup, and it only matters while the window is still
    // within reach of what someone was doing.
    if let Some(o) = db.last_outage()?.filter(|o| o.recent(db::now_ts())) {
        let recovered = match o.journaled {
            0 => "the startup scan found nothing to recover".to_string(),
            // Where they landed is not recorded: the scan journals through the
            // same path as everything else and a live hint keeps its owner.
            n => format!("the startup scan recovered {} file(s)", n),
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
    print_waiting_messages(&db)?;
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
/// What a blame on one line says after listing its owners, or nothing when
/// there is one owner and one row.
///
/// Sessions, not rows: one session can hold two regions that overlap at a line,
/// and an `ortak claim` that moves a row onto a session that already owns the
/// range leaves exactly that. Counting the rows told a denied session that two
/// others were in its way when the answer was one, which is the opposite of the
/// thing blame is read for. Two rows of one session still get a line, because
/// two entries under one name need explaining as much as two names do.
fn holders_note(at: &[&db::Owner]) -> Option<String> {
    let sessions: std::collections::BTreeSet<i64> = at.iter().map(|o| o.session_id).collect();
    let order = "the newest write is listed first";
    match (sessions.len(), at.len()) {
        (held, _) if held > 1 => Some(format!("{held} sessions hold this line; {order}")),
        (_, rows) if rows > 1 => Some(format!(
            "ortak-{} holds this line in {rows} regions; {order}",
            at[0].session_id
        )),
        _ => None,
    }
}

fn blame(target: &str) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    let (file, line) = split_target(target)?;
    // A path outside the workspace has no answer here, and saying it is as the
    // base branch left it claims knowledge of a file this repository has never
    // seen. `claim` and `release` have refused it by name since #7.
    let rel = workspace_path(&ws, file)?;
    let owners = db.file_regions(&rel)?;
    let now = db::now_ts();

    if let Some(line) = line {
        // Every owner, not the first one found. The gate lets two sessions hold
        // ranges that overlap at a line, and blame is what a denied session
        // reads to find out who is in its way: naming one of two, with nothing
        // to say a second exists, sent people looking at the wrong session.
        let mut at: Vec<&db::Owner> = owners
            .iter()
            .filter(|o| o.start <= line && line <= o.end)
            .collect();
        // `file_regions` reads in line order, which is right for a whole file
        // and wrong for one line: the session that wrote it last is the one the
        // reader is asking about.
        at.sort_by_key(|o| std::cmp::Reverse(o.last_ts));
        if at.is_empty() {
            println!(
                "no session has touched line {} of {}; it is as the base branch left it",
                line, rel
            );
            return Ok(());
        }
        println!("{}:{}", rel, line);
        for o in &at {
            println!(
                "  ortak-{} {}, {}{}",
                o.session_id,
                o.agent_name,
                ago(now - o.last_ts),
                owner_note(o)
            );
            println!(
                "    owns {}, intent: {}",
                range(o),
                o.intent.as_deref().unwrap_or("(not reported)")
            );
        }
        if let Some(note) = holders_note(&at) {
            println!("  {note}");
        }
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
fn split_target(target: &str) -> Result<(&str, Option<i64>)> {
    match target.rsplit_once(':') {
        Some((file, line)) => match line.parse::<i64>() {
            Ok(n) if n > 0 => Ok((file, Some(n))),
            // A suffix that is a number and not a line is a typo, not a file
            // whose name ends in one. Falling through to the path made blame
            // answer "no session has touched src/db.rs:0" about a file that
            // cannot exist, in the command somebody runs right after the gate
            // has refused them.
            Ok(n) => anyhow::bail!("{} is not a line number; lines are counted from 1", n),
            Err(_) => Ok((target, None)),
        },
        None => Ok((target, None)),
    }
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

/// Who is holding unread mail. Silent when there is none, which is the normal
/// case: a message reaches a working session before its next tool call.
///
/// The queue nobody drains is the failure this reports. A message for a session
/// that stopped cleanly goes to the next session that starts here, but one
/// whose recipient was killed mid-turn is addressed to a session that still
/// looks active and never will be, and nothing but this line would say so.
fn print_waiting_messages(db: &Db) -> Result<()> {
    let waiting = db.waiting_messages()?;
    if waiting.is_empty() {
        return Ok(());
    }
    let now = db::now_ts();
    println!("\nmessages waiting:");
    for w in &waiting {
        println!(
            "  ortak-{} {}{}: {}, oldest {}",
            w.session_id,
            w.agent_name,
            if w.stopped { " [stopped]" } else { "" },
            w.count,
            ago(now - w.oldest)
        );
    }
    if waiting.iter().any(|w| w.stopped) {
        println!("  a stopped session's mail goes to the next session that starts here; `ortak inbox ortak-N` reads it now");
    }
    Ok(())
}

fn print_sessions(db: &Db) -> Result<()> {
    let now = db::now_ts();
    for s in db.list_sessions()? {
        let edits = db.edit_count(s.id)?;
        // `[active]` is what the session last said about itself, and one killed
        // mid-turn never said otherwise. The age beside it is the only thing
        // here a person can weigh against how long the work should have taken.
        let seen = s
            .last_seen
            .map(|ts| format!(" - last seen {}", ago(now - ts)))
            .unwrap_or_default();
        println!(
            "ortak-{} [{}] {} - {} edits{} - intent: {}",
            s.id,
            s.status,
            s.agent_name,
            edits,
            seen,
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

/// The harness session id this process was started under, if any. Claude Code
/// exports it; a person at a terminal has no such thing.
fn harness_session_id() -> Option<String> {
    std::env::var("CLAUDE_CODE_SESSION_ID").ok()
}

/// The session a command is running as.
///
/// `tell` used to sign every message with the human session, because the only
/// way to name a sender was a flag and in twelve rounds nobody passed it. The
/// harness exports the same string `sessions.external_id` stores, so the answer
/// was always there to look up. Every step from the environment to a session id
/// is allowed to come up empty, and empty means the human: a person at a
/// terminal has no harness id and still has to be able to send a message.
///
/// A harness id this workspace has never registered falls through to the human
/// rather than registering itself. A session registers at `SessionStart`, in
/// the workspace it starts in, and a messaging command that created sessions as
/// a side effect would put rows in `ortak status` for agents that never worked
/// here. That costs one mislabelled message per unregistered session, so it
/// says out loud that it fell through.
fn calling_session(db: &Db, from: Option<&str>, external: Option<&str>) -> Result<i64> {
    // A named sender wins: it is how the watcher speaks for another session and
    // how a test drives this, and a reference it cannot resolve is a typo worth
    // failing on rather than quietly signing with somebody else's name.
    if let Some(reference) = from {
        return Ok(db.resolve_session(reference)?.id);
    }
    let Some(external) = external else {
        return db.ensure_human();
    };
    match db.resolve_session(external) {
        Ok(s) => Ok(s.id),
        Err(_) => {
            eprintln!(
                "no session in this workspace for harness id {external}; \
                 recording this as the human session."
            );
            db.ensure_human()
        }
    }
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
        None => harness_session_id().ok_or_else(|| {
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
    ws.relativize_arg(arg)
        .ok_or_else(|| anyhow::anyhow!("{} is outside the workspace {}", arg, ws.root.display()))
}

/// The other half of `release`. The journal could be told a write was not
/// yours; nothing could tell it that one was, so repairing a misattribution
/// needed the wrong owner to still be around to disown it first.
///
/// A whole file, and no `--all`. One journal row is one write with one shadow
/// commit behind it, so there is no half of it to hand over line by line; and
/// where `release --all` gives everything back, a `claim --all` would take the
/// workspace off everybody at once. Repair is one file at a time.
fn claim(session_ref: &str, file: &str) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;
    let rel = workspace_path(&ws, file)?;
    let (regions, edits, from) = db.claim_file(session.id, &rel)?;
    println!(
        "claimed {} region(s) and {} journal row(s) on {} for ortak-{}",
        regions, edits, rel, session.id
    );
    // Journal rows and held lines are separate things and a claim can take one
    // without the other: `release` deletes the lines it frees, and a later
    // claim finds rows with nothing to hold. Blame reads the lines, so it goes
    // on saying nobody has touched the file, which reads as the claim having
    // failed.
    if regions == 0 {
        println!(
            "no lines were being held on {}, so `ortak log` marks the rows claimed and \
             `ortak blame` still has nothing to show for them",
            rel
        );
    } else {
        println!(
            "`ortak blame {}` and `ortak log` mark them claimed, not written",
            rel
        );
    }
    // Work taken quietly is what this command could become, so the sessions it
    // came from hear it from the journal rather than from a publish that has
    // lost a file.
    for other in &from {
        db.send_message(
            session.id,
            *other,
            &format!(
                "ortak-{} claimed {}: what the journal credited to you there is theirs now. \
                 If that is wrong, `ortak claim ortak-{} {}` takes it back.",
                session.id, rel, other, rel
            ),
        )?;
    }
    if !from.is_empty() {
        let names: Vec<String> = from.iter().map(|id| format!("ortak-{}", id)).collect();
        println!("taken from {}, and they have been told", names.join(", "));
    }
    Ok(())
}

fn intent(session_ref: &str, text: &str) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    let session = db.resolve_session(session_ref)?;
    // `ortak intent ortak-3` on its own used to record an empty one, quietly
    // replacing whatever the session had told everybody it was doing. The
    // intent is what the gate shows a session it is blocking.
    if text.trim().is_empty() {
        anyhow::bail!(
            "say what the task is: ortak intent ortak-{} \"<one sentence>\"",
            session.id
        );
    }
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

    fn owner(session_id: i64) -> db::Owner {
        db::Owner {
            session_id,
            agent_name: "claude-x".into(),
            intent: None,
            start: 1,
            end: 9,
            last_ts: 0,
            attributed_by: None,
        }
    }

    #[test]
    fn blame_counts_the_sessions_holding_a_line_and_not_their_rows() {
        let (mine, theirs, mine_again) = (owner(3), owner(4), owner(3));
        assert_eq!(holders_note(&[&mine]), None, "one owner needs no summary");

        // Two rows, one session, which is what an `ortak claim` leaves behind:
        // it used to read as two sessions and send the reader after a stranger.
        assert_eq!(
            holders_note(&[&mine, &mine_again]).as_deref(),
            Some("ortak-3 holds this line in 2 regions; the newest write is listed first")
        );
        assert_eq!(
            holders_note(&[&mine, &theirs]).as_deref(),
            Some("2 sessions hold this line; the newest write is listed first")
        );
        assert_eq!(
            holders_note(&[&mine, &theirs, &mine_again]).as_deref(),
            Some("2 sessions hold this line; the newest write is listed first"),
            "three rows, two sessions"
        );
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
    let sender = calling_session(&db, from, harness_session_id().as_deref())?;
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
    // Saying where it went matters most when the answer is not "to them": a
    // message to a session that has finished used to report success and then
    // sit in the table for good.
    if recipient.status == "done" {
        println!(
            "ortak-{} {} has stopped. This goes to the next session that starts in this \
             workspace, and `ortak inbox ortak-{}` reads it before then.",
            recipient.id, recipient.agent_name, recipient.id
        );
        return Ok(());
    }
    // Every door a message can arrive through is a hook, and the human session
    // has none: it is the row unclaimed writes fall to, not a party that runs
    // anything. Nothing is lost, but the wait is open-ended, so say that rather
    // than promise a next prompt that will not come.
    if recipient.kind == "human" {
        println!(
            "ortak-{} is the person at this terminal, and no hook delivers to them. It waits \
             under `messages waiting` in `ortak status` until somebody reads `ortak inbox ortak-{}`.",
            recipient.id, recipient.id
        );
        return Ok(());
    }
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
mod sender_tests {
    use super::*;

    /// The environment is the input here, so the test names it directly rather
    /// than setting a variable: `std::env::set_var` is process-wide and the
    /// suite runs its tests in threads.
    #[test]
    fn a_message_is_signed_by_the_session_the_environment_names() {
        let path = std::env::temp_dir().join(format!("ortak-sender-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        let agent = db
            .upsert_session("sess-a", "claude-aaaa", "llm", Some("claude-code"))
            .unwrap();
        let watcher = db
            .upsert_session("sess-w", "claude-wwww", "llm", Some("claude-code"))
            .unwrap();
        let human = db.ensure_human().unwrap();

        assert_eq!(
            calling_session(&db, None, Some("sess-a")).unwrap(),
            agent,
            "the harness id names the sender"
        );
        assert_eq!(
            calling_session(&db, Some("ortak-2"), Some("sess-a")).unwrap(),
            watcher,
            "--from wins, which is how one session speaks for another"
        );
        assert_eq!(
            calling_session(&db, None, None).unwrap(),
            human,
            "a person at a terminal has no harness id"
        );
        // The first message a session sends can arrive before any hook has
        // registered it here, and this used to be the only way `tell` behaved.
        assert_eq!(
            calling_session(&db, None, Some("sess-elsewhere")).unwrap(),
            human,
            "an id this workspace never registered does not fail"
        );
        assert!(
            calling_session(&db, Some("ortak-99"), None).is_err(),
            "a named sender that resolves to nothing is a typo, not the human"
        );
        let _ = std::fs::remove_file(&path);
    }
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
        assert_eq!(
            split_target("src/db.rs:143").unwrap(),
            ("src/db.rs", Some(143))
        );
        assert_eq!(split_target("src/db.rs").unwrap(), ("src/db.rs", None));
        // A filename may hold a colon, and those still resolve as paths.
        assert_eq!(split_target("odd:name.rs").unwrap(), ("odd:name.rs", None));
        // A number that is not a line is a typo and says so, rather than
        // becoming a filename nobody has ever had.
        for typo in ["src/db.rs:0", "src/db.rs:-5"] {
            assert!(split_target(typo).is_err(), "{typo} answered as a path");
        }
    }
}
