use crate::config::Config;
use crate::db::Db;
use crate::regions::{Region, WHOLE_FILE};
use crate::workspace::Workspace;
use anyhow::Result;
use serde_json::Value;
use std::io::Read;
use std::path::Path;

/// Claude Code hook adapters. These read the hook event JSON from stdin.
/// They must never break the agent's session: callers swallow errors and
/// always exit 0.

fn read_stdin_json() -> Result<Value> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

fn workspace_for(input: &Value) -> Result<Workspace> {
    if let Some(cwd) = input.get("cwd").and_then(|v| v.as_str()) {
        return Workspace::discover(Path::new(cwd));
    }
    Workspace::discover_from_cwd()
}

/// Short, human-readable agent name derived from the harness session id.
fn agent_name_for(external_id: &str) -> String {
    let short: String = external_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(4).collect();
    format!("claude-{}", short)
}

pub fn session_start() -> Result<()> {
    let input = read_stdin_json()?;
    let ws = workspace_for(&input)?;
    let db = Db::open(&ws.db_path)?;
    let external_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let id = db.upsert_session(&external_id, &agent_name_for(&external_id), "llm", Some("claude-code"))?;
    let context = format!(
        "ortak is active. The journal attributes this session's file changes to ortak-{id}. \
         Before editing, record your task intent: `ortak intent ortak-{id} \"<one-sentence task>\"`. \
         Publish your changes as a branch and PR with: `ortak publish ortak-{id}`."
    );
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": context,
        }
    });
    println!("{}", out);
    Ok(())
}

pub fn post_edit() -> Result<()> {
    let input = read_stdin_json()?;
    let ws = workspace_for(&input)?;
    let db = Db::open(&ws.db_path)?;
    let external_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    // Session may be unknown if the daemon/plugin were enabled mid-session.
    let session_id =
        db.upsert_session(&external_id, &agent_name_for(&external_id), "llm", Some("claude-code"))?;
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);
    let file = tool_input
        .get("file_path")
        .or_else(|| tool_input.get("notebook_path"))
        .and_then(|v| v.as_str());
    if let Some(f) = file {
        if let Some(rel) = ws.relativize(Path::new(f)) {
            db.insert_hint(&rel, session_id)?;
        }
    }
    Ok(())
}

/// PreToolUse gate: deny an edit that targets another active session's hot
/// region. Deterministic first-toucher-wins; on any doubt or error, allow.
pub fn pre_edit() -> Result<()> {
    let input = read_stdin_json()?;
    // Outside an ortak workspace, stay silent and allow the edit.
    let Ok(ws) = workspace_for(&input) else { return Ok(()) };
    let cfg = Config::load(&ws.config_path).unwrap_or_default();
    let db = Db::open(&ws.db_path)?;
    let external_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let me = db.upsert_session(external_id, &agent_name_for(external_id), "llm", Some("claude-code"))?;

    // Stop-the-line: while any error is open, only its responsible session
    // may edit. This check runs even if the conflict gate is disabled.
    let open = db.open_errors()?;
    if !open.is_empty() && !open.iter().any(|e| e.responsible() == me) {
        let e0 = &open[0];
        let reason = format!(
            "ortak STOPPED THE LINE: \"{}\" (error #{}). ortak-{} {} owns the fix{}. \
             The gate will reject file edits until that session runs `ortak resolved ortak-{}`. \
             Do not write code. You may read, investigate, or wait. Status: ortak errors",
            crate::line::shorten(&e0.excerpt, 160),
            e0.id,
            e0.responsible(),
            e0.responsible_name(),
            e0.fix_brief
                .as_deref()
                .map(|b| format!(" (brief: {})", b))
                .unwrap_or_default(),
            e0.responsible(),
        );
        let out = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        });
        println!("{}", out);
        return Ok(());
    }

    if !cfg.gate.enabled {
        return Ok(());
    }
    let tool_name = input.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);
    let Some((rel, targets)) = target_ranges(&ws, tool_name, &tool_input) else {
        return Ok(());
    };

    let conflicts = db.conflicts(
        &rel,
        &targets,
        me,
        cfg.gate.margin_lines,
        cfg.gate.presence_minutes * 60,
    )?;
    if conflicts.is_empty() {
        return Ok(());
    }

    // Layer 3: with the referee enabled, an intent-aware ruling can overrule
    // the deterministic first-toucher denial. Referee silence means deny.
    if cfg.orchestrator.enabled {
        let my = db.get_session(me)?;
        if let Some((allow, message)) = crate::orchestrator::conflict_verdict(
            &cfg.orchestrator,
            &rel,
            &my.agent_name,
            my.task_intent.as_deref().unwrap_or("(not reported)"),
            &conflicts,
        ) {
            if allow {
                return Ok(());
            }
            let reason = format!(
                "The ortak arbiter denied this edit in {}: {} Do not bypass the denial by writing through Bash. Status: ortak log --session ortak-{}",
                rel, message, conflicts[0].session_id
            );
            let out = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            });
            println!("{}", out);
            return Ok(());
        }
    }

    let mut reason = format!(
        "The ortak gate denied this edit. Your target lines in {} overlap another session's active region.\n",
        rel
    );
    for c in conflicts.iter().take(3) {
        let mins = ((crate::db::now_ts() - c.last_ts) / 60).max(0);
        reason.push_str(&format!(
            "- ortak-{} {} (lines {}-{}, last edit {} min ago), intent: {}\n",
            c.session_id,
            c.agent_name,
            c.start,
            c.end,
            mins,
            c.intent.as_deref().unwrap_or("(not reported)")
        ));
    }
    reason.push_str(&format!(
        "Do not edit this region or bypass the denial through Bash. Continue with non-conflicting work. The region becomes available after its owner has not touched the file for {} min. Inspect the owner's work with: ortak log --session ortak-{}",
        cfg.gate.presence_minutes,
        conflicts[0].session_id
    ));
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    println!("{}", out);
    Ok(())
}

