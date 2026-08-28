# Handle conflicts

The gate denies an edit when its target overlaps another session's active line
region or falls within the configured margin.

## Read the denial

A denial names:

- the file and protected line range
- the owning `ortak-N` session and agent label
- the owner's task intent and last edit age

Inspect the owner when you need more context:

```bash
ortak log --session ortak-3
ortak blame src/config.rs:180
```

## Continue outside the region

Leave the protected lines unchanged and work on independent files or sections.
The gate uses line regions, so another part of the same file may remain open.

Do not bypass the denial through shell writes such as `sed -i`, redirection,
`tee`, or generated patches. Those writes skip the agent edit gate and can
overwrite live work.

## Coordinate with the owner

Send a short dependency or handoff message:

```bash
ortak tell ortak-3 "I need the Config fields after your rename lands" --from ortak-2
```

The region becomes available after its owner stops touching the file for
`gate.presence_minutes`, closes the session, releases the file, or publishes
the work.

## Release or reclaim a file

The current owner can remove a file from its work:

```bash
ortak release ortak-3 src/config.rs
```

Use `claim` only to repair wrong attribution:

```bash
ortak claim ortak-2 src/config.rs
```

The command transfers the file's journal rows and informs previous owners.

## Optional LLM arbitration

Enable the orchestrator when task intent can resolve exceptions that line
overlap alone cannot:

```toml
[orchestrator]
enabled = true
command = "claude"
model = "haiku"
timeout_secs = 20
```

For a conflict, the arbiter receives the file, requester intent, and up to five
active owners with their regions and intents. It returns an `allow` or `deny`
decision. Spawn failures, timeouts, invalid JSON, and unclear output fall back
to the deterministic denial.

Every call is recorded, whether or not it produced a decision, and the rows
appear in `ortak log` beside the journal:

```
[03-11 14:22:07] arbiter conflict on src/db.rs: allow for ortak-4 claude-be11 over ortak-3, haiku 8812ms - the owner has moved on to the README
[03-11 14:31:55] arbiter conflict on src/db.rs: no ruling for ortak-4 claude-be11 over ortak-3, haiku 20041ms (the arbiter ran out of time)
```

An allow leaves no other trace: the hook prints nothing and the edit proceeds
exactly as an uncontested one would. The second line is the case worth reading
for, because a fallback denies the edit with the same wording as a denial the
arbiter reasoned about. `ortak log --session ortak-3` shows both sides of a
ruling, so the owner whose region was defended can see that it was.

See [Configuration](../reference/configuration.md#orchestrator) for global and
workspace precedence.
