use crate::config::Config;
use crate::db::Db;
use crate::regions::{Region, WHOLE_FILE};
use crate::workspace::Workspace;
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

// Claude Code and Codex hook adapters read hook event JSON from stdin. They
// must never break the agent's session: callers swallow errors and exit 0.

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

fn harness_for(input: &Value) -> &'static str {
    let transcript_is_codex = input
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .is_some_and(|path| path.contains("/.codex/"));
    let tool_is_codex = input
        .get("tool_name")
        .and_then(|v| v.as_str())
        .is_some_and(|name| name == "apply_patch");
    if transcript_is_codex || tool_is_codex {
        "codex"
    } else {
        "claude-code"
    }
}

/// Short, human-readable agent name derived from the harness session id.
/// Eight characters, because this name is how a denial message, a status row
/// and a published branch tell two sessions apart; four collide readily
/// (`sess-A` and `sess-B` both became `claude-sess`).
fn agent_name_for(external_id: &str, harness: &str) -> String {
    let short: String = external_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let prefix = if harness == "codex" {
        "codex"
    } else {
        "claude"
    };
    format!("{}-{}", prefix, short)
}

/// The gate compares an edit against regions the daemon records, so a stopped
/// daemon means nothing is journaled and nothing is protected. Staying silent
/// then reads exactly like "no conflicts", which is the wrong thing to believe.
fn daemon_warning(db: &Db) -> Option<String> {
    let tail = match db.heartbeat_age() {
        Ok(Some(age)) if age <= crate::db::HEARTBEAT_ALIVE_SECS => return None,
        Ok(Some(age)) => format!("last heartbeat {age}s ago"),
        _ => "it has not run in this workspace".to_string(),
    };
    Some(format!(
        "ortak WARNING: the ortak daemon is not running ({tail}). Nothing is being journaled, \
         the conflict gate cannot see other sessions' work, and `ortak publish` will have nothing \
         to publish. Start it with `ortak daemon` before editing shared files."
    ))
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
    let harness = harness_for(&input);
    let id = db.upsert_session(
        &external_id,
        &agent_name_for(&external_id, harness),
        "llm",
        Some(harness),
    )?;
    // The no-git rule belongs here rather than only in the skill: a skill loads
    // when the model judges it relevant, and this is the rule people get burned
    // by. Advisory on purpose. Nothing blocks git, because the gate cannot tell
    // a lone session's harmless commit from one that sweeps up another
    // session's uncommitted work.
    let context = format!(
        "ortak is active. The journal attributes this session's file changes to ortak-{id}. \
         Before editing, record your task intent: `ortak intent ortak-{id} \"<one-sentence task>\"`. \
         Publish your changes as a branch and PR with: `ortak publish ortak-{id}`. \
         Do not run `git stash`, `checkout`, `switch`, `branch`, `commit` or `add` in this \
         workspace: other sessions have uncommitted work in the same tree and those commands \
         will take it. Read-only git is fine."
    );
    let context = match daemon_warning(&db) {
        Some(w) => format!("{context}\n\n{w}"),
        None => context,
    };
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
    let cwd_ws = workspace_for(&input).ok();
    let external_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let harness = harness_for(&input);
    let tool_name = input
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);
    for (ws, files) in group_by_workspace(cwd_ws.as_ref(), &target_paths(tool_name, &tool_input)) {
        let db = Db::open(&ws.db_path)?;
        // Session may be unknown if the daemon/plugin were enabled mid-session,
        // and is always unknown the first time this session reaches into a
        // workspace other than its own.
        let session_id = db.upsert_session(
            &external_id,
            &agent_name_for(&external_id, harness),
            "llm",
            Some(harness),
        )?;
        // Record what THIS call wrote, rebuilt from the tool's own input rather
        // than read back off disk. Another session can write the same file
        // between the tool returning and this hook running.
        let shadow = crate::shadow::open(&ws).ok();
        for rel in files {
            let blob = shadow.as_ref().and_then(|repo| {
                let base = snapshot_base(repo, &db, &rel);
                let data = intended_content(&base, tool_name, &tool_input)?;
                Some(repo.blob(&data).ok()?.to_string())
            });
            db.insert_hint(&rel, session_id, blob.as_deref())?;
        }
    }
    Ok(())
}

