//! Ruleset evaluation: last-matching-rule-wins, per
//! `packages/schema/src/permission.ts@9bbb8ce` as named in
//! `docs/architecture/SE_TARGET_ARCHITECTURE.md`. A ruleset with no
//! matching rule defaults to [`Effect::Ask`], not `Allow` — an
//! unconfigured gateway must never be a silent grant.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone)]
enum Pattern {
    Any,
    Exact(String),
    /// Resource scopes are paths; a prefix match covers "everything
    /// under this directory" without pulling in a glob library for
    /// one operator.
    Prefix(String),
}

impl Pattern {
    fn matches(&self, value: &str) -> bool {
        match self {
            Pattern::Any => true,
            Pattern::Exact(s) => s == value,
            Pattern::Prefix(p) => value.starts_with(p.as_str()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    agent: Pattern,
    tool: Pattern,
    resource: Pattern,
    effect: Effect,
}

impl Rule {
    pub fn allow_all() -> Self {
        Self {
            agent: Pattern::Any,
            tool: Pattern::Any,
            resource: Pattern::Any,
            effect: Effect::Allow,
        }
    }

    pub fn deny_all() -> Self {
        Self {
            agent: Pattern::Any,
            tool: Pattern::Any,
            resource: Pattern::Any,
            effect: Effect::Deny,
        }
    }

    pub fn deny_tool(tool: impl Into<String>) -> Self {
        Self {
            agent: Pattern::Any,
            tool: Pattern::Exact(tool.into()),
            resource: Pattern::Any,
            effect: Effect::Deny,
        }
    }

    /// Allows exactly `tool` on exactly `resource`, for any agent. How a
    /// team file pre-approves the vendors it names (`use_provider`).
    pub fn allow_tool_on(tool: impl Into<String>, resource: impl Into<String>) -> Self {
        Self {
            agent: Pattern::Any,
            tool: Pattern::Exact(tool.into()),
            resource: Pattern::Exact(resource.into()),
            effect: Effect::Allow,
        }
    }

    pub fn allow_resource_prefix(agent: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            agent: Pattern::Exact(agent.into()),
            tool: Pattern::Any,
            resource: Pattern::Prefix(prefix.into()),
            effect: Effect::Allow,
        }
    }

    fn matches(&self, agent_id: &str, tool_name: &str, resource_scope: &str) -> bool {
        self.agent.matches(agent_id)
            && self.tool.matches(tool_name)
            && self.resource.matches(resource_scope)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Ruleset {
    rules: Vec<Rule>,
}

impl Ruleset {
    pub fn from_rules(rules: Vec<Rule>) -> Self {
        Self { rules }
    }

    /// Last-matching-rule-wins: later entries override earlier ones,
    /// so an operator's narrow deny appended after a broad allow
    /// takes effect, matching the ordering convention named in the
    /// architecture doc rather than an arbitrary pick of this crate's.
    pub fn evaluate(&self, agent_id: &str, tool_name: &str, resource_scope: &str) -> Effect {
        self.rules
            .iter()
            .rev()
            .find(|r| r.matches(agent_id, tool_name, resource_scope))
            .map(|r| r.effect)
            .unwrap_or(Effect::Ask)
    }

    /// Like [`evaluate`](Self::evaluate), but first intersects with a
    /// specialist's own `tools:` allowlist (Phase 4 item 4: "intersect
    /// with a ceiling, never union"). A tool absent from `allowed` is
    /// `Deny` regardless of what this ruleset would otherwise say --
    /// the allowlist can only narrow what the operator's ruleset
    /// already permits, never grant more. `None` means no specialist
    /// is narrowing this call, so the ruleset alone decides.
    pub fn evaluate_scoped(
        &self,
        agent_id: &str,
        tool_name: &str,
        resource_scope: &str,
        allowed_tools: Option<&[String]>,
    ) -> Effect {
        if let Some(allowed) = allowed_tools {
            if !allowed.iter().any(|t| t == tool_name) {
                return Effect::Deny;
            }
        }
        self.evaluate(agent_id, tool_name, resource_scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_ruleset_defaults_to_ask() {
        let rs = Ruleset::from_rules(vec![]);
        assert_eq!(rs.evaluate("any", "write", "/x"), Effect::Ask);
    }

    #[test]
    fn later_rule_overrides_earlier_matching_rule() {
        let rs = Ruleset::from_rules(vec![Rule::allow_all(), Rule::deny_tool("shell")]);
        assert_eq!(rs.evaluate("a", "write", "/x"), Effect::Allow);
        assert_eq!(rs.evaluate("a", "shell", "/x"), Effect::Deny);
    }

    #[test]
    fn a_tool_outside_the_specialist_allowlist_is_denied_even_if_the_ruleset_allows_it() {
        let rs = Ruleset::from_rules(vec![Rule::allow_all()]);
        let allowed = vec!["read".to_string()];
        assert_eq!(
            rs.evaluate_scoped("backend-lead", "shell", "/x", Some(&allowed)),
            Effect::Deny
        );
        assert_eq!(
            rs.evaluate_scoped("backend-lead", "read", "/x", Some(&allowed)),
            Effect::Allow
        );
    }

    #[test]
    fn an_allowlisted_tool_still_defers_to_the_base_ruleset() {
        // The allowlist can only narrow, never widen: a tool the
        // specialist may use but the ruleset denies stays denied.
        let rs = Ruleset::from_rules(vec![Rule::deny_tool("shell")]);
        let allowed = vec!["shell".to_string()];
        assert_eq!(
            rs.evaluate_scoped("backend-lead", "shell", "/x", Some(&allowed)),
            Effect::Deny
        );
    }

    #[test]
    fn no_allowlist_means_the_ruleset_alone_decides() {
        let rs = Ruleset::from_rules(vec![Rule::allow_all()]);
        assert_eq!(
            rs.evaluate_scoped("backend-lead", "shell", "/x", None),
            Effect::Allow
        );
    }

    #[test]
    fn resource_prefix_rule_only_matches_under_the_prefix() {
        let rs = Ruleset::from_rules(vec![Rule::allow_resource_prefix(
            "backend-lead",
            "/repo/src",
        )]);
        assert_eq!(
            rs.evaluate("backend-lead", "write", "/repo/src/main.rs"),
            Effect::Allow
        );
        assert_eq!(
            rs.evaluate("backend-lead", "write", "/repo/secrets.env"),
            Effect::Ask
        );
    }
}
