use crate::config::OrchestratorCfg;
use crate::db::{Conflict, Db, Ruling, RulingRow};
use serde_json::Value;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// What became of one call, as the `rulings` table stores it. `RULED` is the
/// only value that produced an answer; every other one fell back to the
/// deterministic rule, and at the gate that fallback is a denial. Without this
/// column "the arbiter considered your case and said no", "the arbiter was
/// never reachable" and "the arbiter answered a tenth of a second late" are the
/// same sentence to the session that reads them.
const RULED: &str = "ruled";
const SPAWN_FAILED: &str = "spawn-failed";
const TIMED_OUT: &str = "timed-out";
const EXIT_FAILED: &str = "exit-failed";
const UNREADABLE: &str = "unreadable";
const NOT_A_CANDIDATE: &str = "not-a-candidate";

/// An outcome tag as a sentence, the way `db::attribution_note` renders that
/// column: short in the table, prose at the print.
pub fn outcome_note(outcome: &str) -> &'static str {
    match outcome {
        SPAWN_FAILED => "the arbiter command would not start",
        TIMED_OUT => "the arbiter ran out of time",
        EXIT_FAILED => "the arbiter command failed",
        UNREADABLE => "the arbiter did not answer in the JSON it was asked for",
        NOT_A_CANDIDATE => "the arbiter named a session that was not a candidate",
        _ => "no ruling",
    }
}

