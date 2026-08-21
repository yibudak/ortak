# ortak Design Document

**Status:** Draft from initial design work | **Date:** 2026-08-17

Summary: ortak gives concurrent agents and people **one live workspace** for a project. It records edits in a journal, derives presence from those edits, arbitrates conflicts, and publishes work to Git and Forgejo when a task ends.

---

## 1. Problem

Git serves asynchronous work by one developer. Agentic coding puts several agents and people in the same project **at the same time**. Branch and worktree isolation creates two failures in that setting:

1. **No shared awareness.** Agents in isolated worktrees cannot see each other's intent, edits, or API and contract decisions. Teams discover conflicts during merge, after both sides have completed the work. A merge conflict is a delayed syntactic symptom of an earlier coordination failure.

2. **Broken code-state contracts.** Stateful runtimes such as Odoo with PostgreSQL derive schemas, registries, and views from code. Switching branches rolls back code while the runtime state remains tied to the previous branch. The system then runs code against incompatible state. Git assumes that code represents the whole project, which does not hold for these runtimes.

Git lacks a **coordination plane**.

## 2. Principles

1. **One live workspace and one runtime.** Development uses no branches or worktrees. Everyone edits the same directory and runs the same system. Code and runtime state stay aligned because the workspace has one code state. ortak derives branches from the journal during publication.

2. **Optimistic concurrency without claims or locks.** Agents discover their edit footprint while they work. ortak lets them proceed, detects overlap, and resolves conflicts when they occur.

3. **Observation instead of declarations.** Sessions do not claim files. ortak derives presence from the edit stream. This removes a coordination step that an LLM could forget. Each session declares one item: its task intent, such as "Ticket #87: implement X."

4. **Stop the line and assign the fix.** ortak stops edits when someone reports a broken state. The journal identifies the session that changed the relevant code. That session fixes the error, runs `resolved`, and reopens the line.

5. **Deterministic core with an LLM for exceptions.** The daemon decides routine edits in milliseconds. The arbiter LLM runs for conflicts and ambiguous error ownership. It stays outside the edit hot path.

6. **Git as storage and interchange.** ortak uses Git as the storage engine for shadow history and as the protocol for Forgejo, PRs, and CI. Developers and agents do not use Git commands during development.

## 3. Components

```
┌──────────────────────────── one live workspace ───────────────────────────┐
│  agent A (Claude Code)   agent B (Claude Code)   person (editor)          │
│        │ hooks                 │ hooks                │ fswatch            │
└────────┼───────────────────────┼──────────────────────┼────────────────────┘
         v                       v                      v
   ┌──────────────────────────────────────────────────────────┐
   │                       ortak daemon                       │
   │ journal (shadow Git + SQLite), presence, gate, line      │
   └───────┬──────────────────────────────┬───────────────────┘
           │ exceptions                   │ publish
           v                              v
   arbiter LLM                     Git branch -> Forgejo PR (tea)
```

### 3.1 Daemon

The daemon runs as a local background process and communicates through a Unix socket. It handles:

- **Journal:** Records each edit as `(session, file, hunks, diff, timestamp)`.
- **Presence:** Derives the line ranges each session touched and the time of its last edit.
- **Gate:** Checks for conflicts before an edit and rejects edits blocked by the line or an arbiter ruling.
- **Line:** Stores open or stopped state, error records, and ownership assignments.
- **Publish:** Converts a session's net change into a branch and sends it to Forgejo.

### 3.2 Storage: SQLite and shadow Git

- **SQLite:** Stores sessions, task intents, errors, arbiter rulings, and the edit index.
- **Shadow Git repository:** Records each edit as a micro-commit in hidden workspace history. The commit trailer contains the session ID. This gives ortak three useful Git operations:
  - Git handles hunk math, line movement, diffs, and blame.
  - A session can revert one edit without touching unrelated work.
  - Publishing collects one session's micro-commits on top of the base branch.

### 3.3 Harness adapters

Adapters stay thin. The daemon owns coordination logic. The Claude Code adapter maps events as follows:

| Event | Action |
|---|---|
| `SessionStart` hook | Register the session and collect its task intent. |
| `PreToolUse` (Edit/Write) hook | Ask the gate before an edit. Block denied edits with a reason. This hook enforces the decision. |
| `PostToolUse` (Edit/Write) hook | Add an attribution hint for the edit. |
| `PostToolUse` (Bash) hook | Remind the agent to report a failed command when another session appears responsible. |
| Plugin MCP tools | Expose `report_error`, `resolved`, `publish`, and `status` for deliberate actions. |
| Plugin skill | Teach task intent, unrelated-error reporting, gate handling, and publishing. |

Each harness uses a thin adapter. The daemon and data model stay the same.

The Codex plugin reuses the lifecycle hooks and workflow skill. Codex exposes `apply_patch` as an `Edit` and `Write` matcher alias; the adapter extracts every patch path and journals each resulting file event.

### 3.4 File watcher (person adapter)

Editor saves do not pass through harness hooks. The daemon's file watcher records them under the `human` session. People participate in the same journal. The gate can show a notification but cannot block editor writes.

### 3.5 Arbiter LLM

The daemon calls a fast model such as Haiku in two cases:

1. **Conflict arbitration:** Two sessions have overlapping hunks. The model receives both task intents and recent diffs from the contested file. It returns structured JSON with the session that may proceed, the session that must wait, the fate of prior contested edits, and a message for each side.
2. **Error ownership:** Journal correlation cannot identify one session after an error report. The model receives the error output and edits from the suspect time window. It returns the responsible session and a fix brief.

