# CLI commands

Run `ortak <command> --help` for the complete argument list installed with your
version.

## Set up and maintain

| Command | Purpose |
| --- | --- |
| `ortak init` | Initialize the current repository as an ortak workspace. |
| `ortak daemon` | Run the watcher in the foreground. |
| `ortak daemon --detach` | Start the watcher in the background. |
| `ortak daemon --stop` | Stop the daemon named by `.ortak/daemon.pid`. |
| `ortak doctor` | Check whether this workspace can publish, and say what to fix. Exits non-zero when a check fails. |
| `ortak doctor --json` | Emit the same report for a script. |
| `ortak update` | Update stale binary and installed Claude Code, Codex, and OpenCode integrations. |
| `ortak opencode install` | Install the global OpenCode plugin and workflow skill. |

## Observe the workspace

| Command | Purpose |
| --- | --- |
| `ortak status` | Show daemon health, sessions, protected regions, messages, and journal warnings. |
| `ortak status --json` | Emit machine-readable status. |
| `ortak sessions` | List sessions, intents, edit counts, and published branches. |
| `ortak whoami [session-id]` | Resolve a harness session ID to its current `ortak-N`. |
| `ortak log` | Show the 20 most recent journal entries, including any arbiter rulings. |
| `ortak log --session ortak-2 --limit 50` | Filter and extend the journal view. A ruling matches on either side of it. |
| `ortak log --json` | Emit machine-readable journal rows. Edits only. |
| `ortak blame <file>[:line]` | Show current and historical ownership for a file or line. |
| `ortak impact ortak-2` | Find active files that reference names changed by a session. |

## Coordinate sessions

| Command | Purpose |
| --- | --- |
| `ortak intent ortak-2 "task"` | Record the session's current task. |
| `ortak tell ortak-3 "message" --from ortak-2` | Send a message to one session. Use `all` to broadcast. |
| `ortak tell ... --stdin` | Read the message body from stdin. |
| `ortak inbox ortak-3` | Read messages addressed to a session. |
| `ortak why ortak-2 <file> "reason"` | Attach a reason to the session's owned lines in a file. |
| `ortak why <file>[:line]` | Read recorded reasons. |
| `ortak release ortak-2 <file>` | Remove one file's regions and journal rows from the session. |
| `ortak release ortak-2 --all` | Release everything held by the session. |
| `ortak claim ortak-2 <file>` | Transfer one file's journal rows and regions to the session. |

## Stop and reopen the line

| Command | Purpose |
| --- | --- |
| `ortak report ortak-2 --command "cmd" "output"` | Record an unrelated failure, assign an owner, and stop edits. |
| `ortak errors` | List recent error records and current owners. |
| `ortak errors --json` | Emit machine-readable error rows. |
| `ortak assign <error-id> ortak-3` | Reassign an open error. |
| `ortak resolved ortak-3` | Resolve the errors one session owns or reported. |
| `ortak resolved --all` | Resolve every open error. |

## Publish

```text
ortak publish [OPTIONS] <SESSION>
```

| Option | Purpose |
| --- | --- |
| `--branch <name>` | Choose the branch name. |
| `--exclude <path>` | Keep a workspace-relative file or directory out. Repeat as needed. |
| `--base <branch>` | Build on another base for this run. |
| `-m, --message <subject>` | Override the commit subject. |
| `--all` | Include the session's complete journal history. |
| `--amend` | Rebuild or extend the named branch. Conflicts with `--all`. |
| `--push` | Push the branch and print the forge-specific PR command. |
| `--dry-run` | Rehearse without creating a branch or publish record. |
| `--squash` | Ship a file whose history cannot be replayed as one net change. |

The default mode includes edits after the session's latest publish.

## Hook commands

`ortak hook ...` is the adapter surface for Claude Code, Codex, and OpenCode.
Plugins call these commands with event JSON on stdin:

```text
session-start
pre-edit
post-edit
pre-bash
post-bash
prompt-context
session-end
```

Run them through the maintained plugins. Their JSON contract can change with a
release.
