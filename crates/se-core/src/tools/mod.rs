//! Native tools, callable only with proof that a request already went
//! through `se_permission::Approved::verify_and_consume`.
//!
//! [`Granted`] is that proof. Its only constructor consumes an
//! `Approved` token against a `NonceLedger`; there is no way to obtain
//! one without a successful verify, so a tool function that takes
//! `&Granted` cannot be called without one — the compiler refuses the
//! call, not a runtime check buried inside the function body. This
//! crate never relaxes `se-permission`'s own API to make that easier;
//! `Granted` wraps it.
//!
//! Every tool additionally re-resolves the OS-level identity of the
//! path it is about to touch and compares it against the identity the
//! token was approved for ([`require_fresh_identity`]). `verify_and_consume`
//! already re-checks the token's bound fields against the
//! `ExecutionContext` the caller constructs, but that `ExecutionContext`
//! is itself just data the caller assembled; re-resolving the path at
//! the instant of filesystem access is what closes the remaining gap
//! between "the token still matches what I said I'd do" and "the
//! filesystem still agrees, right now."

pub mod identity;
pub mod shell_env;

mod edit;
mod read;
mod search;
mod shell;
mod write;

pub use edit::edit_file;
pub use read::read_file;
pub use search::{search_text, SearchMatch};
pub use shell::{run_shell, ShellOutput};
pub use write::write_file;

use se_permission::{Approved, ExecutionContext, NonceLedger, ResourceIdentity, VerifyError};
use std::io;
use std::path::Path;

/// Compiler-enforced proof of a successful `Approved::verify_and_consume`.
/// Fields are private; the only constructor is [`Granted::new`], which
/// takes ownership of the `Approved` token (so the same token cannot be
/// handed to a second tool call) and performs the verify itself.
pub struct Granted {
    ctx: ExecutionContext,
}

impl Granted {
    /// Consumes `approved` against `ledger`, checked against `ctx`. The
    /// only way to get a `Granted` back is for that check to succeed —
    /// there is no other path to construct one.
    pub fn new(
        approved: Approved,
        ctx: ExecutionContext,
        ledger: &NonceLedger,
    ) -> Result<Self, VerifyError> {
        approved.verify_and_consume(&ctx, ledger)?;
        Ok(Self { ctx })
    }

    pub fn resource(&self) -> &ResourceIdentity {
        &self.ctx.resource
    }

    pub fn tool_name(&self) -> &str {
        &self.ctx.tool_name
    }

    pub fn cwd(&self) -> &str {
        &self.ctx.cwd
    }

    pub fn env_subset(&self) -> &[(String, String)] {
        &self.ctx.env_subset
    }
}

#[derive(Debug)]
pub enum ToolError {
    /// The path a tool is about to touch no longer resolves to the
    /// identity the token was approved for — a swap between approval
    /// and execution, or a `Granted` being used for the wrong path.
    ResourceMismatch,
    Io(io::Error),
}

impl From<io::Error> for ToolError {
    fn from(e: io::Error) -> Self {
        ToolError::Io(e)
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::ResourceMismatch => write!(
                f,
                "resource identity at execution time does not match the approved token"
            ),
            ToolError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ToolError {}

/// Re-resolves `path`'s current OS-level identity and checks it
/// against `granted`'s. Every tool that acts on an existing path calls
/// this before touching the filesystem.
///
/// A path that does not exist yet (a `write` creating a new file) has
/// no identity to compare, so there is nothing this check can catch
/// there — new-file creation is accepted without it, documented here
/// rather than silently skipped.
pub(crate) fn require_fresh_identity(granted: &Granted, path: &Path) -> Result<(), ToolError> {
    if !path.exists() {
        return Ok(());
    }
    let actual = identity::resolve_resource_identity(path)?;
    if &actual == granted.resource() {
        Ok(())
    } else {
        Err(ToolError::ResourceMismatch)
    }
}

/// Shared test scaffolding: a real `Gateway::decide` → `Granted::new`
/// path, not a hand-built `Granted`, so every tool test exercises the
/// actual gateway rather than a shortcut that could drift from it.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use se_permission::{AuditRecord, AuditSink, Decision, Gateway, Request, Rule, Ruleset};
    use se_protocol::{SessionId, TaskId};

    pub struct NullSink;
    impl AuditSink for NullSink {
        fn record(&self, _r: AuditRecord) -> Result<(), String> {
            Ok(())
        }
    }

    /// Builds a `Granted` for `tool_name` acting on `path`, resolving
    /// `path`'s real OS identity exactly as the token-minting side
    /// would, with `cwd` the directory the tool will run from.
    pub fn granted_for(
        tool_name: &str,
        path: &Path,
        cwd: &Path,
        env_subset: Vec<(String, String)>,
        ledger: &NonceLedger,
    ) -> Granted {
        let resource = identity::resolve_resource_identity(path).unwrap();
        let req = Request {
            agent_id: "test-agent".into(),
            agent_definition_hash: "h".into(),
            session: SessionId::from_raw(1),
            task: TaskId::from_raw(1),
            tool_name: tool_name.into(),
            tool_schema_version: 1,
            resource: resource.clone(),
            resource_scope: path.to_string_lossy().into_owned(),
            cwd: cwd.to_string_lossy().into_owned(),
            env_subset,
            shown_to_human: format!("{tool_name} {}", path.display()),
        };
        let ruleset = Ruleset::from_rules(vec![Rule::allow_all()]);
        let gw = Gateway::new(&ruleset, ledger, &NullSink);
        let Decision::Allow(token) = gw.decide(&req) else {
            panic!("expected allow");
        };
        let ctx = ExecutionContext {
            agent_id: req.agent_id.clone(),
            agent_definition_hash: req.agent_definition_hash.clone(),
            session: req.session,
            task: req.task,
            tool_name: req.tool_name.clone(),
            tool_schema_version: req.tool_schema_version,
            resource,
            cwd: req.cwd.clone(),
            env_subset: req.env_subset.clone(),
        };
        Granted::new(*token, ctx, ledger).unwrap()
    }
}
