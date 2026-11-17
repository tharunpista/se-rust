# Contributing

## Before you open a pull request

All four must pass; CI runs the same commands on Windows, Linux and macOS.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

## House rules

- **Every behaviour change comes with a test that fails without it.** For a
  bug, write the failing test first.
- **No timing-based tests.** Prove ordering from recorded events, and force
  races deterministically (see `a_reply_delivered_before_the_worker_parks_is_still_observed`).
- **No `unwrap()`/`expect()` on anything a user, a file, the network or the
  OS controls.** `expect` is for invariants the code itself guarantees, with
  a message saying which.
- **Fail closed.** A permission, audit or credential path that cannot decide
  denies.
- **Known limits are written down**, as a `shortcut: <the limit>, <when to
  upgrade>` comment at the code, not left implicit.
- `crates/se-core/src/runner.rs` is committed with CRLF line endings; keep
  them.

## Where things live

| Crate | Owns |
|---|---|
| `se-protocol` | Events and operations between the core and any interface |
| `se-provider` | Vendor wire formats, credentials, the provider registry |
| `se-permission` | The permission gateway, rules, tokens, audit contract (pure, no I/O) |
| `se-catalogue` | Reading the specialist catalogue |
| `se-command` | Slash-command registry |
| `se-core` | The engine, tool loop, routing, team runs |
| `se-tui` | The terminal interface |
| `se` | The binary: wiring only |

Plan and status: `docs/architecture/IMPLEMENTATION_SEQUENCE.md`.
