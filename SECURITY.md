# Security policy

SE runs model-written tool calls on your machine and holds your provider
API keys, so security reports are taken seriously.

## Reporting a vulnerability

Please **do not open a public issue**. Report privately through the
repository's GitHub security advisory form ("Report a vulnerability"), with
the version or commit, the steps to reproduce, and the impact you observed.
Expect an acknowledgement within 7 days.

## What SE guarantees, and where to look

| Guarantee | Enforced in |
|---|---|
| Tool paths cannot leave the workspace or touch `.se/` (symlinks and junctions resolved) | `se-core/src/tool_loop.rs` (`confine`) |
| No tool runs, and no non-default vendor credential is used, without an allow rule or human approval | `se-permission` (`Gateway`) |
| Every decision is recorded; a decision that cannot be recorded is a denial | `se-permission` (`AuditSink`), `se-core/src/audit_log.rs` |
| Approval tokens are single-use, short-lived and bound to the exact resource identity | `se-permission` (`Approved`, `NonceLedger`) |
| Shell commands get a scrubbed environment: no provider keys or base-URL overrides | `se-core/src/tools/shell_env.rs` |
| Output of one agent reaches another only as fenced data, never as instructions | `se-core/src/team/driver.rs` (`with_upstream`) |
| Keys are stored in the OS keyring and never logged | `se-provider/src/credentials.rs` |

## Known limits

- A `shell` command can write any file in the workspace. It is gated by
  approval, not by the per-task file claims that protect `write` and `edit`.
- Arguments to `shell` are not path-confined.
