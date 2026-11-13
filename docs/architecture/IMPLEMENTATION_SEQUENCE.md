# Implementation sequence

Phases follow charter §14. The ordering below encodes **real prerequisites**, not preference: each phase's work is blocked by something concrete in the phase above it.

Every phase ends with the full gate: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `cargo build --release`, plus a manual run of the release binary in Windows Terminal. Unit tests cannot prove terminal protocol behaviour, so terminal claims require the manual run.

## Prerequisite chain (the part that is not negotiable)

**Corrected after two independent Phase 0 reviews** (`solution-architect`, `security-architect`). The first version misplaced E9 and described E7 inaccurately; both are fixed below, and a fourth chain (E10) was missing entirely.

```
E7 reconciliation (two ids → one; TaskId on the decision; bind to canonicalized target)  ──►  tool permission gateway  ──►  native tools
E9 fix (recv_timeout + cancellation recheck) + the two open §11 items  ──►  Phase 2a, NOT the team scheduler
E8 fix (task-id-routed approvals, per-task history) + worker pool + semaphores  ──►  team scheduler  ──►  task DAG execution
E10 (cleared child environment)  ──►  the shell tool specifically, not "the tool system" generally
session persistence  ──►  resume
agent loader  ──►  agent invocation  ──►  team orchestration
```

Consequences, corrected:

