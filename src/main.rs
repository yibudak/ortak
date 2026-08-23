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
    /// Show which session owns the lines of a file
    Blame {
        /// File, or file and line: src/db.rs or src/db.rs:143
        target: String,
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
        Command::Blame { target } => blame(&target),
        Command::Sessions => sessions(),
        Command::Intent { session, text } => intent(&session, &text.join(" ")),
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
            "{}:{} - ortak-{} {}, {}",
            rel,
            line,
            o.session_id,
            o.agent_name,
            ago(now - o.last_ts)
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
            "  {:>12}  ortak-{} {}, {}",
            range(o),
            o.session_id,
            o.agent_name,
            ago(now - o.last_ts)
        );
        println!(
            "                intent: {}",
            o.intent.as_deref().unwrap_or("(not reported)")
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
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
        db.insert_edit(a, "src/db.rs", "modify", None, &[]).unwrap();
        db.apply_edit_regions(a, "src/db.rs", &[hunk(1, 3)])
            .unwrap();
        db.insert_edit(b, "src/db.rs", "modify", None, &[]).unwrap();
        db.apply_edit_regions(b, "src/db.rs", &[hunk(40, 2)])
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

    #[test]
    fn a_trailing_line_number_is_not_part_of_the_filename() {
        assert_eq!(split_target("src/db.rs:143"), ("src/db.rs", Some(143)));
        assert_eq!(split_target("src/db.rs"), ("src/db.rs", None));
        assert_eq!(split_target("odd:name.rs"), ("odd:name.rs", None));
    }
}
