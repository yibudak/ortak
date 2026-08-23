use crate::config::Config;
use crate::db::Db;
use crate::orchestrator;
use crate::workspace::Workspace;
use anyhow::Result;

pub fn shorten(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let cut: String = one_line.chars().take(max).collect();
        format!("{}…", cut)
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
    let mut per_session: Vec<(i64, String, Vec<String>, Vec<String>)> = Vec::new(); // id, agent, all files, matched
    for (sid, agent, file, _ts) in &recent {
        let entry = match per_session.iter_mut().find(|(id, ..)| id == sid) {
            Some(e) => e,
            None => {
                per_session.push((*sid, agent.clone(), Vec::new(), Vec::new()));
                per_session.last_mut().unwrap()
            }
        };
        entry.2.push(file.clone());
        let base = file.rsplit('/').next().unwrap_or(file);
        if excerpt.contains(file.as_str()) || (base.len() > 3 && excerpt.contains(base)) {
            entry.3.push(file.clone());
        }
    }
    let best_score = per_session.iter().map(|e| e.3.len()).max().unwrap_or(0);
    let leaders: Vec<&(i64, String, Vec<String>, Vec<String>)> = per_session
        .iter()
        .filter(|e| e.3.len() == best_score && best_score > 0)
        .collect();

    let (culprit, brief, how) = if leaders.len() == 1 {
        let l = leaders[0];
        (l.0, None, format!("file match: {}", l.3.join(", ")))
    } else if cfg.orchestrator.enabled {
        let candidates: Vec<(i64, String, Vec<String>)> = per_session
            .iter()
            .map(|(id, agent, files, _)| (*id, agent.clone(), files.clone()))
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

pub fn resolved(ws: &Workspace, session_ref: Option<&str>) -> Result<()> {
    let db = Db::open(&ws.db_path)?;
    let responsible = match session_ref {
        Some(r) => Some(db.resolve_session(r)?.id),
        None => {
            println!("warning: no session supplied; resolving ALL open errors.");
            None
        }
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

pub fn list(ws: &Workspace) -> Result<()> {
    let db = Db::open(&ws.db_path)?;
    let rows = db.list_errors(20)?;
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
