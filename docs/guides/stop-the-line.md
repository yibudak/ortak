# Stop the line

Report a failure when a real command exposes a problem caused by another
session's work. ortak assigns an owner and blocks other edits until that owner
resolves the error.

## Report an unrelated error

Include the command and the useful part of its output:

```bash
ortak report ortak-2 \
  --command "cargo test" \
  "error[E0061]: this function takes 2 arguments but 1 was supplied at src/api.rs:48"
```

Fix errors caused by your own changes without reporting them. `report` exists
for failures that another session must handle.

The default mid-write guard declines a report when it names a file another
session wrote within the last 90 seconds. Run the command again after the file
settles. This prevents a half-written file from stopping the workspace.

## Ownership assignment

ortak searches recent session files for paths and names found in the error
output. A unique match owns the fix.

When correlation remains ambiguous:

- the optional LLM arbiter can select a candidate session
- the reporting session owns the error when the arbiter is disabled or fails

The report output names the error number, owner, and assignment reason.

## Work while the line is stopped

The gate rejects edits from sessions that do not own an open error. Those
sessions can inspect code, read logs, or send context to the owner.

```bash
ortak errors
ortak tell ortak-3 "failure reproduces in the parser test" --from ortak-2
```

Reassign a wrong owner:

```bash
ortak assign 7 ortak-4
```

## Resolve the error

The assigned session fixes the failure and reruns the command. It then opens the
line:

```bash
ortak resolved ortak-3
```

Use `--all` only when you intend to clear every open error:

```bash
ortak resolved --all
```

If other errors remain, the line stays stopped and `ortak errors` lists their
owners.
