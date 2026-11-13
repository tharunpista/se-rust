# SE — architecture of record

Derived from `SE_RUST_NATIVE_AGENT_ARCHITECTURE_BLUEPRINT.md` (research
snapshot 12 November 2026). This document is the contract the code must
satisfy. Where it states a fact about the previous implementation, that
fact was verified by reading that source before it was removed; where it
states a target, it is labelled as such.

## 1. Why this is a rebuild and not a refactor

The previous Rust workspace worked in parts and failed as a system. Four
findings, each confirmed in its source, explain why the structure itself
had to change rather than the call sites:

1. **The UI owned the agent loop.** `tui/mod.rs::submit_prompt` called
   `agent::run_turn_with_tools` synchronously from inside `handle_key`,
   which ran inside the `event::read()` arm of the event loop. For the
   whole duration of a model call — connect, generation, every tool
   subprocess — the loop never reached `event::poll` again. Nothing the
   user typed was read and Ctrl+C could not be delivered.
2. **Approval ran a second, nested event loop.** `wait_for_approval`
   called `event::read()` directly, competing with the real loop for
   input.
3. **One crate owned everything.** `se-cli` reached 15,022 lines and
   contained the TUI, the agent loop, the tools, routing, orchestration,
   runtime adapters, sessions and project discovery. Nothing prevented
   any layer from reaching into any other, so no boundary could be
   enforced or tested in isolation.
4. **Streaming existed but was unreachable.** `se-model-provider`
   defined `ProviderEvent` and a `stream()` method; no caller in the
   binary ever invoked it. Every response was awaited whole, so the first
   token and the last token arrived at the user at the same moment.

Findings 1, 2 and 4 are the same defect wearing three hats: there was no
boundary between *producing* agent progress and *displaying* it. That
boundary is the first thing this workspace builds, before any provider
exists, because retrofitting it is exactly what failed.

## 2. Invariants

These are not style preferences. A change that breaks one is wrong.

- **I1 — The render/input path never blocks.** No network call, no
  subprocess wait, no file indexing, no lock that a slow operation can
  hold, on the thread that reads input and draws. Enforced structurally:
  the UI crate cannot reach a provider or a process, because it does not
  depend on anything that can.
- **I2 — The UI is an event consumer.** It receives typed events and
  sends typed operations. It never calls into the agent loop.
- **I3 — Queues are bounded.** Every channel has a declared capacity and
  a declared behaviour when full. A slow consumer applies backpressure;
  it never grows memory without limit.
- **I4 — Semantically important events are never dropped.** Visual
  deltas may be coalesced. Approval requests and terminal task states
  may not.
- **I5 — Every turn terminates and reports.** Success, failure,
  cancellation and a panicking worker all produce exactly one terminal
  event, so no view can show a turn as running forever.
- **I6 — Correlated and ordered.** Every event carries session, task and
  turn identity plus a monotonic sequence number, so a transcript can be
  reconstructed and reordering can be detected rather than guessed at.
- **I7 — Evidence, not assertion.** A tool result is recorded as data. A
  command "passed" only if it ran and exited zero. A file "changed" only
  if the filesystem says so. No layer may upgrade a claim into a fact.
- **I8 — Secrets stay in the credential path.** Never in logs, events,
  transcripts, errors or `Debug` output.

## 3. Crate map and dependency direction

Four crates for Phase 1, deliberately few (the blueprint warns against a
micro-crate cathedral). Each exists because it enforces a boundary that
the previous single crate could not.

```text
se            (binary)  wiring, CLI surface, terminal lifecycle
 └── se-tui             ratatui/crossterm views; consumes events, emits ops
      └── se-core       session/task/turn state machine; owns execution
           ├── se-provider   model providers, streaming, credentials
           └── se-protocol   typed ids, operations, events, timings
```

Dependencies point one way only, and `cargo tree` is the test:

- `se-protocol` depends on `serde` and nothing else of ours. It is the
  shared vocabulary; it must stay cheap to compile and free of policy.
