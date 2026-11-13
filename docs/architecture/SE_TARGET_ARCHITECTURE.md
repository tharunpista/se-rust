# SE target architecture

Builds on `docs/ARCHITECTURE.md` (invariants I1-I8, baseline `1de6ab6`) and the decisions in `ADR-001`. This document adds the crates, ownership rules and data flow the charter requires; it does not repeat what the baseline already states.

## 1. Crate map — additions to the existing workspace

Existing: `se-protocol`, `se-provider`, `se-core`, `se-tui`, `se`. Kept unchanged in shape; extended in content.

New crates, added only where ownership/dependency direction requires a boundary (charter §9's own test for whether a crate is justified):

| Crate | Owns | Depends on | Must not depend on |
| --- | --- | --- | --- |
| `se-command` | Typed command registry: id, slash path, aliases, category, arg schema, availability predicate, handler kind (UI action / core op / prompt template), permission requirement, provenance. Resolution errors are typed (`NotFound`, `Ambiguous`, `Collision`), ported from `Command.NotFoundError`/`Command.ExecutionError` shape (`packages/core/src/command.ts@9bbb8ce`). | `se-protocol` | `se-tui`, `se-provider` |
| `se-agents` | Loader/index over `SE\se\agents\<department>\<id>.md`: full frontmatter, original body verbatim, source path, content hash, validation status. Markdown tolerance per `packages/core/src/config/markdown.ts@9bbb8ce` (unquoted-colon retry; no module-global parse cache to poison). | `se-protocol` | `se-provider`, `se-core` (the loader is pure data; invocation lives in `se-core`) |
| `se-skills` | Skill inventory with lazy body loading (metadata first, body on invocation), discovery across `SE\se\skills`, `SE\.agents\skills`, `~\.claude\skills`, read-only — never writes to a foreign source, per `compatibility.ts@9bbb8ce`'s scan-don't-move model. | `se-protocol` | `se-provider`, `se-core` |
| `se-permission` | The gateway: `(agent, task, tool, resource) -> allow/deny/ask`, last-matching-rule-wins ruleset per `packages/schema/src/permission.ts@9bbb8ce`, and `Approved` tokens carrying `(TaskId, ToolCallId, argument_hash)` — the fix for open finding E7. Approval replies carry the `request_id` they answer, never a bare boolean. | `se-protocol` | everything else (this is the one crate every tool-executing path must go through; nothing may bypass it) |
| `se-usage` | Usage ledger: aggregation by run/task/agent/provider/account; `not reported` as a distinct variant from zero; subscription vs. API spend kept apart. | `se-protocol` | — |
| `se-team` *(Phase 4 only)* | Task DAG, scheduler (bounded worker pool + provider/account semaphores extending `se-core`'s existing thread model — **not** a Tokio rewrite, per charter §6.3), artifact store, typed inter-agent message bus, ensemble evaluator. | `se-protocol`, `se-core`, `se-agents`, `se-permission`, `se-usage` | `se-tui` |

`se-core` gains the session/event/persistence layer (durable sessions, checkpoints, resume semantics) and the agent-invocation path (inject the loaded agent body as system prompt, record provenance). `se-tui` gains the in-composer picker and the new browsers (§4 below), dispatching only typed `CoreOp`s — it still never runs an agent loop or a network request directly, per the existing I1-style ownership rule.

### Dependency direction, stated as a rule

`se-tui -> se-command -> {se-core, se-agents, se-skills, se-usage}`, and separately `se-core -> {se-provider, se-permission, se-agents, se-skills}`. `se-permission` has no outbound dependency on execution crates — it is a pure decision function plus an audit sink, so it can be unit-tested without starting anything. A dependency check (cargo-deny banned-dependencies rule, or a workspace lint) enforces that `se-provider` never depends on `se-tui`, matching the existing baseline invariant.

## 2. The `/` command path, end to end

1. `se-tui`'s composer sees a `/` at the start of an otherwise-empty token boundary and enters picker mode, recording the trigger byte offset — same shape as `store.index` in `autocomplete.tsx@9bbb8ce`, adapted to a synchronous Rust composer (no reactive-signal race to work around; the SE composer already fully owns its buffer each frame).
2. Each keystroke recomputes the query as the text between the trigger offset and the cursor, and asks `se-command` for matches (prefix + fuzzy, grouped by category).
3. Enter resolves the highlighted entry. `se-command` returns one of: a UI action (open a browser panel — no core call), a core operation (`CoreOp` sent to `se-core`), or a prompt-template command (loaded from Markdown, arguments substituted using the **documented Claude Code 0-based convention** per `CLAUDE_CODE_PUBLIC_UX_MAP.md`, then submitted as a turn).
4. Escape closes the picker and restores the draft exactly — the picker never mutates the composer buffer except on an explicit selection.
5. `/agent <id>` resolves through `se-agents`; `/agent run <id> <task>` additionally allocates a task id and goes through `se-permission` before any tool is reachable.

Collision rule: built-in commands are loaded first and marked `provenance: builtin`; a project or plugin command with the same slash path is reported as a collision, not silently applied, if the built-in is marked `security_critical` (used for `/permissions`, `/connect`, `/doctor`). Otherwise the most specific provenance (project > global > plugin) wins, and `/help` shows which source won.

## 3. Agent invocation, end to end

1. `se-agents` loads all 194 definitions at startup (and on a watched-directory change), validating each; the 2 non-agent files (`ownership-matrix.md`, `role-inventory.md`) are reported as skipped, with reason, never silently dropped.
2. `/agent run <id> <task>` asks `se-agents` for the definition by id, takes its content hash, and builds an execution profile: provider/model from routing precedence (§6.6 — task override > specialist profile > team profile > department default > global default > fail with actionable guidance), tool allowlist from the catalogue's `tools:` field, budget from `context_budget_tokens`/`max_calls`.
3. `se-core` injects the **full original body** as the system prompt (not a name pasted into a generic template — this is the literal defect charter §5.3 forbids) and starts a `ProviderRunner`-class execution.
4. Every event is recorded with: agent id, content hash, provider/account/model, task id, parent run id, tools made available, permission decisions, start/end time, outcome, artifact references.
5. The `escalates_to` / `collaborators` fields from the catalogue become the delegation edges available to an orchestrator role in Phase 4 — read, not reinterpreted.

## 4. TUI panels

Ten navigable views per charter §4, each a thin renderer over state owned elsewhere — no panel performs I/O:

1. Conversation/task timeline — `se-core` events.
2. Model/provider/account selector — `se-provider` registry + `se-usage` readiness.
3. Agent browser/profile — `se-agents`, shows real frontmatter and body preview, not a synthesized description.
4. Skills browser — `se-skills`.
5. Connector/MCP browser — Phase 5; shows honestly empty until then.
6. Team/task monitor — `se-team`, Phase 4.
7. Usage monitor — `se-usage`.
8. Session/history browser — `se-core` persistence.
9. Permission/approval queue — `se-permission`, correlation id visible per request.
10. Help/keybindings — generated from the `se-command` registry, the same projection the CLI's `se help` uses (charter §12's requirement that TUI and CLI share one metadata source).

## 5. What is explicitly deferred past this document

MCP transport implementation, plugin capability manifests, external-runtime (`claude.exe`/`codex.exe`) adapters beyond detection, and the ensemble evaluator's disagreement-adjudication algorithm are named in the phase sequence (`IMPLEMENTATION_SEQUENCE.md`) but not designed here — designing them before their prerequisite phase would be exactly the "half-built subsystem inside an unrelated commit" pattern the user has already rejected once (E8's disposition).
