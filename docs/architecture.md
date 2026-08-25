# Architecture

This page describes the current implementation. [`DESIGN.md`](https://github.com/yibudak/ortak/blob/main/DESIGN.md)
keeps the original design record and may describe work that has not shipped.

## Components

```text
┌─────────────────────────── shared checkout ───────────────────────────┐
│ Claude Code hooks   Codex hooks   OpenCode plugin   human editor      │
│        │                 │               │                │           │
└────────┼─────────────────┼───────────────┼────────────────┼───────────┘
         │ attribution     │               │                │ file events
         ├─────────────────┴───────────────┴────────────────┤
         ▼                                ▼
  .ortak/db.sqlite                 ortak daemon
  sessions, regions,              watcher + heartbeat
  snapshots, messages,                   │
  errors, publish marks                  ▼
         │                       .ortak/shadow/
         └──────────────► shadow Git micro-commits
                                      │
                                      ▼
                               ortak publish
                                      │
                                      ▼
                         project Git branch and push
```

Hook processes and CLI commands open SQLite and the shadow repository directly.
The daemon supplies file watching, attribution processing, startup recovery,
and health heartbeats. No socket or long-running request broker sits between
the hooks and storage.

## Workspace discovery

Every command walks upward from its current directory until it finds `.ortak/`.
The workspace root contains `ortak.toml`; the hidden directory contains runtime
state and stays out of the project repository.

## Storage

### SQLite

SQLite stores:

- harness sessions, generated agent names, status, and task intent
- line regions with current ownership and edit timestamps
- attribution hints and pending snapshots
- journal rows linked to shadow commit IDs
- messages, notes, errors, and publish high-water marks

### Shadow Git

`.ortak/shadow/` is a private Git repository whose worktree points at the live
checkout. `ortak init` captures a baseline. The daemon then records attributed
snapshots and settled human saves as micro-commits.

The shadow repository has its own excludes. It ignores internal state, the real
`.git/`, common dependency directories, project ignore files, and configured
`workspace.ignore` patterns.

## Agent edit path

1. A pre-tool hook resolves the workspace and requesting session.
2. The gate reads open errors and active regions from SQLite.
3. An allowed tool changes the live file.
4. The post-tool hook reconstructs the intended content when its payload
   permits and records an attribution snapshot.
5. The daemon writes the snapshot as a shadow Git commit and updates line
   ownership.

Codex `apply_patch` supplies paths without stable final ranges, so the gate uses
whole-file targets for those calls.

OpenCode's edit tool can use fuzzy source matching. Ortak protects the whole
file when the literal source range is absent.

## Human edit path

The daemon receives filesystem events through `notify`. It waits 1.5 seconds
for a path without an agent snapshot to stay quiet, then journals the result as
the `human` session. The daemon also scans for missed changes after a heartbeat
gap.

## Conflict gate

The deterministic rule gives the first active session in a line region
priority. `gate.margin_lines` widens the protected range and
`gate.presence_minutes` controls its active lifetime.

An enabled LLM arbiter runs only after the gate finds a conflict. It receives the
requester intent and up to five region owners with their ranges, edit ages, and
intents. It does not receive file contents or recent diffs. An unusable response
keeps the deterministic denial.

## Stop-the-line

`ortak report` extracts the first 4,000 characters of the supplied error and
matches it against files edited within `line.blame_lookback_minutes`. A unique
file match owns the fix. Ambiguous cases can reach the LLM arbiter; the reporter
becomes the fallback owner.

While any error remains open, the edit gate admits only sessions responsible
for an open fix. `ortak resolved` clears matching errors and opens the line when
none remain.

## Publish path

Publish reads a session's shadow commits after its previous high-water mark,
replays them onto the configured base tree, and writes the result through an
in-memory Git index. It creates a project-repository branch ref without moving
`HEAD` or the working directory.

The replay excludes other sessions' file content. It can leave out a file that
depends on unshipped work and reports the missing dependency. A successful
publish records a new high-water mark and frees the shipped active regions.

`--push` uses the configured remote and prints a forge-specific pull-request
command. ortak does not create the pull request itself.

## Current boundaries

- Agent edit hooks enforce the gate; human editors receive visibility but no
  pre-save block.
- Shell writes can evade the edit hook. The bundled workflow tells agents not
  to bypass denials.
- Impact analysis matches defined names as text and remains advisory.
- The LLM arbiter handles exception paths and never replaces the deterministic
  core.
