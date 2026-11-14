//! The permission gateway.
//!
//! Two responsibilities, kept separate: [`Gateway::decide`] is a pure
//! `(agent, task, tool, resource) -> Decision` function over a ruleset,
//! and [`Approved`] is the token that proves a decision actually
//! happened. The token's fields are the ones named in
//! `docs/architecture/SE_TARGET_ARCHITECTURE.md` §3a, corrected after
//! the Phase 0 security review found that an argument hash binds bytes,
//! not meaning, and survives none of: a relative path resolved against
//! a changed cwd, a symlink swapped between approval and execution, a
//! glob re-expanded against a tree a sibling agent just wrote into, or
//! Windows path aliasing where distinct strings name the same target.
//!
//! This crate has no outbound dependency on execution crates (no `fs`,
//! no `std::process`) and resolves no paths itself: canonicalization
//! and OS-level file identity are the caller's job (`se-core`, which
//! actually touches the filesystem). This crate only compares the
//! already-resolved [`ResourceIdentity`] values for equality, which is
//! what keeps it a pure decision function that can be unit-tested
//! without starting anything.
//!
//! The crate boundary alone enforces none of this. What enforces it is
//! that [`Approved`]'s fields are private and its only constructor is
//! [`Gateway::decide`] (via [`Decision::Allow`]); a tool-executing path
//! in `se-core` that wants to run must hold one at the type level, not
//! by convention.

mod nonce;
mod ruleset;

pub use nonce::{Nonce, NonceLedger};
pub use ruleset::{Effect, Rule, Ruleset};

use se_protocol::{SessionId, TaskId};
use std::time::{Duration, SystemTime};

/// The already-resolved identity of the thing a tool would act on.
///
/// Opaque on purpose: the gateway never resolves a path, so it cannot
/// be tricked by one changing meaning between approval and execution.
/// The caller is expected to build this from OS-level file identity
/// (volume + file id on Windows, device + inode on Unix) for anything
/// path-shaped, not from the path string itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceIdentity(pub String);

/// What a tool call will actually run with, at approval time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub agent_id: String,
    /// Content hash of the agent's own definition, so a request cannot
    /// be replayed under a since-edited agent body.
    pub agent_definition_hash: String,
    pub session: SessionId,
    pub task: TaskId,
    pub tool_name: String,
    pub tool_schema_version: u32,
    pub resource: ResourceIdentity,
    pub resource_scope: String,
    pub cwd: String,
    /// Only the env vars the tool will actually see, not the whole
    /// process environment (that itself is the E10 boundary).
    pub env_subset: Vec<(String, String)>,
    /// The exact bytes shown to the human — not a separately
    /// constructed display string built from the same request.
    pub shown_to_human: String,
}

/// The outcome of [`Gateway::decide`].
#[derive(Debug)]
pub enum Decision {
    Allow(Box<Approved>),
    Deny {
        reason: String,
    },
    /// The ruleset defers to a human. The caller still must not run the
    /// tool until a human approval produces an `Approved` token through
    /// [`Gateway::approve`] (never through this variant directly).
    Ask,
}

/// Proof that a request was decided `Allow` or human-approved, and has
/// not yet been spent. Fields are private; the only way to get one is
/// through [`Gateway`]. The only way to spend one is
/// [`Approved::verify_and_consume`], which re-checks every bound field
/// against the execution-time context rather than trusting the token's
/// mere existence — a token is proof a decision happened, not proof the
/// world hasn't changed since.
#[derive(Debug)]
pub struct Approved {
    agent_id: String,
    agent_definition_hash: String,
    session: SessionId,
    task: TaskId,
    tool_name: String,
    tool_schema_version: u32,
    resource: ResourceIdentity,
    resource_scope: String,
    cwd: String,
    env_subset: Vec<(String, String)>,
    shown_to_human: String,
    nonce: Nonce,
    issued_at: SystemTime,
    ttl: Duration,
}

/// Everything re-verified at execution time. Built by the caller from
/// what it is about to actually do, not copied from the request that
/// produced the token — the whole point is to catch drift between the
/// two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionContext {
    pub agent_id: String,
    pub agent_definition_hash: String,
    pub session: SessionId,
    pub task: TaskId,
    pub tool_name: String,
    pub tool_schema_version: u32,
    pub resource: ResourceIdentity,
    pub cwd: String,
    pub env_subset: Vec<(String, String)>,
}

/// Why a held [`Approved`] token was refused at execution time.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    Expired,
    AlreadyConsumed,
    ContextMismatch,
}

