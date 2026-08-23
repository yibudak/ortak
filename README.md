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
