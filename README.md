<p align="center">
  <img src="assets/ortak-banner.png" alt="ortak: one workspace, many agents">
</p>

ortak lets coding agents and people work in the same live directory without separate branches or worktrees. It records each edit and creates task branches when the work ends.

## How it works

1. Everyone edits the same workspace and uses the same running system.
2. ortak records each edit with its session, file, and changed lines.
3. The gate prevents sessions from editing the same active region. Reported errors stop edits until the assigned session fixes them.
4. `ortak publish` creates a clean branch from one session's recorded changes.

## Install

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/yibudak/ortak/main/install.sh | sh
```

The installer verifies the release checksum and writes the binary to `~/.local/bin`. Set `ORTAK_INSTALL_DIR` to use another directory or `ORTAK_VERSION` to pin a release.

To build from source instead:

```bash
cargo install --git https://github.com/yibudak/ortak --locked
```

Initialize ortak inside your project and start the daemon:

```bash
cd /path/to/your-project
ortak init
ortak daemon
```

That runs in the foreground. To put it in the background instead, `ortak daemon --detach` sends its output to `.ortak/daemon.log` and writes its process id to `.ortak/daemon.pid`, and `ortak daemon --stop` stops it again. Either way a workspace takes one daemon and a second refuses to start, since two of them race on the shadow repository and drop each other's edits. A detached daemon is what you would point a launchd agent or a systemd unit at; writing that file is left to you.

For Claude Code, install the plugin from this repository:

```bash
claude plugin marketplace add yibudak/ortak
claude plugin install ortak@ortak
```

For Codex, add this repository as a marketplace and install the plugin:

```bash
codex plugin marketplace add yibudak/ortak
codex plugin add ortak@ortak
```

When working on ortak itself, point the marketplace at a clone instead so the
plugin reflects local changes:

```bash
claude plugin marketplace add /path/to/your/ortak/clone
```

Switch back to the published plugin with `claude plugin marketplace remove ortak`
followed by the `add` command above.

## Working alongside other sessions

ortak journals and gates file edits. It does not touch Git, and Git is the one
place it cannot protect you: `git stash`, `checkout`, `switch`, `branch`,
`commit` and `add` all act on the whole tree, including the uncommitted work of
every other session in it. A stash takes their changes with yours; a commit
ships files you never looked at. Leave those commands alone while other sessions
are working and let `ortak publish` build the branch from your own journal
instead. `git diff`, `git log` and `git status` change nothing, so use them
freely.

The SessionStart context states this rule and the plugin's skill expands on it,
but neither is enforced. If you want it to hold every time, put it in your
project's own `CLAUDE.md` or `AGENTS.md`, where the agent reads it on every turn
instead of once at startup.

A session that was already running when you installed the plugin does not have
the hooks, because a harness reads them at session start. That session never
registers, everything it writes lands on `human` in the journal, and
`ortak publish` has no branch to build for it. Restart or resume it to pick the
hooks up.

## What ortak can see

ortak records a change when two things are true: it happened inside the
workspace the daemon watches, and a hook told the daemon who made it. An edit
written with your agent's edit tools, in the workspace, satisfies both. Every
gap below is one of the two failing.

A git worktree is another directory, so the daemon never sees it. That is the
usual reason `ortak log` is empty after a busy afternoon: the session registered
in one directory and did its work in `/tmp/wt-fix`. `ortak publish` reads the
same journal, so it has nothing to build either, and the other sessions reading
`ortak status` see a session that has done nothing all day.

A shell command that writes a file is a weaker signal than an edit tool. The
edit hooks name the file before it is written; `sed -i`, a heredoc, a formatter
or a code generator name nothing, so the daemon takes the owner from whichever
session has a command open at the time and marks the row as inferred. `ortak
log` and `ortak blame` print that mark, and those rows are the ones to check
when attribution looks wrong. While two sessions have commands running the
daemon declines to guess at all and the change lands on `human`, which keeps
that edit out of your branch and beats crediting the other session for it.

Answering review used to send work out of the workspace as well, since a pushed
branch is a git operation and ortak had nothing for it.
`ortak publish <session> --amend --branch <branch>` rebuilds a branch from the
journal instead, including one this session never published: whatever is already
committed on the branch stays, and the session's newer edits go on top of it.

When a file is credited to a session that did not write it,
`ortak release <session> <file>` gives the lines back and drops the gate's hold
on them, rather than waiting out `presence_minutes`.

## Workspaces and repository layout

One workspace belongs to one git repository. `ortak publish` builds its branch in the repository at the workspace root, so run `ortak init` at the root of the repository the work should land in.

A directory your project's `.gitignore` excludes is invisible to the journal. The daemon skips it, nobody gets credit for what they do inside it, and `ortak publish` leaves it out of the branch. The case that hurts is a repository keeping other repositories behind one ignore rule, `repos/` or `vendor/` or `addons/`. Running `ortak init` at the top succeeds and then records nothing anybody does in any of them, so init names those directories when it finds them. Each one you work in needs its own `ortak init`.

Nested workspaces are fine. ortak walks up from your session's directory and stops at the first `.ortak` it finds, so a session inside the inner workspace journals there and a session further up journals into the outer one.

Each workspace needs its own `ortak daemon`. One daemon watches one root and writes to that root's `.ortak`.

`ortak init` writes `ortak.toml` at the root, where it shows up as an untracked file. Commit it if its settings suit everyone working in the repository. If they do not, `echo ortak.toml >> .git/info/exclude` keeps it out of the way in your clone alone.

## Use

```bash
ortak status
ortak log
ortak intent ortak-2 "Implement the login page"
ortak report ortak-2 --command "pytest" "relevant error output"
ortak resolved ortak-2
ortak publish ortak-2 --push
```

Publishing requires a Git repository with at least one commit and a configured remote.

### Where `ortak publish --push` pushes

To `origin`, unless you say otherwise. Set your own remote once per clone:

```bash
git config ortak.remote my-fork
```

Anyone contributing through a fork has to do this before the first `--push`, or the push
goes to the upstream they cannot write to. It is the one setup step nothing prompts you
for.

The setting lives in git config rather than in `ortak.toml` because the right remote
differs per clone. One contributor pushes to a fork, another has commit rights and pushes
to upstream, and both are working in the same repository. `ortak.toml` is shared and
committed, so it is the wrong place for an answer that is different for each person
holding a checkout. `ortak.toml` still accepts a `remote` key for a team that genuinely
does share one, and git config wins where both are set.

### Set `base_branch` before the first publish

`ortak publish` builds every branch on top of `[publish] base_branch` from `ortak.toml`,
which defaults to `main`:

```toml
[publish]
base_branch = "main"
```

That default is wrong for any repository whose trunk goes by another name, and plenty do:
`master`, `develop`, or a version branch like `16.0`. Point it at the branch these tasks
merge into. Until you do, publishing refuses and names what it could not find:

```
error: base branch '16.0' does not exist in this repository (HEAD is on 'task/ortak-2-fix-login').
Set [publish] base_branch in ortak.toml to the branch these tasks merge into
```

It refuses instead of guessing. The obvious guess is HEAD, and HEAD in a shared workspace
is whatever branch the tree happens to sit on, which can be another session's task branch.
