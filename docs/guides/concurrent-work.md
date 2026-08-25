# Share a workspace

Run each agent from the same checkout and keep one daemon attached to that
workspace.

## Before the first edit

Check the daemon and session list:

```bash
ortak status
```

Confirm your identity, then record the task:

```bash
ortak whoami
ortak intent ortak-2 "Add layered cache invalidation"
```

Run `ortak whoami <session-id>` when your harness does not export its session ID
to the shell.

## Work without switching Git state

All sessions see the same files and runtime. Keep these commands out of the
shared checkout:

```text
git add
git branch
git checkout
git commit
git stash
git switch
```

They operate on the repository as a whole and can capture or hide another
session's uncommitted work. You can still inspect the repository with `git
diff`, `git log`, and `git status`.

## Read unexpected changes

Another session may have changed a file since you last opened it. Check the
journal before replacing unfamiliar content:

```bash
ortak log --session ortak-3
ortak blame src/service.rs
```

The output names the agent, active lines, edit age, and task intent.

## Send a handoff

Tell another session when you change a contract it uses:

```bash
ortak tell ortak-3 "parse_config now returns Result<Config>" --from ortak-2
```

Broadcast a workspace-wide warning:

```bash
ortak tell all "database migration is in progress" --from ortak-2
```

Use stdin for text that contains shell syntax or several lines:

```bash
ortak tell ortak-3 --from ortak-2 --stdin <<'EOF'
parse_config now returns Result<Config>.
Update the caller before running the integration test.
EOF
```

The recipient receives the message before its next edit or command. Read the
queue manually with `ortak inbox ortak-3`.

## Check downstream impact

The gate catches nearby line edits in one file. It cannot infer that a renamed
function breaks a caller in another file. Run:

```bash
ortak impact ortak-2
```

ortak scans names defined in the session's active regions and reports other
sessions working in files that reference those names. Treat the result as a
text-match warning and inspect each match.

## Finish the task

```bash
ortak publish ortak-2 --dry-run
ortak publish ortak-2 --push
```

Publish frees the shipped line regions. The journal keeps their historical
ownership for `log` and `blame`.
