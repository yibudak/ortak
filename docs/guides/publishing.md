# Publish work

`ortak publish` reconstructs one session's net change on a base branch. It
creates a branch reference without checking it out or changing the live files.

## Rehearse first

```bash
ortak publish ortak-2 --dry-run
```

The dry run reports:

- the branch name and files it would carry
- files it cannot replay without another branch
- references that another active session may depend on
- existing branch names that would block publication

It creates no branch, publish record, or push.

## Create a branch

```bash
ortak publish ortak-2
```

By default, the branch contains journaled edits made since that session's last
publish. The generated name combines `publish.branch_prefix`, the `ortak-N`
identity, and a slug from the task intent.

Override the branch or commit subject:

```bash
ortak publish ortak-2 \
  --branch fix/config-precedence \
  --message "Add layered config precedence"
```

## Leave a file out

Repeat `--exclude` for workspace-relative paths that must stay out of this
branch:

```bash
ortak publish ortak-2 \
  --exclude notes.txt \
  --exclude tmp/generated.json
```

A pattern is a path prefix, so naming a directory keeps everything under it out
and stops at the separator: `--exclude src` holds back `src/db.rs` and leaves
`srcgen/main.rs` alone.

A miss produces a warning, because a pattern matching nothing is a typo and the
warning is the only thing that says so. Excluded edits remain eligible for a
later publish.

## Ship a history that cannot be replayed

Publish rebuilds each file by replaying the session's own micro-commits onto the
base branch, which is what keeps a concurrent session's edits out of the branch.
The price is that every intermediate state has to apply, not only the last one.

A hunk the session discarded itself can therefore block the publish: edit line 3,
let the trunk change line 3 underneath, then rewrite the file without that line-3
edit. The final content merges cleanly and the replay still dies on the middle
step.

`--squash` takes such a file out of the replay and puts its net change back as
one commit, from the content the session first saw to the content it ended with:

```bash
ortak publish ortak-2 --squash
```

It applies per file and only where the replay was blocked, so everything else
replays as before. The net change is still merged rather than pasted over, which
keeps an unrelated trunk change elsewhere in the file. You give up the merge that
holds another session's lines out of that one file. Publish prints which files it
squashed and where to read them, and the blocked-replay error names the flag,
which is where you will meet it.

## Publish a complete session history

```bash
ortak publish ortak-2 --all
```

`--all` rebuilds a branch from everything the session has touched. The ordinary
incremental mode protects later tasks from republishing earlier deliverables.

## Stack work on another branch

```bash
ortak publish ortak-2 --base task/ortak-3-schema
```

The named branch becomes the publish base for this run. With `--push`, ortak
pushes a missing stack base before the new branch.

## Amend a branch

```bash
ortak publish ortak-2 --branch fix/config-precedence --amend
```

If the session published that branch, ortak rebuilds its own commit. If another
session or workspace created the branch, ortak preserves its current tip and
adds this session's work on top. A rewritten remote branch requires
`--force-with-lease`.

## Push and open a pull request

```bash
ortak publish ortak-2 --push
```

ortak resolves the push remote in this order:

1. `git config ortak.remote <name>`
2. `[publish] remote` in the merged config
3. `origin`

Set the per-clone remote when you publish through a fork:

```bash
git config ortak.remote my-fork
```

After a successful push, ortak inspects the remote URL and prints a matching
`gh pr create` command for GitHub or `tea pr create` command for Forgejo and
Gitea.

!!! warning "Read incomplete-branch warnings"
    A file may depend on unshipped edits from another session. ortak leaves an
    unreplayable file out, marks the branch incomplete, and tells you which work
    must publish and merge first. Do not open the pull request until the branch
    contains the intended file set.
