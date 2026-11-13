//! Model routing precedence (charter §6.6).
//!
//! The six levels are: explicit task override, specialist-specific
//! profile, team profile, department default, global default, clear
//! failure with actionable guidance. Only two of those six have
//! anything to read from today: the task override travels on
//! [`UserTurn::model`], and the global default is whatever a
//! `ProviderRunner` was constructed with at startup (today's
//! `--provider`/`--model` CLI flags, resolved once).
//!
//! The specialist, team and department levels have no store yet --
//! Phase 3 and Phase 4 add the catalogue and the team profile this
//! would read from. [`resolve`] accepts all five input levels now, so
//! adding that store later is a caller passing `Some(...)` instead of
//! `None`, not a change to this function's signature or the precedence
//! it encodes. No profile registry, no routing-policy trait: a
//! `None` at a level too early to exist is the honest state, not a
//! placeholder to work around.

use se_protocol::ModelRef;

/// Resolves a model for one turn, by precedence, high to low.
///
/// Returns the first level that is `Some`. If every level is `None`,
/// returns an actionable error naming what to do rather than a bare
/// "not configured" -- the clear-failure-with-guidance that charter
/// §6.6 names as the sixth level, not merely an absence of a result.
pub fn resolve(
    task_override: Option<&ModelRef>,
    specialist: Option<&ModelRef>,
    team: Option<&ModelRef>,
    department: Option<&ModelRef>,
    global: Option<&ModelRef>,
) -> Result<ModelRef, String> {
    task_override
        .or(specialist)
        .or(team)
        .or(department)
        .or(global)
        .cloned()
        .ok_or_else(|| {
            "no model is configured at any level (task, specialist, team, department, or global) \
             -- run `se connect` to configure a provider, or select a model with `/model`"
                .to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, model: &str) -> ModelRef {
        ModelRef {
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    #[test]
    fn an_explicit_task_override_wins_over_every_other_level() {
        let task = model("anthropic", "task-model");
        let specialist = model("openai", "specialist-model");
        let global = model("groq", "global-model");
        assert_eq!(
            resolve(Some(&task), Some(&specialist), None, None, Some(&global)),
            Ok(task)
        );
    }

    #[test]
    fn precedence_falls_through_levels_in_order() {
        let team = model("deepseek", "team-model");
        let department = model("gemini", "department-model");
        let global = model("groq", "global-model");
        // No task override and no specialist: team wins over
        // department and global.
        assert_eq!(
            resolve(None, None, Some(&team), Some(&department), Some(&global)),
            Ok(team)
        );
        // No task, specialist, or team: department wins over global.
        assert_eq!(
            resolve(None, None, None, Some(&department), Some(&global)),
            Ok(department)
        );
    }

    #[test]
    fn the_global_default_is_the_last_resort_before_failing() {
        let global = model("ollama", "llama3");
        assert_eq!(resolve(None, None, None, None, Some(&global)), Ok(global));
    }

    #[test]
    fn nothing_configured_anywhere_is_a_clear_actionable_failure() {
        let err = resolve(None, None, None, None, None).unwrap_err();
        assert!(err.contains("se connect"), "{err}");
        assert!(err.contains("/model"), "{err}");
    }
}
