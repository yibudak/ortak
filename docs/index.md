---
hide:
  - navigation
  - toc
---

<div class="ortak-hero" markdown>
<p class="ortak-eyebrow">Coordination for shared code</p>

# One live workspace. <span>Many agents.</span>

<p class="ortak-lede">
ortak records who changed each line, blocks overlapping edits, assigns broken
builds to an owner, and publishes one session's work without switching the
checkout.
</p>

<div class="ortak-actions" markdown>
[Get started](getting-started.md){ .md-button .md-button--primary }
[See how it works](concepts/how-ortak-works.md){ .md-button }
</div>

<div class="ortak-track" aria-label="Agent edits converge into one published branch">
  <span>agent-a / edit</span>
  <span>agent-b / edit</span>
  <span>human / save</span>
  <span>ortak / publish</span>
</div>
</div>

## Keep the runtime and the code together

Concurrent coding sessions often use separate branches or worktrees. That
separates their code, context, and runtime state. ortak keeps the team in one
checkout and derives publishable branches from an edit journal.

<div class="ortak-pipeline">
  <div class="ortak-stage">
    <span class="ortak-step">01</span>
    <strong>Observe</strong>
    <p>Hooks and the file watcher attribute edits to agent sessions or the human editor.</p>
  </div>
  <div class="ortak-stage">
    <span class="ortak-step">02</span>
    <strong>Coordinate</strong>
    <p>The gate protects active line regions and stop-the-line gives each failure an owner.</p>
  </div>
  <div class="ortak-stage">
    <span class="ortak-step">03</span>
    <strong>Publish</strong>
    <p>ortak replays one session's journal onto the base branch without touching the live tree.</p>
  </div>
</div>

## Install and start

```bash
curl -fsSL https://github.com/yibudak/ortak/raw/main/install.sh | sh

cd your-project
ortak init
ortak daemon --detach
```

Then [connect Claude Code or Codex](reference/agent-integrations.md) and start
your agent sessions in the same checkout.

<div class="ortak-callout">
Keep mutating Git commands out of a live ortak workspace. Use
<code>ortak publish</code> to build branches from the journal.
</div>

## Choose your path

- [Set up your first workspace](getting-started.md)
- [Run concurrent sessions](guides/concurrent-work.md)
- [Handle a denied edit](guides/conflicts.md)
- [Publish a session](guides/publishing.md)
- [Configure the gate and optional LLM arbiter](reference/configuration.md)