impl Approved {
    /// Re-verifies every bound field against `ctx`, then consumes the
    /// nonce so the same token can never authorize a second execution.
    /// Expiry and single-use are checked before the field comparison so
    /// a stale or replayed token never gets to leak which field would
    /// have mismatched.
    pub fn verify_and_consume(
        self,
        ctx: &ExecutionContext,
        ledger: &NonceLedger,
    ) -> Result<(), VerifyError> {
        if SystemTime::now()
            .duration_since(self.issued_at)
            .unwrap_or(Duration::MAX)
            > self.ttl
        {
            return Err(VerifyError::Expired);
        }
        if !ledger.consume(&self.nonce) {
            return Err(VerifyError::AlreadyConsumed);
        }
        let matches = self.agent_id == ctx.agent_id
            && self.agent_definition_hash == ctx.agent_definition_hash
            && self.session == ctx.session
            && self.task == ctx.task
            && self.tool_name == ctx.tool_name
            && self.tool_schema_version == ctx.tool_schema_version
            && self.resource == ctx.resource
            && self.cwd == ctx.cwd
            && self.env_subset == ctx.env_subset;
        if matches {
            Ok(())
        } else {
            Err(VerifyError::ContextMismatch)
        }
    }

    pub fn shown_to_human(&self) -> &str {
        &self.shown_to_human
    }

    pub fn resource_scope(&self) -> &str {
        &self.resource_scope
    }
}

/// Audit record of a decision, with secrets never present in the first
/// place rather than redacted after the fact: `env_subset` values are
/// dropped, only the variable names kept, since a record that must be
/// kept forever is the wrong place to be trusting a redaction pass.
#[derive(Debug, Clone)]
pub struct AuditRecord {
    pub agent_id: String,
    pub session: SessionId,
    pub task: TaskId,
    pub tool_name: String,
    pub resource_scope: String,
    pub effect: Effect,
}

pub trait AuditSink {
    fn record(&self, record: AuditRecord);
}

/// An audit sink that drops every record. Only for tests and for
/// callers that log elsewhere; the architecture doc leaves the audit
/// sink's durability as an open question, so this crate does not
/// invent a durable one.
pub struct NullAuditSink;

impl AuditSink for NullAuditSink {
    fn record(&self, _record: AuditRecord) {}
}

/// Default token lifetime: long enough to cover a human's approval
/// click, short enough that a forgotten token cannot be executed hours
/// later against a world that has since changed.
pub const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(120);

pub struct Gateway<'a> {
    ruleset: &'a Ruleset,
    ledger: &'a NonceLedger,
    audit: &'a dyn AuditSink,
}

impl<'a> Gateway<'a> {
    pub fn new(ruleset: &'a Ruleset, ledger: &'a NonceLedger, audit: &'a dyn AuditSink) -> Self {
        Self {
            ruleset,
            ledger,
            audit,
        }
    }

    /// Evaluates the ruleset against `req`. Never mints a token on
    /// `Ask` — only [`Gateway::approve`], called after a human actually
    /// approves, does that; this is what keeps a ruleset change from
    /// silently granting something a human already said no to in a
    /// still-open approval dialog.
    pub fn decide(&self, req: &Request) -> Decision {
        self.decide_scoped(req, None)
    }

    /// Like [`decide`](Self::decide), but narrowed by a specialist's
    /// own `tools:` allowlist via [`Ruleset::evaluate_scoped`]. `None`
    /// behaves exactly like `decide`.
    pub fn decide_scoped(&self, req: &Request, allowed_tools: Option<&[String]>) -> Decision {
        match self.ruleset.evaluate_scoped(
            &req.agent_id,
            &req.tool_name,
            &req.resource_scope,
            allowed_tools,
        ) {
            Effect::Allow => {
                self.audit(req, Effect::Allow);
                Decision::Allow(Box::new(self.mint(req)))
            }
            Effect::Deny => {
                self.audit(req, Effect::Deny);
                Decision::Deny {
                    reason: format!(
                        "denied by ruleset: {} may not use {} on {}",
                        req.agent_id, req.tool_name, req.resource_scope
                    ),
                }
            }
            Effect::Ask => {
                self.audit(req, Effect::Ask);
                Decision::Ask
            }
        }
    }

    /// Mints a token for a request a human has just approved out of
    /// band (the `Ask` path). Still goes through the ruleset's audit
    /// trail as an `Allow`, since that is what actually happened.
    pub fn approve(&self, req: &Request) -> Box<Approved> {
        self.audit(req, Effect::Allow);
        Box::new(self.mint(req))
    }

    fn audit(&self, req: &Request, effect: Effect) {
        self.audit.record(AuditRecord {
            agent_id: req.agent_id.clone(),
            session: req.session,
            task: req.task,
            tool_name: req.tool_name.clone(),
            resource_scope: req.resource_scope.clone(),
            effect,
        });
    }

