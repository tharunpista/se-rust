# Claude Code public UX map

**Scope and boundary.** Everything here comes from the public documentation pages listed below, fetched on 2026-11-13. Claude Code's implementation is proprietary and was not inspected, decompiled or inferred. Nothing in this document describes Claude Code internals, and no proprietary code or prompt is reproduced. Where the charter's own description of a behaviour disagrees with the published documentation, the documentation wins and the difference is called out.

## Pages fetched

| Charter URL | Final URL after redirect | Note |
| --- | --- | --- |
| `docs.anthropic.com/en/docs/claude-code/slash-commands` | `https://code.claude.com/docs/en/slash-commands` | 301 Moved Permanently |
| `docs.anthropic.com/en/docs/claude-code/sub-agents` | `https://code.claude.com/docs/en/sub-agents` | fetched directly at the new host |
| `docs.anthropic.com/en/docs/claude-code/skills` | not fetched | covered by the slash-commands page, which documents skills as the recommended command format |
| `docs.anthropic.com/en/docs/claude-code/mcp` | not fetched | deferred to Phase 5, where connectors are actually designed |

The whole `docs.anthropic.com/en/docs/claude-code/*` path now redirects to `code.claude.com/docs/en/*`. The charter's URLs should be updated.

## 1. Slash commands — documented behaviour

**Built-in commands named on the page:** `/help`, `/compact`, `/cost` (alias of `/usage`), `/usage`, `/status`, `/context`, `/model`, `/permissions`, `/plugin`, `/add-dir`, `/cd`, `/rewind`, `/init`, `/login`, `/feedback`.

**Bundled skills that present as commands:** `/doctor`, `/code-review` (alias `/review`), `/batch`, `/debug`, `/loop`, `/simplify`, `/claude-api`, `/verify`, `/run`, `/run-skill-generator`, `/security-review`, `/skills`, `/reload-skills`, `/reload-plugins`, `/skill-doctor`, `/workflow-authoring`.

**Precedence, as documented:** a custom skill or command with the same name replaces the bundled command, **but not its aliases**. If a skill and a command file share a name, the skill runs.

**Definition locations:**

| Kind | Project | Personal |
| --- | --- | --- |
| Command file (older format, still supported) | `.claude/commands/<name>.md` | `~/.claude/commands/<name>.md` |
| Skill (recommended format, creates the same commands) | `.claude/skills/<name>/SKILL.md` | `~/.claude/skills/<name>/SKILL.md` |

Subdirectories are namespaced with a colon: `.claude/commands/frontend/component.md` becomes `/frontend:component`.

**Frontmatter fields** (all optional; `description` recommended; command files accept the same set except `name` and `paths`): `name`, `description`, `when_to_use`, `argument-hint`, `arguments`, `disable-model-invocation`, `user-invocable`, `allowed-tools`, `disallowed-tools`, `model`, `effort`, `context` (`fork` runs it in a subagent), `agent`, `background`, `hooks`, `paths`, `shell` (`bash` or `powershell`), and the accepted-but-inert `metadata`, `license`, `compatibility`.

**Argument placeholders — and a correction to the charter.** The documented set is:

- `$ARGUMENTS` — all arguments as typed. If no placeholder consumes them, `ARGUMENTS: <value>` is appended to the content.
- `$ARGUMENTS[N]` — argument by **0-based** index.
- `$N` — shorthand for `$ARGUMENTS[N]`, so **`$0` is the first argument** and `$1` is the second.
- `$name` — a named argument declared in the `arguments` list.
- Indexed arguments use shell-style quoting: `/my-skill "hello world" second` sets `$0` to `hello world`.
- A literal dollar before a digit or name is escaped with a backslash: `\$1.00`.

Charter §3.3 asks for "`$ARGUMENTS`, `$1`, `$2` and positional arguments". Under the documented Claude Code convention `$1` is the **second** argument, while OpenCode-style and shell-style conventions make `$1` the first. **SE must pick one and document it.** Recommendation: adopt the documented 0-based `$0`/`$ARGUMENTS[N]` form as canonical, and reject `$1`-as-first rather than guessing, because a silent off-by-one in argument substitution is the kind of defect that only shows up in someone's custom command.

## 2. Subagents — documented behaviour

