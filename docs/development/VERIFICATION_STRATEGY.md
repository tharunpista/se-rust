# Verification strategy

Scope: Phases 1–4 of `docs/architecture/IMPLEMENTATION_SEQUENCE.md`. Binding inputs: charter §13 (testing and evidence) and §15 (status vocabulary) of `SE_OPENCODE_FRAMEWORK_PORT_AND_MULTI_MODEL_ORCHESTRATION.md`.

Measured at worktree `HEAD = fe761a0`, Windows 11, 2026-11-13.

Status labels are the charter §15 eight, used verbatim and never blurred:
`Implemented and unit-tested` · `Implemented and integration-tested` · `Manually verified on Windows Terminal` · `Live-provider verified` · `Partially implemented` · `Mock-only` · `Designed, not implemented` · `Unsupported`.

**Label rules, stated once because they are the whole point of the vocabulary.**
- A synthetic key event never earns `Manually verified on Windows Terminal`. Only a recorded run of the release binary in Windows Terminal does.
- A mock-provider test never earns more than `Mock-only` for any claim about provider behaviour. It can earn `Implemented and unit-tested` for *SE's own* logic (scheduling, routing, packet construction).
- An in-process test against a real local TCP server or a real temp filesystem earns `Implemented and integration-tested`.
- `Live-provider verified` requires explicit user authorization for a paid call and a recorded response. Absent that, the row reads `NOT LIVE-VERIFIED`.

---

## A. Measured baseline

`cargo test --workspace` at `fe761a0`: **exit code 0, 189 tests, 0 failed, 0 ignored** (`grep -c '^test '` over the output = 198 = 189 test lines + 9 `test result` lines).

| Crate | Target | Tests | `#[test]` sites |
| --- | --- | --- | --- |
| `se` (binary) | `src/main.rs` | **0** | `crates/se/src/main.rs`: 0 |
| `se-core` | `src/lib.rs` | **45** | `engine.rs` 10, `provider_runner.rs` 17, `runner.rs` 10, `sink.rs` 8 |
| `se-protocol` | `src/lib.rs` | **16** | `ids.rs` 5, `lib.rs` 6, `timings.rs` 5 |
| `se-provider` | `src/lib.rs` | **63** | `openai_compatible.rs` 39, `credentials.rs` 16, `lib.rs` 8 |
| `se-tui` | `src/lib.rs` | **64** | `state.rs` 19, `render.rs` 16, `input.rs` 14, `composer.rs` 8, `lib.rs` 7 |
| doc-tests | `se-core/src/lib.rs:12` | **1** (compile-only) | — |

Structural facts about the baseline:

- Every test target is `unittests src\lib.rs` or `src\main.rs`. **There is no `tests/` directory in any crate** (`ls crates/*/tests` → no such file). There are no integration targets, no benches, no fixtures.
- The binary crate `se` has **zero** tests. The CLI surface in `crates/se/src/main.rs` (6 clap subcommands, 510 lines) is untested.
- The one doc-test is `compile`-only; it asserts nothing at runtime.
- **No CI configuration exists** (`ls -a` at repo root: `.claude`, `.git`, `.gitignore`, `Cargo.toml`, `Cargo.lock`, `crates`, `docs`, `target` — no `.github`). Every gate is currently a human typing a command. That is the single largest hole in the evidence chain: nothing mechanically prevents a commit with red tests.
- No coverage and no mutation numbers exist for this repo, and none are quoted here. The honest statement is the list of uncovered subsystems below.
- **Two groups of tests are integration-level by the §B definitions even though they run in the unit target.** (a) The 16 tests in `crates/se-provider/src/credentials.rs` use `keyring::Entry` against the **real OS credential store** — they create, read and delete scratch entries in Windows Credential Manager under a shared `test_lock()` (`credentials.rs:291`, `:301`). They are machine-dependent, serialized, and will behave differently on a machine with no credential backend. (b) The streaming tests built on `serve_slowly` (`crates/se-provider/src/openai_compatible.rs:1697`) bind a **real TCP listener** on `127.0.0.1:0`. Neither group is a pure unit test, and both should be labelled `Implemented and integration-tested`, not `unit-tested`.
- `crates/se-tui/src/input.rs:239` is `shift_enter_and_alt_enter_both_insert_a_newline` — a synthetic-event test asserting behaviour for a key Windows Terminal may never deliver distinguishably. It proves the mapping, not the delivery, and is exactly the trap §C1 exists to prevent. The charter forbids advertising Shift+Enter unless measured distinguishable; this test is not that measurement.

### Not measured at this HEAD