/// The content this session's edit applies to: its own newest pending snapshot
/// when it has one, else the last state the journal recorded, else nothing.
fn snapshot_base(repo: &git2::Repository, db: &Db, rel: &str) -> Vec<u8> {
    if let Ok(Some(id)) = db.latest_snapshot(rel) {
        if let Some(blob) = git2::Oid::from_str(&id)
            .ok()
            .and_then(|oid| repo.find_blob(oid).ok())
        {
            return blob.content().to_vec();
        }
    }
    crate::shadow::head_blob(repo, rel)
        .map(|b| b.content().to_vec())
        .unwrap_or_default()
}

/// Apply one tool call to `base`, giving this session's own result. `None` when
/// the tool's input cannot reproduce it, and the caller then leaves a blob-less
/// hint for the daemon to resolve by reading the file. `apply_patch` and
/// NotebookEdit always land there: neither input can be replayed.
fn intended_content(base: &[u8], tool_name: &str, tool_input: &Value) -> Option<Vec<u8>> {
    if tool_name == "Write" {
        return tool_input
            .get("content")
            .and_then(|v| v.as_str())
            .map(|c| c.as_bytes().to_vec());
    }
    let edits: Vec<&Value> = match tool_name {
        "Edit" => vec![tool_input],
        "MultiEdit" => tool_input.get("edits")?.as_array()?.iter().collect(),
        _ => return None,
    };
    let mut text = String::from_utf8(base.to_vec()).ok()?;
    for e in edits {
        let old = e.get("old_string").and_then(|v| v.as_str())?;
        let new = e.get("new_string").and_then(|v| v.as_str())?;
        if !text.contains(old) {
            return None;
        }
        text = if e.get("replace_all").and_then(|v| v.as_bool()) == Some(true) {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
    }
    Some(text.into_bytes())
}

/// PreToolUse gate: deny an edit that targets another active session's hot
/// region. Deterministic first-toucher-wins; on any doubt or error, allow.
///
/// This is also the door messages arrive through. `UserPromptSubmit` fires when
/// a person types, and an agent working through a brief types once and then
/// runs for an hour, so everything sent in that hour waited for a turn that
/// never came. PreToolUse fires before every edit instead, and the harness
/// shows `additionalContext` whether the call is allowed or denied, so a
/// message reaches a working session without the gate having to deny anything
/// in order to say it.
pub fn pre_edit() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd_ws = workspace_for(&input).ok();
    let external_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let harness = harness_for(&input);
    let tool_name = input
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);

    let mut groups = group_by_workspace(cwd_ws.as_ref(), &target_paths(tool_name, &tool_input));
    // The session's own workspace answers even when the call targets nothing in
    // it, so a stopped line there still holds the session still.
    if let Some(ws) = cwd_ws.clone() {
        if !groups.iter().any(|(w, _)| w.root == ws.root) {
            groups.push((ws, Vec::new()));
        }
    }
    let mut denial = None;
    for (ws, files) in &groups {
        if let Some(reason) = verdict(ws, external_id, harness, files, tool_name, &tool_input)? {
            denial = Some(reason);
            break;
        }
    }
    // Drained after the ruling, not before: `take_messages` stamps a row
    // delivered, and a run that errors on its way to the verdict prints
    // nothing. Draining first would mark a message handed over that nobody
    // ever saw.
    let waiting = match &cwd_ws {
        Some(ws) => {
            let db = Db::open(&ws.db_path)?;
            let me = db.upsert_session(
                external_id,
                &agent_name_for(external_id, harness),
                "llm",
                Some(harness),
            )?;
            pending_messages(&db, me)?
        }
        None => None,
    };
    if let Some(out) = pre_tool_use_output(denial, waiting) {
        println!("{}", out);
    }
    Ok(())
}

/// One PreToolUse answer: a denial, a message, both, or nothing at all.
///
/// The denial keeps the text the gate wrote, and a message rides beside it in
/// `additionalContext` rather than inside it, so an already dense denial does
/// not get denser. A message on its own carries no `permissionDecision`, which
/// leaves the edit to the harness exactly as if this hook had said nothing.
fn pre_tool_use_output(denial: Option<String>, waiting: Option<String>) -> Option<Value> {
    if denial.is_none() && waiting.is_none() {
        return None;
    }
    let mut hook = serde_json::json!({ "hookEventName": "PreToolUse" });
    if let Some(reason) = denial {
        hook["permissionDecision"] = Value::from("deny");
        hook["permissionDecisionReason"] = Value::from(reason);
    }
    if let Some(block) = waiting {
        hook["additionalContext"] = Value::from(block);
    }
    Some(serde_json::json!({ "hookSpecificOutput": hook }))
}