- **The tool gateway cannot be built before E7 is reconciled.** The original description of E7 was wrong: call-id correlation already exists and is tested (`sink.rs`, `engine.rs`'s stray-decision regression test). What is actually open is narrower and still blocking — `request_approval` and `tool_requested` mint two different ids for the same call; the decision carries no `TaskId`; and nothing binds the approved bytes to a canonicalized, execution-time-verified target (a hash of the display string does not survive a symlink, relative path, glob, or Windows path-aliasing substitution — see `SE_TARGET_ARCHITECTURE.md` §3a). All three must close before the gateway, or the gateway ships the gap into the one component whose job is authorization.
- **E9 is reachable today, single-task, and must be fixed in Phase 2, not deferred to the scheduler.** The approval channel is 1-slot with every send as `try_send` (error discarded), and the approval-wait loop blocks on a bare `recv()` with no cancellation recheck. The moment any real tool exists (Phase 2 step 4), a stray decision or a concurrent interrupt can park the worker forever — this has nothing to do with multiple tasks. Fix is a `recv_timeout` loop rechecking cancellation each pass; two lines, not a scheduler rework.
- **The team scheduler cannot be built before E8 is fixed.** E8 — `Approve`/`Interrupt` ignoring task id, a shared `History` mutex — is the genuine multi-task defect and stays scheduled for Phase 4, where the worker pool and semaphores are built anyway.
- **E10 has no phase slot in the original sequence, despite Phase 2 shipping a shell tool that needs it immediately.** The first `Command::new` in the codebase inherits every provider API key and base-url override into a model-authored command. Added below as Phase 2 step 4a, immediately before the shell tool.
- **Resume cannot be claimed before persistence exists.** Today history is in-memory (`crates/se-core/src/engine.rs`); there is nothing to resume from. Additionally: **the userinfo/transport-redaction fix (§11, E6) must land before persistence**, or one release of durable session history writes unredacted credential-bearing URLs to disk before the fix exists. Reordered below.
- **Audit-record durability has no owner.** `se-permission` is specified as a decision function plus an audit sink with no durable store named, so approvals could not be reconstructed after a restart, and a persisted `Approved` token must not be honoured after resume. Named explicitly in Phase 2a below.

## Phase 0 — Audit and design *(this phase)*

Deliverables: `docs/reference/OPENCODE_SOURCE_AUDIT.md`, `docs/reference/CLAUDE_CODE_PUBLIC_UX_MAP.md`, `docs/architecture/FRAMEWORK_PARITY_MATRIX.md`, `docs/architecture/SE_TARGET_ARCHITECTURE.md`, this file, `docs/adr/ADR-001-...md`.

**Gate:** the design maps every asset found on this machine (194 agents, 27 skills across 4 roots, 194 derived commands, 0 `.opencode` sources), answers the port-vs-imitate question with evidence, and explains how multi-model collaboration works. **No code moves, no destructive cleanup, no new crates before this gate is reviewed.**

Status: deliverables written. Gate awaiting user review.

## Phase 1 — Command and interaction framework

1. `se-command` crate: registry, typed resolution errors, categories, provenance, collision rules.
2. In-composer `/` picker in `se-tui`: trigger offset, live query, fuzzy filter, grouped results, Up/Down/Enter/Tab/Esc, empty-result message, draft preservation.
3. Markdown command loader with the documented 0-based argument convention and quoted-argument handling.
4. One shared metadata projection feeding both `/help` and `se help` (charter §12).
5. Keybinding registry + a Windows Terminal key-delivery diagnostic (which of Ctrl+J / Alt+Enter / Ctrl+Enter actually arrive — the charter forbids advertising Shift+Enter unless it is distinguishable, and this must be measured, not assumed).
6. Browsers for agents/models/skills/connectors/usage/team may show honest empty or unconfigured states.

**Gate:** `/` is reliable in Windows Terminal with real key events; every top-level product feature is discoverable from `/`; no `/`-command can be sent to the model as a prompt.

**Acceptance tests:** `/` opens on one synthetic keypress with an empty composer; `/mod` filters to `/models`; Esc leaves the draft byte-identical; a project command cannot shadow `/permissions`; `cmd "hello world" second` binds `$0` and `$1` correctly; `\$1` stays literal.

## Phase 2a — Runtime correctness and durability

**Split from the original single Phase 2**, per the `solution-architect` review: the original phase bundled five independently-failable subsystems behind one gate sentence that only exercised the last of them. This half has nothing in it that depends on the gateway.

1. **Redaction fix first, before persistence exists**: reject URL userinfo at the one shared point where `is_safe_origin()` and `origin_of()` parse an authority; wrap both `Transport(t.to_string())` sites in `first_line()`; never echo a URL in an error message.
2. **E9 fix**: `recv_timeout` loop in the approval wait, rechecking cancellation each pass. Regression test: a stale decision queued, then an interrupt, must free the worker (not reproduced yet — write it before calling this fixed).
3. **Cancel-vs-deadline precedence, made testable**: extract the tie-break into a pure function with a four-row truth table (deadline × cancel, both orderings); cover the second override site in `engine.rs` in the same table.
4. **Keyless-provider HTTPS gap (E6b)**: require loopback or https for a keyless service too — not arbitrary remote `http://` — since a keyless provider is a *data*-confidentiality boundary, not just a credential one.
5. Session/event/persistence layer with migrations; checkpoints; undo/redo semantics. **Durable audit records now**, including the rule that a persisted `Approved` token is never honoured after resume.
6. **DONE.** The model-resolution prerequisite §6.6 needs: `UserTurn` gained `model: Option<ModelRef>` in `se-protocol` (level 1, the task override), `se-core::routing::resolve(task_override, specialist, team, department, global)` encodes the full five-level precedence with an actionable sixth-level error, and `CoreHandle::submit_with_model` is the seam a `/model` command or a specialist profile will call once either exists. `ProviderRunner` does not yet consult any of this — it still runs with the provider/model it was constructed with, fixed for the process's lifetime — so a submitted override is accepted by the protocol and the resolver but has no effect on what actually executes until `ProviderRunner` (or its replacement) is made registry-backed. That wiring is real remaining work, not yet started.

**Gate:** kill the process mid-turn; on restart the task reads `interrupted`, never `completed`. A stray approval followed by an interrupt frees the worker (regression test, not a self-report). The cancel-vs-deadline truth table passes all four rows. A credential-bearing URL with userinfo is refused, and the refusal message never contains the URL. A keyless provider refuses a non-loopback `http://` base URL. `UserTurn` can carry an explicit model override and `resolve()` picks it over every lower level.

## Phase 2b — Authorization and tools

1. **E7 reconciliation**: one `ToolCallId` per call (stop minting a second one in `tool_requested`); `TaskId` added to `ApprovalDecision`; `Approved` token per the corrected design in `SE_TARGET_ARCHITECTURE.md` §3a (canonicalized target + OS-level identity, session+task id, tool name+schema version, cwd/env subset, single-use nonce, the exact bytes shown to the human) — not the bare argument-hash first proposed. `ApprovalRunner`/`TwoApprovalRunner` in `se-core/src/runner.rs` are gated `#[cfg(test)]` or fixed to stop minting the second id, whichever lands first.
2. **E10**: native tools spawn with a cleared environment and an explicit allowlist — immediately, as part of this phase, not deferred past it.
3. `se-permission` gateway: ruleset evaluation (last match wins), deny-before-allow ordering, audit records with secret redaction, `Approved` unconstructible outside the gateway.
4. Native tools (read/write/edit/search/shell) behind the gateway — **none before step 3**, and the shell tool specifically depends on step 2.
5. Prompt/context assembly; native agent loop with tool-call lifecycle.
6. A property test: the gateway's allow/deny decision for a fixed (agent, tool, resource) is invariant under a model change, closing the §6.6 profile-bundling gap the security review found.

**Gate:** a configured native provider executes a real coding task with tool approvals, session history and verifiable output. Every tool invocation has exactly one matching gateway decision in the audit record, asserted by count, not inspection. A denied call does not execute. No tool path is reachable without an `Approved` token. Mark live-provider status separately and only with explicit authorization for a paid call.

## Phase 3 — SE specialist integration

1. `se-catalogue` loader (merged agent + skill modules, per `SE_TARGET_ARCHITECTURE.md` §1) over the 194 canonical definitions; the 2 non-agent files reported as skipped-with-reason. Lives under `se-core`; no directory watcher.
2. Skill inventory, lazy bodies, read-only foreign-source scanning.
3. Browse/search/inspect/invoke from `/`; per-specialist execution profile from the catalogue's own fields (`tools`, `context_budget_tokens`, `max_calls`); `tools:` narrowable by a profile, never widenable (§6.6 correction).
4. Invocation provenance records including agent content hash, pinned for the run and never re-read mid-run.

**Gate:** several real specialists are invoked by their real ids; their definitions, models and outputs are recorded and inspectable; the catalogue is unmodified (verify by hashing the tree before and after).

## Phase 4 — Multi-model team orchestration

1. Bounded worker pool + per-provider/account semaphores; **E8 fixed here** (task-id-routed approvals, per-task history — E9 already fixed in Phase 2a).
2. Per-agent credential scoping (a free-function loader keyed only by service id means any task can trigger a read of any provider's key — closed before concurrent tasks exist).
3. Task DAG with durable state; dependency edges; the full state set (pending/ready/running/waiting-approval/blocked/completed/failed/cancelled); checkpoints. New `se-protocol` types this requires: a `RunId`, task-graph/run/artifact `Payload` variants, and a run-terminal event (today's per-task `TaskFinished` has no run-level analogue).
4. Typed message bus + artifact store with provenance. **Data-vs-instruction boundaries enforced here, per `SE_TARGET_ARCHITECTURE.md` §3a**: orchestrator output may request a task but never author its permission fields directly (intersect with a ceiling, never union); cross-agent artifacts enter as delimited untrusted data, never as instructions; the evaluator gets no tools (or read-only) and emits a typed verdict, never free text another stage parses.
5. The three strategies: parallel decomposition, same-subproblem ensemble with byte-identical versioned packets and isolated contexts, pipeline handoff.
6. Evaluator that surfaces disagreement with evidence — **never majority voting**. This is the charter's stated differentiator (§6.2B) and must be a gated, tested assertion, not an implied property of "ensemble works."
7. File-conflict safety (ownership rules or isolated worktrees), tested on Windows.
8. Per-task rate limiting on the approval queue (consent-fatigue: one agent flooding requests must not get another agent's request reflex-approved); every approval request displays task id, agent id, model and resolved target.
9. Reserved budget for verification/review stages, independent of other tasks' spend, so exhausting the run budget cannot specifically starve the security-reviewer stage.
10. Team monitor panel; usage attribution per task/agent/model/account.

**Gate (charter's own):** one goal completed by ≥3 specialist roles using ≥2 distinct models/providers, with concurrent work, a review stage, conflict-safe integration and an inspectable run record. **Mock providers** with distinct identities and deterministic timing; live verification marked separately.

**Acceptance tests:** ensemble packets are byte-identical across providers; **the evaluator is tested on a planted disagreement and asserted to surface it with evidence rather than average or vote across it** (the charter's differentiator, gated explicitly per `docs/development/VERIFICATION_STRATEGY.md` finding F); a planted prompt-injection in an artifact is asserted not to alter a sibling agent's tool permissions; cancellation propagates to children; one failed task does not erase completed artifacts; two workers cannot silently overwrite one file; resume restores pending/running state without replaying an unknown-outcome side effect, and a persisted approval token from before the restart is never honoured; budget denial stops or asks before further spend.

## Phase 5 — Provider breadth and connectors

Additional native providers; local runtimes (Ollama — **currently not installed on this machine**, so its local-live test is unavailable until it is); LM Studio / vLLM / llama.cpp-compatible endpoints; supported subscription-backed auth only through official interfaces (no token extraction, no private OAuth replay); MCP stdio + Streamable HTTP; plugin manifests; pricing/accounting.

External runtime note: `claude.exe` 2.1.323 and `codex.exe` 0.166.0 are present; `opencode` is a `.ps1` that `CreateProcess` cannot execute directly. **Corrected per the Phase 0 security review**: documenting that an external runtime owns its own tools and permissions is a disclosure, not a control, and charter §11 ("model identity is not authorization") requires a ceiling. SE must refuse to run an agent on an external backend whose effective permissions exceed the agent's catalogue allowlist, or mark the run explicitly un-governed with per-run operator consent recorded in the invocation record — selecting that backend is itself a permission-set change SE must not let pass silently.

**Gate:** the support matrix reflects tested behaviour, with every untested row labelled.

## Phase 6 — Hardening and release

Security review; full QA matrix; Windows Terminal manual testing; concurrency benchmarks; docs, migration, installation, shell completions; release packaging.

**Gate:** documented installation launches SE; `/` exposes usable commands; a real configured provider works; user agents are invocable; a multi-model team run works; no critical security defect is left undispositioned.

## Verification strategy

`docs/development/VERIFICATION_STRATEGY.md` (written by `qa-lead` during Phase 0) has the full per-phase test matrix mapping every charter §13 bullet to a specific assertion, the measured baseline (189 tests at `fe761a0`, with 16 `se-provider` credential tests and the streaming tests reclassified as integration-level because they hit the real OS credential store and a real TCP listener), and a blunt critique: no gate below names an E7/E8/E9 regression test by name, Phase 4's acceptance list as originally written carried only 6 of 12 charter MM bullets as real assertions, and "cargo test green" alone is satisfiable by deleting tests. The acceptance-test lines in this document have been corrected against that critique where noted above; the full matrix in that file is the authority for what evidence a phase gate actually requires.

## Next phase and its measurable acceptance criteria

**Next: Phase 1.** Done when all of the following hold:

| Criterion | Measurement |
| --- | --- |
| Picker opens on first `/` | synthetic-key test, zero additional input |
| Live filtering | `/mod` yields `/models` ranked first |
| Draft safety | Esc restores the buffer byte-for-byte (property test over random drafts) |
| No prompt leakage | a registry-known slash path is never submitted as a turn (test asserts on the submitted payload) |
| Precedence | a project command colliding with a `security_critical` builtin is refused and reported |
| Argument binding | quoted and indexed arguments bind per the documented 0-based convention; `\$1` literal |
| One metadata source | a test diffs `/help` output against `se help` output and fails on divergence |
| Key delivery | a recorded Windows Terminal diagnostic naming which newline shortcuts actually arrive |
| Gate commands | test/clippy/fmt/release all green; release binary manually run in Windows Terminal |
