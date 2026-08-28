use crate::config::Config;
use crate::db::Db;
use crate::regions::{Region, WHOLE_FILE};
use crate::workspace::Workspace;
use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

// Claude Code, Codex, and OpenCode hook adapters read hook event JSON from stdin. They
// must never break the agent's session: callers swallow errors and exit 0.

fn read_stdin_json() -> Result<Value> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

/// The directory the tool call ran in, as the payload gives it. Every path a
/// command writes without naming a root is relative to this one.
fn cwd_for(input: &Value) -> Option<PathBuf> {
    input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
}

fn workspace_for(input: &Value) -> Result<Workspace> {
    match cwd_for(input) {
        Some(cwd) => Workspace::discover(&cwd),
        None => bail!("no working directory in the hook payload"),
    }
}

fn harness_for(input: &Value) -> &'static str {
    if input.get("harness").and_then(Value::as_str) == Some("opencode") {
        return "opencode";
    }
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
    let prefix = match harness {
        "codex" => "codex",
        "opencode" => "opencode",
        _ => "claude",
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
    let agent = agent_name_for(&external_id, harness);
    let id = db.upsert_session(&external_id, &agent, "llm", Some(harness))?;
    // Nothing of this session's is running yet, so a Bash claim in its name is
    // what a harness killed mid-command left behind. It matters because the
    // line above has just stamped the session live again: `--resume` comes back
    // under the same id, and without this the leftover claim starts answering
    // for other people's writes again, until a command of this session's
    // happens to finish and `post-bash` closes it.
    db.drop_bash_claims(id)?;
    // Two things are being said at once here. The no-git rule belongs in this
    // hook rather than only in the skill: a skill loads when the model judges
    // it relevant, and this is the rule people get burned by. Advisory on
    // purpose. Nothing blocks git, because the gate cannot tell a lone
    // session's harmless commit from one that sweeps up another session's
    // uncommitted work. And the session is given both of its names, because the
    // number is the one that moves: `ortak-{id}` is a row id in this
    // workspace's database, reassigned when sessions register in a different
    // order or `.ortak` is rebuilt, and a resumed session carries the old one
    // in its context with no way to notice.
    let context = format!(
        "ortak is active. The journal attributes this session's file changes to ortak-{id} \
         ({agent}). If you are ever unsure which number is yours, `ortak whoami` answers from \
         the harness session id, which does not move. \
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
    let context = match inherited_messages(&db, id)? {
        Some(block) => format!("{context}\n\n{block}"),
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

/// Mail this session is inheriting: sent to a session that has since stopped,
/// and never read.
///
/// The other doors need their recipient to do something. A session that has
/// finished does nothing ever again, so its last message, which is usually the
/// handover, waits in the table for a turn that is not coming. This is the one
/// moment a workspace has a reader who is not busy.
fn inherited_messages(db: &Db, me: i64) -> Result<Option<String>> {
    let waiting = db.take_orphan_messages(me)?;
    if waiting.is_empty() {
        return Ok(None);
    }
    let mut block = String::from(
        "ortak MESSAGES left for sessions that have stopped. They were not addressed to you: \
         the session each went to ended before reading it, and you are the next session here.",
    );
    for m in &waiting {
        block.push_str(&format!(
            "\n- ortak-{} {}, sent to ortak-{}: {}",
            m.from_session, m.from_name, m.to_session, m.text
        ));
    }
    Ok(Some(block))
}

pub fn post_edit() -> Result<()> {
    post_edit_for(&read_stdin_json()?)
}

fn post_edit_for(input: &Value) -> Result<()> {
    let cwd_ws = workspace_for(input).ok();
    let external_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let harness = harness_for(input);
    let tool_name = input
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tool_input = input.get("tool_input").cloned().unwrap_or(Value::Null);
    let mut far = Vec::new();
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
        if !cwd_ws.as_ref().is_some_and(|home| home.root == ws.root) {
            far.push(ws.root.clone());
        }
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
    note_far_workspaces(cwd_ws.as_ref(), &external_id, harness, &far);
    Ok(())
}

/// Write down, in this session's own journal, the workspaces this call
/// registered it in.
///
/// #101 closed this for the Bash door and left the Edit door open. Registering
/// in another workspace hands that journal a session row reading active, and it
/// has no way of ever learning otherwise: `session_end` is handed one
/// workspace, the one the harness ran in. So the roots go where the session
/// lives and are read back on the way out.
///
/// Nothing here is worth failing the hook for. A session that cannot be
/// recorded as having reached somewhere leaves a row that reads active in a
/// workspace it is not working in, which is the old behaviour and not a reason
/// to cost the agent its edit.
fn note_far_workspaces(
    home: Option<&Workspace>,
    external_id: &str,
    harness: &str,
    far: &[PathBuf],
) {
    if far.is_empty() {
        return;
    }
    let Some(home) = home else {
        return;
    };
    let Ok(db) = Db::open(&home.db_path) else {
        return;
    };
    let Ok(me) = db.upsert_session(
        external_id,
        &agent_name_for(external_id, harness),
        "llm",
        Some(harness),
    ) else {
        return;
    };
    for root in far {
        let _ = db.note_reached(me, root);
    }
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
        "Edit" | "OpenCodeEdit" => vec![tool_input],
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
/// in order to say it. A stopped line's owner is told through the same door,
/// and for the same reason: see `pending_context`.
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
            pending_context(&db, me)?
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

/// Everything this session is owed before its next call: the stopped line it
/// owns, then whatever the other sessions sent it. The line comes first because
/// it is the one thing holding up the whole workspace.
///
/// The gate exempts the session responsible for an open error, which is right,
/// because it has to edit to fix the thing. It also leaves that session the
/// only party the stop-the-line never reaches: everybody else is told by the
/// denial, and the notice naming the owner is written for a prompt that a
/// session working through a brief submits once, at the start. So the owner
/// fixes the code without ever learning an error was open, leaves it open
/// behind them, and every other session stays stopped.
fn pending_context(db: &Db, me: i64) -> Result<Option<String>> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(e) = db.take_owner_notice(me)? {
        parts.push(owner_notice(me, &e));
    }
    if let Some(block) = pending_messages(db, me)? {
        parts.push(block);
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts.join("\n\n")))
}

/// What the session that owns a stopped line is told, at a prompt or before an
/// edit. One wording, because the two doors say the same thing.
fn owner_notice(me: i64, e: &crate::db::ErrorRow) -> String {
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
        if targets.is_empty() {
            if let Some(reason) = vanished_target(&db, &cfg, rel, me)? {
                return Ok(Some(reason));
            }
            continue;
        }
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
    let mut abandoned = None;
    if cfg.orchestrator.enabled {
        let my = db.get_session(me)?;
        match crate::orchestrator::conflict_verdict(&db, &cfg.orchestrator, &rel, &my, &conflicts) {
            Ok((true, _)) => return Ok(None),
            Ok((false, message)) => {
                let reason = format!(
                    "The ortak arbiter denied this edit in {}: {} Do not bypass the denial by writing through Bash. Status: ortak log --session ortak-{}",
                    rel, message, conflicts[0].session_id
                );
                return Ok(Some(reason));
            }
            // The session waited for a ruling and did not get one. Falling
            // through here is right, and denying without saying so is what made
            // a timeout, a missing binary and a considered no the same message.
            Err(why) => abandoned = Some(why),
        }
    }

    let mut reason = format!(
        "The ortak gate denied this edit. Your target lines in {} overlap another session's active region.\n",
        rel
    );
    for c in conflicts.iter().take(3) {
        let mins = ((crate::db::now_ts() - c.last_ts) / 60).max(0);
        reason.push_str(&format!(
            "- ortak-{} {} (lines {}-{}, last edit {} min ago), {}\n",
            c.session_id,
            c.agent_name,
            c.start,
            c.end,
            mins,
            crate::db::intent_line(c.intent.as_deref(), c.intent_at)
        ));
    }
    // After the owners, because it is about this denial rather than about them.
    reason.push_str(&abandoned_note(abandoned));
    reason.push_str(&format!(
        "Do not edit this region or bypass the denial through Bash. Continue with non-conflicting work. The region becomes available after its owner has not touched the file for {} min. Inspect the owner's work with: ortak log --session ortak-{}",
        cfg.gate.presence_minutes,
        conflicts[0].session_id
    ));
    Ok(Some(reason))
}

/// What the deterministic denial adds when the arbiter was asked and answered
/// nothing. Empty when it was never asked, which is the ordinary case.
///
/// Without it a session that waited half a minute and was refused reads a
/// message about first-toucher-wins and has no way to know a model was ever
/// involved, let alone that its ruling arrived late and was thrown away.
fn abandoned_note(why: Option<&str>) -> String {
    match why {
        None => String::new(),
        Some(w) => format!(
            "No arbiter ruling was made: {}. This is the deterministic rule, not a decision about your case; `ortak log` records the attempt and what it cost.\n",
            crate::orchestrator::outcome_note(w)
        ),
    }
}

/// Every file path a tool call names, as written in the tool input. An empty
/// result means there is nothing to journal or check.
fn target_paths(tool_name: &str, tool_input: &Value) -> Vec<String> {
    match tool_name {
        "apply_patch" => patch_command(tool_input)
            .map(patch_paths)
            .unwrap_or_default(),
        "Write" | "Edit" | "OpenCodeEdit" | "MultiEdit" | "NotebookEdit" => tool_input
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
/// Why the text an Edit meant to replace is not in the file, when the answer is
/// another session. `None` when nobody is working there, and then the tool's
/// own "String to replace not found" is the whole story.
///
/// The edit cannot land either way, so this denial costs nothing and buys the
/// one thing the tool's own error cannot say: which session rewrote those lines
/// while this one was reading them.
fn vanished_target(db: &Db, cfg: &Config, rel: &str, me: i64) -> Result<Option<String>> {
    let whole = [Region {
        start: 1,
        end: WHOLE_FILE,
    }];
    let hot = db.conflicts(
        rel,
        &whole,
        me,
        cfg.gate.margin_lines,
        cfg.gate.presence_minutes * 60,
    )?;
    let Some(c) = hot.first() else {
        return Ok(None);
    };
    Ok(Some(format!(
        "The text this edit replaces is no longer in {}. ortak-{} {} has been writing there \
         (lines {}-{}, last edit {} min ago), intent: {}\n\
         Read the file again before retrying: as written this edit cannot apply, and the lines \
         it was aimed at are somebody else's work now. Inspect it with: ortak log --session ortak-{}",
        rel,
        c.session_id,
        c.agent_name,
        c.start,
        c.end,
        ((crate::db::now_ts() - c.last_ts) / 60).max(0),
        c.intent.as_deref().unwrap_or("(not reported)"),
        c.session_id
    )))
}

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
        "Edit" | "OpenCodeEdit" => {
            let old = tool_input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let all = tool_input
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let ranges = ranged(ws, rel, &[(old, all)], whole.clone());
            if tool_name == "OpenCodeEdit" {
                // OpenCode may fuzzy-match oldString. If the literal text is
                // absent, protect the file instead of assuming the tool fails.
                Some(ranges.unwrap_or(whole))
            } else {
                ranges
            }
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
    // Empty is not None. A file that is not there has nothing to protect; a
    // file whose old_string is gone has lines somebody may be holding, and who
    // took the text away is the question worth answering.
    Some(targets)
}

/// PostToolUse on Bash: when a command fails and the line is open, nudge the
/// agent to report the error if it looks foreign. Bridges LLM forgetfulness.
/// PreToolUse on Bash: claim whatever the command is about to write. The edit
/// hooks cannot see a shell redirect or an in-place rewrite, so without this the
/// daemon attributes that work to the human and the session publishes nothing.
pub fn pre_bash() -> Result<()> {
    pre_bash_for(&read_stdin_json()?)
}

fn pre_bash_for(input: &Value) -> Result<()> {
    let (home, reached) = bash_sessions(input);
    for (db, me) in home.iter().chain(reached.iter()) {
        // ponytail: the claim covers every unhinted change until the command
        // ends, so a human editor save during a long build lands on this
        // session too. Coarse, but the alternative loses the agent's own work.
        let _ = db.insert_hint(crate::db::BASH_CLAIM, *me, None);
    }
    // Messages after the claims rather than between them, so a workspace that
    // fails on its way to a verdict cannot cost the workspaces behind it the
    // claim they were about to get.
    let Some((db, me)) = home else {
        return Ok(());
    };
    // A session can go a long way between edits while it reads, greps and runs
    // tests, and a message that waits for the next edit can wait that long.
    // Bash is the call an agent makes constantly, so it opens the same door.
    if let Some(out) = pre_tool_use_output(None, pending_context(&db, me)?) {
        println!("{}", out);
    }
    Ok(())
}

/// How many distinct paths out of one command are worth resolving. A command
/// naming more places than this is a batch job, and by then the workspaces it
/// reaches have been named several times over. It is a bound on the work as
/// much as a cap on the answer: dropping repeats is a scan of what is already
/// there, and this hook runs on every Bash call an agent makes.
const COMMAND_PATHS_MAX: usize = 16;

/// Absolute paths a shell command names, resolved against the directory it runs
/// in, in the order they appear and without repeats.
///
/// A word with no separator in it is skipped: a bare filename is in `cwd`, and
/// `cwd`'s own workspace is claimed regardless. Everything else is taken as
/// typed, which is enough for a redirect written against the path, a quoted
/// path and `--out=/x/y`. `$VAR/file` and a path the shell reads from somewhere
/// resolve to nonsense and `Workspace::discover` drops them.
fn command_paths(cwd: &Path, command: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for word in command.split(|c: char| c.is_whitespace() || ";|&<>()\"'`".contains(c)) {
        // `--out=/x/y` and `DIR=/x/y` both carry their path after the sign.
        let word = word.rsplit_once('=').map_or(word, |(_, tail)| tail);
        if !word.contains('/') {
            continue;
        }
        let path = Path::new(word);
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        if !out.contains(&abs) {
            out.push(abs);
        }
        if out.len() == COMMAND_PATHS_MAX {
            break;
        }
    }
    out
}

/// Every ortak workspace one shell command can write to: the one it runs in
/// first, then one for each path in the command text that lands in a different
/// one.
///
/// An edit call carries a file list and a shell command does not, which is the
/// whole reason the Bash hooks claimed in a single workspace while
/// `group_by_workspace` spread an edit across all of them. Three files deleted
/// in another checkout by a session working here were recorded there against
/// the person, with nothing in the row saying it was a guess. This is the same
/// move that function makes, from the only paths a Bash payload carries: its
/// command text. `cd /other/repo && rm x` is covered too, because the claim is
/// on the workspace and not on a file.
///
/// It misses what the shell works out while it runs: `$VAR/file`, a path read
/// out of a file, a `cd` through a variable. Those land where they landed
/// before, on the workspace `cwd` names, so this finds more of a command's work
/// than the old code and never less.
///
/// ponytail: the ceiling is that the command text is not the command. The exact
/// version diffs the tree afterwards and groups what actually changed, at the
/// price of a workspace walk on every Bash call, which is the call an agent
/// makes constantly; it is parked with that price on it. This spends one
/// `Workspace::discover` per distinct path named, capped at
/// `COMMAND_PATHS_MAX`, so a handful of `is_dir` calls and no walk. The other
/// cost is real: a command that only reads another workspace claims it just the
/// same, and one more claimant there is one more way for that workspace's own
/// sessions to end up contested.
fn command_workspaces(home: Option<&Workspace>, input: &Value) -> Vec<Workspace> {
    let mut out: Vec<Workspace> = home.into_iter().cloned().collect();
    let Some(cwd) = cwd_for(input) else {
        return out;
    };
    let command = input
        .get("tool_input")
        .and_then(|t| t.get("command"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    for abs in command_paths(&cwd, command) {
        let Ok(ws) = Workspace::discover(&abs) else {
            continue;
        };
        if !out.iter().any(|w| w.root == ws.root) {
            out.push(ws);
        }
    }
    out
}

/// One workspace's journal and the id this session goes by inside it.
type BashSession = (Db, i64);

/// This session's journal and its id in every workspace one shell command
/// reaches: the workspace it runs in, then the rest. Both Bash hooks walk this
/// list, built from the same command text, so every claim `pre-bash` opens is
/// one `post-bash` closes.
///
/// The workspace the command runs in is kept apart because it is the only one
/// that answers for anything besides the claim: the messages waiting for this
/// session and the nudge after a failed command are its own workspace's
/// business.
///
/// The id is read per workspace on purpose. `sessions` is per workspace and
/// hands out its own numbers, so the id this session holds in one journal names
/// somebody else in the next, and registering here is also what puts the
/// session in that workspace's `ortak status`. A workspace whose journal will
/// not open is dropped in silence: a hook that cannot do its job says nothing,
/// and the workspaces either side of it still get their claim.
fn bash_sessions(input: &Value) -> (Option<BashSession>, Vec<BashSession>) {
    let external_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let harness = harness_for(input);
    let home = workspace_for(input).ok();
    let (mut here, mut reached, mut far) = (None, Vec::new(), Vec::new());
    for ws in command_workspaces(home.as_ref(), input) {
        let Ok(db) = Db::open(&ws.db_path) else {
            continue;
        };
        let Ok(me) = db.upsert_session(
            external_id,
            &agent_name_for(external_id, harness),
            "llm",
            Some(harness),
        ) else {
            continue;
        };
        if home.as_ref().is_some_and(|h| h.root == ws.root) {
            here = Some((db, me));
        } else {
            far.push(ws.root);
            reached.push((db, me));
        }
    }
    // Registering in a far workspace hands it a row that reads active, and that
    // journal has no way of learning the session ended: `session_end` is handed
    // one workspace, the one the harness ran in. So the roots are written down
    // here, where they are in hand, and read back on the way out.
    if let Some((db, me)) = &here {
        for root in &far {
            let _ = db.note_reached(*me, root);
        }
    }
    (here, reached)
}

pub fn post_bash() -> Result<()> {
    post_bash_for(&read_stdin_json()?)
}

fn post_bash_for(input: &Value) -> Result<()> {
    // The command has finished, but what it wrote has not reached the daemon
    // yet: an unhinted change waits out a quiet window first. Closing the claim
    // stamps it rather than deleting it, so it still answers for that window.
    // Before the early returns below, so a failing command closes it too, and
    // over every workspace the command reached, so one that wrote into three of
    // them leaves none of the three holding an open claim.
    let (home, reached) = bash_sessions(input);
    for (db, me) in home.iter().chain(reached.iter()) {
        let _ = db.clear_bash_claim(*me);
    }
    let Some((db, me)) = home else {
        return Ok(());
    };

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
    // Read, never taken: a person who types deserves the status of the line
    // whether or not their session has already been handed the assignment.
    let context = if let Some(e) = mine.first() {
        owner_notice(me, e)
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
    session_end_for(&read_stdin_json()?)
}

fn session_end_for(input: &Value) -> Result<()> {
    let ws = workspace_for(input)?;
    let db = Db::open(&ws.db_path)?;
    let Some(external_id) = input.get("session_id").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    // The workspaces this session's commands reached hold a row for it too, and
    // they are ended first so that nothing about them can stop the one that
    // matters. Nothing includes reading the list: a `?` here would hand the far
    // workspaces the power to cost this session its own ending, which is the
    // one thing this hook exists to do. A far workspace that has been deleted,
    // unmounted or had its `.ortak` wiped is passed over in silence, and a
    // journal that is gone is never rebuilt on the way out: `Db::open` would
    // create the file.
    for root in db.reached_roots(external_id).unwrap_or_default() {
        let far = Workspace::at(Path::new(&root));
        if !far.db_path.exists() {
            continue;
        }
        if let Ok(far_db) = Db::open(&far.db_path) {
            let _ = far_db.end_session(external_id);
        }
    }
    db.end_session(external_id)?;
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

    /// Three denials that used to be one sentence: the arbiter said no, the
    /// arbiter could not be started, and the arbiter answered a second late.
    /// Only the first is a decision, and the session cannot act on the other
    /// two without being told which it got.
    #[test]
    fn a_denial_nobody_ruled_on_says_which_silence_it_was() {
        assert!(abandoned_note(None).is_empty(), "never asked, say nothing");
        let late = abandoned_note(Some("timed-out"));
        assert!(late.contains("No arbiter ruling was made"), "{late}");
        assert!(late.contains("ran out of time"), "{late}");
        assert!(late.contains("not a decision about your case"), "{late}");
        assert!(
            abandoned_note(Some("spawn-failed")).contains("would not start"),
            "each outcome reaches the session in its own words"
        );
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
    fn detects_opencode_from_the_explicit_adapter_marker() {
        let input = serde_json::json!({ "harness": "opencode", "tool_name": "apply_patch" });
        assert_eq!(harness_for(&input), "opencode");
        assert_eq!(
            agent_name_for("session-123456789", "opencode"),
            "opencode-session1"
        );
    }

    #[test]
    fn opencode_fuzzy_edits_fall_back_to_whole_file_protection() {
        let input = serde_json::json!({
            "file_path": "/tmp/ortak-missing.rs",
            "old_string": "text OpenCode may fuzzy match",
            "new_string": "replacement"
        });
        let ws = Workspace::at(Path::new("/tmp/ortak-project"));
        assert_eq!(
            target_regions(&ws, "missing.rs", "OpenCodeEdit", &input),
            Some(vec![Region {
                start: 1,
                end: WHOLE_FILE,
            }])
        );
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

    /// The paths a command hands over, and the ones it does not. A word with no
    /// separator in it names a file in `cwd`, and that workspace is claimed
    /// whatever the command says.
    #[test]
    fn a_command_gives_up_the_paths_it_names() {
        let cwd = Path::new("/work/repo");
        assert_eq!(
            command_paths(
                cwd,
                "printf hi >/other/repo/out.txt; cp -r sub/tree --dest=/third/repo/x /other/repo/out.txt",
            ),
            vec![
                PathBuf::from("/other/repo/out.txt"),
                PathBuf::from("/work/repo/sub/tree"),
                PathBuf::from("/third/repo/x"),
            ],
            "a redirect written against its path, a relative path, a flag's value, and no repeats"
        );
        assert!(command_paths(cwd, "cargo test --locked").is_empty());
    }

    /// A command that writes into another checkout is claimed there too. Three
    /// files deleted in `/opt/odoo/v16` by a session whose workspace was
    /// somewhere else went into that journal against the person, with nothing
    /// in the row saying anybody had guessed.
    #[test]
    fn a_command_reaching_another_workspace_claims_the_write_there() {
        let base = std::env::temp_dir().join(format!("ortak-hooks-reach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (mine, theirs) = (base.join("mine"), base.join("theirs"));
        for root in [&mine, &theirs] {
            std::fs::create_dir_all(root.join(crate::workspace::ORTAK_DIR)).unwrap();
        }
        std::fs::create_dir_all(base.join("loose")).unwrap();
        // The two journals have to number this session differently, or a claim
        // written with the wrong workspace's id would pass unnoticed.
        let their_db = Db::open(&Workspace::at(&theirs).db_path).unwrap();
        their_db.ensure_human().unwrap();
        their_db
            .upsert_session("someone-else", "claude-else", "llm", None)
            .unwrap();

        let payload = |response: Value| {
            serde_json::json!({
                "cwd": mine.to_str().unwrap(),
                "session_id": "sess-reach",
                "tool_input": { "command": format!(
                    "rm -f {}/api.rs {}/loose/file.txt", theirs.display(), base.display()) },
                "tool_response": response,
            })
        };
        pre_bash_for(&payload(Value::Null)).unwrap();

        let my_db = Db::open(&Workspace::at(&mine).db_path).unwrap();
        let mine_id = my_db.resolve_session("sess-reach").unwrap().id;
        let theirs_id = their_db.resolve_session("sess-reach").unwrap().id;
        assert_ne!(mine_id, theirs_id, "the journals number sessions apart");
        assert_eq!(
            their_db.peek_hint("api.rs", 30 * 60).unwrap(),
            Some((theirs_id, crate::db::Attribution::Claim)),
            "the workspace written into names the session that wrote"
        );
        assert_eq!(
            my_db.peek_hint("src/x.rs", 30 * 60).unwrap(),
            Some((mine_id, crate::db::Attribution::Claim)),
            "and the workspace the command ran in still does"
        );

        // Whatever `pre-bash` opened, `post-bash` has to close, and it gets
        // there from the same command text. A path in no workspace is dropped
        // by both.
        let after = payload(serde_json::json!({ "exit_code": 0 }));
        assert_eq!(
            command_workspaces(workspace_for(&after).ok().as_ref(), &after)
                .into_iter()
                .map(|w| w.root)
                .collect::<Vec<_>>(),
            vec![mine.clone(), theirs.clone()],
            "its own workspace first, and a path in no workspace dropped"
        );
        post_bash_for(&after).unwrap();
        // Closed, and still inside its grace, so it answers for what the
        // command wrote a moment ago.
        assert_eq!(
            their_db.peek_hint("api.rs", 30 * 60).unwrap(),
            Some((theirs_id, crate::db::Attribution::Claim))
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// A session that reached another workspace is ended there too. #95 gives
    /// it a row in every workspace its commands name, and `cat /other/repo/x`
    /// names one, so those journals were left calling it active for as long as
    /// they live: nothing but the session's own journal knows where they are.
    #[test]
    fn a_session_ends_in_every_workspace_it_reached() {
        let base = std::env::temp_dir().join(format!("ortak-hooks-ends-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (mine, theirs, untouched) = (
            base.join("mine"),
            base.join("theirs"),
            base.join("untouched"),
        );
        for root in [&mine, &theirs, &untouched] {
            std::fs::create_dir_all(root.join(crate::workspace::ORTAK_DIR)).unwrap();
        }
        // A third workspace this session is registered in and never named. It
        // has to be left alone: the list is of workspaces reached, not of every
        // journal that happens to hold this session.
        let elsewhere = Db::open(&Workspace::at(&untouched).db_path).unwrap();
        elsewhere
            .upsert_session("sess-ends", "claude-ends", "llm", None)
            .unwrap();

        let input = serde_json::json!({
            "cwd": mine.to_str().unwrap(),
            "session_id": "sess-ends",
            "tool_input": { "command": format!("cat {}/api.rs", theirs.display()) },
        });
        pre_bash_for(&input).unwrap();
        let their_db = Db::open(&Workspace::at(&theirs).db_path).unwrap();
        assert_eq!(
            their_db.resolve_session("sess-ends").unwrap().status,
            "active",
            "reading a path over there is enough to be registered there"
        );

        session_end_for(&input).unwrap();
        assert_eq!(
            their_db.resolve_session("sess-ends").unwrap().status,
            "done"
        );
        let my_db = Db::open(&Workspace::at(&mine).db_path).unwrap();
        assert_eq!(my_db.resolve_session("sess-ends").unwrap().status, "done");
        assert_eq!(
            elsewhere.resolve_session("sess-ends").unwrap().status,
            "active",
            "a workspace the session never named is not touched"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// The same leak through the other door. #101 recorded the workspaces a
    /// shell command reached; an Edit whose path lands in another workspace
    /// registers a session there too, and that row read active for as long as
    /// the journal lived.
    #[test]
    fn an_edit_into_another_workspace_ends_there_too() {
        let base = std::env::temp_dir().join(format!("ortak-hooks-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (mine, theirs) = (base.join("mine"), base.join("theirs"));
        for root in [&mine, &theirs] {
            std::fs::create_dir_all(root.join(crate::workspace::ORTAK_DIR)).unwrap();
        }
        let their_file = theirs.join("api.rs");
        std::fs::write(&their_file, "pub fn go() {}\n").unwrap();

        let edit = serde_json::json!({
            "cwd": mine.to_str().unwrap(),
            "session_id": "sess-edit",
            "tool_name": "Edit",
            "tool_input": {
                "file_path": their_file.to_str().unwrap(),
                "old_string": "go",
                "new_string": "went",
            },
        });
        post_edit_for(&edit).unwrap();
        let their_db = Db::open(&Workspace::at(&theirs).db_path).unwrap();
        assert_eq!(
            their_db.resolve_session("sess-edit").unwrap().status,
            "active",
            "editing a file over there is enough to be registered there"
        );

        let ending = serde_json::json!({
            "cwd": mine.to_str().unwrap(),
            "session_id": "sess-edit",
        });
        session_end_for(&ending).unwrap();
        assert_eq!(
            their_db.resolve_session("sess-edit").unwrap().status,
            "done"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// An Edit whose `old_string` has been rewritten under it has no target to
    /// protect, so the gate used to skip the file in silence and let the tool
    /// fail with "String to replace not found". Who took the text is the
    /// question, and only the journal can answer it.
    #[test]
    fn a_target_that_vanished_names_the_session_that_took_it() {
        let path = std::env::temp_dir().join(format!(
            "ortak-hooks-vanished-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        let cfg = Config::default();
        let me = db
            .upsert_session("mine", "claude-mine", "llm", None)
            .unwrap();
        let them = db
            .upsert_session("theirs", "claude-theirs", "llm", None)
            .unwrap();

        // Nobody else is working here: the tool's own error is the whole story.
        assert!(vanished_target(&db, &cfg, "src/x.rs", me)
            .unwrap()
            .is_none());

        let hunk = crate::regions::Hunk {
            old_start: 20,
            old_lines: 1,
            new_start: 20,
            new_lines: 1,
        };
        db.apply_edit_regions(them, "src/x.rs", &[hunk], None)
            .unwrap();
        db.insert_edit(them, "src/x.rs", "modify", None, &[hunk], None)
            .unwrap();

        let said = vanished_target(&db, &cfg, "src/x.rs", me)
            .unwrap()
            .expect("the other session is named");
        assert!(said.contains("claude-theirs"), "{said}");
        assert!(said.contains("src/x.rs"), "{said}");
        // Nobody is told they are in their own way.
        assert!(vanished_target(&db, &cfg, "src/x.rs", them)
            .unwrap()
            .is_none());
    }
}
