use crate::config::Config;
use crate::db::Db;
use crate::orchestrator;
use crate::workspace::Workspace;
use anyhow::{bail, Result};

pub fn shorten(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let cut: String = one_line.chars().take(max).collect();
        format!("{}…", cut)
    }
}

/// How strongly an error excerpt implicates one file. A full workspace-relative
/// path names the session's own file beyond doubt; a bare basename is a guess,
/// and a traceback mentioning a dependency's `models.py` should never outweigh
/// the session that actually edited `src/api/models.py`.
/// One session weighed as the author of an error, over the blame lookback.
struct Suspect {
    id: i64,
    agent: String,
    files: Vec<String>,
    matched: Vec<String>,
    score: u32,
}

/// The files an error excerpt names that another session was writing seconds
/// ago. A build that catches a file half-written fails for a reason that fixes
/// itself, and stopping the line over it halts everyone else until somebody
/// notices and clears it.
fn still_being_written(
    excerpt: &str,
    reporter: i64,
    recent: &[(i64, String, String, i64)],
    now: i64,
    window: i64,
) -> Vec<(String, i64, String, i64)> {
    if window <= 0 {
        return Vec::new();
    }
    let mut fresh: Vec<(String, i64, String, i64)> = recent
        .iter()
        .filter(|(sid, _, file, ts)| {
            *sid != reporter && now - *ts <= window && blame_score(excerpt, file) > 0
        })
        .map(|(sid, agent, file, ts)| (file.clone(), *sid, agent.clone(), (now - *ts).max(0)))
        .collect();
    fresh.sort_by_key(|(_, _, _, secs)| *secs);
    fresh
}

fn blame_score(excerpt: &str, file: &str) -> u32 {
    if excerpt.contains(file) {
        return 2;
    }
    let base = file.rsplit('/').next().unwrap_or(file);
    if base.len() > 3 && excerpt.contains(base) {
        1
    } else {
        0
    }
}

/// An agent hit an error it believes is not its own: stop the line, hunt the
/// culprit (file correlation, arbiter LLM, then the reporter), and
/// record the obligation.
pub fn report(
    ws: &Workspace,
    cfg: &Config,
    session_ref: &str,
    command: Option<&str>,
    text: &str,
) -> Result<()> {
    let db = Db::open(&ws.db_path)?;
    let reporter = db.resolve_session(session_ref)?;
    let excerpt: String = text.chars().take(4000).collect();

    // Deterministic hunt: whose recently-edited files appear in the output?
    let recent = db.recent_session_files(cfg.line.blame_lookback_minutes * 60)?;

    // Stopping the line is the heaviest thing this tool does: every other
    // session is held until an owner clears it. A file that was being written
    // while the command ran is the one case where it is nearly always wrong,
    // and it is not rare. Declining rather than warning, because the reporter
    // was going to run the command again anyway, and because waiting is the
    // whole escape: nothing new to remember, no flag, and a file nobody has
    // touched for a minute and a half stops the line exactly as it always did.
    let fresh = still_being_written(
        &excerpt,
        reporter.id,
        &recent,
        crate::db::now_ts(),
        cfg.line.mid_write_seconds,
    );
    if let Some((file, sid, agent, secs)) = fresh.first() {
        bail!(
            "not stopping the line: {file} was written {secs}s ago by ortak-{sid} {agent}, so this \
             command caught it mid-edit. A half-written file fails and then builds again on its \
             own. Run the command again: after {window}s untouched the same report goes \
             through. If it keeps failing while that session works, say so with \
             `ortak tell ortak-{sid} \"<what broke>\" --from ortak-{}`",
            reporter.id,
            window = cfg.line.mid_write_seconds
        );
    }

    let mut per_session: Vec<Suspect> = Vec::new();
    for (sid, agent, file, _ts) in &recent {
        let entry = match per_session.iter_mut().find(|s| s.id == *sid) {
            Some(e) => e,
            None => {
                per_session.push(Suspect {
                    id: *sid,
                    agent: agent.clone(),
                    files: Vec::new(),
                    matched: Vec::new(),
                    score: 0,
                });
                per_session.last_mut().unwrap()
            }
        };
        entry.files.push(file.clone());
        let score = blame_score(&excerpt, file);
        if score > 0 {
            entry.matched.push(file.clone());
            entry.score += score;
        }
    }
    let best_score = per_session.iter().map(|s| s.score).max().unwrap_or(0);
    let leaders: Vec<&Suspect> = per_session
        .iter()
        .filter(|s| s.score == best_score && best_score > 0)
        .collect();

    let (culprit, brief, how) = if leaders.len() == 1 {
        let l = leaders[0];
        (l.id, None, format!("file match: {}", l.matched.join(", ")))
    } else if cfg.orchestrator.enabled {
        let candidates: Vec<(i64, String, Vec<String>)> = per_session
            .iter()
            .map(|s| (s.id, s.agent.clone(), s.files.clone()))
            .collect();
        let reporter_label = format!("ortak-{} {}", reporter.id, reporter.agent_name);
        match orchestrator::blame_verdict(&cfg.orchestrator, &excerpt, &reporter_label, &candidates)
        {
            Some((id, brief)) => (id, Some(brief), "arbiter verdict".to_string()),
            None => (
                reporter.id,
                None,
                "arbiter returned no answer; reporter owns the error by default".to_string(),
            ),
        }
    } else {
        (
            reporter.id,
            None,
            "ambiguous match; reporter owns the error by default".to_string(),
        )
    };

    let err_id = db.insert_error(
        reporter.id,
        command,
        &excerpt,
        Some(culprit),
        brief.as_deref(),
    )?;
    let culprit_session = db.get_session(culprit)?;
    println!("line STOPPED (error #{}).", err_id);
    println!(
        "responsible: ortak-{} {} ({})",
        culprit_session.id, culprit_session.agent_name, how
    );
    if let Some(b) = &brief {
        println!("fix brief: {}", b);
    }
    println!(
        "the gate will reject other sessions' edits until the owner runs `ortak resolved ortak-{}`.",
        culprit_session.id
    );
    Ok(())
}

