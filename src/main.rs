mod config;
mod daemon;
mod db;
mod hooks;
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
    Status,
    /// Show recent journal entries
    Log {
        /// Session reference (ortak-3, human, or agent name)
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
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
    /// Send a message to another session, or to all of them
    Tell {
        /// Recipient: ortak-3, an agent name, or "all"
        to: String,
        /// Message text
        text: Vec<String>,
        /// Sending session (default: the human session)
        #[arg(long)]
        from: Option<String>,
    },
    /// Show the messages a session has been sent
    Inbox {
        /// Session reference (ortak-3)
        session: String,
    },
    /// Publish a session's net changes as a branch
    Publish {
        /// Session reference (ortak-3, human, or agent name)
        session: String,
        /// Branch name (default: <prefix>ortak-<id>-<slug>)
        #[arg(long)]
        branch: Option<String>,
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
        /// Session (ortak-N); omit to resolve all open errors
        session: Option<String>,
    },
    /// List error records
    Errors,
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
    /// Claude Code PostToolUse (Bash): error-reporting reminder
    PostBash,
    /// Claude Code UserPromptSubmit: hat durumu enjeksiyonu
    PromptContext,
    /// Claude Code SessionEnd
    SessionEnd,
}

fn main() {
    let cli = Cli::parse();
    // Hook adapters must never break the agent's session: swallow errors, exit 0.
    if let Command::Hook { event } = &cli.command {
        let res = match event {
            HookEvent::SessionStart => hooks::session_start(),
            HookEvent::PreEdit => hooks::pre_edit(),
            HookEvent::PostEdit => hooks::post_edit(),
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
        Command::Status => status(),
        Command::Log { session, limit } => log(session.as_deref(), limit),
        Command::Sessions => sessions(),
        Command::Intent { session, text } => intent(&session, &text.join(" ")),
        Command::Tell { to, text, from } => tell(&to, &text.join(" "), from.as_deref()),
        Command::Inbox { session } => inbox(&session),
        Command::Publish {
            session,
            branch,
            push,
        } => {
            let ws = Workspace::discover_from_cwd()?;
            let cfg = Config::load(&ws.config_path)?;
            publish::run(&ws, &cfg, &session, branch.as_deref(), push)
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
        Command::Resolved { session } => {
            let ws = Workspace::discover_from_cwd()?;
            line::resolved(&ws, session.as_deref())
        }
        Command::Errors => {
            let ws = Workspace::discover_from_cwd()?;
            line::list(&ws)
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
    if ws.ortak_dir.exists() {
        println!("already initialized: {}", ws.ortak_dir.display());
        return Ok(());
    }
    std::fs::create_dir_all(&ws.ortak_dir)?;
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
    println!("\nworkspace ready: {}", ws.root.display());
    println!("next: run `ortak daemon` in another terminal or in the background");
    Ok(())
}

fn status() -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let cfg = Config::load(&ws.config_path)?;
    let db = Db::open(&ws.db_path)?;
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

fn log(session: Option<&str>, limit: u32) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    let session_id = match session {
        Some(r) => Some(db.resolve_session(r)?.id),
        None => None,
    };
    for e in db.recent_edits(session_id, limit)? {
        let t = chrono::DateTime::from_timestamp(e.ts, 0)
            .map(|d| d.format("%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();
        println!(
            "[{}] {:6} {} - {} (ortak-{})",
            t, e.change_kind, e.file, e.agent_name, e.session_id
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

fn tell(to: &str, text: &str, from: Option<&str>) -> Result<()> {
    if text.is_empty() {
        anyhow::bail!("nothing to send; give the message as the remaining arguments");
    }
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    // No sender named means a person at a terminal typed this.
    let sender = match from {
        Some(r) => db.resolve_session(r)?.id,
        None => db.ensure_human()?,
    };
    if to == "all" {
        match db.broadcast_message(sender, text)? {
            0 => println!("no other active sessions; nothing was sent."),
            1 => println!("sent to 1 other active session."),
            n => println!("sent to {} other active sessions.", n),
        }
        return Ok(());
    }
    let recipient = db.resolve_session(to)?;
    db.send_message(sender, recipient.id, text)?;
    println!(
        "sent to ortak-{} {}; it arrives at the start of that session's next turn.",
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