/// Which line ranges of which file is this tool call about to touch?
/// `None` means nothing can be checked, so allow. Conservative fallbacks return a
/// whole-file range.
fn target_ranges(ws: &Workspace, tool_name: &str, tool_input: &Value) -> Option<(String, Vec<Region>)> {
    let whole = vec![Region { start: 1, end: WHOLE_FILE }];
    match tool_name {
        "Write" => {
            let rel = rel_file(ws, tool_input, "file_path")?;
            Some((rel, whole))
        }
        "NotebookEdit" => {
            let rel = rel_file(ws, tool_input, "notebook_path")?;
            Some((rel, whole))
        }
        "Edit" => {
            let rel = rel_file(ws, tool_input, "file_path")?;
            let old = tool_input.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
            let all = tool_input.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
            Some(ranged(ws, rel, &[(old, all)], whole)?)
        }
        "MultiEdit" => {
            let rel = rel_file(ws, tool_input, "file_path")?;
            let edits: Vec<(&str, bool)> = tool_input
                .get("edits")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| {
                            let old = e.get("old_string")?.as_str()?;
                            let all = e.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
                            Some((old, all))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(ranged(ws, rel, &edits, whole)?)
        }
        _ => None,
    }
}

fn rel_file(ws: &Workspace, tool_input: &Value, key: &str) -> Option<String> {
    let p = tool_input.get(key)?.as_str()?;
    ws.relativize(Path::new(p))
}

/// Locate old_string occurrences in the current file content and turn them
/// into line ranges. A missing file cannot be checked because Edit will fail.
/// An unreadable binary file gets a conservative whole-file range.
fn ranged(
    ws: &Workspace,
    rel: String,
    needles: &[(&str, bool)],
    whole: Vec<Region>,
) -> Option<(String, Vec<Region>)> {
    let abs = ws.root.join(&rel);
    let content = match std::fs::read_to_string(&abs) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some((rel, whole)),
    };
    let mut targets = Vec::new();
    for (needle, all) in needles {
        if needle.is_empty() {
            continue;
        }
        let n_lines = needle.lines().count().max(1) as i64;
        for (off, _) in content.match_indices(needle) {
            let start = content[..off].bytes().filter(|b| *b == b'\n').count() as i64 + 1;
            targets.push(Region { start, end: start + n_lines - 1 });
            if !*all || targets.len() >= 50 {
                break;
            }
        }
    }
    if targets.is_empty() {
        return None;
    }
    Some((rel, targets))
}