    fn mint(&self, req: &Request) -> Approved {
        Approved {
            agent_id: req.agent_id.clone(),
            agent_definition_hash: req.agent_definition_hash.clone(),
            session: req.session,
            task: req.task,
            tool_name: req.tool_name.clone(),
            tool_schema_version: req.tool_schema_version,
            resource: req.resource.clone(),
            resource_scope: req.resource_scope.clone(),
            cwd: req.cwd.clone(),
            env_subset: req.env_subset.clone(),
            shown_to_human: req.shown_to_human.clone(),
            nonce: self.ledger.issue(),
            issued_at: SystemTime::now(),
            ttl: DEFAULT_TOKEN_TTL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> Request {
        Request {
            agent_id: "backend-lead".into(),
            agent_definition_hash: "hash1".into(),
            session: SessionId::from_raw(1),
            task: TaskId::from_raw(1),
            tool_name: "write".into(),
            tool_schema_version: 1,
            resource: ResourceIdentity("vol1:fid42".into()),
            resource_scope: "/repo/src/main.rs".into(),
            cwd: "/repo".into(),
            env_subset: vec![],
            shown_to_human: "write /repo/src/main.rs".into(),
        }
    }

    fn ctx_from(req: &Request) -> ExecutionContext {
        ExecutionContext {
            agent_id: req.agent_id.clone(),
            agent_definition_hash: req.agent_definition_hash.clone(),
            session: req.session,
            task: req.task,
            tool_name: req.tool_name.clone(),
            tool_schema_version: req.tool_schema_version,
            resource: req.resource.clone(),
            cwd: req.cwd.clone(),
            env_subset: req.env_subset.clone(),
        }
    }

    #[test]
    fn allow_rule_mints_a_token_that_verifies_against_the_same_context() {
        let ruleset = Ruleset::from_rules(vec![Rule::allow_all()]);
        let ledger = NonceLedger::new();
        let gw = Gateway::new(&ruleset, &ledger, &NullAuditSink);
        let r = req();
        let ctx = ctx_from(&r);
        let Decision::Allow(token) = gw.decide(&r) else {
            panic!("expected Allow");
        };
        token.verify_and_consume(&ctx, &ledger).unwrap();
    }

    #[test]
    fn a_token_cannot_be_spent_twice() {
        let ruleset = Ruleset::from_rules(vec![Rule::allow_all()]);
        let ledger = NonceLedger::new();
        let gw = Gateway::new(&ruleset, &ledger, &NullAuditSink);
        let r = req();
        let Decision::Allow(token) = gw.decide(&r) else {
            panic!("expected Allow");
        };
        let nonce = token.nonce;
        let ctx = ctx_from(&r);
        token.verify_and_consume(&ctx, &ledger).unwrap();
        // The token itself is consumed (moved) by the line above; a
        // replayed copy of the same nonce must still find the ledger
        // already spent, which is what actually prevents a second
        // execution from the same approval.
        assert!(!ledger.consume(&nonce));
    }

    #[test]
    fn deny_rule_produces_no_token() {
        let ruleset = Ruleset::from_rules(vec![Rule::deny_all()]);
        let ledger = NonceLedger::new();
        let gw = Gateway::new(&ruleset, &ledger, &NullAuditSink);
        match gw.decide(&req()) {
            Decision::Deny { .. } => {}
            _ => panic!("expected Deny"),
        }
    }

    #[test]
    fn a_token_rejects_a_changed_resource_at_execution_time() {
        let ruleset = Ruleset::from_rules(vec![Rule::allow_all()]);
        let ledger = NonceLedger::new();
        let gw = Gateway::new(&ruleset, &ledger, &NullAuditSink);
        let r = req();
        let Decision::Allow(token) = gw.decide(&r) else {
            panic!("expected Allow");
        };
        let mut ctx = ctx_from(&r);
        // A symlink swapped between approval and execution: the string
        // path can stay identical while the resolved identity changes.
        ctx.resource = ResourceIdentity("vol1:fid999-swapped".into());
        assert_eq!(
            token.verify_and_consume(&ctx, &ledger),
            Err(VerifyError::ContextMismatch)
        );
    }

    #[test]
    fn a_token_rejects_an_agent_whose_definition_changed_since_approval() {
        let ruleset = Ruleset::from_rules(vec![Rule::allow_all()]);
        let ledger = NonceLedger::new();
        let gw = Gateway::new(&ruleset, &ledger, &NullAuditSink);
        let r = req();
        let Decision::Allow(token) = gw.decide(&r) else {
            panic!("expected Allow");
        };
        let mut ctx = ctx_from(&r);
        ctx.agent_definition_hash = "hash2-edited".into();
        assert_eq!(
            token.verify_and_consume(&ctx, &ledger),
            Err(VerifyError::ContextMismatch)
        );
    }

    /// Phase 2b item 6: "the gateway's allow/deny decision for a fixed
    /// (agent, task, tool, resource) is invariant under a model
    /// change."
    ///
    /// `Request` (and `ExecutionContext`, and `Rule`) carry no field
    /// that names a model or a provider at all -- there is no
    /// `model: ModelRef` anywhere in this crate's public API. That is
    /// itself the proof the architecture doc's §3a requirement asks
    /// for (charter §11: model identity must never change what is
    /// authorized): a decision cannot depend on something it has no
    /// way to receive.
    ///
    /// This is not a vacuous test. It does not merely assert the field
    /// is absent by inspection; it drives `Gateway::decide` through
    /// `Ruleset::evaluate` with many requests that are identical except
    /// for every field a *different model choice could plausibly leak
    /// through if `Request` ever grew a model-shaped field* --
    /// `agent_definition_hash`, `cwd`, `env_subset` and
    /// `shown_to_human` all vary per case below, standing in for the
    /// kind of per-model rendering difference (a different prompt
    /// hash, a different declared capability, a different display
    /// string) that a future change might attach to the request. None
    /// of those fields reach `Ruleset::evaluate` (only `agent_id`,
    /// `tool_name` and `resource_scope` do, read directly from its
    /// signature above), so every one of these requests decides
    /// identically. A real "invariant under a model change" guarantee
    /// will stay true only for as long as that stays true of any field
    /// added later -- this test is what would need to be extended, not
    /// silently kept green, the day a model-identifying field is added
    /// to `Request`.
    #[test]
    fn decision_is_invariant_under_every_field_a_model_change_could_plausibly_touch() {
        let ruleset = Ruleset::from_rules(vec![
            Rule::allow_resource_prefix("backend-lead", "/repo/src"),
            Rule::deny_tool("shell"),
        ]);
        let ledger = NonceLedger::new();
        let gw = Gateway::new(&ruleset, &ledger, &NullAuditSink);

        // Stand-ins for "the request as rendered under model A" vs
        // "...under model B": same agent/tool/resource, everything else
        // that text generation could plausibly vary.
        let variants: Vec<Request> = (0..6)
            .map(|i| Request {
                agent_id: "backend-lead".into(),
                agent_definition_hash: format!("hash-rendered-by-model-{i}"),
                session: SessionId::from_raw(i as u64),
                task: TaskId::from_raw(i as u64),
                tool_name: "write".into(),
                tool_schema_version: 1,
                resource: ResourceIdentity(format!("vol1:fid{i}")),
                resource_scope: "/repo/src/main.rs".into(),
                cwd: format!("/repo-as-seen-by-model-{i}"),
                env_subset: vec![(format!("MODEL_{i}_VAR"), "x".into())],
                shown_to_human: format!("model {i} would phrase this differently: write main.rs"),
            })
            .collect();

        let decisions: Vec<&'static str> = variants
            .iter()
            .map(|r| match gw.decide(r) {
                Decision::Allow(_) => "allow",
                Decision::Deny { .. } => "deny",
                Decision::Ask => "ask",
            })
            .collect();
        assert!(
            decisions.iter().all(|d| *d == "allow"),
            "every variant names the same agent/tool/resource, so the decision must be \
             identical regardless of anything that varies only because a different model \
             produced the request: {decisions:?}"
        );

        // Same proof for the denied case: only `tool_name` changes to
        // the one the ruleset's deny rule matches; every model-shaped
        // field still varies per case.
        let denied_variants: Vec<Request> = (0..4)
            .map(|i| Request {
                agent_id: "backend-lead".into(),
                agent_definition_hash: format!("hash-rendered-by-model-{i}"),
                session: SessionId::from_raw(i as u64),
                task: TaskId::from_raw(i as u64),
                tool_name: "shell".into(),
                tool_schema_version: 1,
                resource: ResourceIdentity(format!("vol1:fid{i}")),
                resource_scope: "/repo/src".into(),
                cwd: format!("/repo-as-seen-by-model-{i}"),
                env_subset: vec![],
                shown_to_human: format!("model {i}: run a shell command"),
            })
            .collect();
        for r in &denied_variants {
            assert!(
                matches!(gw.decide(r), Decision::Deny { .. }),
                "a ruleset deny must hold regardless of which model rendered the request"
            );
        }
    }

    #[test]
    fn an_expired_token_is_rejected_even_with_a_matching_context() {
        let ruleset = Ruleset::from_rules(vec![Rule::allow_all()]);
        let ledger = NonceLedger::new();
        let gw = Gateway::new(&ruleset, &ledger, &NullAuditSink);
        let r = req();
        let Decision::Allow(mut token) = gw.decide(&r) else {
            panic!("expected Allow");
        };
        token.ttl = Duration::from_secs(0);
        token.issued_at = SystemTime::now() - Duration::from_secs(1);
        let ctx = ctx_from(&r);
        assert_eq!(
            token.verify_and_consume(&ctx, &ledger),
            Err(VerifyError::Expired)
        );
    }
}
