# How ortak works

ortak treats one checkout as the shared source of truth. Agent hooks and a file
watcher feed an edit journal; commands read that journal to coordinate work and
build isolated branches.

## The runtime path

```text
agent edit request
        │
        ▼
PreToolUse gate ── deny when another active region overlaps
        │ allow
        ▼
file changes in the shared checkout
        │
        ├── PostToolUse attributes an agent snapshot
        └── file watcher attributes unmatched saves to the human
        │
        ▼
SQLite index + shadow Git micro-commit
        │
        ▼
ortak publish replays one session onto the base branch
```

The hooks open the workspace database directly. The daemon watches files,
maintains a heartbeat, and converts attributed snapshots or human saves into
journal entries. ortak does not use a local socket.

## Gate decisions

The gate runs before supported edit tools. It checks two conditions in order:

1. An open error has stopped the line and another session owns the fix.
2. The requested lines overlap another session's active region, including the
   configured line margin.

The first session to touch a region keeps priority under the deterministic
rule. The optional LLM arbiter may allow an edit when the two task intents show
that the work can proceed independently.

The gate protects agent tool calls. A human editor can still save a file, so
the daemon records those saves and makes the new ownership visible to later
agent calls.

## Publish isolation

The live file may contain edits from several sessions. `ortak publish` does not
copy that file from disk. It replays the selected session's shadow commits onto
the configured base branch and writes a branch reference without checking out
the branch.

This design keeps another session's uncommitted work out of the published tree.
It also lets the workspace keep running with one code state while Git stores
each finished task separately.

## Failure behavior

ortak favors deterministic fallbacks:

- a failed or invalid arbiter response leaves the ordinary gate denial in place
- ambiguous error ownership falls back to the reporting session
- a failed publish leaves the live checkout untouched
- a stopped daemon produces warnings because the gate lacks current journal data

Use [`ortak status`](../reference/commands.md#observe-the-workspace) to check the
daemon, journal failures, active sessions, and protected regions.
