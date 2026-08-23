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
        let t = chrono::DateTime::from_timestamp(e.ts_opened, 0)
            .map(|d| d.format("%m-%d %H:%M").to_string())
            .unwrap_or_default();
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
}