The daemon enforces each ruling through the gate. Sessions receive instructions instead of a request to cooperate.

## 4. Data model (draft)

```sql
sessions (
  id, agent_name, kind,           -- llm | human
  harness,                        -- claude-code | fswatch | ...
  task_intent,                    -- free-form task intent
  status,                         -- active | waiting | fixing | done
  started_at, ended_at
)

edits (
  id, session_id, file,
  hunks_json,                     -- [{start, end}] in original coordinates
  shadow_commit,                  -- micro-commit hash in shadow Git
  ts
)

errors (
  id, reporter_session, command, output_excerpt,
  status,                         -- open | fixing | resolved
  culprit_session,                -- null while assignment is pending
  ts_opened, ts_resolved
)

rulings (
  id, kind,                       -- conflict | blame
  input_refs_json, verdict_json, ts
)
```

## 5. Flows

### 5.1 Session start

The agent starts -> the `SessionStart` hook registers it -> the plugin asks for task intent -> the daemon returns current line state and hot files.

### 5.2 Edit gate

An edit request reaches `PreToolUse`, then the daemon checks:

1. The line is stopped and another session owns the error: **deny** with the error summary and owner.
2. The target range overlaps or sits within the configured margin of another active session's current hunks: run the conflict flow in section 5.3.
3. No rule blocks the edit: **allow**.

### 5.3 Conflict arbitration

The daemon detects overlap, calls the arbiter, and rejects the losing edit with a reason. A ruling can preserve, revert, or queue contested edits that the waiting session wrote before the conflict. When the owner's presence window expires, the daemon tells the waiting session that the region is available.

### 5.4 Error reports and stop-the-line

ortak runs no central health command or scheduled check. A session reports an error after a real command exposes it.

1. An agent receives an error unrelated to its changes and calls `report_error(command, output)`. The Bash hook reminds the agent after a failed command.
2. The daemon stops the line. It correlates file paths from the stack trace with edits since the last known good state. The arbiter decides when correlation remains ambiguous. The daemon treats the reporter's ownership claim as untrusted.
3. The assigned session receives a fix brief. The gate rejects edits from other sessions.
4. The owner calls `resolved`. The daemon reopens the line. A repeated report stops it again and updates the assignment.

The MVP stops the whole workspace. A later layer can stop a smaller affected region.

### 5.5 Publish

`publish` collects a session's micro-commits from shadow history, squashes the net change onto the base, creates a branch, pushes to Forgejo, and opens a PR through `tea`. The PR description uses the task intent and reasons from the journal. If one session built on another session's changes, the PR records the dependency and merge order.

The gate prevents concurrent work in the same line region, so session diffs remain separate enough for end-of-task branch extraction.

## 6. Conflict definition

**A conflict exists when two active sessions have hunks in the same file whose current line ranges overlap or fall within N lines of each other. N defaults to 3.**

- The journal already contains hunks from each diff, which gives ortak line-region granularity.
- The margin catches edits to adjacent lines in the same function.
- Shadow Git maps old hunk coordinates to the current file state.
- A region stays active while its session remains open and touched the file within T minutes. T defaults to 30 minutes. Closing the session or exceeding the presence window removes its hunks from conflict checks.
- Concurrent edits to distant lines in the same file do not conflict.

## 7. Layered roadmap

Each layer extends a working end-to-end product.

**Layer 0: Black box and publish (MVP). Implemented.**

The daemon, shadow Git journal, Claude Code and Codex registration and attribution hooks, file watcher, and `publish` command produce Forgejo PRs. This layer supports concurrent tasks in one workspace and runtime without branch switching or code-state mismatch. Each task ends with a separate PR.

**Layer 1: Gate and deterministic priority. Implemented.**

The PreToolUse gate detects line-region conflicts. The first session to touch a region has priority, and the gate gives later sessions a reason to wait. A startup scan records changes made while the daemon was stopped under the human session. The gate sees Edit and Write tools. The skill prohibits file writes through Bash because hooks cannot enforce those yet.

**Layer 2: Stop-the-line. Implemented.**

`report_error` and `resolved`, the Bash reminder, journal-based ownership, and a global line stop keep one session responsible for each error. Assignment order is file correlation, arbiter when enabled, then reporter ownership. `ortak assign` supports manual reassignment. UserPromptSubmit injects line state into each session.

**Layer 3: Arbiter LLM. Implemented.**

The arbiter makes intent-aware conflict decisions and assigns ambiguous errors. It runs `claude -p --model haiku` as a subprocess and stays disabled by default under `[orchestrator]`. Errors, timeouts, invalid JSON, and unknown session IDs fall back to deterministic rules. The subprocess runs from a temporary directory so ortak's hooks do not trigger inside it. Measured latency is about 10 to 15 seconds per ruling on the exception path.

**Layer 4 and later**

Regional line stops; `interface_change` events; cross-task contract and dependency detection; a PR dependency graph; symbol or AST granularity; more harness adapters; agent negotiation; `ortak why <file>:<line>` through shadow blame; journal export to an orphan branch; transcript locations in edit hints.

## 8. Open questions

1. Which defaults should ortak use for the N-line conflict margin and T-minute presence window?
2. Which policy should govern edits that a waiting session wrote before a ruling: preserve, revert, or queue for reapplication?
3. Should the owner's `resolved` claim reopen the line, or should the reporter reproduce the command first?
4. Can one session own several tasks, or should the session-to-task mapping remain one-to-one?
5. Which model and JSON schema should the arbiter use?
6. Does save-level diffing capture enough detail for editor changes, or does ortak need an LSP or editor integration?
7. Which additional documentation needs English localization before an open-source release?
