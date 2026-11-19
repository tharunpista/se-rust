//! A typed registry for SE's `/` commands.
//!
//! This crate holds command metadata and resolves a typed path or alias
//! to a [`CommandSpec`]; it starts nothing and depends on no other SE
//! crate, so a collision rule or a fuzzy-filter rule can be unit-tested
//! without a running core or a terminal. Its one I/O surface is
//! [`discovery`], which reads local command `.md` files -- the same
//! kind of file [`markdown`] already parses from a string -- and is
//! still not a runtime: it never executes a command, only reads the
//! files that describe one. Turning a resolved command into an effect
//! -- a `CoreOp`, a prompt template substitution, a UI action -- is the
//! caller's job, not this crate's.

pub mod builtins;
pub mod discovery;
pub mod markdown;

use std::collections::HashMap;

/// Where a command definition came from. Determines who wins a
/// collision: the more specific source wins, except that nothing may
/// silently shadow a [`CommandSpec::security_critical`] builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Provenance {
    /// Lowest precedence: ships with SE.
    Builtin,
    /// A plugin's bundled commands.
    Plugin,
    /// `~/.se/commands` or equivalent -- the user's own, across projects.
    Global,
    /// `.se/commands` in the current project -- highest precedence.
    Project,
}

impl Provenance {
    fn rank(self) -> u8 {
        match self {
            Provenance::Builtin => 0,
            Provenance::Plugin => 1,
            Provenance::Global => 2,
            Provenance::Project => 3,
        }
    }
}

/// What resolving a command actually produces. The registry does not
/// execute anything; it only says which of these three kinds of effect
/// the caller must now perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerKind {
    /// Opens a panel or otherwise changes only UI state. Never reaches
    /// the core.
    UiAction,
    /// Dispatches a typed operation to the core.
    CoreOp,
    /// Loads a Markdown template and submits the result as a turn.
    PromptTemplate,
    /// Submits the trailing text as a turn routed through one named
    /// catalogue specialist (`path` is that specialist's bare agent
    /// id). Distinct from `PromptTemplate`: there is no template body
    /// to substitute into, and the specialist's own definition --
    /// resolved elsewhere, never by this dependency-free crate --
    /// supplies the system prompt, not the command's own text.
    SpecialistInvoke,
    /// Selects the model for later turns (`path` is `provider/model`).
    /// Built at runtime from what providers actually report.
    ModelSelect,
    /// Connects one provider (`path` is its id): a hidden key entry for an
    /// API provider, the login state for an agent CLI.
    ProviderConnect,
}

/// Builds the stable id for a loaded (non-builtin) command: its
/// provenance and path together.
///
/// `register`'s reload rule treats two specs with the same id as the
/// same command being reloaded, regardless of provenance rank (see its
/// comment) -- which is correct only if a bare path can never collide
/// with a *different* command from a different source. A loaded
/// command's path alone cannot guarantee that (a project file and a
/// global file can legitimately share a path, and should compete on
/// provenance, not silently merge into one "reload"), so this function
/// is the one place a loaded `CommandSpec::id` is built, and every
/// caller that discovers a command from disk must go through it rather
/// than using the bare path as the id.
pub fn loaded_id(provenance: Provenance, path: &str) -> String {
    format!("{provenance:?}:{path}").to_lowercase()
}

/// A registered command's metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Stable identifier, independent of path. Two commands with the
    /// same id are the same command re-registered, not a collision --
    /// true for a builtin's fixed id, and true for a loaded command
    /// only when built through [`loaded_id`], which folds provenance in
    /// so a same-path command from a *different* source gets a
    /// different id and competes on provenance instead of merging.
    pub id: String,
    /// Canonical slash path, without the leading `/`. Namespaced paths
    /// use `:`, e.g. `frontend:component`.
    pub path: String,
    pub aliases: Vec<String>,
    pub category: String,
    pub description: String,
    pub provenance: Provenance,
    pub handler: HandlerKind,
    /// A security-critical builtin (`/permissions`, `/connect`,
    /// `/doctor`, ...) cannot be shadowed by a higher-precedence
    /// source; registering a conflicting command at its path or alias
    /// is refused outright rather than resolved by precedence.
    pub security_critical: bool,
}