/// PostToolUse on Bash: when a command fails and the line is open, nudge the
/// agent to report the error if it looks foreign. Bridges LLM forgetfulness.
pub fn post_bash() -> Result<()> {
    let input = read_stdin_json()?;
    let Ok(ws) = workspace_for(&input) else { return Ok(()) };
    let resp = input.get("tool_response").cloned().unwrap_or(Value::Null);
    let exit = resp
        .get("exit_code")
        .or_else(|| resp.get("exitCode"))
        .or_else(|| resp.get("code"))
        .and_then(|v| v.as_i64());
    if exit == Some(0) || exit.is_none() {
        return Ok(());
    }
    // Only nudge on error-looking output; grep/diff style exit-1 with silent
    // output would otherwise spam the context.
    let text = format!(
        "{} {}",
        resp.get("stdout").and_then(|v| v.as_str()).unwrap_or(""),
        resp.get("stderr").and_then(|v| v.as_str()).unwrap_or("")
    )
    .to_lowercase();
    if !["error", "traceback", "exception", "fail", "panic"].iter().any(|k| text.contains(k)) {
        return Ok(());
    }
    let db = Db::open(&ws.db_path)?;
    if !db.open_errors()?.is_empty() {
        return Ok(()); // line already stopped; prompt-context handles messaging
    }
    let external_id = input.get("session_id").and_then(|v| v.as_str()).unwrap_or("unknown");
    let me = db.upsert_session(external_id, &agent_name_for(external_id), "llm", Some("claude-code"))?;
    let context = format!(
        "ortak: the last command failed. Fix the error if your changes caused it. Report it if it \
         appears unrelated so ortak can assign an owner: \
         `ortak report ortak-{} --command \"<command>\" \"<relevant error output>\"`",
        me
    );
    let out = serde_json::json!({
        "hookSpecificOutput": { "hookEventName": "PostToolUse", "additionalContext": context }
    });
    println!("{}", out);
    Ok(())
}

/// UserPromptSubmit: while the line is stopped, push its status into every
/// session's context so the responsible session sees its assignment.
pub fn prompt_context() -> Result<()> {
    let input = read_stdin_json()?;
    let Ok(ws) = workspace_for(&input) else { return Ok(()) };
    let db = Db::open(&ws.db_path)?;
    let open = db.open_errors()?;
    if open.is_empty() {
        return Ok(());
    }
    let external_id = input.get("session_id").and_then(|v| v.as_str()).unwrap_or("unknown");
    let me = db.upsert_session(external_id, &agent_name_for(external_id), "llm", Some("claude-code"))?;
    let mine: Vec<&crate::db::ErrorRow> = open.iter().filter(|e| e.responsible() == me).collect();
    let context = if !mine.is_empty() {
        let e = mine[0];
        format!(
            "ortak LINE STATUS: the line is stopped and YOU own the fix (ortak-{}). Error #{}: \"{}\"{} \
             Fix this error before continuing your task. Run `ortak resolved ortak-{}` after the fix \
             so other sessions can continue.",
            me,
            e.id,
            crate::line::shorten(&e.excerpt, 300),
            e.fix_brief
                .as_deref()
                .map(|b| format!(" Fix brief: {}", b))
                .unwrap_or_default(),
            me
        )
    } else {
        let e = &open[0];
        format!(
            "ortak LINE STATUS: the line is stopped by error #{}: \"{}\". ortak-{} {} owns the fix. \
             The gate will reject your file edits until the owner finishes. Continue with work that \
             does not require code changes, such as reading, analysis, or planning.",
            e.id,
            crate::line::shorten(&e.excerpt, 200),
            e.responsible(),
            e.responsible_name()
        )
    };
    let out = serde_json::json!({
        "hookSpecificOutput": { "hookEventName": "UserPromptSubmit", "additionalContext": context }
    });
    println!("{}", out);
    Ok(())
}

pub fn session_end() -> Result<()> {
    let input = read_stdin_json()?;
    let ws = workspace_for(&input)?;
    let db = Db::open(&ws.db_path)?;
    if let Some(external_id) = input.get("session_id").and_then(|v| v.as_str()) {
        db.end_session(external_id)?;
    }
    Ok(())
}
