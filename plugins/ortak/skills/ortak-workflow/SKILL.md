---
name: ortak-workflow
description: Work in an ortak workspace whose root contains ortak.toml and .ortak. Use for task intent, journal awareness, conflict-gate handling, stop-the-line errors, and publishing completed work.
---

# ortak workflow

Multiple agents and people work in this ortak workspace **at the same time, in the same live directory, without switching branches**. ortak records file changes in its journal. Follow these rules.

## 1. Record your intent when the session starts

Use the session ID from the SessionStart context (`ortak-N`) before editing:

```bash
ortak intent ortak-N "One-sentence task description"
```

ortak uses this intent for the branch and PR title. Other sessions use it to understand your work.

## 2. Do not manage branches or commits with Git

Do not run `git checkout`, `git branch`, `git commit`, or `git stash` while developing in this workspace. Those commands break the single live workspace. ortak handles versioning; edit files without changing Git state. You may run `git diff` and `git log` because they do not change state.

## 3. Respect gate denials

The message "The ortak gate denied this edit" means another session owns an active region that overlaps your target.

- Read the owner's identity and intent. Inspect their changes with `ortak log --session ortak-N` when needed.
- Leave the region untouched and continue with non-conflicting work. The region becomes available after its owner stops editing the file for the configured presence window.
- Do not bypass the denial by writing through Bash with `echo`, `sed`, `tee`, `cat >`, or similar commands.
- If the task cannot proceed without that region, explain the conflict to the user and ask for direction.

## 4. Report unrelated errors (stop-the-line)

Report an error when a command fails for a reason **unrelated to your changes**, such as a schema broken by another module or a traceback from a file you did not touch:

```bash
ortak report ortak-N --command "pytest" "relevant error output"
```

This command stops the line. ortak assigns an owner from the journal and rejects other sessions' edits until the owner fixes the error.

- If your changes caused the error, fix it without reporting it.
- If the line is stopped and the gate denies your edit, do not write code. Read, investigate, or wait.
- If ortak assigns the error to you, fix it before returning to your task. Then run:

```bash
ortak resolved ortak-N
```

## 5. Publish completed work

Run one of these commands when the task is complete or the user asks you to publish:

```bash
ortak publish ortak-N          # create the branch in the local repository
ortak publish ortak-N --push   # push the branch; use the printed tea command to open a PR
```

## Useful commands

```bash
ortak status         # daemon and sessions
ortak log            # recent journal entries and their owners
ortak log --session ortak-N
ortak impact ortak-N # who else is working on files that use what you changed
```

Check `ortak log` before changing unexpected content in a file. Another session may own that change.

Run `ortak impact ortak-N` after changing a function signature, a schema, or anything else another file calls. The gate compares line ranges, so it cannot see that your change at line 10 of one file breaks a call at line 200 of another.
