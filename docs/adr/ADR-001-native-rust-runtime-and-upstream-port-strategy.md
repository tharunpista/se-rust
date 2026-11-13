# ADR-001: Native Rust runtime; port OpenCode's contracts, reuse no OpenCode source

## Status

Accepted.

## Context

The charter asks one explicit question: is SE porting OpenCode's major subsystems into Rust, selectively reusing MIT-licensed code where appropriate, or merely imitating the UI? It rejects the third answer outright.

`docs/reference/OPENCODE_SOURCE_AUDIT.md` establishes the fact that decides this: at `v2.2.5` (`9bbb8ce18c80f099e0e448775f59d965ead40a21`), OpenCode is 4789 TypeScript files and 833 TSX files; its only Rust package (`packages/process-daemon`, 34 files) is an unrelated PTY/terminal daemon with no agent, provider, session, tool, command or orchestration logic. There is no Rust agent-framework source to copy.

SE's existing workspace (`se-protocol`, `se-provider`, `se-core`, `se-tui`, `se`) is a thread-based, channel-driven Rust runtime with 189 passing tests at baseline commit `1de6ab6`, including a typed provider-error taxonomy, turn deadlines, origin-pinned credentials and redaction already shipped. Rewriting it in TypeScript, or bolting Bun onto it, would discard verified, tested work for no stated benefit, and the charter's own boundary §1 forbids exactly that without a written comparison and explicit approval — neither of which this task has.

## Decision

1. **The runtime stays native Rust**, built on the current workspace. No TypeScript/Bun rewrite.
2. **No OpenCode source code is reused.** There is none in the needed language to reuse. If a future need arises to adopt a non-code asset verbatim — a JSON schema fixture, a documented prompt template — the MIT notice and provenance are added at that time, not preemptively.
3. **SE ports OpenCode's contracts and proven behaviour**, translated into Rust idiom, not mechanically transliterated file-by-file:
   - the command-registry shape (typed resolution errors, not strings) — `packages/core/src/command.ts@9bbb8ce`
   - the Markdown prompt-template command shape (`template`, optional `agent`/`model`) — `packages/schema/src/config/command.ts@9bbb8ce`
   - the agent `mode` (primary/subagent/all) plus tolerant partial-decode-don't-drop behaviour — `packages/schema/src/config/agent.ts@9bbb8ce`
   - the permission rule/request/reply shape with a correlated request id — `packages/schema/src/permission.ts@9bbb8ce` (this directly satisfies open finding E7)
   - skill-as-distinct-from-command with lazy body loading — `packages/schema/src/skill.ts@9bbb8ce`
   - non-destructive, skip-one-on-failure discovery of foreign agent/skill sources — `packages/core/src/config/plugin/compatibility.ts@9bbb8ce`
   - frontmatter tolerance for real-world Markdown (unquoted colons, cache-poisoning avoidance) — `packages/core/src/config/markdown.ts@9bbb8ce`
   - the in-composer, trigger-plus-cursor picker model — `packages/tui/src/component/prompt/autocomplete.tsx@9bbb8ce`
4. **Claude Code informs public UX only**, from its published documentation (`docs/reference/CLAUDE_CODE_PUBLIC_UX_MAP.md`), never from inference about its implementation. Where its documented argument-indexing convention (`$0`-based) conflicts with a casual reading of the charter's example (`$1`, `$2`), SE adopts the documented convention and states so explicitly, rather than silently picking one.
5. **SE does not adopt the Effect runtime, nor a client/server split.** Those are upstream's technology choices for its own deployment model (hosted, multi-client); SE is local-first and single-process, and copying that shape would be §9's forbidden "mechanical translation."
6. **The existing security and reliability work is a baseline, not a casualty.** The typed provider-error taxonomy, turn deadlines, credential redaction and origin pinning already shipped are carried forward unchanged into the new architecture; nothing in this ADR authorizes regressing them.

## Consequences

- Every claim in the parity matrix and target architecture that cites OpenCode must carry a `path@SHA`. A claim without one is not evidence.
- New Rust crates are added only where charter §10's ownership rules require a new boundary (see `SE_TARGET_ARCHITECTURE.md`), not to mirror upstream's package list one-to-one.
- The two highest-risk subsystems — the multi-model task DAG (§6) and the tool permission gateway (§8, blocked on E7) — have **no upstream Rust implementation to lean on at all**; they are original engineering, gated behind mock-provider tests before any live run, per charter §13.
- This ADR will be revisited if a future need identifies a specific MIT-licensed non-code asset worth reusing verbatim; until then, "port, don't copy" is binding.
