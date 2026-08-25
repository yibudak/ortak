# Get started

This guide takes a Git repository from installation to its first ortak-published
branch.

## Requirements

Use a macOS or Linux checkout with:

- a Git repository that has at least one commit
- a local trunk branch such as `main` or `master`
- a Git remote if you plan to push published branches

## Install ortak

The installer selects the release for your operating system and architecture,
checks its SHA-256 digest, and writes the binary to `~/.local/bin`.

```bash
curl -fsSL https://github.com/yibudak/ortak/raw/main/install.sh | sh
```

Choose another directory or pin a release when needed:

```bash
ORTAK_INSTALL_DIR="$HOME/bin" curl -fsSL https://github.com/yibudak/ortak/raw/main/install.sh | sh
ORTAK_VERSION=v0.2.1 curl -fsSL https://github.com/yibudak/ortak/raw/main/install.sh | sh
```

Build from source instead:

```bash
cargo install --git https://github.com/yibudak/ortak --locked
```

## Initialize the checkout

Run `init` at the repository root:

```bash
cd your-project
ortak init
```

The command creates:

- `.ortak/db.sqlite` for sessions, edit ownership, messages, and errors
- `.ortak/shadow/` for the private edit history
- `ortak.toml` for workspace settings

ortak detects the trunk branch during initialization. Check
[`publish.base_branch`](reference/configuration.md#publish) if the detected
branch does not match your workflow.

## Start the daemon

```bash
ortak daemon --detach
```

The detached process watches the checkout and writes its PID and log under
`.ortak/`.

```bash
ortak status
```

The status output should start with `daemon: running`. Stop it with:

```bash
ortak daemon --stop
```

## Connect an agent

=== "Claude Code"

    ```bash
    claude plugin marketplace add yibudak/ortak
    claude plugin install ortak@ortak
    ```

=== "Codex"

    ```bash
    codex plugin marketplace add yibudak/ortak
    codex plugin add ortak@ortak
    ```

Start a new agent session after installation. The plugin loads its hooks at
session startup.

## Start work

The SessionStart hook reports the session's current `ortak-N` identity. Confirm
it when needed:

```bash
ortak whoami
```

Codex and other harnesses that do not export `CLAUDE_CODE_SESSION_ID` pass their
harness session ID:

```bash
ortak whoami <session-id>
```

Record one sentence that tells the other sessions what you will change:

```bash
ortak intent ortak-2 "Add timeout handling to the publish path"
```

Run other agents from the same checkout. Check the shared state with:

```bash
ortak status
ortak log
```

!!! warning "Keep the checkout stable"
    Do not run `git add`, `commit`, `checkout`, `switch`, `stash`, or `branch`
    while sessions share the workspace. Those commands operate on the whole
    checkout. Read-only Git commands remain safe.

## Publish the first task

Rehearse the publish before creating anything:

```bash
ortak publish ortak-2 --dry-run
```

Create and push the branch:

```bash
ortak publish ortak-2 --push
```

ortak prints the matching `gh pr create` or `tea pr create` command after a
successful push. See [Publish work](guides/publishing.md) for exclusions,
stacked branches, and amendments.

## Update ortak

```bash
ortak update
```

The command checks the latest release and updates stale binary and plugin
components. Restart active agent sessions after a plugin update.
