# Agent integrations

The Claude Code and Codex plugins connect harness lifecycle events to the same
ortak workspace database and journal.

## Claude Code

```bash
claude plugin marketplace add yibudak/ortak
claude plugin install ortak@ortak
```

Restart Claude Code after installation. The plugin registers hooks for:

- session start and end
- edit approval and attribution
- Bash attribution and failure reminders
- prompt-time messages and stopped-line context

Claude Code edit tools provide enough structure for line-oriented gate checks.

## Codex

```bash
codex plugin marketplace add yibudak/ortak
codex plugin add ortak@ortak
```

Start a new Codex session after installation. Codex uses the same hook adapter
and bundled workflow skill.

Codex `apply_patch` events expose file paths but do not provide stable final line
coordinates. ortak protects the whole target file for those calls. Other
supported edit payloads use the most precise ranges their input provides.

## One checkout per collaboration group

Start all collaborating agent sessions in the same initialized checkout. A
separate worktree needs its own `ortak init` and daemon, and its sessions do not
share journal state with the first workspace.

At session start, the plugin reports:

- the current `ortak-N` and generated agent label
- the command for recording task intent
- the no-mutating-Git rule
- daemon or stopped-line warnings
- unread handoff messages from sessions that ended

## Update plugins

```bash
ortak update
```

The update command checks the installed binary and each available agent plugin
against the latest release. It skips missing tools and components already on
that version. Restart active sessions when hooks or skills change.

## Work without an agent plugin

A person can use `ortak status`, `log`, `blame`, messages, error commands, and
publish from the shell. The daemon attributes unmatched editor saves to the
`human` session.

Without harness hooks, ortak cannot deny an editor save before it happens. Read
`ortak status` and `ortak blame` before changing a protected area.