pub fn resolved(ws: &Workspace, session_ref: Option<&str>, all: bool) -> Result<()> {
    let db = Db::open(&ws.db_path)?;
    let responsible = match session_ref {
        Some(r) => Some(db.resolve_session(r)?.id),
        // Clearing every error restarts every stopped session, so it should not
        // also be the shortest thing to type.
        None if all => {
            println!("resolving ALL open errors.");
            None
        }
        None => bail!(
            "name the session whose error is fixed, e.g. `ortak resolved ortak-2`, \
             or pass --all to clear every open error"
        ),
    };
    let n = db.resolve_errors(responsible)?;
    let remaining = db.open_errors()?;
    println!("resolved {} errors.", n);
    if remaining.is_empty() {
        println!("line OPEN: all sessions may continue.");
    } else {
        println!("line remains stopped; open errors:");
        for e in remaining {
            println!(
                "  #{} owner ortak-{} {} - {}",
                e.id,
                e.responsible(),
                e.responsible_name(),
                shorten(&e.excerpt, 80)
            );
        }
    }
    Ok(())
}

pub fn list(ws: &Workspace, as_json: bool) -> Result<()> {
    let db = Db::open(&ws.db_path)?;
    let rows = db.list_errors(20)?;
    if as_json {
        return crate::json::print(&crate::json::errors(&rows));
    }
    if rows.is_empty() {
        println!("no errors recorded; line is open.");
        return Ok(());
    }
    for e in &rows {
        let t = crate::db::fmt_local(e.ts_opened, "%m-%d %H:%M");
        println!(
            "#{} [{}] {} - reporter ortak-{} {}, owner ortak-{} {}",
            e.id,
            e.status,
            t,
            e.reporter,
            e.reporter_name,
            e.responsible(),
            e.responsible_name()
        );
        println!("   {}", shorten(&e.excerpt, 120));
        if let Some(b) = &e.fix_brief {
            println!("   brief: {}", b);
        }
    }
    Ok(())
}

pub fn assign(ws: &Workspace, error_id: i64, session_ref: &str) -> Result<()> {
    let db = Db::open(&ws.db_path)?;
    let s = db.resolve_session(session_ref)?;
    db.assign_error(error_id, s.id)?;
    println!(
        "assigned error #{} to ortak-{} {}.",
        error_id, s.id, s.agent_name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_path_outweighs_a_bare_filename() {
        let excerpt = "File \"/venv/lib/django/db/models.py\", line 5\n  \
                       File \"src/api/models.py\", line 9, in save";
        assert_eq!(blame_score(excerpt, "src/api/models.py"), 2);
        // Only the basename appears, and it came from a dependency's frame.
        assert_eq!(blame_score(excerpt, "billing/models.py"), 1);
        assert_eq!(blame_score(excerpt, "src/api/views.py"), 0);
        // Basenames this short match too much to mean anything.
        assert_eq!(blame_score("cannot open db", "src/db"), 0);
    }

    /// The round-6 case: `cargo build` caught the other session mid-edit twice
    /// in ten minutes, and one of those compiled again five seconds later with
    /// nobody touching it.
    #[test]
    fn a_file_still_being_written_is_not_a_breakage() {
        let excerpt =
            "error[E0433]: failed to resolve: use of undeclared type\n --> src/db.rs:412:9";
        let now = 1_000_000;
        let mid_edit = vec![
            (2, "claude-a".to_string(), "src/db.rs".to_string(), now - 5),
            // Fresh, but the failure does not name it.
            (
                2,
                "claude-a".to_string(),
                "src/daemon.rs".to_string(),
                now - 5,
            ),
        ];
        let held = still_being_written(excerpt, 3, &mid_edit, now, 90);
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].0, "src/db.rs");
        assert_eq!(held[0].3, 5);

        // The same file an hour later is finished work, and a failure naming it
        // stops the line exactly as it did before this rule existed.
        let finished = vec![(
            2,
            "claude-a".to_string(),
            "src/db.rs".to_string(),
            now - 3600,
        )];
        assert!(still_being_written(excerpt, 3, &finished, now, 90).is_empty());

        // The reporter's own half-written file is its own problem to fix.
        let mine = vec![(3, "claude-b".to_string(), "src/db.rs".to_string(), now - 5)];
        assert!(still_being_written(excerpt, 3, &mine, now, 90).is_empty());
    }
}