- `se-provider` depends on `se-protocol`-free types of its own plus
  `ureq` and `keyring`. It knows nothing about sessions or terminals.
- `se-core` depends on `se-protocol` and `se-provider`. It knows nothing
  about terminals.
- `se-tui` depends on `se-protocol` and `se-core`'s handle type only. It
  must remain unable to perform I/O other than drawing.
- `se` wires them together and owns raw mode, the alternate screen and
  restoration.

Crates added in later phases (`se-provider`, `se-tools`, `se-session`)
attach under `se-core` and must not be reachable from `se-tui`. That
unreachability is how I1 stays true as the system grows.

## 4. Lifecycle vocabulary

Taken from the blueprint; a single model response is not a task.

- **Session** — durable conversation identity and history.
- **Task** — one user-requested unit of work; may span several turns.
- **Turn** — one model response cycle plus any tool execution needed
  before continuing.
- **Operation** (`CoreOp`) — input to the core: submit, interrupt,
  approve, configure, shutdown.
- **Event** (`CoreEvent`) — output from the core: deltas, tool
  lifecycle, approval requests, usage, terminal state.

## 5. Phase plan and gates

Phases follow the blueprint's sequence. A phase is done when its gate
passes with recorded evidence, not when it looks finished.

| Phase | Scope | Gate |
|---|---|---|
| 0 | Decisions, invariants, evidence index | This document; workspace builds; git history exists |
| 1 | Responsive shell: typed core, event loop, input/focus, cancellation | **Passed.** UI stays responsive under a slow turn; deltas render as received; interrupt works; no blocking call in render path; terminal restored after panic |
| 2 | One native provider, streaming, real session | **Passed.** `hi` causes exactly one model request; deltas arrive as sent (measured, not assumed); ttft recorded separately; credentials in OS storage |
| 3 | Typed tool registry, central permission engine, sandboxed execution | Every call schema-validated and policy-checked; timeouts and output caps; results recorded with exit codes |
| 4 | Durable sessions, resume, crash recovery | Interrupted tasks recover as interrupted, never as completed |
| 5 | Specialist execution with output contracts and independent verification | Selected specialist demonstrably invoked; claims cite evidence; a verifier can reject |
| 6 | External runtime adapters, provider ecosystem | Adapter uses the vendor's supported interface; runtime vs. provider never conflated |
| 7 | Project intelligence, lazy MCP, context budgets | No whole-repository injection; MCP starts on demand |
| 8 | Hardening: cross-platform, security, perf regression, fault injection | Benchmarks recorded before/after; no fabricated performance claims |

## 6. Latency budget (targets, not promises)

Measured locally, excluding model time, on Windows 11 / PowerShell 7.
Model latency is reported separately as TTFT and tokens/sec; a remote
model is never benchmarked by timing only the final response.

| Operation | Target |
|---|---|
| Local command dispatch after process start | p95 < 10 ms |
| TUI input feedback | p95 < 16 ms |
| Render of a received delta | within one frame |
| UI responsiveness during a model wait | never blocked; input always usable |

## 6a. Measured, not assumed

Recorded on this machine (Windows 11, PowerShell, release build) so
later changes have a baseline to be compared against:

| Measurement | Result |
|---|---|
| `se doctor` internal startup span | 214µs |
| Full process round trip (`se exec`, 30 runs) | ~36ms each, dominated by Windows process creation |
| Delta arrival through the release binary, against a server pausing 300ms per word | +0.02s, +0.32s, +0.62s, +0.92s |
| Time to first token, recorded separately from generation | yes, `Stage::TimeToFirstToken` |

The streaming figure is the one that matters: a buffered implementation
produces an identical final string, so incrementality is asserted on
arrival times, both in `se-provider`'s tests and by hand against the
real executable.

## 7. Evidence classes

Used in commit messages and reports, per the blueprint:

- **VERIFIED** — read in current source, or reproduced by running it.
- **DOCUMENTED** — stated by official product documentation.
- **OBSERVED** — reproduced by running an official CLI and recording it.
- **PROPOSAL** — engineering judgement. Never written as fact about a
  third party.
