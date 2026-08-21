# ortak Prior-Art Review

**Date:** 2026-08-17 | Web review with source links in each entry.

Research question: Does another product combine one live workspace, an observation journal, an optimistic line-region gate, stop-the-line handling, and an LLM arbiter?

No reviewed product provides the full combination. The field falls into seven groups. GitButler and Cursor validate separate parts of the design.

---

## Group 1: Isolation through worktrees and containers

Most tools give each agent an isolated copy.

- **Git worktree workflows:** Warp documentation, Conductor, and many articles recommend one worktree per agent. Claude Code also provides `--worktree`. [docs.warp.dev](https://docs.warp.dev/guides/agent-workflows/how-to-run-multiple-ai-coding-agents/), [conductor.build](https://www.conductor.build/docs/guides/parallel-agents/run-multiple-claude-code-sessions)
- **Dagger container-use:** An MCP server that gives each agent a fresh container and Git branch. [github.com/dagger/container-use](https://github.com/dagger/container-use)
- **Imbue Sculptor:** Containers and a UI for parallel agents. [imbue.com/blog/sculptor-announce](https://imbue.com/blog/sculptor-announce)

**Relation to ortak:** Isolation hides intent, edits, and contract decisions from other agents until merge. It also breaks code-state contracts in stateful runtimes such as Odoo with PostgreSQL. These tools cannot run two code states against one schema-dependent runtime.

## Group 2: Task-board orchestration

- **OpenAI Symphony:** An open-source specification that uses Linear as the agent control plane. Each issue gets an agent, an isolated workspace, and a ticket-driven state machine. Some teams reported a 500 percent increase in landed PRs. [openai.com](https://openai.com/index/open-source-codex-orchestration-symphony/), [github.com/openai/symphony](https://github.com/openai/symphony)

**Relation to ortak:** Symphony coordinates task assignment. ortak coordinates edits. Symphony can distribute work above ortak while ortak prevents line-region conflicts in the shared workspace below it.

## Group 3: One working directory with branches organized later

- **GitButler:** Keeps one working directory and organizes changes into virtual branches without one worktree per task. It provides the `but` CLI, Claude Code hooks, and an Agents tab that runs Claude per branch. GitButler raised a **$17 million Series A** in 2026 around its post-Git thesis. [docs.gitbutler.com/ai-agents/overview](https://docs.gitbutler.com/ai-agents/overview), [blog.gitbutler.com](https://blog.gitbutler.com/)

**Relation to ortak:** GitButler validates Layer 0: one directory with branches organized later. GitButler serves as a Git client and human review tool. It does not coordinate arbitrary concurrent agents through presence, a conflict gate, stop-the-line ownership, or an arbiter.

## Group 4: Coordination-plane tools

- **Too Many Cooks:** An MCP server for file locking, messaging, and shared plans among agents editing one codebase. It includes a VS Code dashboard. [tmc-mcp.dev](https://tmc-mcp.dev/)
- **AutoMobile multi-agent filesystem contract:** Runs several MCP clients against one host daemon with lock coordination and version reconciliation for mobile testing. [design document](https://kaeawc.github.io/auto-mobile/design-docs/mcp/daemon/multi-agent-filesystem-contract/)

**Relation to ortak:** Too Many Cooks requires pessimistic file locks and voluntary MCP calls. Agents cannot predict their edit footprint before investigation, and they can forget to acquire a lock. ortak derives presence from observed edits and enforces decisions through hooks.

## Group 5: Internal swarm infrastructure

- **Cursor agent swarm:** Cursor built a VCS in Rust for a swarm that rewrote SQLite. Cursor reported throughput around 1,000 commits per second, compared with about 1,000 commits per hour in the prior system. The VCS exposes conflicts as they occur. A neutral merge agent resolves them. Shared decision documents, compile-checked references, reconcilers, and a curated Field Guide handle design contracts and shared knowledge. [cursor.com/blog/agent-swarm-model-economics](https://cursor.com/blog/agent-swarm-model-economics)
- **Lexifina:** Uses target-scoped compare-and-swap staging for agents editing legal documents. Disjoint targets do not conflict. A rejected attempt receives a bounded peer packet with competing text, intent, and targets. An unresolved conflict becomes a durable obligation that blocks a success result. Lexifina tested 40 uncoordinated concurrent agents. [lexifina.com/blog/adding-a-multi-agent-write-lock](https://lexifina.com/blog/adding-a-multi-agent-write-lock)

**Relation to ortak:** Cursor validates a neutral merge agent and contract reconciliation. Lexifina validates an optimistic edit gate and durable conflict obligations. Each implementation remains tied to its own product rather than a local, harness-independent developer tool.

## Group 6: Agent-oriented VCS interfaces

- **agentjj:** Adds agent-oriented commands such as `orient`, `checkpoint`, and `undo` on top of Jujutsu, with JSON output and Git compatibility. [2389-research.github.io/agentjj](https://2389-research.github.io/agentjj/)
- **AgentGit (arXiv 2511.00628):** Applies Git-like rollback and branching to multi-agent system state rather than source code. [arxiv.org/abs/2511.00628](https://arxiv.org/abs/2511.00628)
- **Jujutsu (jj):** Treats conflicts as first-class values and models the working copy as a commit. Its concurrency design may suit a future shadow-history engine. [docs.jj-vcs.dev](https://docs.jj-vcs.dev/latest/technical/concurrency/)

**Relation to ortak:** These tools improve checkpoint and undo ergonomics for one agent. They do not coordinate concurrent agents.

## Group 7: Session recording and provenance

- **Entire CLI:** Hooks into Git and eight agent harnesses to record transcripts, prompts, files, token use, and tool calls. It links those records to commits and stores them on an `entire/checkpoints/v1` branch. It supports session resume, rollback, commit-time summaries, `entire blame`, and `entire why`. Entire also provides an organization and project control plane. [github.com/entireio/cli](https://github.com/entireio/cli)

**Relation to ortak:** Entire validates edit provenance and session recording. It observes concurrent sessions without blocking or arbitrating them. It keeps branch and worktree workflows and creates checkpoints around commits. ortak journals edits after a short debounce and removes development-time branches. Entire's eight harness adapters give it a strong path into coordination if the company adds that layer.

---

## Gap analysis

No reviewed product combines these five capabilities:

1. **One live workspace and runtime:** This preserves the code-state contract for systems whose database schema and runtime registry depend on source code.
2. **Observed presence with hook enforcement:** ortak derives presence from edits and does not require lock calls or file claims.
3. **Local, harness-independent optimistic line-region gate:** Cursor and Lexifina use related mechanisms inside closed product contexts.
4. **Stop-the-line with journal-based error ownership:** The journal assigns the session that must repair a shared broken state.
5. **Journal-derived branches and Forgejo PRs:** GitButler organizes changes later, but ortak produces task-specific branches and PRs from attributed edit history.

## Ideas to borrow

- **Durable obligations from Lexifina:** A conflict or error assignment should change the session's completion conditions. A session with an open obligation cannot report success. A SessionEnd or Stop hook can enforce this rule.
- **Bounded peer packets from Lexifina:** Limit competing alternatives and context size in denial messages. Give each packet a schema-valid conservative exit.
- **Neutral merge agent from Cursor:** Give contested merges to a third agent instead of either participant. A later arbiter could perform small merges as well as issue rulings.
- **Decision records with references from Cursor:** Store typed contract and design decisions. Let code reference them and revalidate dependent work after a change.
- **Stigmergy and the Field Guide from Cursor:** Let agents maintain shared operational knowledge as an extension of the journal's reasons.
- **Orphan-branch metadata from Entire:** Export the journal to a branch such as `ortak/journal` for team and machine synchronization through the existing Git remote. Keep the shadow repository local as the mechanical engine.
- **Transcript links from Entire:** Add the harness transcript location to edit hints so each diff links back to the conversation that caused it.
- **`blame` and `why` from Entire:** Implement `ortak why file:line` through shadow blame and the session trailer. Layer 0 already stores the required data.

## Threat assessment

- **GitButler:** Its funding and single-directory model make it the closest funded competitor. Adding real-time agent coordination would put it against ortak. ortak's daemon and hooks work across harnesses and stacks, while GitButler centers its GUI Git client.
- **Cursor:** Cursor could expose its internal VCS as a product. Current evidence shows it serving Cursor's own swarms.
- **Symphony:** If Symphony standardizes task orchestration, ortak can serve as its edit-level coordination layer.
- **Entire:** Entire already covers provenance and supports eight harnesses. A coordination feature would give it a large adapter advantage. ortak should center its gate, stop-the-line flow, and arbiter rather than compete on recording alone.