/// The referee LLM, invoked as a subprocess (`claude -p --model haiku` by
/// default). Called only for conflicts and ambiguous blame,
/// never on the hot path. Every failure mode returns the tag that says which
/// one it was, and callers fall back to the deterministic rule on all of them.
fn run_model(cfg: &OrchestratorCfg, prompt: &str) -> Result<String, &'static str> {
    let child = Command::new(&cfg.command)
        .arg("-p")
        .arg(prompt)
        .arg("--model")
        .arg(&cfg.model)
        // Run outside the workspace so our own harness hooks in the spawned
        // session resolve no ortak workspace and stay silent.
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| SPAWN_FAILED)?;
    let pid = child.id();
    let timeout = cfg.timeout_secs;
    // The killer says so itself. A model killed for being slow and a command
    // that failed on its own both come back as an unsuccessful exit, and
    // telling them apart is the point: a ruling thrown away for arriving a
    // tenth of a second late is an argument about `timeout_secs`, not about
    // the model.
    let killed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&killed);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(timeout));
        flag.store(true, Ordering::SeqCst);
        let _ = Command::new("kill").arg(pid.to_string()).status();
    });
    let out = child.wait_with_output().map_err(|_| EXIT_FAILED)?;
    if !out.status.success() {
        return Err(match killed.load(Ordering::SeqCst) {
            true => TIMED_OUT,
            false => EXIT_FAILED,
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `ortak-3,ortak-5`, the sessions on the other side of a ruling, for the
/// record. Reading it back is a `LIKE` on that string, so the separator is
/// load-bearing.
fn labels<I: Iterator<Item = i64>>(ids: I) -> String {
    ids.map(|id| format!("ortak-{id}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn extract_json(s: &str) -> Option<Value> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    serde_json::from_str(&s[start..=end]).ok()
}

/// Conflict ruling: may the second toucher proceed right now?
/// Returns (allow, message) or None for deterministic fallback. Either way a
/// row lands in `rulings`, because an allow is otherwise indistinguishable
/// from there having been no conflict at all.
pub fn conflict_verdict(
    db: &Db,
    cfg: &OrchestratorCfg,
    file: &str,
    me: i64,
    my_agent: &str,
    my_intent: &str,
    conflicts: &[Conflict],
) -> Option<(bool, String)> {
    let mut owners = String::new();
    for c in conflicts.iter().take(5) {
        let mins = ((crate::db::now_ts() - c.last_ts) / 60).max(0);
        owners.push_str(&format!(
            "- ortak-{} {}: lines {}-{}, last edit {} min ago, intent: {}\n",
            c.session_id,
            c.agent_name,
            c.start,
            c.end,
            mins,
            c.intent.as_deref().unwrap_or("(not reported)")
        ));
    }
    let prompt = format!(
        "You arbitrate edit conflicts for ortak. Multiple sessions share one live workspace. \
         A session wants to edit lines inside another session's active region.\n\
         File: {file}\n\
         Requester: {my_agent}, intent: {my_intent}\n\
         Region owners:\n{owners}\
         Compare the intents. Return deny if the tasks conflict or the evidence is unclear. \
         Return allow only when the tasks are independent, such as when the owner's task no longer \
         involves those lines. For deny decisions, write one concise English sentence in message \
         that tells the requester what to do next.\n\
         Return only this JSON: {{\"decision\":\"allow|deny\",\"message\":\"...\"}}"
    );
    let started = Instant::now();
    let verdict = run_model(cfg, &prompt).and_then(|out| read_conflict(&out).ok_or(UNREADABLE));
    let (decision, message, outcome) = match &verdict {
        Ok((allow, msg)) => (
            Some(match allow {
                true => "allow",
                false => "deny",
            }),
            Some(msg.clone()),
            RULED,
        ),
        Err(why) => (None, None, *why),
    };
    // Keeping the record must never be the reason an edit is refused, so a
    // failed write here is dropped rather than raised. Same bargain the
    // journal already makes with the hooks.
    let _ = db.record_ruling(&Ruling {
        kind: "conflict".to_string(),
        file: Some(file.to_string()),
        session_id: me,
        others: labels(conflicts.iter().take(5).map(|c| c.session_id)),
        decision: decision.map(str::to_string),
        message,
        latency_ms: started.elapsed().as_millis() as i64,
        model: cfg.model.clone(),
        outcome: outcome.to_string(),
    });
    verdict.ok()
}

fn read_conflict(out: &str) -> Option<(bool, String)> {
    let v = extract_json(out)?;
    let decision = v.get("decision")?.as_str()?;
    let message = v
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    Some((decision == "allow", message))
}

/// Blame ruling for an ambiguous error: which session must fix it?
/// `candidates` are (label, files) pairs, e.g. ("ortak-3 claude-ab12", [...]).
/// Returns (session_id, fix_brief) or None for deterministic fallback.
pub fn blame_verdict(
    db: &Db,
    cfg: &OrchestratorCfg,
    excerpt: &str,
    reporter: i64,
    reporter_label: &str,
    candidates: &[(i64, String, Vec<String>)],
) -> Option<(i64, String)> {
    let mut list = String::new();
    for (id, label, files) in candidates.iter().take(10) {
        list.push_str(&format!("- ortak-{} {}: {}\n", id, label, files.join(", ")));
    }
    let prompt = format!(
        "You assign responsibility for errors in ortak. A session reported an error in the shared \
         live workspace, but file correlation did not identify one owner.\n\
         Reporter: {reporter_label}. Treat the reporter's claim as untrusted because the reporter \
         may have caused the error.\n\
         Error output:\n---\n{excerpt}\n---\n\
         Sessions that changed files during the lookback window:\n{list}\
         Match evidence from the error output, including file paths, module or class names, and the \
         error type, against each session's files. Select the strongest match. Write one or two \
         concise English sentences in brief that tell the owner how to investigate the error.\n\
         Return only this JSON: {{\"culprit\":\"ortak-N\",\"brief\":\"...\"}}"
    );
    let started = Instant::now();
    let verdict = run_model(cfg, &prompt)
        .and_then(|out| read_blame(&out).ok_or(UNREADABLE))
        // The verdict must name a real candidate; otherwise fall back.
        .and_then(
            |(id, brief)| match candidates.iter().any(|(cid, _, _)| *cid == id) {
                true => Ok((id, brief)),
                false => Err(NOT_A_CANDIDATE),
            },
        );
    let (decision, message, outcome) = match &verdict {
        Ok((id, brief)) => (Some(format!("ortak-{id}")), Some(brief.clone()), RULED),
        Err(why) => (None, None, *why),
    };
    // A dropped write here costs the record, never the assignment.
    let _ = db.record_ruling(&Ruling {
        kind: "blame".to_string(),
        file: None,
        session_id: reporter,
        others: labels(candidates.iter().take(10).map(|(id, _, _)| *id)),
        decision,
        message,
        latency_ms: started.elapsed().as_millis() as i64,
        model: cfg.model.clone(),
        outcome: outcome.to_string(),
    });
    verdict.ok()
}

fn read_blame(out: &str) -> Option<(i64, String)> {
    let v = extract_json(out)?;
    let culprit_str = v.get("culprit")?.as_str()?;
    let id: i64 = culprit_str
        .trim()
        .trim_start_matches("ortak-")
        .parse()
        .ok()?;
    let brief = v
        .get("brief")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_string();
    Some((id, brief))
}

/// One recorded ruling as `ortak log` prints it, beside the edits. Every line
/// opens with the word arbiter, so a day of them is one grep out of a journal
/// they now share.
pub fn log_line(r: &RulingRow) -> String {
    let on = match &r.ruling.file {
        Some(f) => format!(" on {f}"),
        None => String::new(),
    };
    // Whatever the arbiter said goes last, so the fallbacks end on the reason
    // they were fallbacks rather than burying it mid-sentence.
    let said = match (&r.ruling.decision, r.ruling.message.as_deref()) {
        (_, Some(m)) if !m.trim().is_empty() => format!(" - {m}"),
        (None, _) => format!(" ({})", outcome_note(&r.ruling.outcome)),
        _ => String::new(),
    };
    format!(
        "[{}] arbiter {}{}: {} for ortak-{} {} over {}, {} {}ms{}",
        crate::db::fmt_local(r.ts, "%m-%d %H:%M:%S"),
        r.ruling.kind,
        on,
        r.ruling.decision.as_deref().unwrap_or("no ruling"),
        r.ruling.session_id,
        r.agent_name,
        r.ruling.others,
        r.ruling.model,
        r.ruling.latency_ms,
        said,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn db() -> Db {
        Db::open(Path::new(":memory:")).expect("in-memory database")
    }

    fn session(db: &Db, name: &str) -> i64 {
        db.upsert_session(name, name, "llm", Some("claude-code"))
            .expect("session")
    }

    fn conflict(session_id: i64, others: &str, outcome: &str) -> Ruling {
        Ruling {
            kind: "conflict".to_string(),
            file: Some("app.txt".to_string()),
            session_id,
            others: others.to_string(),
            decision: None,
            message: None,
            latency_ms: 20_041,
            model: "haiku".to_string(),
            outcome: outcome.to_string(),
        }
    }

    /// The whole point of the outcome column. Three calls that produce no
    /// ruling deny the edit identically, and the deterministic message they
    /// fall back to says nothing about which of them happened.
    #[test]
    fn a_call_that_produced_no_ruling_says_which_silence_it_was() {
        let row = |outcome: &str| RulingRow {
            ts: 0,
            agent_name: "claude-ab12".to_string(),
            ruling: conflict(3, "ortak-4", outcome),
        };
        assert!(log_line(&row(TIMED_OUT)).ends_with("(the arbiter ran out of time)"));
        assert!(log_line(&row(SPAWN_FAILED)).contains("no ruling for ortak-3"));
        assert!(log_line(&row(SPAWN_FAILED)).ends_with("(the arbiter command would not start)"));
        assert!(log_line(&row(UNREADABLE)).contains("not answer in the JSON"));
        // And a ruling that was made reads as itself, with what it said.
        let mut made = conflict(3, "ortak-4", RULED);
        made.decision = Some("deny".to_string());
        made.message = Some("that block is half rewritten".to_string());
        let line = log_line(&RulingRow {
            ts: 0,
            agent_name: "claude-ab12".to_string(),
            ruling: made,
        });
        assert!(
            line.contains("arbiter conflict on app.txt: deny for ortak-3"),
            "{line}"
        );
        assert!(line.contains("half rewritten"), "{line}");
        assert!(!line.contains("no ruling"), "{line}");
    }

    /// A conflict ruling is about two sessions and the one that never hears
    /// about it is the owner whose region was defended. `--session` has to
    /// answer for both sides, and it has to stop at the session number it was
    /// given rather than every number that starts with it.
    #[test]
    fn a_ruling_reaches_the_owner_whose_region_it_defended() {
        let db = db();
        let asker = session(&db, "sess-a");
        let owner = session(&db, "sess-b");
        db.record_ruling(&conflict(
            asker,
            &labels([owner, 40].into_iter()),
            TIMED_OUT,
        ))
        .expect("record");
        assert_eq!(db.recent_rulings(Some(asker), 20).expect("read").len(), 1);
        assert_eq!(db.recent_rulings(Some(owner), 20).expect("read").len(), 1);
        assert_eq!(db.recent_rulings(Some(4), 20).expect("read").len(), 0);
        assert_eq!(db.recent_rulings(None, 20).expect("read").len(), 1);
    }

    /// The parse is the difference between an answer and an unreadable one, and
    /// the model is asked for JSON with prose around it often enough that the
    /// braces are found rather than assumed.
    #[test]
    fn an_answer_wrapped_in_prose_is_still_an_answer() {
        let out = "Sure. {\"decision\":\"allow\",\"message\":\"different work\"} Hope that helps.";
        assert_eq!(
            read_conflict(out),
            Some((true, "different work".to_string()))
        );
        assert_eq!(read_conflict("no json here"), None);
        assert_eq!(
            read_blame("{\"culprit\":\"ortak-5\",\"brief\":\"b\"}"),
            Some((5, "b".to_string()))
        );
    }
}