/// Whatever this session has been sent and not yet been handed, as one block.
/// `take_messages` stamps every row delivered, so whichever door opens first
/// hands the message over and the others stay quiet about it.
fn pending_messages(db: &Db, me: i64) -> Result<Option<String>> {
    let waiting = db.take_messages(me)?;
    if waiting.is_empty() {
        return Ok(None);
    }
    let mut block = String::from("ortak MESSAGES from other sessions in this workspace:");
    for m in &waiting {
        block.push_str(&format!(
            "\n- ortak-{} {}: {}",
            m.from_session, m.from_name, m.text
        ));
    }
    Ok(Some(block))
}

/// One workspace's ruling on a tool call. `Some(reason)` denies the edit;
/// `None` allows it, which is also what any doubt or error resolves to.
fn verdict(
    ws: &Workspace,
    external_id: &str,
    harness: &str,
    files: &[String],
    tool_name: &str,
    tool_input: &Value,
) -> Result<Option<String>> {
    let cfg = Config::load(&ws.config_path).unwrap_or_default();
    let db = Db::open(&ws.db_path)?;
    let me = db.upsert_session(
        external_id,
        &agent_name_for(external_id, harness),
        "llm",
        Some(harness),
    )?;

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
        return Ok(Some(reason));
    }

    if !cfg.gate.enabled {
        return Ok(None);
    }
    let mut blocked = None;
    for rel in files {
        let Some(targets) = target_regions(ws, rel, tool_name, tool_input) else {
            continue;
        };
        let conflicts = db.conflicts(
            rel,
            &targets,
            me,
            cfg.gate.margin_lines,
            cfg.gate.presence_minutes * 60,
        )?;
        if !conflicts.is_empty() {
            blocked = Some((rel.clone(), conflicts));
            break;
        }
    }
    let Some((rel, conflicts)) = blocked else {
        return Ok(None);
    };

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
                return Ok(None);
            }
            let reason = format!(
                "The ortak arbiter denied this edit in {}: {} Do not bypass the denial by writing through Bash. Status: ortak log --session ortak-{}",
                rel, message, conflicts[0].session_id
            );
            return Ok(Some(reason));
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
    Ok(Some(reason))
}

