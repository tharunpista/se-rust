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