`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `cargo build --release`, and the manual Windows Terminal run were **not executed for this document**. They are required by charter §13 and by every phase gate; their status here is *not measured*, not *passing*.

### What is NOT covered today

Derived from the 189 test names and the file inventory:

1. **Everything in Phases 1–4.** There is no `se-command`, no `se-agents`, no `se-skills`, no `se-permission`, no tool layer, no session store, no scheduler, no task DAG, no artifact store (`FRAMEWORK_PARITY_MATRIX.md` §B rows 1–9, 12–20: **absent**).
2. **Terminal key delivery.** `crates/se-tui/src/input.rs` is tested against synthetic `KeyEvent` values only. Nothing proves Windows Terminal delivers those events at all.
3. **Live provider compatibility.** No `se-provider` test makes a live call: the 47 non-credential tests run against local loopback servers or fixtures, and the 16 credential tests touch the OS store, not a provider. Zero live calls. Parity matrix row 10: "none live-verified".
4. **Concurrency beyond one task.** `se-core` is a single-task supervisor. Findings E8 (approve/interrupt ignore task id; `History` is a shared mutex) and E9 (lost wakeup on the 1-slot approval channel) are open and **have no failing regression test**.
5. **Approval binding (E7).** Approval plumbing exists (`crates/se-core/src/sink.rs`) but an approval is not bound to the call it authorises. No test asserts it must be.
6. **Cancel-vs-deadline precedence.** See §D3.
7. **URL userinfo rejection.** Open per the parity matrix §D; not implemented, so untested.
8. **Transport-error redaction.** `first_line()` redacts bodies; transport error strings still embed the URL. `Partially implemented`, and the bypass path has no test.
9. **Persistence and resume.** History is in-memory (`crates/se-core/src/engine.rs`). There is nothing to restore, so nothing to test.
10. **The CLI.** `se exec`, `connect`, `disconnect`, `providers`, `models`, `doctor` — no test touches them.
11. **Windows filesystem semantics.** No test creates a worktree, a long path, or a locked file.
12. **Paste.** Newline insertion (`input.rs:239`) and transcript scroll (`input.rs:358`, `:377`, `render.rs:541`, `:664`) and multi-line cursor geometry (`composer.rs:230`, `:244`) *are* covered. **Bracketed paste is not** — no test feeds a multi-line paste payload. Rows 1.15–1.17 below therefore extend existing tests rather than start from nothing; only the paste half is new.

---

## B. Risk-allocated test matrix, Phases 1–4

Effort is allocated by (likelihood of regression × cost of the failure), not by surface area. The security gateway, approval binding and conflict-safe writes get disproportionate attention. A settings browser's empty state gets one assertion.

Test levels: **U** unit (in-crate, no I/O) · **I** integration (real local server, real temp FS, real process) · **T** terminal-manual (release binary in Windows Terminal, recorded) · **M** mock-provider (deterministic fake identities/delays, no network).

Charter §13 bullet IDs used below: `SC1–SC9` (slash), `AS1–AS7` (agent/skill), `MM1–MM12` (multi-model), `TU1–TU7` (TUI). All 35 are mapped. Where a bullet spans phases it is placed where its assertion can **first** fail, with the later half noted.

### Phase 1 — Command and interaction framework

| # | What is verified | §13 | Level | The assertion that fails on regression | Label a pass earns |
| --- | --- | --- | --- | --- | --- |
| 1.1 | A registry-known slash path is never submitted as a model turn | SC8 | U | The submitted turn payload is asserted absent, not merely that a command also ran. Must assert on what reached the provider boundary. | Implemented and unit-tested |
| 1.2 | Picker opens on one synthetic `/` with an empty composer | SC1 | U | After exactly one `KeyEvent('/')` the picker state is open and the trigger offset equals the cursor; a test needing a second event is a failure. | Implemented and unit-tested |
| 1.3 | Live filtering: `/mod` ranks `/models` first | SC2 | U | Assert `results[0].id == "models"`, not `results.contains("models")`. Containment passes for an unfiltered list. | Implemented and unit-tested |
| 1.4 | `/agent` lists the agent namespace (shell; real catalogue in 3.1) | SC3 | U | `/agent` resolves to a registry entry and the picker shows the agent category; an empty-but-honest state is acceptable here and must say "unconfigured", never show fabricated rows. | Partially implemented |
| 1.5 | Esc closes the picker, draft byte-identical | SC9 | U | Property test over random drafts incl. multi-byte and `/`: `before == after` as bytes. An `is_empty()`-style assertion proves nothing. | Implemented and unit-tested |
| 1.6 | Up/Down/Enter/Tab in the picker | SC9 | U | Down past the last entry does not panic and does not wrap silently unless specified; Tab completes without submitting; Enter submits exactly the highlighted id, not the typed text. | Implemented and unit-tested |
| 1.7 | Picker focus: keys route to the picker, not the composer, while open | SC9, TU6 | U | A `j`/arrow while open must not be inserted into the draft. | Implemented and unit-tested |
| 1.8 | Nested/custom command parsing (`/ns:name`, directory nesting) | SC4 | U | A nested id round-trips to its source path; two different nestings do not collapse to one id. | Implemented and unit-tested |
| 1.9 | Argument binding, 0-based, quoted | SC5 | U | `cmd "hello world" second` binds `$0 == "hello world"` and `$1 == "second"`; `\$1` stays literal in the rendered prompt. Three separate asserts — the off-by-one is the likely defect, so assert the *value*, not the count. | Implemented and unit-tested |
| 1.10 | Unknown command / invalid args give typed errors | SC6 | U | The error is a typed variant naming the near-miss candidate; a generic string error fails the test. | Implemented and unit-tested |
| 1.11 | Collision precedence; no project command shadows a security-critical builtin | SC7 | U | A project command named `permissions` is **refused and reported**, and `/permissions` still resolves to the builtin. Two assertions: refusal, and the builtin still reachable. | Implemented and unit-tested |
| 1.12 | `/help` and `se help` are one projection | — | U | A test diffs the two rendered lists and fails on any divergence, including ordering and description text. | Implemented and unit-tested |
| 1.13 | Windows Terminal key delivery diagnostic: which of Ctrl+J / Alt+Enter / Ctrl+Enter / Shift+Enter actually arrive | TU1 | T | A recorded diagnostic table from the release binary, naming terminal + version. Shift+Enter must not be advertised unless *measured* distinguishable. Unprovable by `cargo test`. | Manually verified on Windows Terminal |
| 1.14 | `/` picker usable in Windows Terminal with real key events | TU1 | T | Recorded run: `/` opens, filters, Esc restores draft, on real key events. | Manually verified on Windows Terminal |
| 1.15 | Multiline editor and paste (newline insertion already covered at `input.rs:239`; paste is not) | TU2 | U + T | U: a bracketed-paste payload with `\n` and `\r\n` becomes one draft, not N submissions. T: real paste of a 50-line block in Windows Terminal. | Implemented and unit-tested + Manually verified on Windows Terminal |
| 1.16 | Cursor and transcript scroll independently (scroll covered at `input.rs:358`/`:377`; independence is not) | TU3 | U | Scrolling the transcript does not move the composer cursor index, and vice versa. | Implemented and unit-tested |
| 1.17 | Streaming does not steal viewport position | TU4 | U | With the transcript scrolled up, N streamed deltas leave the scroll offset unchanged; only a user action or bottom-anchored state may move it. | Implemented and unit-tested |
| 1.18 | Model/agent/skill/connector/usage/team commands reachable from `/` | TU5 | U | Every top-level product feature has a registry entry reachable by prefix query. Assert set equality against the feature list, not a non-empty result. | Implemented and unit-tested |
| 1.19 | No fake state when a provider is unconfigured | TU7 | U | An unconfigured provider renders an explicit "not configured" state; assert no placeholder model names and no zeroed usage appear. Extends the existing `unreported_usage_is_never_shown_as_zero` (`crates/se-tui/src/state.rs`). | Implemented and unit-tested |
| 1.20 | Keybinding hints stay synchronized with the registry | TU6 | U | A test derives the hint line from the keybinding registry and fails if a hint names a binding the registry does not have. A hardcoded hint string is the defect this catches. | Implemented and unit-tested |

### Phase 2 — Native agent runtime (highest security risk in the plan)

| # | What is verified | §13 | Level | The assertion that fails on regression | Label a pass earns |
| --- | --- | --- | --- | --- | --- |
| 2.1 | **E7**: an approval authorises exactly one call | — | U | Approve `(task A, call 1, hash H)`, then present `(task A, call 2, hash H)` and `(task A, call 1, hash H')`. **Both must be refused.** This is the negative case; a test that only shows the matching call succeeding proves nothing. Write it failing against today's code first. | Implemented and unit-tested |
| 2.2 | Replies carry the `request_id` they answer | — | U | A reply whose `request_id` is unknown or stale is rejected, not applied to the pending request. | Implemented and unit-tested |
| 2.3 | Gateway ruleset: last match wins | — | U | Table-driven over ordered rules; assert the resolved effect per case. | Implemented and unit-tested |
| 2.4 | Deny-before-allow: a later allow cannot re-grant a denial | — | U | `deny tool=shell` then `allow tool=*` → shell still denied. | Implemented and unit-tested |
| 2.5 | Every tool call has a gateway decision in the audit record | — | U | Enumerate executed tool calls from a scripted run and assert a 1:1 join against audit decisions. Zero unmatched. | Implemented and unit-tested |
| 2.6 | Audit records redact secrets | — | U | An argument containing a key-shaped string and a URL with credentials appears redacted in the record **and** on the transport error path (the known bypass, parity matrix §D). | Implemented and unit-tested |
| 2.7 | Reject URL userinfo (open §11 item) | — | U | `https://user:pw@host/` is refused at construction with a typed error; no request is made. | Implemented and unit-tested |
| 2.8 | **Cancel-vs-deadline precedence** (open §11 item) | — | U | Set a deadline and cancel within the same poll window; the outcome is `Cancelled`, never `Incomplete`/`Failed{turn}`. Must be forced deterministically (see §D3), not raced. | Implemented and unit-tested |
| 2.9 | Native tools cannot execute outside the gateway | — | U | A direct tool invocation bypassing the gateway fails to compile or returns a typed refusal. Prefer making it unrepresentable. | Implemented and unit-tested |
| 2.10 | Path confinement for read/write/edit | — | I | Real temp dir: `..` traversal, absolute path outside root, Windows `\\?\` and drive-relative forms, and a symlink/junction pointing outside are each refused. Windows-specific — see §C4. | Implemented and integration-tested |
| 2.11 | Session persistence: restart restores a transcript | — | I | Real file/DB on disk, process-equivalent restart, transcript compared event-by-event including ordering. | Implemented and integration-tested |
| 2.12 | Migrations apply forward from an older schema fixture | — | I | A checked-in v1 store opens under current code without data loss. | Implemented and integration-tested |
| 2.13 | Resume never replays an unknown-outcome side effect (store half) | MM9 | U | A task recorded as `running` with no recorded outcome resumes to a state requiring re-confirmation, never to "completed" and never to automatic re-execution. Scheduler half in 4.12. | Implemented and unit-tested |
| 2.14 | Checkpoints / undo-redo | — | U | Undo after N events restores exactly the state at checkpoint N; redo is not a no-op. | Implemented and unit-tested |
| 2.15 | A supplied system prompt reaches the provider request verbatim (prompt passthrough; the *loaded agent body* half is row 3.11, because the loader is Phase 3) | AS3 | U + M | The provider request's system message equals the string the runtime was given, byte for byte — no truncation, no template wrapper. | Implemented and unit-tested |
| 2.16 | A real coding task end to end with approvals | — | T | Recorded release-binary run: tool approvals requested, file modified, transcript persisted. | Manually verified on Windows Terminal; provider row `NOT LIVE-VERIFIED` unless authorized |

### Phase 3 — SE specialist integration

| # | What is verified | §13 | Level | The assertion that fails on regression | Label a pass earns |
| --- | --- | --- | --- | --- | --- |
| 3.1 | All 194 definitions load; `/agent` discovers the real catalogue | AS1, SC3 | I | Count asserted `== 194` against the real tree, and the id set compared to a checked-in sorted id list. A `> 150` assertion is worthless. | Implemented and integration-tested |
| 3.2 | The 2 non-agent files are reported skipped-with-reason | AS5 | I | The skip list equals `["ownership-matrix.md", "role-inventory.md"]` with a reason string each. Silent drop fails. | Implemented and integration-tested |
| 3.3 | Frontmatter parsing is robust | AS2 | U | Fixtures: unquoted colon in a value, CRLF, BOM, trailing whitespace, empty list, missing optional key (`status` present on only 42 of 194, `verify_commands` on 191). Each loads or reports — never panics, never loses the file. | Implemented and unit-tested |
| 3.4 | Definitions are preserved — the catalogue is not mutated | AS1 | I | Hash the three source trees before and after a full scan plus several invocations; assert byte-identical. | Implemented and integration-tested |
| 3.5 | Agent content hash appears in the run record and tracks the file | AS4 | I | A run record names agent id + hash; editing the file changes the hash on the next run. Both halves, or the hash could be a constant. | Implemented and integration-tested |
| 3.6 | Skill discovery is lazy | AS6 | I | Instrument reads: listing all skills performs **0** body reads; invoking one performs **exactly 1**. Assert the count, not the latency. | Implemented and integration-tested |
| 3.7 | Skill provenance retained | AS6 | I | Each skill carries source root + path + the `Role hash:` comment where present; a plugin-cache skill is marked read-only. | Implemented and integration-tested |
| 3.8 | Skills and commands remain distinct | AS7 | U | A skill and a command sharing a name both remain addressable and resolve per the documented rule; neither disappears from listings. | Implemented and unit-tested |
| 3.9 | Execution profile comes from the catalogue's own fields | — | U | `tools` is enforced as the permission allowlist by the Phase 2 gateway (a tool outside `tools` is denied), and `context_budget_tokens` / `max_calls` bound the run. Assert the denial, not that the field was parsed. | Implemented and unit-tested |
| 3.11 | The invoked agent's own file body is the system prompt | AS3 | I + M | The request's system message **equals the bytes of the agent file body** as loaded from the real catalogue. A generic prompt that merely names the agent is an explicit failing case. | Implemented and integration-tested |
| 3.10 | Several real specialists invoked by real id | — | M + T | Mock providers for determinism; one recorded terminal run. | Mock-only + Manually verified on Windows Terminal |

### Phase 4 — Multi-model team orchestration (highest design risk; no upstream analogue)

All provider-facing rows are **Mock-only** by construction: distinct mock identities, deterministic delays, no paid calls (charter §13).

| # | What is verified | §13 | Level | The assertion that fails on regression | Label a pass earns |
| --- | --- | --- | --- | --- | --- |
| 4.1 | Two models run independent subtasks concurrently | MM1 | M | Observed overlap: task B starts before task A finishes, proven from recorded start/end ordering of events, **not** from wall-clock total. Wall-clock totals are the flake source (§D). | Mock-only |
| 4.2 | One goal uses ≥3 specialists and ≥2 providers | MM2 | M | The run record contains ≥3 distinct agent ids and ≥2 distinct provider ids attributed to one goal id. | Mock-only |
| 4.3 | Ensemble packets are byte-identical across providers | MM3 | M | Capture both serialized packets and assert `==` as bytes, after stripping only the fields documented as per-provider. Any un-stripped difference (timestamp, nonce, ordering) is a real defect, not test noise. | Mock-only |
| 4.4 | Ensemble contexts are isolated | MM3 | M | Provider A's prior output never appears in provider B's request bytes. | Mock-only |
| 4.5 | Evaluator reports disagreement with evidence, never majority vote | MM4 | U | Three mock answers 2–1: the output is a disagreement record citing both positions. A test asserting "the majority answer was chosen" is the inverse of the requirement and must not exist. Also: 2–1 and 1–1–1 both produce disagreement. | Implemented and unit-tested |
| 4.6 | Dependency ordering respected | MM5 | U | For every edge `a→b`, `start(b) > finish(a)` across a DAG with a diamond and a long chain; assert on the event log. | Implemented and unit-tested |
| 4.7 | Global / per-provider / per-account concurrency limits | MM6 | M | A max-observed-concurrency counter never exceeds each limit, with more ready tasks than slots, at 8 and 32 streams. Assert the peak counter, not a duration. | Mock-only |
| 4.8 | **E8**: approvals and interrupts are routed by task id | — | U | With two tasks waiting, approving task A's request leaves task B still waiting and does not execute B's call. Per-task history, not a shared mutex. The negative case is the whole test. | Implemented and unit-tested |
| 4.9 | **E9**: no lost wakeup on the approval channel | — | U | Deliver the reply before the waiter parks (force the interleaving deterministically, e.g. a test-only barrier), then assert the waiter still observes it. Looping 10 000 times and hoping is not a test — see §D5. | Implemented and unit-tested |
| 4.10 | Cancellation propagates to child tasks | MM7 | U | Cancelling a parent moves every descendant to `cancelled`, including a `waiting-approval` child, and no descendant starts afterwards. | Implemented and unit-tested |
| 4.11 | One failed task does not erase completed artifacts | MM8 | U | After a mid-DAG failure, the artifacts of completed tasks are byte-identical to before and still addressable. | Implemented and unit-tested |
| 4.12 | Resume restores pending/running without replaying unknown-outcome side effects | MM9 | I | Kill mid-run with a shell tool call in flight; on resume the call is not re-executed and the task is marked needing confirmation. Real store on disk. | Implemented and integration-tested |
| 4.13 | Two workers cannot silently overwrite one file | MM10 | I | Real temp FS, two workers, same path: the second write is **refused with a typed conflict**, not merged, not last-write-wins. Assert the refusal and the file content. | Implemented and integration-tested |
| 4.14 | Worktree / ownership isolation on Windows | — | I | Long path (>260 chars), a file held open by another handle, and a path differing only in case. Each handled or refused explicitly. See §C4. | Implemented and integration-tested |
| 4.15 | Usage charged to the correct task/agent/model/account | MM11 | M | Mock providers report distinct usage; assert per-task/agent/model/account attribution equals the per-provider ground truth, and that a non-reporting provider yields `not reported`, never 0. | Mock-only |
| 4.16 | Budget denial stops or asks before further spend | MM12 | M | At the cap, the next call is **not issued** (assert the mock received no request) and the user is asked. Asserting only that a warning was logged proves nothing. | Mock-only |
| 4.17 | Full team run: ≥3 roles, ≥2 providers, concurrency, review stage, conflict-safe integration, inspectable record | — | M + T | The charter's own Phase 4 gate, as one recorded run with an inspectable record. | Mock-only + Manually verified on Windows Terminal |
| 4.18 | Team monitor panel reflects real scheduler state | TU5, TU7 | U | Panel rows are derived from the scheduler's state set; a task in `blocked` cannot render as `running`, and an empty team renders empty, not fabricated. | Implemented and unit-tested |

Coverage of charter §13: SC1 1.2/1.14 · SC2 1.3 · SC3 1.4/3.1 · SC4 1.8 · SC5 1.9 · SC6 1.10 · SC7 1.11 · SC8 1.1 · SC9 1.5/1.6/1.7 · AS1 3.1/3.4 · AS2 3.3 · AS3 2.15/3.11 · AS4 3.5 · AS5 3.2 · AS6 3.6/3.7 · AS7 3.8 · MM1 4.1 · MM2 4.2 · MM3 4.3/4.4 · MM4 4.5 · MM5 4.6 · MM6 4.7 · MM7 4.10 · MM8 4.11 · MM9 2.13/4.12 · MM10 4.13 · MM11 4.15 · MM12 4.16 · TU1 1.13/1.14 · TU2 1.15 · TU3 1.16 · TU4 1.17 · TU5 1.18/4.18 · TU6 1.7/1.20 · TU7 1.19/4.18. All 35 mapped.

---

## C. What `cargo test` cannot prove

Stated plainly, because a green suite will otherwise be read as covering these.

### C1. Terminal key delivery in Windows Terminal
`crates/se-tui/src/input.rs` constructs `KeyEvent`s in-process. That proves the *mapping* from event to action. It cannot prove Windows Terminal emits the event: Shift+Enter, Ctrl+Enter, Ctrl+J and Alt+Enter are delivered differently by terminal, conhost mode and keyboard layout, and some are indistinguishable from plain Enter.

**Verified instead by:** a diagnostic in the release binary (name proposed, e.g. `se doctor --keys`) in the release binary that prints each received key's raw code and modifiers, run manually in Windows Terminal, and a recorded table naming terminal version, OS build and layout. Output committed under `docs/development/`. Only this earns `Manually verified on Windows Terminal`. Any shortcut not in that table must not be advertised in the UI.

### C2. Live provider compatibility
All 63 `se-provider` tests use in-process servers (e.g. the local `serve_slowly` harness in `crates/se-provider/src/openai_compatible.rs`). They prove SE's parsing and framing against *SE's idea* of the wire format. They cannot detect a provider that renamed a field, changed an error shape, or streams differently.

**Verified instead by:** one authorized, minimal, low-token live call per service, recorded (request shape, status, first frames, usage block), labelled `Live-provider verified` **per service**. Until then every provider row reads `NOT LIVE-VERIFIED` — including rows whose unit tests are green. No live call without explicit user authorization.

### C3. Concurrency behaviour under real load
Unit tests can prove the *invariants* (peak concurrency ≤ limit, routing by task id, no lost wakeup when the interleaving is forced). They cannot prove behaviour at 32 real streams with real latency variance, socket limits and Windows thread-pool scheduling. Timing-based concurrency tests are the standard source of flakes (§D).

**Verified instead by:** a separate, non-blocking load harness (name proposed, e.g. `se bench team`; mock providers with randomized delays, N=32, repeated runs) that reports peak concurrency, queue depth and any invariant violation. Invariant violations fail. Durations are reported, never asserted. Run before a phase gate, not on every change.

### C4. Windows filesystem behaviour for worktree / file isolation
Path length limits, case-insensitive but case-preserving names, mandatory file locking (an open handle blocks delete/rename where POSIX allows it), junctions/symlinks needing privilege, reserved names (`CON`, `NUL`, `AUX`), and trailing-dot/space normalization. None of this is visible to logic tests with mocked I/O.

**Verified instead by:** integration tests that touch a real temp directory on this machine (rows 2.10, 4.13, 4.14) — runnable under `cargo test` only because they are *not* mocked; plus a manual check of `git worktree add` under a >260-char path with long paths disabled. Any case that cannot be made deterministic is documented as a known limitation rather than asserted.

---

## D. Known flaky / unprovable tests in this repo

Each verified against the code. **Flake rates are not measured** — these are risks by inspection. No test was looped to establish a rate; doing so is the first task of the Phase 1 gate work.

### D1. 150 ms wall-clock threshold in the streaming-order test
`crates/se-provider/src/openai_compatible.rs:1749` — `deltas_arrive_as_the_provider_sends_them_not_all_at_the_end`, `const GAP = 150ms`. Three frames at 150 ms gaps; asserts `arrivals[0] < GAP * 2` (300 ms) and `arrivals[2] - arrivals[0] >= GAP`.

- **False failure:** `started` is taken before the HTTP request, so TCP connect, thread spawn and server startup sit inside the 300 ms budget. On a loaded machine or a cold first run this can exceed 300 ms with correct code. Real risk, moderate.
- **False pass, which matters more:** an implementation lagging exactly one frame behind (delivering delta *n* only when frame *n+1* arrives) produces arrivals at ≈150/300/450 ms. `150 < 300` passes and `spread = 300 >= 150` passes. **The test does not catch a one-frame buffering bug** — and its own comment says it exists precisely because a buffered implementation was observed in a manual run. The assertion is weaker than its stated rationale.
- **Fix:** keep the test (the behaviour is real and worth guarding) but assert *relative to the server's own send times* rather than the client clock, and tighten to catch one-frame lag: delta 1 must arrive before frame 2 is sent. Server-reported send timestamps make this a causality assertion instead of a stopwatch assertion.

### D2. 80 ms turn deadline
`crates/se-core/src/provider_runner.rs:845` — `a_turn_that_outruns_its_deadline_is_incomplete_not_completed`: a never-stopping mock at 5 ms/frame against an 80 ms deadline (`with_turn_deadline` at line 848), expecting `Outcome::Incomplete` with non-empty `partial_text`.

The mock (`ScriptedProvider::stream`, `provider_runner.rs:350`–380) checks cancellation, sleeps `per_frame_delay`, re-checks, then emits. So if the process stalls for >80 ms before the first frame lands, the deadline fires with an empty answer, the runner returns `Outcome::Failed` (`provider_runner.rs:245`–249), and the test panics with `"a deadline must not report success"` — a **misleading message for a timing flake**, which is how such flakes get misdiagnosed as real defects. Risk is low on an idle machine (16 frames fit in 80 ms) and real under parallel `cargo test` on a loaded box.

The sibling at `provider_runner.rs:867` (1 ms deadline, 50 ms/frame) is **not** at the same risk: the mock sleeps 50 ms before its second cancellation check, so the 1 ms deadline is always observed first and `Failed` is deterministic. That one is sound.

- **Fix:** make the deadline logical, not wall-clock — inject the clock, or have the mock emit K frames before the deadline is allowed to fire. Failing that, widen the ratio (deadline ≫ frame interval) and make the panic message name timing as a probable cause.

Separately, the second 80 ms at `provider_runner.rs:660` (`std::thread::sleep(80ms)` before `interrupt`) in `interrupting_a_stream_reports_cancelled_not_a_partial_answer` is a sleep-then-act, not an assertion, so it cannot fail early — but it hard-codes 80 ms of dead time and, at 200 frames × 10 ms, is not *guaranteed* to interrupt mid-stream on a very slow machine. Low risk, slow by construction.

### D3. Cancel-vs-deadline precedence — asserted only in a comment
`crates/se-core/src/provider_runner.rs:239`–244 is a six-line comment explaining that a fired deadline and a user cancellation are indistinguishable to `stream()` (both arrive as the same closure returning `true`), that `is_cancelled` is the tie-breaker, and that cancellation must always win. The logic is line 245: `if deadline_fired.load(...) && !sink.is_cancelled()`.

**Confirmed: there is no test.** The test functions touching either concept are `interrupting_a_stream_reports_cancelled_not_a_partial_answer` (line 650, no deadline set), `a_turn_that_outruns_its_deadline_is_incomplete_not_completed` (845, no cancel), `a_deadline_that_fires_before_any_token_is_a_failure_not_a_partial_answer` (867, no cancel), `a_zero_deadline_is_refused_at_construction` (882). None sets a deadline *and* cancels.

This is the worst of the three items: not a flaky test but **a load-bearing branch with zero coverage**, documented well enough that a reader will believe it is verified. By inspection, deleting `&& !sink.is_cancelled()` leaves all 189 tests still passing — none of them sets both a deadline and a cancel — and turns every user interrupt near a deadline into a reported timeout — a silently wrong user-visible outcome. Owner: Phase 2, row 2.8. The test must be deterministic (cancel flag already set before the deadline check, not a race), and and its adequacy must be demonstrated on a scratch copy of the file with line 245 mutated, never by mutating committed production code.

### D4. Other wall-clock / race-dependent tests found in a sweep (not in the reported three)
- `crates/se-core/src/engine.rs:696` (`for round in 0..150` at line 699) — `a_submission_right_after_a_finished_task_is_never_dropped`, 150 rounds hunting a small window with a 5 s per-round escape hatch. Not flaky in the false-fail sense, but **probabilistic**: it can pass while the race still exists. Record it as such; a pass is not proof. Its doc comment honestly says "run many times, because the gap is small" — that is the right disclosure.
- `provider_runner.rs:466`, `:663`, `:786` — 10 s / 10 s / 5 s drain deadlines with `panic!` on expiry. Correct pattern (generous bound, deterministic failure message); this is the house style new async tests should copy.
- `crates/se-tui/src/state.rs:72` — `CTRL_C_WINDOW = 1500ms` is production behaviour, and `state.rs:451`/`:459` test it via state transitions rather than by sleeping. Keep that approach.

### D5. Policy for this repo
1. A timing assertion is permitted only when it asserts **causality** (A observed before B was sent) or an **invariant** (peak ≤ limit). A bare duration threshold is not an assertion, it is a bet.
2. A test that forces an interleaving (barrier, injected clock, test-only hook) beats a test that loops and hopes. Loop-and-hope tests must say so in their name or doc comment, as `engine.rs:696` does.
3. Flakes are defects with owners and dates: quarantined to a separately-run target, fixed or deleted within a week, never retried green in the gate. There is no CI today, so the quarantine mechanism must be built alongside the CI in Phase 1.
4. Measure before claiming: a test suspected flaky is looped (≥50 runs, including under `cargo test` parallelism on a loaded machine) and the pass rate recorded. Nothing in §D has such a number yet, and this document says so rather than guessing.

---

## E. Quality gates

### Evidence rules, binding on every phase
1. **A self-report is not evidence.** "Implemented and tested" without a recorded command and exit code is an unverified claim. Each gate item needs: the exact command, its exit code, and the date and commit it ran at.
2. **A passing test that does not assert the behaviour in question proves nothing.** Before a gate item is accepted, the test must be shown to fail when the behaviour is broken — by reverting the implementation line, or by hand-mutating it once and recording that the test went red. §D3 is the standing example: 189 green tests, one unguarded branch.
3. **Negative cases are mandatory** for every authorisation, validation and budget path. A suite that only shows the system working does not show it refusing. Rows 2.1, 2.4, 2.7, 4.8, 4.13, 4.16 are negative-case rows, and a gate does not pass without them.
4. **No rounding up.** A check that could not run is reported as *unverified*, separately from *completed*. `cargo test` green plus an unrun clippy is "completed with one unverified check", never "verified".
5. **One label per claim, per charter §15.** Mock evidence never promotes a provider claim past `Mock-only`; a synthetic key event never reaches `Manually verified on Windows Terminal`.
6. **Gate integrity.** A raised timeout, a weakened assertion, an added `#[ignore]`, a deleted test or a widened tolerance is a quality decision and must be surfaced by name in the phase record with who approved it. It must never pass as "a test change".

### Per-phase gate

**Every phase** (charter §13 "Full verification"), each with a recorded exit code:
`cargo test --workspace` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo fmt --all -- --check` · `cargo build --release` · a recorded manual run of the release binary in Windows Terminal. Plus: the workspace test count **did not decrease**, and that phase's new rows are present by name.

**Phase 1 passes when:** rows 1.1–1.20 are green; 1.13 has a committed key-delivery table; 1.1 asserts on the submitted payload; 1.5 is a property test; 1.11's refusal is demonstrated against a real colliding project command; 1.12's `/help` ↔ `se help` diff test exists; and a CI workflow running the five gate commands on every push exists (there is none today, §A). **The CI requirement is this document's proposal, not a charter requirement — it needs a user decision.**

**Phase 2 passes when:** rows 2.1–2.16 are all green, and in particular: 2.1 (E7) was written failing against pre-fix code and that red run is recorded; 2.8 (cancel-vs-deadline) is green and shown red against a mutated `provider_runner.rs:245`; no tool is reachable without a gateway decision (2.5 joins 1:1 with zero unmatched); 2.7 and the transport-redaction bypass are closed; 2.11 restores a transcript from a real on-disk store. A live-provider claim is a separate line, `NOT LIVE-VERIFIED` by default.

**Phase 3 passes when:** rows 3.1–3.11 are all green, and in particular: 3.1 asserts exactly 194 against a committed id list; 3.2 names both skipped files with reasons; 3.4 shows the three source trees byte-identical by hash before and after; 3.6 asserts read counts 0 and 1; rows 3.1/3.2/3.4 read a tree outside this repo (the canonical catalogue under the user profile) and must **fail loudly as `unverified: catalogue root absent`** rather than silently pass where the tree is missing, which is what will happen in any CI container; 3.9 shows a tool outside an agent's `tools` list actually denied by the Phase 2 gateway.

**Phase 4 passes when:** rows 4.1–4.18 are all green — including 4.1 (MM1), 4.6 (MM5), 4.7 (MM6) and 4.15 (MM11), which the sequence's own list omits — the charter's own gate run exists (4.17), *and* 4.3 (byte-identical packets), 4.5 (no majority vote, with a disagreement record), 4.8 (E8), 4.9 (E9, forced interleaving not a loop), 4.10, 4.11, 4.12, 4.13 (refusal, not merge) and 4.16 (no request issued at the cap) are each green with their negative case; the load harness (§C3) reports zero invariant violations at N=32; and every provider-facing row is labelled `Mock-only`.

---

## F. Where `IMPLEMENTATION_SEQUENCE.md` gates are too weak

Blunt, per the ask. Line references are to `docs/architecture/IMPLEMENTATION_SEQUENCE.md`.

1. **"`cargo test --workspace` green" is not a gate.** It is satisfied by deleting tests, by adding none, and by tests that assert nothing. Every phase's gate opens with this command (line 5). It must be paired with: test count non-decreasing, the phase's named rows present, and at least one demonstrated red-then-green per security row. As written, Phase 2 could ship the permission gateway with zero new tests and pass.

2. **No gate names an E7, E8 or E9 regression test.** The file argues correctly and at length that the gateway cannot be built before E7 (line 18) and the scheduler not before E8/E9 (line 19) — then the Phase 2 gate (line 52) asks only that "a configured native provider executes a real coding task with tool approvals", and the Phase 4 gate asks for a team run. Neither requires the specific test that proves the specific defect is gone. A gateway that happily approves the wrong call still executes a real coding task with tool approvals. **This is the single biggest hole in the sequence.**

3. **Phases 2 and 3 have a gate but no acceptance-test list.** Phases 1 (line 41) and 4 (line 75) list acceptance tests; Phases 2 and 3 do not. The two phases without lists are the ones containing the security core (gateway, tools, persistence) and the catalogue-integrity work. Inverted priority.

4. **Phase 4's acceptance list covers 6 of the 12 charter §13 multi-model bullets.** Listed: ensemble packets, cancel propagation, failed-task artifacts, two-worker overwrite, resume, budget denial. **Missing: MM1 (concurrent independent subtasks), MM2 (multiple specialists and providers on one goal), MM4 (evaluator reports disagreement with evidence, never majority), MM5 (dependency ordering), MM6 (per-provider/account limits), MM11 (usage attribution).** MM4 is the differentiator's correctness property — the thing separating adjudication from concatenation — and it is absent from the gate entirely.

5. **Phase 1's acceptance list omits SC4, SC6 and half of SC9.** Listed at line 41 and in the Phase-1 criteria table (lines 93–105): picker open, `/mod`, Esc draft, precedence, argument binding, no prompt leakage, one metadata source, key delivery. **Missing: nested/custom command parsing, unknown-command and invalid-argument errors, and Up/Down/Enter/Tab picker navigation.** The gate text claims "`/` is reliable in Windows Terminal" while never requiring that Enter selects the highlighted entry.

6. **Four TUI bullets have no owning phase.** TU2 (multiline editor and paste), TU3 (independent cursor/transcript scroll), TU4 (streaming does not steal viewport position) and TU6 (keybinding hints stay synchronized) appear in charter §13 and in no phase of the sequence. Phase 1 item 2 covers the picker only; Phase 6 says "Windows Terminal manual testing" with no list. They are assigned to Phase 1 in §B above because that is where the composer is built and where they can first regress.

7. **The Phase 3 gate's "verify by hashing the tree before and after" (line 61) is the one strong gate in the file** — falsifiable and cheap. It is the model the other gates should copy and do not.

8. **Phase 4's "live verification marked separately" does not say who marks it or where.** With no CI and no evidence file, "marked separately" is a promise, not a mechanism. The per-service `NOT LIVE-VERIFIED` table in §C2 is where it must live.

9. **"Manual run of the release binary in Windows Terminal" has no pass criteria.** It appears in every phase gate (line 5) with no checklist, so it will be satisfied by "it launched". Each phase needs its own named manual checklist, and the output must be committed — a manual run leaves no artifact otherwise.

10. **Phase 6 is where "full QA matrix" and "concurrency benchmarks" live** (line 87). Deferring the QA matrix to the last phase means Phases 1–5 ship against ad-hoc judgement. That is what this document replaces.