/// What registering one command changed, so a caller can report a
/// collision instead of letting one happen silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// No existing command used this path or any of these aliases.
    Added,
    /// A lower-precedence command already there was displaced.
    Replaced { displaced_id: String },
    /// A higher-precedence command already there keeps the path; the
    /// incoming command was not added under that name.
    Shadowed { winner_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// The incoming command would shadow a security-critical builtin.
    /// Refused unconditionally -- there is no precedence that makes
    /// this acceptable.
    SecurityCriticalShadowed { path: String, builtin_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotFound(String),
}

/// The registry. Holds every registered command and an index from
/// every path and alias it currently answers to, to the command that
/// currently owns that name.
#[derive(Debug, Default, Clone)]
pub struct Registry {
    commands: Vec<CommandSpec>,
    /// name (path or alias) -> index into `commands`.
    names: HashMap<String, usize>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one command. A name collision is resolved by
    /// precedence and reported through the return value; it is never
    /// silent. A security-critical builtin can never be displaced,
    /// regardless of the incoming command's provenance.
    pub fn register(&mut self, spec: CommandSpec) -> Result<RegisterOutcome, RegisterError> {
        let names: Vec<String> = std::iter::once(spec.path.clone())
            .chain(spec.aliases.iter().cloned())
            .collect();

        // A security-critical builtin blocks the whole registration if
        // any requested name collides with it, even if other requested
        // names are free -- a command cannot be registered under some
        // of its names and refused under others.
        for name in &names {
            if let Some(&idx) = self.names.get(name.as_str()) {
                let existing = &self.commands[idx];
                if existing.security_critical {
                    return Err(RegisterError::SecurityCriticalShadowed {
                        path: name.to_string(),
                        builtin_id: existing.id.clone(),
                    });
                }
            }
        }

        // Determine the single winner across every requested name: the
        // lowest-precedence existing occupant decides the outcome, so a
        // command that would displace one name but be shadowed on
        // another is reported as the weaker of the two, never as a
        // partial success.
        let mut outcome = RegisterOutcome::Added;
        for name in &names {
            if let Some(&idx) = self.names.get(name.as_str()) {
                let existing = &self.commands[idx];
                // The incoming command takes the name either because it
                // is the same command being reloaded -- same id, per
                // the contract on `CommandSpec::id`, and a reload must
                // not report itself as shadowed by itself -- or because
                // it comes from a more specific source.
                let incoming_wins =
                    existing.id == spec.id || spec.provenance.rank() > existing.provenance.rank();
                if incoming_wins {
                    outcome = match outcome {
                        RegisterOutcome::Shadowed { .. } => outcome,
                        _ => RegisterOutcome::Replaced {
                            displaced_id: existing.id.clone(),
                        },
                    };
                } else {
                    outcome = RegisterOutcome::Shadowed {
                        winner_id: existing.id.clone(),
                    };
                }
            }
        }

        if matches!(outcome, RegisterOutcome::Shadowed { .. }) {
            return Ok(outcome);
        }

        // A displaced command's old slot is left in place rather than
        // removed, so existing indices never shift under a caller
        // holding one; `query` filters dead slots out by checking that
        // the slot's own path still resolves back to it (see below).
        let idx = self.commands.len();
        self.commands.push(spec);
        for name in names {
            self.names.insert(name.to_string(), idx);
        }
        Ok(outcome)
    }

    /// Drops every command `drop` matches (a model list that changed, say).
    /// Slots stay so indices never shift; only the names go, which is what `query` checks.
    pub fn unregister_where(&mut self, drop: impl Fn(&CommandSpec) -> bool) {
        let commands = &self.commands;
        self.names.retain(|_, idx| !drop(&commands[*idx]));
    }

    /// Number of commands actually resolvable right now -- excludes
    /// slots displaced by a later, higher-precedence registration.
    pub fn live_len(&self) -> usize {
        self.commands
            .iter()
            .enumerate()
            .filter(|(idx, c)| self.names.get(&c.path) == Some(idx))
            .count()
    }

    /// Resolves an exact path or alias, ignoring leading `/`.
    pub fn resolve(&self, input: &str) -> Result<&CommandSpec, ResolveError> {
        let key = input.strip_prefix('/').unwrap_or(input);
        self.names
            .get(key)
            .map(|&idx| &self.commands[idx])
            .ok_or_else(|| ResolveError::NotFound(key.to_string()))
    }

    /// Commands whose path, an alias, or the description contains
    /// `query` (case-insensitive), ranked path-prefix matches first,
    /// then alphabetically by path. An empty query returns every
    /// command, in the same order, so "just opened the picker" and
    /// "typed then deleted it" look the same.
    pub fn query(&self, query: &str) -> Vec<&CommandSpec> {
        let needle = query.strip_prefix('/').unwrap_or(query).to_lowercase();
        let mut matches: Vec<&CommandSpec> = self
            .commands
            .iter()
            .enumerate()
            // A displaced command's old slot stays in `commands` (see
            // the comment on `register`), so only an entry whose own
            // path still points back to it is actually live; otherwise
            // a replaced command would keep showing up in every query.
            .filter(|(idx, c)| self.names.get(&c.path) == Some(idx))
            .map(|(_, c)| c)
            .filter(|c| {
                needle.is_empty()
                    || c.path.to_lowercase().contains(&needle)
                    || c.aliases.iter().any(|a| a.to_lowercase().contains(&needle))
                    || c.description.to_lowercase().contains(&needle)
            })
            .collect();
        matches.sort_by(|a, b| {
            let a_prefix = a.path.to_lowercase().starts_with(&needle);
            let b_prefix = b.path.to_lowercase().starts_with(&needle);
            b_prefix.cmp(&a_prefix).then_with(|| a.path.cmp(&b.path))
        });
        matches
    }

    /// Total registered slots, including any displaced by a later,
    /// higher-precedence registration. Prefer [`Registry::live_len`]
    /// unless you specifically need the historical count.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin(id: &str, path: &str, security_critical: bool) -> CommandSpec {
        CommandSpec {
            id: id.to_string(),
            path: path.to_string(),
            aliases: Vec::new(),
            category: "test".to_string(),
            description: format!("the {id} command"),
            provenance: Provenance::Builtin,
            handler: HandlerKind::UiAction,
            security_critical,
        }
    }

    #[test]
    fn a_registered_command_resolves_by_its_exact_path() {
        let mut reg = Registry::new();
        reg.register(builtin("models", "models", false)).unwrap();
        let found = reg.resolve("/models").unwrap();
        assert_eq!(found.id, "models");
        // A leading slash is optional -- both spellings must resolve.
        assert_eq!(reg.resolve("models").unwrap().id, "models");
    }

    #[test]
    fn an_alias_resolves_to_the_same_command_as_its_canonical_path() {
        let mut reg = Registry::new();
        let mut spec = builtin("usage", "usage", false);
        spec.aliases.push("cost".to_string());
        reg.register(spec).unwrap();
        assert_eq!(reg.resolve("/cost").unwrap().id, "usage");
    }

    #[test]
    fn an_unregistered_path_is_reported_not_found() {
        let reg = Registry::new();
        assert_eq!(
            reg.resolve("/nope"),
            Err(ResolveError::NotFound("nope".to_string()))
        );
    }

    #[test]
    fn a_security_critical_builtin_cannot_be_shadowed_by_a_project_command() {
        let mut reg = Registry::new();
        reg.register(builtin("permissions", "permissions", true))
            .unwrap();
        let mut project_cmd = builtin("evil-permissions", "permissions", false);
        project_cmd.provenance = Provenance::Project;
        let err = reg.register(project_cmd).unwrap_err();
        assert_eq!(
            err,
            RegisterError::SecurityCriticalShadowed {
                path: "permissions".to_string(),
                builtin_id: "permissions".to_string(),
            }
        );
        // The builtin still owns the path -- the refused registration
        // left nothing behind.
        assert_eq!(reg.resolve("/permissions").unwrap().id, "permissions");
    }

    #[test]
    fn a_security_critical_refusal_is_total_even_if_only_one_alias_collides() {
        let mut reg = Registry::new();
        reg.register(builtin("doctor", "doctor", true)).unwrap();
        let mut project_cmd = builtin("checkup", "checkup", false);
        project_cmd.provenance = Provenance::Project;
        project_cmd.aliases.push("doctor".to_string());
        assert!(reg.register(project_cmd).is_err());
        // Neither the path nor the alias was registered -- a partial
        // registration would be its own silent collision.
        assert_eq!(
            reg.resolve("/checkup"),
            Err(ResolveError::NotFound("checkup".to_string()))
        );
    }

    #[test]
    fn a_non_critical_collision_is_won_by_the_more_specific_provenance() {
        let mut reg = Registry::new();
        reg.register(builtin("builtin-review", "review", false))
            .unwrap();
        let mut project_cmd = builtin("project-review", "review", false);
        project_cmd.provenance = Provenance::Project;
        let outcome = reg.register(project_cmd).unwrap();
        assert_eq!(
            outcome,
            RegisterOutcome::Replaced {
                displaced_id: "builtin-review".to_string()
            }
        );
        assert_eq!(reg.resolve("/review").unwrap().id, "project-review");
    }

    #[test]
    fn a_lower_precedence_command_registered_after_is_shadowed_not_silently_dropped() {
        let mut reg = Registry::new();
        let mut project_cmd = builtin("project-review", "review", false);
        project_cmd.provenance = Provenance::Project;
        reg.register(project_cmd).unwrap();
        let outcome = reg
            .register(builtin("builtin-review", "review", false))
            .unwrap();
        assert_eq!(
            outcome,
            RegisterOutcome::Shadowed {
                winner_id: "project-review".to_string()
            }
        );
        // The project command still owns the path.
        assert_eq!(reg.resolve("/review").unwrap().id, "project-review");
    }

    #[test]
    fn query_with_an_empty_string_returns_every_command() {
        let mut reg = Registry::new();
        reg.register(builtin("models", "models", false)).unwrap();
        reg.register(builtin("agents", "agents", false)).unwrap();
        assert_eq!(reg.query("").len(), 2);
    }

    #[test]
    fn query_filters_case_insensitively_and_ranks_prefix_matches_first() {
        let mut reg = Registry::new();
        reg.register(builtin("models", "models", false)).unwrap();
        reg.register(builtin("modsettings", "mod-settings", false))
            .unwrap();
        reg.register(builtin("agents", "agents", false)).unwrap();
        let results = reg.query("MOD");
        assert_eq!(results.len(), 2);
        // "models" starts with "mod"; "mod-settings" also starts with
        // "mod" -- both rank above a non-prefix match, and ties break
        // alphabetically, so the order is deterministic.
        assert_eq!(results[0].path, "mod-settings");
        assert_eq!(results[1].path, "models");
    }

    #[test]
    fn query_matches_an_alias_even_when_the_path_does_not_match() {
        let mut reg = Registry::new();
        let mut spec = builtin("usage", "usage", false);
        spec.aliases.push("cost".to_string());
        reg.register(spec).unwrap();
        assert_eq!(reg.query("cost")[0].id, "usage");
    }

    #[test]
    fn re_registering_the_same_id_at_the_same_provenance_and_path_replaces_cleanly() {
        // Not a collision in the adversarial sense: this is a reload of
        // the same command definition, and must not report itself as
        // having been shadowed by itself, nor leak the old slot into a
        // query.
        let mut reg = Registry::new();
        reg.register(builtin("models", "models", false)).unwrap();
        let outcome = reg.register(builtin("models", "models", false)).unwrap();
        assert_eq!(
            outcome,
            RegisterOutcome::Replaced {
                displaced_id: "models".to_string()
            }
        );
        assert_eq!(reg.live_len(), 1);
        assert_eq!(reg.query("").len(), 1);
    }

    #[test]
    fn a_replaced_commands_old_slot_never_appears_in_a_query() {
        let mut reg = Registry::new();
        reg.register(builtin("builtin-review", "review", false))
            .unwrap();
        let mut project_cmd = builtin("project-review", "review", false);
        project_cmd.provenance = Provenance::Project;
        reg.register(project_cmd).unwrap();
        let results = reg.query("review");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "project-review");
    }
}
