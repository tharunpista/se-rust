# SE target architecture

Builds on `docs/ARCHITECTURE.md` (invariants I1-I8, baseline `1de6ab6`) and the decisions in `ADR-001`. This document adds the crates, ownership rules and data flow the charter requires; it does not repeat what the baseline already states.

## 1. Crate map — additions to the existing workspace

Existing: `se-protocol`, `se-provider`, `se-core`, `se-tui`, `se`. Kept unchanged in shape; extended in content.

**Revised after Phase 0 review.** The first draft proposed six new crates; a `solution-architect` review found three unjustified by charter §10's own test ("add a crate only when ownership/dependency boundaries justify it" — correcting this document's earlier mis-citation of that rule as §9). The corrected map:

| Crate | Owns | Depends on | Enforces (the actual boundary, not just a dependency row) |
| --- | --- | --- | --- |
| `se-command` | Typed command registry: id, slash path, aliases, category, arg schema, availability predicate, handler kind (UI action / core op / prompt template), permission requirement, provenance. Resolution errors are typed (`NotFound`, `Ambiguous`, `Collision`), ported from `Command.NotFoundError`/`Command.ExecutionError` shape (`packages/core/src/command.ts@9bbb8ce`). | `se-protocol` only — registry metadata and a handler-kind enum, nothing more | Resolution-to-effect happens in `se-core`, which can unit-test command *matching* with no runtime started at all. (The first draft's dependency row and its stated rule contradicted each other; this is the resolution — `se-command` depends on nothing but `se-protocol`.) |
| `se-catalogue` | **Merged from the first draft's `se-agents` + `se-skills`.** They had identical shape — read-only Markdown scan, frontmatter tolerance, provenance hash, lazy body — and would otherwise duplicate the frontmatter parser or need a third crate to share it. One crate, two modules (`agent`, `skill`). Loader/index over `SE\se\agents\<department>\<id>.md` and skill roots (`SE\se\skills`, `SE\.agents\skills`, `~\.claude\skills`): full frontmatter, original body verbatim, source path, content hash, validation status. Markdown tolerance per `packages/core/src/config/markdown.ts@9bbb8ce` (unquoted-colon retry; no module-global parse cache to poison). Discovery is read-only, never writes to a foreign source, per `compatibility.ts@9bbb8ce`'s scan-don't-move model. **Lives under `se-core`, not reachable from `se-tui`** — see the I1 correction below; loading 194 files plus the skill trees is exactly the "file indexing" operation `docs/ARCHITECTURE.md`'s I1 already names, and the TUI must get catalogue data only as published snapshots. No directory watcher in any phase's scope; an explicit `/agents refresh` suffices until something needs more. | `se-protocol` | Construction only from `se-core`; the TUI never calls the loader directly. |
| `se-permission` | The gateway: `(agent, task, tool, resource) -> allow/deny/ask`, last-matching-rule-wins ruleset per `packages/schema/src/permission.ts@9bbb8ce`. **The `Approved`-token design is corrected per the Phase 0 security review** — see §3a below; a crate boundary alone enforces nothing, so the mechanism is a private, unconstructible token type, not just module separation. | `se-protocol` | The gateway is the only path to `Approved`; its constructor is not `pub`, and the tool-executing path in `se-core` requires one at the type level, not by convention. |
| `se-team` *(Phase 4 only)* | Task DAG, scheduler (bounded worker pool + provider/account semaphores extending `se-core`'s existing thread model — **not** a Tokio rewrite, per charter §6.3), artifact store, typed inter-agent message bus, ensemble evaluator. | `se-core` (not the other way around — see the cycle fix below) | `se-core` owns the supervisor and exposes a dispatch trait shaped like the existing `Runner` trait (`crates/se-core/src/runner.rs`, injected as a type parameter at the engine and wired in `se/src/main.rs`, exactly as `ProviderRunner` is today); `se-team` implements that trait. No new `se-orchestration-traits` crate. |

**`se-usage` is dropped as a crate.** Its job — `not reported` as a distinct variant from zero, subscription vs. API spend kept apart — is already half-built in `se-protocol`'s `Usage`/`UsageSource::Unavailable` with an existing test that it never defaults to zero. It becomes `se-core::usage`, a ledger module over the existing protocol types, not a separate crate with an empty "must not depend on" row.

`se-core` gains the session/event/persistence layer (durable sessions, checkpoints, resume semantics) and the agent-invocation path (inject the loaded agent body as system prompt, record provenance), plus the `se-catalogue` and `se-usage` ownership above. `se-tui` gains the in-composer picker and the new browsers (§4 below), dispatching only typed `CoreOp`s and reading only typed `Payload`s — see the protocol additions this requires, named in §4a.

### Dependency direction, stated as a rule — corrected

`se-tui -> se-command -> se-core`, and `se-core -> {se-provider, se-permission, se-catalogue}`. `se-team -> se-core` (not the reverse). `se-permission` has no outbound dependency on execution crates — it is a pure decision function plus an audit sink, so it can be unit-tested without starting anything, though the audit sink's *durability* is still an open question — see §3a.

**The dependency check the first draft proposed guarded the wrong direction.** It said "enforce that `se-provider` never depends on `se-tui`" — nothing would ever write that dependency by accident. The rule that needs machine enforcement is the reverse: a workspace lint or cargo-deny banned-dependencies rule must assert that `se-tui` **cannot** depend on `se-provider`, `se-catalogue`, or `se-permission`.

### Correction to `docs/ARCHITECTURE.md`'s I1 invariant, made explicit rather than silently contradicted

`docs/ARCHITECTURE.md` states that crates "attach under `se-core` and must not be reachable from `se-tui`," and that this unreachability is how I1 stays true. This document's first draft put `se-agents`/`se-skills` directly in the `se-tui` dependency chain, which would have broken I1 for exactly the operation I1 was written to cover. The fix is in the design, not the invariant: `se-catalogue` lives under `se-core`, as stated above, and the invariant is otherwise reaffirmed unchanged. One related clarification, not a change: I1's enforcement for `ProviderRunner` today is a private constructor (`TurnSink::new` is `pub(crate)`), not the dependency graph alone — `se-core` does re-export `ProviderRunner`, so a UI crate can construct one but cannot drive it. The same private-constructor mechanism, not crate boundaries alone, is what will enforce `se-permission`'s `Approved` token.

## 2. The `/` command path, end to end

1. `se-tui`'s composer sees a `/` at the start of an otherwise-empty token boundary and enters picker mode, recording the trigger byte offset — same shape as `store.index` in `autocomplete.tsx@9bbb8ce`, adapted to a synchronous Rust composer (no reactive-signal race to work around; the SE composer already fully owns its buffer each frame).
2. Each keystroke recomputes the query as the text between the trigger offset and the cursor, and asks `se-command` for matches (prefix + fuzzy, grouped by category).
3. Enter resolves the highlighted entry. `se-command` returns one of: a UI action (open a browser panel — no core call), a core operation (`CoreOp` sent to `se-core`), or a prompt-template command (loaded from Markdown, arguments substituted using the **documented Claude Code 0-based convention** per `CLAUDE_CODE_PUBLIC_UX_MAP.md`, then submitted as a turn).
4. Escape closes the picker and restores the draft exactly — the picker never mutates the composer buffer except on an explicit selection.
5. `/agent <id>` resolves through `se-agents`; `/agent run <id> <task>` additionally allocates a task id and goes through `se-permission` before any tool is reachable.

Collision rule: built-in commands are loaded first and marked `provenance: builtin`; a project or plugin command with the same slash path is reported as a collision, not silently applied, if the built-in is marked `security_critical` (used for `/permissions`, `/connect`, `/doctor`). Otherwise the most specific provenance (project > global > plugin) wins, and `/help` shows which source won.

## 3. Agent invocation, end to end

1. `se-catalogue` loads all 194 definitions at startup, validating each; the 2 non-agent files (`ownership-matrix.md`, `role-inventory.md`) are reported as skipped, with reason, never silently dropped. No directory watcher — reload is the explicit `/agents refresh`.
2. `/agent run <id> <task>` asks `se-catalogue` for the definition by id, takes its content hash, and builds an execution profile: provider/model from routing precedence (§6.6 — task override > specialist profile > team profile > department default > global default > fail with actionable guidance), tool allowlist from the catalogue's `tools:` field, budget from `context_budget_tokens`/`max_calls`.
3. **`tools:` is derived only from the agent definition and its content hash; a task override or team profile may narrow it, never widen it.** This closes a gap the security review found in the first draft: bundling model selection and the tool allowlist into one "execution profile" selected by routing precedence would let whoever picks the model also pick the allowlist. The gateway's decision for a given (agent, tool, resource) must be invariant under a model change — a property to test directly, not just a design sentence.
4. `se-core` injects the **full original body** as the system prompt (not a name pasted into a generic template — this is the literal defect charter §5.3 forbids) and starts a `ProviderRunner`-class execution. The content hash is pinned for the run at invocation time and is never re-read mid-run, so a concurrent write to the agent root (Phase 4) cannot change a running agent's instructions.
5. Every event is recorded with: agent id, content hash, provider/account/model, task id, parent run id, tools made available, permission decisions, start/end time, outcome, artifact references.
6. The `escalates_to` / `collaborators` fields from the catalogue become the delegation edges available to an orchestrator role in Phase 4 — read, not reinterpreted, and in Phase 4 these are the **only** legal delegation targets (see §3a item 1).

## 3a. The permission gateway and the `Approved` token — corrected design

The first draft proposed `Approved(TaskId, ToolCallId, argument_hash)` as the fix for E7. The Phase 0 security review found this insufficient: a hash binds bytes, not meaning, and does not survive a relative path resolved against a changed cwd, a symlink swapped between approval and execution (classic TOCTOU), a glob that expands differently against a tree a sibling agent just wrote into, environment-dependent values a shell re-expands after approval, or Windows path aliasing (drive-relative paths, 8.3 names, `\\?\`, UNC, case-insensitivity) where distinct byte strings name the same target or identical strings name different ones. The review also found that the human approves a caller-supplied display string, not the real arguments — the gap between what was shown and what gets hashed is itself the attack.

**Corrected token contents**, all bound at approval time and re-verified at execution, not re-resolved:

- the canonicalized, fully-resolved target, identified post-symlink by OS-level file identity (volume + file id, or a validated handle) rather than by path string;
- the declared resource scope it was checked against;
- agent id **and** agent-definition content hash;
- session id **and** task id (not task id alone — routing by task id alone is still finding E7's open item 2, the supervisor's "current task" assumption);
- tool name and tool schema version;
- the cwd and the env subset the tool will actually see;
- a single-use nonce with expiry, consumed on execution, invalidated on process restart — **a persisted token must never be honoured after resume**;
- the exact bytes shown to the human, not a separately-constructed display string.

Tokens are non-transferable across tasks. The crate boundary (`se-permission`) does not by itself enforce any of this — the mechanism is that `Approved`'s constructor is private to the gateway, and every tool-executing path in `se-core` requires one at the type level.

**New trust boundaries introduced by multi-model orchestration (Phase 4), named so they are designed for rather than discovered in review later:**

1. **Orchestrator output must not author authorization.** Charter §6.3 puts tool/connector/skill permissions and allowed paths inside the task packet. If an orchestrator model — whose context includes repository files, i.e. attacker-writable content — authors those fields directly, a prompt injection in a README escalates a child agent's privileges. Rule: a model may only *request* a task; permissions are **intersected** with an operator/catalogue ceiling, never unioned; no authorization-bearing field is ever populated from model text; delegation targets are restricted to the static `escalates_to`/`collaborators` edges, enforced as a check.
2. **Cross-agent artifacts are untrusted data, not instructions.** One agent's artifact entering another agent's context (the message bus, §6.4) is external content at the lowest privilege level. The system-prompt position is reserved for the hash-verified agent body; every artifact, finding and other-model output enters in a delimited data region with provenance attached.
3. **The evaluator is an injection target too.** A reviewer/evaluator stage that reads N untrusted model outputs must have no tools (or read-only), produce a typed verdict rather than free text another stage parses, and require machine evidence (test exit codes) at the integration checkpoint — a model verdict never authorizes integration by itself.
4. **The approval queue is a consent-fatigue attack surface once N agents compete for it.** Per-task rate limiting is required; approval events are never coalesced (an existing protocol invariant, reaffirmed); every request displays task id, agent id, model and resolved target so a flood from one agent cannot be reflex-approved as another's.
5. **Shared process state is a cross-agent channel, not just a concurrency bug.** The single `History` mutex (E8) and the free-function credential loader (any task can trigger a read of any provider's key) both need per-agent scoping before Phase 4, not just per-task locking.
6. **Choosing an external runtime backend (`claude.exe`, `codex.exe`) is itself a permission-set selection SE does not govern.** `IMPLEMENTATION_SEQUENCE.md` states the ownership boundary as a disclosure ("SE's gateway does not govern their side effects"); charter §11 requires model identity never change what's authorized, which makes this a **ceiling**, not a disclosure. SE must refuse to run an agent on an external backend whose effective permissions exceed the agent's catalogue allowlist, or mark the run explicitly un-governed with per-run operator consent recorded in the invocation record.

## 4. TUI panels

Ten navigable views per charter §4, each a thin renderer over state owned elsewhere — no panel performs I/O. **The first draft under-specified what data each panel reads; two of the ten need new protocol types, named here rather than discovered during implementation:**

1. Conversation/task timeline — `se-core` events.
2. Model/provider/account selector — `se-provider::detect()` for readiness (not `se-usage`, which cannot know provider readiness — a first-draft error), plus the model registry.
3. Agent browser/profile — `se-catalogue`, shows real frontmatter and body preview, not a synthesized description.
4. Skills browser — `se-catalogue`.
5. Connector/MCP browser — Phase 5; shows honestly empty until then.
6. Team/task monitor — `se-team`, Phase 4. **Requires new `Payload` variants in `se-protocol`** — today `Payload` has no task-graph, run, or artifact event, and no `RunId` exists (`CoreEvent` carries only session/task/turn). Also needs a run-terminal event: today one `TaskFinished` per task is guaranteed, but a run of ≥3 tasks has no terminal signal of its own, so without this a monitor would show a completed run as still running. Name both as Phase 4 protocol work, not an implementation detail to improvise.
7. Usage monitor — `se-core::usage`.
8. Session/history browser — `se-core` persistence.
9. Permission/approval queue — `se-permission`, correlation id visible per request.
10. Help/keybindings — generated from the `se-command` registry, the same projection the CLI's `se help` uses (charter §12's requirement that TUI and CLI share one metadata source).

Every panel above dispatches/reads only typed `CoreOp`/`Payload` values; today's `CoreOp` has four variants (`SubmitUserTurn`, `Interrupt`, `Approve`, `Shutdown`) and none of them can request a model list, a readiness state, or an approval queue, so panels 2, 6 and 9 are statements of required protocol growth, not panels reachable with the protocol as it stands.

## 5. What is explicitly deferred past this document

MCP transport implementation, plugin capability manifests, external-runtime (`claude.exe`/`codex.exe`) adapters beyond detection, and the ensemble evaluator's disagreement-adjudication algorithm are named in the phase sequence (`IMPLEMENTATION_SEQUENCE.md`) but not designed here — designing them before their prerequisite phase would be exactly the "half-built subsystem inside an unrelated commit" pattern the user has already rejected once (E8's disposition).
