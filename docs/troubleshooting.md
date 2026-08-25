# Troubleshooting

## Workspace not found

```text
ortak workspace not found ... run `ortak init` first
```

Run commands inside the repository that contains `.ortak/`, or initialize the
repository root:

```bash
ortak init
```

## Daemon is not running

Check its heartbeat and log:

```bash
ortak status
tail -n 100 .ortak/daemon.log
```

Start it again:

```bash
ortak daemon --detach
```

The gate cannot trust stale ownership while the watcher is down. Touch a file
again if the startup scan did not recover a change made during the outage.

## Another daemon owns the workspace

`ortak daemon` refuses to start when `.ortak/daemon.pid` names a live process.
Stop that process through ortak:

```bash
ortak daemon --stop
ortak daemon --detach
```

Remove a stale PID file only after confirming that the named process no longer
runs. Two daemons can race on the shadow repository and drop journal entries.

## The gate denied an edit

Read the owner and its task before doing more work:

```bash
ortak blame path/to/file:123
ortak log --session ortak-3
```

Continue outside the protected region, contact the owner, or wait for the
presence window. Do not use shell writes to bypass the hook.

## An error report was declined

The mid-write guard found a named file that another session changed within
`line.mid_write_seconds`. Run the failing command again after the file settles.
Send the owner a message if repeated runs fail while that session works.

## Publish reports no new work

The session has changed nothing since its latest publish. Use the existing
branch, make the next edit, or pass `--all` when you intend to rebuild the full
session history.

## Publish cannot replay a file

The file may depend on another session's unshipped work. Publish and merge that
dependency first, or use `--base` to build a deliberate stack. Do not use
`--exclude` to hide a checkout that is behind its configured base.

Check the warning and compare:

```bash
ortak log --session ortak-2
git diff --stat
```

## Push went to the wrong remote

The local branch already exists even when its push fails. Point ortak at a
writable remote and push that branch:

```bash
git config ortak.remote my-fork
git push my-fork task/ortak-2-example
```

Do not republish the same journal slice.

## Global config breaks every workspace

ortak validates `~/.ortak/config.toml` before it applies workspace overrides.
The error message names the file and invalid field. Fix or move the global file,
then rerun the command.

## Plugin changes do not appear

Agent sessions load hooks and bundled skills at startup. Run `ortak update`,
close the active session, and start a new one.
