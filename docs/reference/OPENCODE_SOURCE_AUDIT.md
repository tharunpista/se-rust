# OpenCode source audit — pinned reference

**Status of this document:** evidence gathered by direct inspection of a pinned checkout. Every claim below cites `path@SHA`.

## 1. Pinned source

| Item | Value |
| --- | --- |
| Repository | `https://github.com/anomalyco/opencode` (resolved and live; the URL in the charter is correct) |
| Tag studied | `v2.2.5` |
| Commit SHA | `9bbb8ce18c80f099e0e448775f59d965ead40a21` |
| Commit date | 2026-10-30T22:06:51Z |
| Why this tag | It matches the `opencode` 2.2.5 binary already installed on this machine, so source and local behaviour corroborate each other. Deliberately **not** a moving `dev` branch. |
| Checkout method | `git fetch --depth 1 <sha>` into an empty scratchpad directory, read-only. No `bun install`, no build, nothing executed. |

Tags `v2.2.6` and `v2.2.7` exist upstream; they were not studied, so this audit is pinned, not current-tracking.

### License

`LICENSE@9bbb8ce` is the MIT License, `Copyright (c) 2025 opencode`.

Obligation: if any file or substantial excerpt is copied or adapted, the copyright notice and permission notice must be retained, with provenance recorded. **Nothing has been copied**, so no notice is required yet. Third-party files vendored inside the upstream tree (`patches/`, bundled assets) carry their own licences and were not examined; none may be reused without checking individually.

## 2. Language composition — the fact that decides the port strategy

File counts by extension at `9bbb8ce` (`git ls-tree -r --name-only HEAD`):

| Extension | Files |
| --- | --- |
| `.ts` | 4789 |
| `.tsx` | 833 |
| `.mdx` | 632 (documentation site) |
| `.json` | 540 |
| `.rs` | **34** |

All 34 Rust files are in one package, `packages/process-daemon/`, a terminal/PTY process daemon (`src/daemon.rs`, `src/pty`, `src/ghostty/ffi.rs`, `src/transport/windows.rs`). It contains **no agent, provider, session, tool, command or orchestration logic**.

**Consequence:** there is no Rust implementation of OpenCode's agent framework to reuse. Source-code reuse into a Rust workspace is unavailable for every subsystem SE needs. The honest options are to **port the contracts and behaviour** or to **imitate the UI**. This audit selects porting; see `docs/adr/ADR-001-native-rust-runtime-and-upstream-port-strategy.md`.

## 3. Subsystem map (`packages/`)

`packages/` holds 36 workspace packages. The ones that matter to SE:

| Package | Role |
| --- | --- |
| `core` | the runtime: agents, commands, skills, providers, sessions, tools, permissions, MCP, config, storage |
| `schema` | the versioned type/schema definitions every other package shares |
| `tui` | the terminal interface (TypeScript + JSX, `packages/tui/src/component/...`) |
| `cli` | non-interactive command surface |
| `server`, `client`, `sdk`, `protocol` | the client/server split and its wire protocol |
| `plugin`, `plugins`, `gui-plugins` | plugin host and bundled plugins |
| `process-daemon` | the Rust PTY daemon (the only Rust in the tree) |

`packages/core/src` top level (~90 entries) has one module per concern: `agent.ts`, `command.ts`, `skill.ts`, `permission.ts` + `permission/`, `tool.ts` + `tool/`, `provider.ts`, `model-resolver.ts`, `model-retry.ts`, `session/`, `mcp/`, `oauth/`, `credential/`, `artifact.ts`, `bus.ts`, `job/`, `snapshot.ts`, `usage-limits.ts`, `worktree.ts`, `workspace.ts`, `plugin/`.

That decomposition — one module per concern, plus a bus, an artifact store, a snapshot/worktree layer and a usage-limit layer — is the structural lesson, and it is close to what charter §9 already asks for.

## 4. Contracts worth porting, with citations

### 4.1 Command registry — `packages/core/src/command.ts@9bbb8ce`

The registry is deliberately thin. `Definition` is `{ name, description?, execute(Invocation) }` (lines 24–28); `Invocation` is `{ sessionID, prompt, delivery }` (lines 17–21). The service exposes `get` / `list` / `execute` with two typed errors, `Command.NotFoundError` and `Command.ExecutionError` (lines 30–40). Commands register through a mutable `Editor` (`add`) over a reloadable `State`, and every change publishes `Command.Event.Updated` on the bus (lines 54–72).

Lesson: a registry of handlers plus a bus event on change, with resolution failure **typed, not stringly**. Note the rich per-command metadata charter §3.2 wants is *not* in this upstream type — it lives in the config schema and the TUI. SE should carry it in the registry entry itself.

### 4.2 Markdown command definitions — `packages/schema/src/config/command.ts@9bbb8ce`

A configured command is `{ template, description?, agent?, model?, subagent?, subtask? }`, where `subtask` survives only as a deprecated alias for `subagent`. Upstream's Markdown command is therefore a **prompt template** with optional agent and model selection — exactly the charter's distinction between a prompt-template command and a core operation.

### 4.3 Agent definitions — `packages/schema/src/config/agent.ts@9bbb8ce`

`{ model?, request?, system?, description?, mode?, hidden?, color?, steps?, disabled?, permissions? }`, where `mode` ∈ `subagent` / `primary` / `all` and `permissions` is a `Permission.Ruleset`.

One detail worth copying: an unparseable `color` is caught and dropped rather than failing the whole agent (`Schema.catchDecoding`, lines 20–23). **Partial validity beats silently losing an agent.**