**Definition locations and precedence (highest first):** managed settings (organization-wide) → `--agents` CLI flag (session only) → `.claude/agents/` (project) → `~/.claude/agents/` (all projects) → a plugin's `agents/` directory. Both agent directories are scanned **recursively, and subfolders do not affect identity** — only the `name` field does, so names must be unique across the tree.

**Frontmatter:** `name` and `description` are required; `name` is up to 256 characters and may not contain `:`. Optional: `tools` (allowlist, comma-separated string or YAML list; inherits everything if omitted), `model` (`sonnet`, `opus`, `haiku`, `fable`, a full model ID, or `inherit`). Other supported fields include `disallowedTools`, `permissionMode`, `maxTurns`, `skills`, `mcpServers`, `hooks`, `memory`, `background`, `omitClaudeMd`, `effort`, `isolation`, `color`, `initialPrompt`, `experimental`. Field names are case-sensitive camelCase and unrecognized fields are silently ignored.

`disallowedTools` is applied **first**, then `tools` resolves against what remains. That ordering matters for SE's permission gateway: a deny list that can be re-widened by an allow list is not a deny list.

**Model resolution order:** per-invocation `model` parameter (aliases only) → `model` frontmatter (`inherit` means the parent's model) → `CLAUDE_CODE_SUBAGENT_MODEL` → the main conversation's model. Charter §6.6's routing precedence is the same idea with more levels; it is consistent with this, not in conflict.

**Invocation paths:** automatic delegation by matching the task against each `description`; natural language ("Use the code-reviewer subagent to…"); `@"code-reviewer (agent)"` mention; session-wide via `claude --agent <id>` or `"agent"` in `.claude/settings.json`, where the flag overrides the setting. A subagent is disabled with `"permissions": { "deny": ["Agent(my-custom-agent)"] }`.

**Context isolation, as documented:**

- Fresh context window: a subagent sees neither the conversation history nor previously read files; it works from a delegation message. A *fork* is the exception and inherits the parent conversation.
- Its system prompt is its own markdown body plus basic environment details — not the host system prompt.
- Loaded at startup: `CLAUDE.md` files (unless `omitClaudeMd: true`), a git status snapshot, and any skills listed in `skills`.
- Not inherited: output style, auto memory, conversation history.
- Only the subagent's final summary returns to the parent, so verbose output stays out of the parent context.
- Nesting is capped — three layers deep by default, configurable via `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`.

## 3. Behaviours SE adopts, and why

| Documented behaviour | SE decision | Reason |
| --- | --- | --- |
| Commands and skills share one `/` namespace with documented precedence | Adopt | Charter §3.2 demands deterministic precedence and collision reporting; a published precedence order is a better starting point than inventing one. |
| Subdirectory namespacing with a separator (`/frontend:component`) | Adopt, with `:` | 194 SE agents across 15 departments need namespacing. Using the same separator avoids surprising anyone who also uses Claude Code. |
| Recursive scan where **only the declared id** establishes identity | Adopt | SE's canonical catalogue is already `<department>/<id>.md` with an explicit `id:` field, so identity is already independent of path. |
| Required-vs-optional frontmatter, unknown fields ignored | Adopt | SE's catalogue has 29 distinct frontmatter keys with a long tail (`blocked_on` on 6 files, `owns_spec_sections` on 3). A strict loader would reject the catalogue. |
| Deny list evaluated before allow list | Adopt as an invariant | Keeps a tool allowlist from re-granting something a deny rule removed. |
| Fresh context per subagent; only a final summary returns | Adopt | This *is* charter §6.5. The published model confirms the design rather than inspiring it. |
| Explicit nesting depth cap | Adopt | Charter §6.3 wants bounded scheduling; an orchestrator that can spawn orchestrators needs a hard depth limit. |
| Model alias resolution with `inherit` | Adopt the concept, not the aliases | SE routes by `provider/model`, per charter §7. `inherit` becomes "inherit the parent task's execution profile". |
| Automatic delegation by matching a description | **Do not adopt in Phase 3** | SE's charter requires explicit, recorded agent selection. Description-matched auto-delegation across 194 agents is unpredictable and would make invocation records hard to justify. Revisit only with an evaluation harness. |

## 4. Boundaries respected

- No claim is made about how Claude Code implements any of the above.
- No proprietary prompt, system message or binary was read or reproduced.
- Behaviour was taken from documentation, not from observing this session, because this session's behaviour is not a specification.
