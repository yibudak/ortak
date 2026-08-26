# Configuration

ortak combines global defaults with one workspace file.

## Precedence

The files load in this order:

1. `~/.ortak/config.toml`
2. `<workspace>/ortak.toml`

The workspace value wins when both files define the same field. Omitted fields
inherit from the global file; built-in defaults fill fields missing from both.
ortak validates each file before merging, so a workspace override cannot hide
an invalid global value.

Use the global file for settings shared by several checkouts:

```toml
# ~/.ortak/config.toml
[orchestrator]
enabled = true
command = "claude"
model = "haiku"
timeout_secs = 20
```

Override one field in a workspace:

```toml
# ./ortak.toml
[orchestrator]
model = "sonnet"
```

Existing workspace files created before global config support may contain a
complete `[orchestrator]` block. Remove or comment out fields that should inherit
from the global file.

## Does `ortak.toml` belong in git?

Commit it. Everything the file holds is project policy that the whole team should
share: the trunk to publish onto, the branch prefix, the gate's margin and
presence window, the extra ignore patterns.

The one setting that could not be committed was the push remote, which differs
per clone, a fork for one contributor and upstream for another. That lives in git
config now, where per-clone settings already belong and where nothing can commit
it by accident:

```bash
git config ortak.remote <name>
```

`.ortak/` is the opposite case and stays out of git. It holds the journal
database and the shadow repository, both of them machine-local. It needs no entry
in the project's `.gitignore`, because `ortak init` writes one inside the
directory itself rather than editing a file the whole team shares.

## workspace

```toml
[workspace]
ignore = ["tmp/", "*.local"]
```

| Field | Default | Meaning |
| --- | --- | --- |
| `ignore` | `[]` | Gitignore-style patterns appended to the shadow repository's excludes. |

ortak also excludes `.ortak/`, `.git/`, common dependency directories, the
project `.gitignore`, and `.git/info/exclude`. A later `!pattern` in
`workspace.ignore` can include a path excluded earlier.

## publish

```toml
[publish]
base_branch = "main"
branch_prefix = "task/"
# remote = "origin"
```

| Field | Default | Meaning |
| --- | --- | --- |
| `base_branch` | `main` | Branch used as the base for published work. `ortak init` writes the detected trunk instead. |
| `branch_prefix` | `task/` | Prefix for generated branch names. |
| `remote` | unset | Fallback push remote when clone-local Git config has no `ortak.remote`. |

Push remote precedence differs from TOML precedence because the first value is
clone-local:

1. `git config ortak.remote <name>`
2. merged `[publish] remote`
3. `origin`

## gate

```toml
[gate]
enabled = true
margin_lines = 3
presence_minutes = 30
```

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Reject supported agent edits that overlap another active region. |
| `margin_lines` | `3` | Count targets within this many lines as conflicts. |
| `presence_minutes` | `30` | Keep a session's region active this long after its last edit to that file. |

Disabling the conflict gate does not disable stop-the-line. An open error still
blocks sessions that do not own the fix.

## line

```toml
[line]
blame_lookback_minutes = 120
mid_write_seconds = 90
```

| Field | Default | Meaning |
| --- | --- | --- |
| `blame_lookback_minutes` | `120` | Search this edit window when assigning a reported error. |
| `mid_write_seconds` | `90` | Decline a report that names a file another session changed within this window. Set `0` to disable the guard. |

## orchestrator

```toml
[orchestrator]
enabled = false
command = "claude"
model = "haiku"
timeout_secs = 20
```

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Ask the LLM arbiter about conflicts and ambiguous error ownership. |
| `command` | `claude` | Executable launched for each ruling. |
| `model` | `haiku` | Value passed to the executable's `--model` option. |
| `timeout_secs` | `20` | Wall-clock limit for one arbiter subprocess. |

ortak launches the command in this fixed form:

```bash
<command> -p "<prompt>" --model <model>
```

`command` names one executable, not a shell command with extra arguments. Use a
wrapper executable when another provider needs a different command line. The
subprocess inherits its environment and authentication, runs from the system
temporary directory, and receives no stdin.

Any spawn error, non-zero exit, timeout, invalid JSON, or invalid session ID
returns control to the deterministic fallback.