### 4.4 Permissions — `packages/schema/src/permission.ts@9bbb8ce`

- A rule is `{ action, resource, effect }` with `effect` ∈ `allow` / `deny` / `ask` (lines 56–66); a ruleset is an ordered array and, per the docs, the **last matching rule wins**.
- A request is `{ id, sessionID, action, resources, save?, metadata?, source?, message? }`, where `id` is a branded `Permission.ID` required to start with `per` (lines 10–14, 26–39). Replies are `once` / `always` / `reject` (line 42), and `permission.replied` carries `{ sessionID, requestID, reply }` (lines 46–52).

That `requestID` on the reply is precisely the correlation SE's finding **E7** says is missing. Adopt this shape.

### 4.5 Skills — `packages/schema/src/skill.ts@9bbb8ce`

`Skill.Info` is `{ id, name, description?, autoinvoke?, path, content }` with branded `ID`/`Name`, and a `Source` union of `directory` / `url` / `embedded`. Skills are a first-class inventory with provenance, distinct from commands — matching charter §3.3.

### 4.6 Compatibility discovery — `packages/core/src/config/plugin/compatibility.ts@9bbb8ce`

Foreign skill directories are adopted **non-destructively and read-only**, by scanning `<root>/skills` for the roots named in `config.compatibility()`, keyed `{ claude: [...], agents: [...] }` (lines 23–24, 43–57). The directory literals used across `packages/core/src` are `".agents"`, `".claude"`, `".opencode"`. Loading is debounced behind a filesystem watcher (100 ms), and a failed parse skips one file rather than the batch.

This is the model for SE's compatibility import: **scan, don't move; skip one, don't fail all.**

### 4.7 Frontmatter parsing — `packages/core/src/config/markdown.ts@9bbb8ce`

Two lessons, both scars from real bugs:

1. `matter(content, {})` is called with an options object **specifically to bypass gray-matter's module-global content cache**, because a failed parse poisons the cache and every later parse of the same content silently replays the failure (comment at lines 5–9, citing `jonschlinkert/gray-matter#166`).
2. On a YAML failure the content is retried through `sanitize()`, which rewrites `key: value with: colons` into a YAML block scalar (lines 24–39), because other coding agents accept unquoted colons.

SE's loader must be tolerant the same way: an agent file with an unquoted colon must not vanish.

### 4.8 Slash picker — `packages/tui/src/component/prompt/autocomplete.tsx@9bbb8ce`

The picker lives **inside the composer**, not in a separate dialog. Observed structure:

- One autocomplete store with a `visible` mode (`"command"`, `"reference"`, `"directory"`) and an `index` marking where the trigger character sits. The live query is simply the text between trigger and cursor: `getTextRange(store.index + 1, cursorOffset)` (line 157).
- Trigger prefixes: `/` for commands, `@` for references — files, **agents** and **skills** are all `@` insertions (`insertPart`, lines 171–205), and the prefix is re-derived at render (line 582).
- Results merge keymap commands (line 492) with server-provided commands (lines 503–509), de-duplicated by name, so one list spans local UI actions and core operations — charter §3.2's distinction.
- Agents offered for mention are filtered `!agent.hidden && agent.mode !== "primary"` (line 426).
- A deliberate comment (lines 161–166) explains the query signal is copied inside an effect because reactive text and non-reactive cursor state can be briefly out of sync on a keypress. SE's synchronous composer has no such hazard; it derives the query from (trigger index, cursor) each frame.

## 5. Reuse classification

| Class | Decision |
| --- | --- |
| **Source-code reuse** | **None available.** Every subsystem SE needs is TypeScript; the only Rust package is an unrelated PTY daemon. No file is copied, so no MIT notice is required yet. If a non-code asset (schema, prompt text, command-format fixture) is later adopted verbatim, the notice and provenance must be added at that time. |
| **Architecture / behavioural port** | The contracts in §4: command registry with typed resolution errors; markdown prompt-template commands; agent `mode` + ruleset + tolerant partial validation; permission rule/request/reply with a correlated request ID; skill inventory with provenance and lazily loaded body; non-destructive compatibility scanning; frontmatter tolerance; the in-composer trigger/query/merge picker model. |
| **Public UX inspiration** | Trigger-on-first-keystroke, fuzzy filter, grouped results, `@` for entity references versus `/` for commands. See `CLAUDE_CODE_PUBLIC_UX_MAP.md`. |

## 6. Where SE must deliberately diverge

1. **No Effect runtime.** Upstream is built on `effect` (`Layer`, `Effect.gen`, `FiberMap`, `Semaphore`, `PubSub`). SE is thread-based with channels and typed events. Port the *shapes*, not the monad.
2. **No client/server split.** SE is local-first and single-process; `server`/`client`/`sdk` have no SE analogue and must not be imitated for their own sake.
3. **Entity mention syntax.** Upstream puts agents and skills behind `@`. Charter §3.1 requires them under `/` (`/agent <id>`, `/skills`). SE follows the charter; `@` may be added later for in-prose references.
4. **Deprecated aliases** such as `subtask` are an upstream compatibility scar. SE has no installed base to carry and must not invent aliases.

## 7. Not studied — stated so it is not assumed

`packages/core/src/session/*` internals, the job scheduler (`packages/core/src/job/`), snapshot and worktree mechanics, the MCP client implementation, the OAuth flows, the `packages/ai` provider adapters, and the entire web/desktop surface. These are the next reads when Phase 2 and Phase 4 begin; any claim about them today would be unsourced.
