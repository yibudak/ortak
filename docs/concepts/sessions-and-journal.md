# Sessions and journal

The journal connects a harness session, its task intent, and each edit it makes.

## Session identity

Each registered agent receives a workspace-local name such as `ortak-2`. The
number comes from the workspace database and can change after `.ortak/` is
rebuilt or sessions register in another order.

Use the harness session ID as the stable handle:

```bash
ortak whoami
ortak whoami <session-id>
```

The generated agent label, such as `claude-a1b2c3d4`, `codex-a1b2c3d4`, or
`opencode-a1b2c3d4`, helps people distinguish sessions in status and gate
messages.

## Task intent

Record an intent before editing:

```bash
ortak intent ortak-2 "Repair publish attribution"
```

The gate shows this sentence to a session that reaches an owned region. Publish
also uses it for the default branch slug and commit subject.

Keep one sentence broad enough to describe the active task. Pass
`ortak publish -m "..."` when a later deliverable from the same session needs a
more specific commit subject.

## Edit attribution

Agent hooks rebuild supported edit operations from their tool input and record
an attribution hint. The daemon writes each snapshot as its own shadow Git
micro-commit. This separates two agent writes even when they reach the same file
close together.

The file watcher waits for an unmatched save to settle, then attributes it to
the `human` session. A startup scan also records changes made while the daemon
was offline as human work.

## Read the journal

```bash
ortak log
ortak log --session ortak-2 --limit 50
ortak blame src/publish.rs
ortak blame src/publish.rs:120
```

`log` lists journal entries. `blame` reports the session that owns each active
line region and marks ownership repaired through `claim` rather than written by
that session.

Record a reason when code alone cannot explain a choice:

```bash
ortak why ortak-2 src/publish.rs "replay starts from base so concurrent disk edits stay out"
ortak why src/publish.rs
```

## Repair ownership

Release work that should not belong to a session:

```bash
ortak release ortak-2 src/generated.rs
ortak release ortak-2 --all
```

Claim a whole file when the journal credited it to the wrong session:

```bash
ortak claim ortak-2 src/config.rs
```

`release` removes the matching journal rows from that session's future publish.
`claim` transfers the file's rows and regions, then tells the previous owners.
