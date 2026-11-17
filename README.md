# SE

A standalone terminal coding agent that runs a **team** of AI specialists on
**several vendors' models at once**, passes each agent's results to the
agents that depend on it, and picks the right specialist for a task by itself.

- **Any vendor, per task.** One run can use Anthropic, OpenAI, Gemini,
  OpenRouter, DeepSeek, Groq, NVIDIA NIM, Ollama or any OpenAI-compatible
  endpoint, with a concurrency cap per vendor.
- **194 specialists.** Name one (`backend-lead`, `qa-lead`, ...) or let SE
  choose: two short multiple-choice questions to the model, then a keyword
  fallback, then a plain turn. It never fails a request for lack of a match.
- **Ensembles that never vote.** Ask several models the same question; an
  evaluator with no tools reports every disagreement with quotes checked
  against each answer, and keeps every full answer. It cannot pick a winner.
- **Safe by default.** Tool paths are confined to the workspace, every
  permission decision is written to `.se/audit.jsonl` (no record, no action),
  two agents cannot silently overwrite one file, and a token budget with a
  reserved share for review stops spending before it is exceeded.
- **Resumable.** Team runs are journaled under `.se/runs/`. After a crash,
  finished work is kept, and a task that was mid-flight is never re-run
  without your say-so.

> **Status: pre-1.0.** Built and tested against mock vendors (473 tests).
> Not yet verified against live provider APIs, and the TUI has not had a
> manual Windows Terminal pass. See
> [`docs/architecture/IMPLEMENTATION_SEQUENCE.md`](docs/architecture/IMPLEMENTATION_SEQUENCE.md)
> for exactly what is done and what is open.

## Install

Requires Rust 1.88 or newer (checked in CI).

```sh
cargo build --release
# binary: target/release/se(.exe)
```

The specialist catalogue is read from `~/SE/se/agents` (override with
`SE_CATALOGUE_ROOT`). Without it, SE still runs plain turns.

## Connect a model

```sh
se providers                                   # what is configured
se connect anthropic --from-env ANTHROPIC_API_KEY   # stored in the OS keyring
se models anthropic                            # proves the key works
```

Keys can also come from the usual environment variables
(`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, ...). `--from-env` keeps a key out of
shell history.

## Use it

```sh
se                                   # interactive TUI
se exec "explain src/main.rs"        # one request, streamed to stdout
se --provider openai --model gpt-5 exec "..."
```

In the TUI, `/` opens the command picker, `/agents` browses the
specialists, `/models <provider>/<model>` switches model, and `/team` runs
and monitors a team file.

## Team runs

```yaml
# team.yaml
max_concurrent: 3
budget_tokens: 400000          # optional
review_reserve_tokens: 50000   # only review/evaluate tasks may spend this
limits:
  openai: 2                    # optional per-vendor cap
tasks:
  - id: plan
    agent: solution-architect
    request: Plan an orders table for the shop.
  - id: db
    deps: [plan]
    ensemble: [anthropic/claude-sonnet-4-5, openai/gpt-5]
    request: Which database fits the orders table?
  - id: build
    agent: backend-lead        # omit `agent` to have SE pick one
    model: openai/gpt-5
    deps: [db]
    request: Implement the schema and the create-order endpoint.
  - id: review
    agent: qa-lead
    review: true
    deps: [build]
    request: Review the change.
```

```sh
se team run team.yaml
se team resume .se/runs/<id>.jsonl
```

Naming a vendor in the file is your consent for that run to use its stored
credential; every tool call still goes through approval. Each task's answer
goes to stdout, progress and a per-task cost table to stderr. See
[`examples/`](examples/) for more.

## Develop

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Architecture: [`docs/architecture/SE_TARGET_ARCHITECTURE.md`](docs/architecture/SE_TARGET_ARCHITECTURE.md).
Contributing: [`CONTRIBUTING.md`](CONTRIBUTING.md). Security reports:
[`SECURITY.md`](SECURITY.md).

## License

Apache-2.0. See [`LICENSE`](LICENSE).
