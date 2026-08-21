use crate::config::OrchestratorCfg;
use crate::db::Conflict;
use serde_json::Value;
use std::process::{Command, Stdio};

/// The referee LLM, invoked as a subprocess (`claude -p --model haiku` by
/// default). Called only for conflicts and ambiguous blame,
/// never on the hot path. Every failure mode returns None so callers fall
/// back to the deterministic rule.
fn run_model(cfg: &OrchestratorCfg, prompt: &str) -> Option<String> {
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
        .ok()?;
    let pid = child.id();
    let timeout = cfg.timeout_secs;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(timeout));
        let _ = Command::new("kill").arg(pid.to_string()).status();
    });
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn extract_json(s: &str) -> Option<Value> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    serde_json::from_str(&s[start..=end]).ok()
}

/// Conflict ruling: may the second toucher proceed right now?
/// Returns (allow, message) or None for deterministic fallback.
pub fn conflict_verdict(
    cfg: &OrchestratorCfg,
    file: &str,
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
    let out = run_model(cfg, &prompt)?;
    let v = extract_json(&out)?;
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
    cfg: &OrchestratorCfg,
    excerpt: &str,
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
    let out = run_model(cfg, &prompt)?;
    let v = extract_json(&out)?;
    let culprit_str = v.get("culprit")?.as_str()?;
    let id: i64 = culprit_str.trim().trim_start_matches("ortak-").parse().ok()?;
    // The verdict must name a real candidate; otherwise fall back.
    if !candidates.iter().any(|(cid, _, _)| *cid == id) {
        return None;
    }
    let brief = v
        .get("brief")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_string();
    Some((id, brief))
}