/// Every file path a tool call names, as written in the tool input. An empty
/// result means there is nothing to journal or check.
fn target_paths(tool_name: &str, tool_input: &Value) -> Vec<String> {
    match tool_name {
        "apply_patch" => patch_command(tool_input)
            .map(patch_paths)
            .unwrap_or_default(),
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => tool_input
            .get("file_path")
            .or_else(|| tool_input.get("notebook_path"))
            .and_then(|v| v.as_str())
            .map(|p| vec![p.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Group a tool call's targets by the ortak workspace that owns each file,
/// which is the workspace whose journal, gate and config govern that edit. It
/// is not always the one the session runs in: an agent working in an app repo
/// reaches into the backend repo next to it, and one call can legitimately
/// span both. Relative paths resolve against `base`, the session's own
/// workspace, as before. A file that belongs to no workspace is skipped in
/// silence.
///
/// A session that edits across workspaces gets a session row, and a different
/// `ortak-N`, in each of them. Session numbering is per workspace, so that is
/// correct, but the SessionStart context only ever names the id of the
/// workspace the session started in.
fn group_by_workspace(base: Option<&Workspace>, paths: &[String]) -> Vec<(Workspace, Vec<String>)> {
    let mut out: Vec<(Workspace, Vec<String>)> = Vec::new();
    for p in paths {
        let raw = Path::new(p);
        let abs = if raw.is_absolute() {
            raw.to_path_buf()
        } else if let Some(ws) = base {
            ws.root.join(raw)
        } else {
            continue;
        };
        let Ok(ws) = Workspace::discover(&abs) else {
            continue;
        };
        let Some(rel) = ws.relativize(&abs) else {
            continue;
        };
        match out.iter_mut().find(|(w, _)| w.root == ws.root) {
            Some((_, files)) => files.push(rel),
            None => out.push((ws, vec![rel])),
        }
    }
    out
}

/// Which line ranges is this tool call about to touch inside `rel`?
/// `None` means nothing can be checked, so leave the file alone. Conservative
/// fallbacks return a whole-file range.
fn target_regions(
    ws: &Workspace,
    rel: &str,
    tool_name: &str,
    tool_input: &Value,
) -> Option<Vec<Region>> {
    let whole = vec![Region {
        start: 1,
        end: WHOLE_FILE,
    }];
    match tool_name {
        "Edit" => {
            let old = tool_input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let all = tool_input
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            ranged(ws, rel, &[(old, all)], whole)
        }
        "MultiEdit" => {
            let edits: Vec<(&str, bool)> = tool_input
                .get("edits")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| {
                            let old = e.get("old_string")?.as_str()?;
                            let all = e
                                .get("replace_all")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            Some((old, all))
                        })
                        .collect()
                })
                .unwrap_or_default();
            ranged(ws, rel, &edits, whole)
        }
        _ => Some(whole),
    }
}

fn patch_command(tool_input: &Value) -> Option<&str> {
    tool_input
        .get("command")
        .or_else(|| tool_input.get("patch"))
        .and_then(|v| v.as_str())
        .or_else(|| tool_input.as_str())
}

fn patch_paths(patch: &str) -> Vec<String> {
    const PATH_PREFIXES: [&str; 4] = [
        "*** Add File: ",
        "*** Update File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    let mut files = BTreeSet::new();
    for line in patch.lines() {
        let Some(file) = PATH_PREFIXES
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
        else {
            continue;
        };
        files.insert(file.trim().to_string());
    }
    files.into_iter().collect()
}

/// Locate old_string occurrences in the current file content and turn them
/// into line ranges. A missing file cannot be checked because Edit will fail.
/// An unreadable binary file gets a conservative whole-file range.
fn ranged(
    ws: &Workspace,
    rel: &str,
    needles: &[(&str, bool)],
    whole: Vec<Region>,
) -> Option<Vec<Region>> {
    let abs = ws.root.join(rel);
    let content = match std::fs::read_to_string(&abs) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(whole),
    };
    let mut targets = Vec::new();
    for (needle, all) in needles {
        if needle.is_empty() {
            continue;
        }
        let n_lines = needle.lines().count().max(1) as i64;
        for (off, _) in content.match_indices(needle) {
            let start = content[..off].bytes().filter(|b| *b == b'\n').count() as i64 + 1;
            targets.push(Region {
                start,
                end: start + n_lines - 1,
            });
            if !*all || targets.len() >= 50 {
                break;
            }
        }
    }
    if targets.is_empty() {
        return None;
    }
    Some(targets)
}

/// PostToolUse on Bash: when a command fails and the line is open, nudge the
/// agent to report the error if it looks foreign. Bridges LLM forgetfulness.
/// PreToolUse on Bash: claim whatever the command is about to write. The edit
/// hooks cannot see a shell redirect or an in-place rewrite, so without this the
/// daemon attributes that work to the human and the session publishes nothing.
pub fn pre_bash() -> Result<()> {
    let input = read_stdin_json()?;
    let Ok(ws) = workspace_for(&input) else {
        return Ok(());
    };
    let db = Db::open(&ws.db_path)?;
    let external_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let harness = harness_for(&input);
    let me = db.upsert_session(
        external_id,
        &agent_name_for(external_id, harness),
        "llm",
        Some(harness),
    )?;
    // ponytail: the claim covers every unhinted change until the command ends,
    // so a human editor save during a long build lands on this session too.
    // Coarse, but the alternative loses the agent's own work.
    db.insert_hint(crate::db::BASH_CLAIM, me, None)?;
    // A session can go a long way between edits while it reads, greps and runs
    // tests, and a message that waits for the next edit can wait that long.
    // Bash is the call an agent makes constantly, so it opens the same door.
    if let Some(out) = pre_tool_use_output(None, pending_messages(&db, me)?) {
        println!("{}", out);
    }
    Ok(())
}

pub fn post_bash() -> Result<()> {
    let input = read_stdin_json()?;
    let Ok(ws) = workspace_for(&input) else {
        return Ok(());
    };
    let db = Db::open(&ws.db_path)?;
    let external_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let harness = harness_for(&input);
    let me = db.upsert_session(
        external_id,
        &agent_name_for(external_id, harness),
        "llm",
        Some(harness),
    )?;
    // The command has finished, so whatever it wrote has already reached the
    // daemon. Close the claim before the early returns below.
    db.clear_bash_claim(me)?;

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
    if !["error", "traceback", "exception", "fail", "panic"]
        .iter()
        .any(|k| text.contains(k))
    {
        return Ok(());
    }
    if !db.open_errors()?.is_empty() {
        return Ok(()); // line already stopped; prompt-context handles messaging
    }
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

/// UserPromptSubmit: hand this session anything waiting for it, and while the
/// line is stopped, push its status in so the responsible session sees its
/// assignment.
pub fn prompt_context() -> Result<()> {
    let input = read_stdin_json()?;
    let Ok(ws) = workspace_for(&input) else {
        return Ok(());
    };
    let db = Db::open(&ws.db_path)?;
    let warning = daemon_warning(&db);
    let external_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let harness = harness_for(&input);
    let me = db.upsert_session(
        external_id,
        &agent_name_for(external_id, harness),
        "llm",
        Some(harness),
    )?;

    let mut parts: Vec<String> = Vec::new();
    if let Some(warning) = warning {
        parts.push(warning);
    }

    // The original door, and still the one a person typing comes through. The
    // edit and Bash hooks drain the same queue, so a message already handed to
    // this session mid-task does not arrive a second time here.
    if let Some(block) = pending_messages(&db, me)? {
        parts.push(block);
    }

    let open = db.open_errors()?;
    let failing = db.journal_failures()?;
    if let Some(newest) = failing.first() {
        parts.push(format!(
            "ortak JOURNAL FAILING: {} file(s) are not reaching the journal, most recently {}: {}. \
             Edits to those files are attributed to nobody and will not publish. Run `ortak status`.",
            failing.len(),
            newest.file,
            newest.reason
        ));
    }
    if open.is_empty() {
        return emit_prompt_context(parts);
    }
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
    parts.push(context);
    emit_prompt_context(parts)
}

fn emit_prompt_context(parts: Vec<String>) -> Result<()> {
    if parts.is_empty() {
        return Ok(());
    }
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": parts.join("\n\n"),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_from_one_harness_get_distinct_names() {
        // Real Claude Code session ids share a generous prefix length.
        let a = agent_name_for("63a2d8fa-3038-4728-874c-be1a21b07aab", "claude-code");
        let b = agent_name_for("63a2d8fb-1111-4728-874c-be1a21b07aab", "claude-code");
        assert_ne!(a, b);
    }

    /// A denial's text is the gate's own, and a waiting message rides beside it
    /// rather than inside it. The case that matters most is a message with no
    /// denial: it must not carry a permission decision, or delivering the news
    /// would block the edit it was delivered before.
    #[test]
    fn a_message_rides_beside_a_denial_without_becoming_one() {
        assert!(pre_tool_use_output(None, None).is_none(), "nothing to say");

        let msg = "ortak MESSAGES from other sessions in this workspace:\n\
                   - ortak-3 claude-75c62662: db.rs is mid-refactor";
        let alone = pre_tool_use_output(None, Some(msg.to_string())).expect("a message is output");
        assert_eq!(alone["hookSpecificOutput"]["additionalContext"], msg);
        assert!(
            alone["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none(),
            "delivering a message must leave the edit alone"
        );

        let denied = pre_tool_use_output(Some("the ortak gate denied this edit".into()), None)
            .expect("a denial is output");
        assert_eq!(denied["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            denied["hookSpecificOutput"]
                .get("additionalContext")
                .is_none(),
            "a quiet queue adds nothing to a denial"
        );

        let both = pre_tool_use_output(
            Some("the ortak gate denied this edit".into()),
            Some(msg.to_string()),
        )
        .expect("both are output");
        assert_eq!(
            both["hookSpecificOutput"]["permissionDecisionReason"],
            "the ortak gate denied this edit",
            "the denial reads as the gate wrote it"
        );
        assert_eq!(both["hookSpecificOutput"]["additionalContext"], msg);
    }

    #[test]
    fn rebuilds_an_edit_from_its_own_input() {
        let out = intended_content(
            b"one\ntwo\nthree\n",
            "Edit",
            &serde_json::json!({"old_string": "two", "new_string": "TWO"}),
        )
        .expect("edit replays");
        assert_eq!(out, b"one\nTWO\nthree\n");
    }

    #[test]
    fn replace_all_reaches_every_occurrence() {
        let out = intended_content(
            b"x\nx\n",
            "Edit",
            &serde_json::json!({"old_string": "x", "new_string": "y", "replace_all": true}),
        )
        .expect("edit replays");
        assert_eq!(out, b"y\ny\n");
    }

    #[test]
    fn multiedit_applies_its_edits_in_order() {
        let out = intended_content(
            b"a\nb\n",
            "MultiEdit",
            &serde_json::json!({"edits": [
                {"old_string": "a", "new_string": "A"},
                {"old_string": "A\nb", "new_string": "A\nB"},
            ]}),
        )
        .expect("edits replay");
        assert_eq!(out, b"A\nB\n");
    }

    #[test]
    fn unmatched_old_string_has_no_snapshot() {
        // The daemon reads the file instead of trusting a guess.
        assert!(intended_content(
            b"one\ntwo\n",
            "Edit",
            &serde_json::json!({"old_string": "missing", "new_string": "x"}),
        )
        .is_none());
    }

    #[test]
    fn detects_codex_from_apply_patch_and_transcript() {
        assert_eq!(
            harness_for(&serde_json::json!({ "tool_name": "apply_patch" })),
            "codex"
        );
        assert_eq!(
            harness_for(&serde_json::json!({
                "transcript_path": "/tmp/.codex/sessions/example.jsonl"
            })),
            "codex"
        );
        assert_eq!(harness_for(&serde_json::json!({})), "claude-code");
    }

    #[test]
    fn extracts_all_paths_from_codex_patch() {
        let patch = "*** Begin Patch\n\
*** Update File: src/main.rs\n\
*** Move to: src/bin/main.rs\n\
*** Add File: tests/smoke.rs\n\
*** Delete File: old.rs\n\
*** End Patch";
        assert_eq!(
            patch_paths(patch),
            vec![
                "old.rs".to_string(),
                "src/bin/main.rs".to_string(),
                "src/main.rs".to_string(),
                "tests/smoke.rs".to_string(),
            ]
        );
    }

    #[test]
    fn codex_patch_targets_whole_files() {
        let input = serde_json::json!({
            "command": "*** Begin Patch\n*** Update File: src/main.rs\n*** End Patch"
        });
        assert_eq!(
            target_paths("apply_patch", &input),
            vec!["src/main.rs".to_string()]
        );
        let ws = Workspace::at(Path::new("/tmp/ortak-project"));
        assert_eq!(
            target_regions(&ws, "src/main.rs", "apply_patch", &input),
            Some(vec![Region {
                start: 1,
                end: WHOLE_FILE,
            }])
        );
    }

    /// A tool call is grouped by the workspace that owns each file, not by the
    /// one the session runs in, and a file outside every workspace is dropped.
    #[test]
    fn groups_targets_by_the_workspace_that_owns_them() {
        let base = std::env::temp_dir().join(format!("ortak-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (one, two) = (base.join("one"), base.join("two"));
        std::fs::create_dir_all(one.join(crate::workspace::ORTAK_DIR)).unwrap();
        std::fs::create_dir_all(two.join(crate::workspace::ORTAK_DIR)).unwrap();
        std::fs::create_dir_all(base.join("neither")).unwrap();

        let paths = vec![
            one.join("src/app.py").to_string_lossy().into_owned(),
            two.join("api.rs").to_string_lossy().into_owned(),
            base.join("neither/stray.txt")
                .to_string_lossy()
                .into_owned(),
            "notes.md".to_string(),
        ];
        let groups = group_by_workspace(Some(&Workspace::at(&one)), &paths);

        assert_eq!(groups.len(), 2, "expected one group per owning workspace");
        assert_eq!(groups[0].0.root, one);
        // The relative path resolves against the session's own workspace.
        assert_eq!(groups[0].1, vec!["src/app.py", "notes.md"]);
        assert_eq!(groups[1].0.root, two);
        assert_eq!(groups[1].1, vec!["api.rs"]);
        std::fs::remove_dir_all(&base).ok();
    }
}
