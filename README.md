<p align="center">
  <img src="assets/ortak-banner.png" alt="ortak: one workspace, many agents">
</p>

ortak lets people and coding agents edit one live checkout without stepping on
the same lines. It journals edits by session and publishes one session's work as
a branch.

## Install

```bash
curl -fsSL https://github.com/yibudak/ortak/raw/main/install.sh | sh
```

The installer downloads the release for your OS and architecture, verifies its SHA-256 checksum, and installs `ortak` to `~/.local/bin`.

Set `ORTAK_INSTALL_DIR` to choose another directory or `ORTAK_VERSION` to pin a release. To build from source:

```bash
cargo install --git https://github.com/yibudak/ortak --locked
```

## Quick start

Run these commands at the root of your Git repository:

```bash
ortak init
ortak daemon --detach
```

The daemon writes its log and PID under `.ortak/`. Stop it with:

```bash
ortak daemon --stop
```

## Connect your agent

Claude Code:

```bash
claude plugin marketplace add yibudak/ortak
claude plugin install ortak@ortak
```

Codex:

```bash
codex plugin marketplace add yibudak/ortak
codex plugin add ortak@ortak
```

Start a new agent session after installing the plugin so the hooks load.

## How it works

Hooks identify the session behind each edit. The daemon records the file and
changed lines in `.ortak`. The gate rejects edits that overlap another active
session. `ortak publish` replays one session's journal onto a clean branch.

## Common commands

| Task | Command |
| --- | --- |
| Show the current session | `ortak whoami` |
| Check daemon and sessions | `ortak status` |
| Read one session's journal | `ortak log --session ortak-2` |
| Find who owns a line | `ortak blame src/publish.rs:120` |
| Message another session | `ortak tell ortak-3 "db.rs is changing" --from ortak-2` |
| Record why code changed | `ortak why ortak-2 src/db.rs "reason"` |
| Check affected callers | `ortak impact ortak-2` |
| Give back or reclaim a file | `ortak release ortak-2 src/db.rs` / `ortak claim ortak-2 src/db.rs` |

Run `ortak --help` or `ortak <command> --help` for the full command list.

## Publish your work

```bash
ortak intent ortak-2 "Fix publish attribution"
ortak publish ortak-2 --dry-run
ortak publish ortak-2 --push
```

Publish uses the session's edits since its previous publish. Run
`ortak publish --help` for stacked branches, exclusions, amendments, and custom
commit messages.

## Working rules

- Keep collaborating sessions in the same checkout. A separate worktree needs its own `ortak init` and daemon.
- Avoid `git checkout`, `switch`, `stash`, `add`, and `commit` while other
  sessions are working. Those commands affect the whole checkout. `git diff`,
  `log`, and `status` are safe.
- Run one daemon per workspace.
- Run `ortak publish` instead of building a task branch from the live working tree.

## Configuration

`ortak init` writes `ortak.toml`, detects the trunk branch, and prints the remote used by `--push`.

Choose another push remote per clone with:

```bash
git config ortak.remote my-fork
```

If your trunk is not `main` or `master`, set `[publish] base_branch` in `ortak.toml`.
