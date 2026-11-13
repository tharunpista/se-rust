# Implementation sequence

Phases follow charter §14. The ordering below encodes **real prerequisites**, not preference: each phase's work is blocked by something concrete in the phase above it.

Every phase ends with the full gate: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `cargo build --release`, plus a manual run of the release binary in Windows Terminal. Unit tests cannot prove terminal protocol behaviour, so terminal claims require the manual run.

## Prerequisite chain (the part that is not negotiable)

```
E7 approval binding  ──►  tool permission gateway  ──►  native tools
worker pool + semaphores + E8/E9  ──►  team scheduler  ──►  task DAG execution
session persistence  ──►  resume
agent loader  ──►  agent invocation  ──►  team orchestration
```

Three consequences worth stating plainly:

- **The tool gateway cannot be built before E7 is fixed.** E7 is that an approval is not bound to the call it authorises. Building a gateway on top of an unbound approval would ship the vulnerability into the one component whose entire job is authorization.
- **The team scheduler cannot be built before E8/E9.** E8 is that `Approve`/`Interrupt` ignore task id and `History` is a shared mutex; E9 is a lost wakeup on the 1-slot approval channel. Both are single-task assumptions. A multi-task scheduler on top of them would route one task's approval to another task.
- **Resume cannot be claimed before persistence exists.** Today history is in-memory (`crates/se-core/src/engine.rs`); there is nothing to resume from.

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

## Phase 2 — Native agent runtime

1. Session/event/persistence layer with migrations; checkpoints; undo/redo semantics.
2. **E7 fix first**: `Approved` token carrying `(TaskId, ToolCallId, argument_hash)`, replies carrying the `request_id` they answer.
3. `se-permission` gateway: ruleset evaluation (last match wins), deny-before-allow ordering, audit records with secret redaction.
4. Native tools (read/write/edit/search/shell) behind the gateway — **none before step 3**.
5. Prompt/context assembly; native agent loop with tool-call lifecycle.
6. Also close the two open §11 items that belong here: **reject URL userinfo**, and a real **cancel-vs-deadline precedence test** (currently asserted only in a comment).

**Gate:** a configured native provider executes a real coding task with tool approvals, session history and verifiable output. Mark live-provider status separately and only with explicit authorization for a paid call.

## Phase 3 — SE specialist integration

1. `se-agents` loader over the 194 canonical definitions; the 2 non-agent files reported as skipped-with-reason.
2. `se-skills` inventory, lazy bodies, read-only foreign-source scanning.
3. Browse/search/inspect/invoke from `/`; per-specialist execution profile from the catalogue's own fields (`tools`, `context_budget_tokens`, `max_calls`).
4. Invocation provenance records including agent content hash.

**Gate:** several real specialists are invoked by their real ids; their definitions, models and outputs are recorded and inspectable; the catalogue is unmodified (verify by hashing the tree before and after).

## Phase 4 — Multi-model team orchestration

1. Bounded worker pool + per-provider/account semaphores; **E8/E9 fixed here** (task-id-routed approvals, per-task history, no lost wakeup).
2. Task DAG with durable state; dependency edges; the full state set (pending/ready/running/waiting-approval/blocked/completed/failed/cancelled); checkpoints.
3. Typed message bus + artifact store with provenance.
4. The three strategies: parallel decomposition, same-subproblem ensemble with byte-identical versioned packets and isolated contexts, pipeline handoff.
5. Evaluator that surfaces disagreement with evidence — **never majority voting**.
6. File-conflict safety (ownership rules or isolated worktrees), tested on Windows.
7. Team monitor panel; usage attribution per task/agent/model/account.

**Gate (charter's own):** one goal completed by ≥3 specialist roles using ≥2 distinct models/providers, with concurrent work, a review stage, conflict-safe integration and an inspectable run record. **Mock providers** with distinct identities and deterministic timing; live verification marked separately.

**Acceptance tests:** ensemble packets are byte-identical across providers; cancellation propagates to children; one failed task does not erase completed artifacts; two workers cannot silently overwrite one file; resume restores pending/running state without replaying an unknown-outcome side effect; budget denial stops or asks before further spend.

## Phase 5 — Provider breadth and connectors

Additional native providers; local runtimes (Ollama — **currently not installed on this machine**, so its local-live test is unavailable until it is); LM Studio / vLLM / llama.cpp-compatible endpoints; supported subscription-backed auth only through official interfaces (no token extraction, no private OAuth replay); MCP stdio + Streamable HTTP; plugin manifests; pricing/accounting.

External runtime note: `claude.exe` 2.1.323 and `codex.exe` 0.166.0 are present; `opencode` is a `.ps1` that `CreateProcess` cannot execute directly. Each adapter must document that the **external runtime owns its own tools and permissions** — SE's gateway does not govern their side effects.

**Gate:** the support matrix reflects tested behaviour, with every untested row labelled.

## Phase 6 — Hardening and release

Security review; full QA matrix; Windows Terminal manual testing; concurrency benchmarks; docs, migration, installation, shell completions; release packaging.

**Gate:** documented installation launches SE; `/` exposes usable commands; a real configured provider works; user agents are invocable; a multi-model team run works; no critical security defect is left undispositioned.

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
