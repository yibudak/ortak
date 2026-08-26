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

## How a write is attributed

Every journal row names a session. Some of those names are evidence and some are
inference. The row says which.

### An edit tool names its file

Agent hooks rebuild supported edit operations from their tool input and record an
attribution hint for that path. The daemon writes each snapshot as its own shadow
Git micro-commit, which separates two agent writes even when they reach the same
file close together. The tool said which file it was writing, so the row carries
no marker.

### A shell command claims the workspace

A redirect, a heredoc, `sed -i`, a formatter and a code generator all write files
that no edit tool named. Nothing in a Bash tool call says which paths it will
touch, so ortak claims rather than names. `pre-bash` opens a claim for the
session, `post-bash` closes it, and while it is open any write in that workspace
that no hook named belongs to that session.

The claim covers the workspace and not one file, because one command writes many.
It does not expire while the command runs, because a build outlives every other
window in the journal. Three rules bound it instead:

| Rule | Why |
| --- | --- |
| It still answers for three seconds after the command ends. | The daemon waits out a quiet window before journaling. A claim closed on the spot would be gone by the time anything asked. |
| It stops answering after thirty minutes with nothing heard from the session. | A harness killed mid-command leaves a claim that nothing else would ever close. The `last_seen` stamp that every hook writes is what measures this. |
| Ending a session retires its claim. | A session that has said goodbye is not running a command. |

While a command runs, your own editor save in that workspace lands on the session
running it. That trade is deliberate, because the alternative loses the agent's
work, and `ortak claim` and `ortak release` repair either mistake.

### Two sessions with commands open

Claims alone cannot say whose command wrote a file, but the journal can. A
session that has already written the file has a stake one that never touched it
does not, so the file goes to whichever claimant wrote it last. A file no
claimant has ever written is ambiguous, and it goes to `human` marked contested.

### The order the daemon decides in

1. A hook named this file within the last fifteen seconds. That session wrote it.
2. Exactly one session has a claim open. That session wrote it.
3. Several do. It goes to the one that wrote this file most recently, or to
   `human` if none of them ever has.
4. Nobody has a claim, but a session wrote this file within the last five
   seconds. That is the same write settling, and it stays with that session.
   Format-on-save lands here.
5. Nothing above. The write is the person's.

A startup scan records changes made while the daemon was offline the same way.

### What the markers mean

`ortak log` and `ortak blame` print a marker when the session on a row was worked
out rather than named. A row with no marker was named by a hook, or is the
person's own.

| Marker | Means |
| --- | --- |
| `inferred from a running command` | A Bash claim. That session had a command open and no hook named the file. |
| `two sessions had commands open` | More than one claim, and none of those sessions had written the file before. The row is the person's. |
| `claimed after the fact` | Somebody ran `ortak claim`. |
| `rewritten right after that session wrote it` | Nobody had a command open, but this session wrote the same file seconds earlier. A formatter or a codegen step following its own write. |

### A session can work in more than one workspace

Each workspace keeps its own database, so session numbers are local to it:
`ortak-3` in one checkout and `ortak-3` in another are unrelated sessions. The
harness session ID spans both, which is what `ortak whoami` reports.

An edit tool that writes into a second workspace is recorded there, because the
hook has the path and can find the workspace that owns it. A shell command
carries no path list, so ortak reads the paths out of the command text: every
workspace named by a path in the command gets a claim, and `post-bash` closes all
of them. `cd /other/repo && rm x` is covered too, because the claim is on the
workspace rather than on a file.

This misses whatever the shell works out while it runs: `$VAR/file`, a path read
out of a file, a `cd` through a variable. Writes like those land where they
landed before, on the person.

A session that reaches a second workspace registers there and appears in that
workspace's `ortak status`. It is not ended there when the harness exits, because
`session-end` reaches only the workspace it ran in. That row goes on reading
active, and its claim stops speaking after thirty minutes.

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
