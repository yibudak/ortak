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

/// One session weighed as the author of an error, over the blame lookback.
struct Suspect {
    id: i64,
    agent: String,
    files: Vec<String>,
    matched: Vec<String>,
    score: u32,
}

/// How strongly an error excerpt implicates one file. A full workspace-relative
/// path names the session's own file beyond doubt; a bare basename is a guess,
/// and a traceback mentioning a dependency's `models.py` should never outweigh
/// the session that actually edited `src/api/models.py`.
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

/// Every session with a recent edit, scored by how far the output implicates
/// its files. The reporter is in the list, because the arbiter should see what
/// it was working on, and always scores zero.
///
/// A traceback names where a failure surfaced, not where it came from, and
/// where it surfaced is the reporter's own file: it is the session that ran the
/// command. Both collisions that have actually cost this project time read that
/// way. A renamed method names only the call site, so scoring the reporter's
/// file made the session that noticed the error its author, with `file match`
/// confidence. A changed signature names the call site and the definition, so
/// both sessions tied and the tie fell back to the reporter as ambiguous. The
/// reporter loses either way, which is the one thing `report` exists to say.
///
/// A reporter that really did break its own file still ends up with it: nothing
/// else scores, and the fallback below hands it back. What it no longer does is
/// outrank a session the journal actually implicates.
fn suspects(excerpt: &str, reporter: i64, recent: &[(i64, String, String, i64)]) -> Vec<Suspect> {
    let mut per_session: Vec<Suspect> = Vec::new();
    for (sid, agent, file, _ts) in recent {
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
        let score = if *sid == reporter {
            0
        } else {
            blame_score(excerpt, file)
        };
        if score > 0 {
            entry.matched.push(file.clone());
            entry.score += score;
        }
    }
    per_session
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
    let per_session = suspects(&excerpt, reporter.id, &recent);
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
        // Two different silences, and the reader has to act on them
        // differently: nobody implicated is a hunt that found nothing to go on,
        // and a tie is a hunt that found too much. The old wording called both
        // ambiguous, which sent a reader looking for a rival that was not there.
        let how = if best_score == 0 {
            "nothing in the output names another session's recent files; reporter owns the error \
             by default"
                .to_string()
        } else {
            format!(
                "ambiguous: {} match equally; reporter owns the error by default",
                leaders
                    .iter()
                    .map(|l| format!("ortak-{}", l.id))
                    .collect::<Vec<_>>()
                    .join(" and ")
            )
        };
        (reporter.id, None, how)
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

    fn score_of(found: &[Suspect], session: i64) -> u32 {
        found
            .iter()
            .find(|s| s.id == session)
            .map_or(0, |s| s.score)
    }

    /// Both shapes are real rustc output for the two collisions that have cost
    /// this project time. A changed signature names the call site and the
    /// definition; a renamed method names only the call site. Either way the
    /// call site is the reporter's file, because the reporter ran the build.
    #[test]
    fn the_session_that_ran_the_build_is_not_its_author() {
        let author = 2;
        let reporter = 3;
        let recent = vec![
            (author, "claude-a".to_string(), "src/rows.rs".to_string(), 0),
            (
                reporter,
                "claude-b".to_string(),
                "src/render.rs".to_string(),
                0,
            ),
        ];

        let changed_signature = "error[E0061]: this method takes 1 argument but 0 arguments were \
             supplied\n --> src/render.rs:4:9\nnote: method defined here\n --> src/rows.rs:6:12";
        let found = suspects(changed_signature, reporter, &recent);
        assert_eq!(score_of(&found, author), 2, "the definition is named");
        assert_eq!(
            score_of(&found, reporter),
            0,
            "and the call site is not evidence against whoever is standing there"
        );

        // Renamed: rustc names no file but the caller's, so nobody is
        // implicated and the fallback hands it back to the reporter. Honest,
        // where scoring the caller was confidently wrong.
        let renamed = "error[E0599]: no method named `inferred` found for reference `&EditRow`\n \
                       --> src/render.rs:4:9";
        let found = suspects(renamed, reporter, &recent);
        assert!(found.iter().all(|s| s.score == 0));

        // The exemption follows the reporter, it is not a rule about one file.
        let found = suspects(changed_signature, author, &recent);
        assert_eq!(score_of(&found, author), 0);
        assert_eq!(score_of(&found, reporter), 2);
    }

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
