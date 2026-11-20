//! The built-in command set.
//!
//! This is the single metadata source charter §12 requires: the TUI's
//! `/help` and the CLI's `se help` are both projections of this list,
//! so a command cannot exist in one surface and be missing from the
//! other. Adding a command here is what makes it discoverable in both.
//!
//! A command listed here with [`Availability::Unconfigured`] appears in
//! the picker but says honestly why it cannot run, rather than being
//! hidden (which would make the product look smaller than it is) or
//! appearing to work (which charter §1 forbids outright).

use crate::{CommandSpec, HandlerKind, Provenance};

/// Whether a command can actually run right now, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Ready,
    /// Listed, selectable, and honest: selecting it explains what is
    /// missing instead of pretending to work.
    Unconfigured(&'static str),
    /// Listed so the product's shape is visible, but not built yet.
    /// Charter §15's "designed, not implemented" status.
    NotImplemented(&'static str),
}

/// One builtin's definition, before it becomes a [`CommandSpec`].
pub struct Builtin {
    pub id: &'static str,
    pub path: &'static str,
    pub aliases: &'static [&'static str],
    pub category: &'static str,
    pub description: &'static str,
    pub handler: HandlerKind,
    pub security_critical: bool,
    pub availability: Availability,
}

/// Categories, in the order a picker should group them.
pub const CATEGORIES: &[&str] = &[
    "Session",
    "Models & providers",
    "Agents & skills",
    "Team",
    "Tools & connectors",
    "Diagnostics",
    // Every command `discovery` loads from a project or global
    // directory lands here (`discovery::CATEGORY`), so a picker that
    // groups strictly by this list does not silently drop a loaded
    // command the moment one exists.
    "Custom",
];

/// Every builtin command SE knows about.
///
/// The `NotImplemented` entries are deliberate: charter §3.1 requires
/// every top-level product feature to be reachable from `/`, and §1
/// forbids pretending something works. Listing a planned command with
/// an honest reason satisfies both; hiding it satisfies neither.
pub fn builtins() -> Vec<Builtin> {
    vec![
        // ---- Session -------------------------------------------------
        Builtin {
            id: "help",
            path: "help",
            aliases: &["?"],
            category: "Session",
            description: "List commands and keyboard shortcuts",
            handler: HandlerKind::UiAction,
            security_critical: true,
            availability: Availability::Ready,
        },
        Builtin {
            id: "commands",
            path: "commands",
            aliases: &[],
            category: "Session",
            description: "Browse every registered command and where it came from",
            handler: HandlerKind::UiAction,
            security_critical: true,
            availability: Availability::Ready,
        },
        Builtin {
            id: "new",
            path: "new",
            aliases: &["clear"],
            category: "Session",
            description: "Start a fresh conversation (/clear); the current one stays saved",
            handler: HandlerKind::CoreOp,
            security_critical: false,
            availability: Availability::Ready,
        },
        Builtin {
            id: "sessions",
            path: "sessions",
            aliases: &["resume"],
            category: "Session",
            description: "Resume an earlier conversation (/resume): pick, rename, pin, order, delete",
            handler: HandlerKind::UiAction,
            security_critical: false,
            availability: Availability::Ready,
        },
        Builtin {
            id: "copy",
            path: "copy",
            aliases: &[],
            category: "Session",
            description: "Copy the last answer to the clipboard (/copy N: the Nth from last)",
            handler: HandlerKind::UiAction,
            security_critical: false,
            availability: Availability::Ready,
        },
        Builtin {
            id: "mouse",
            path: "mouse",
            aliases: &[],
            category: "Session",
            description: "Toggle mouse capture (F2): on scrolls with the wheel, off lets you select text",
            handler: HandlerKind::UiAction,
            security_critical: false,
            availability: Availability::Ready,
        },
        Builtin {
            id: "mode",
            path: "mode",
            aliases: &[],
            category: "Session",
            description: "Work mode (Shift+Tab): manual asks before each change, auto runs without asking, plan only reads",
            handler: HandlerKind::UiAction,
            // Auto turns approvals off: a project command must not be able to stand in for it.
            security_critical: true,
            availability: Availability::Ready,
        },
        Builtin {
            id: "exit",
            path: "exit",
            aliases: &["quit"],
            category: "Session",
            description: "Leave SE",
            handler: HandlerKind::UiAction,
            // Protected with /help and /commands: a project must not be able to hide the way out.
            security_critical: true,
            availability: Availability::Ready,
        },
        // ---- Models & providers -------------------------------------
        Builtin {
            id: "models",
            path: "models",
            aliases: &[],
            category: "Models & providers",
            description: "Choose from the models a configured provider actually reports",
            handler: HandlerKind::UiAction,
            security_critical: false,
            availability: Availability::Ready,
        },
        Builtin {
            id: "providers",
            path: "providers",
            aliases: &[],
            category: "Models & providers",
            description: "List providers and their real configuration state",
            handler: HandlerKind::UiAction,
            security_critical: false,
            availability: Availability::Ready,
        },
        Builtin {
            id: "connect",
            path: "connect",
            aliases: &[],
            category: "Models & providers",
            description: "Store a provider credential in OS-native secure storage",
            handler: HandlerKind::CoreOp,
            // Shadowing this would let a project command intercept a
            // credential entry flow.
            security_critical: true,
            availability: Availability::Ready,
        },
        Builtin {
            id: "usage",
            path: "usage",
            aliases: &["cost"],
            category: "Models & providers",
            description: "Token and cost accounting, as reported by the provider",
            handler: HandlerKind::UiAction,
            security_critical: false,
            availability: Availability::Ready,
        },
        // ---- Agents & skills ----------------------------------------
        Builtin {
            id: "agents",
            path: "agents",
            aliases: &[],
            category: "Agents & skills",
            description: "Browse the specialist catalogue",
            handler: HandlerKind::UiAction,
            security_critical: false,
            availability: Availability::Ready,
        },
        Builtin {
            id: "skills",
            path: "skills",
            aliases: &[],
            category: "Agents & skills",
            description: "Browse available skills",
            handler: HandlerKind::UiAction,
            security_critical: false,
            availability: Availability::NotImplemented(
                "the skill loader exists, but SE does not run skills yet (Phase 5)",
            ),
        },
        // ---- Team ----------------------------------------------------
        Builtin {
            id: "team",
            path: "team",
            aliases: &[],
            category: "Team",
            description: "Run a team file: many agents and models at once, with a live monitor",
            handler: HandlerKind::UiAction,
            security_critical: false,
            availability: Availability::Ready,
        },
        // ---- Tools & connectors --------------------------------------
        Builtin {
            id: "permissions",
            path: "permissions",
            aliases: &[],
            category: "Tools & connectors",
            description: "Review and change tool permission rules",
            handler: HandlerKind::UiAction,
            // The whole point of the gateway is that nothing can
            // intercept or redefine the permission surface.
            security_critical: true,
            availability: Availability::NotImplemented(
                "the gateway is built, but there is no rules file yet: in manual mode every tool call asks for approval (see /mode)",
            ),
        },
        Builtin {
            id: "connectors",
            path: "connectors",
            aliases: &["mcp"],
            category: "Tools & connectors",
            description: "Manage MCP connectors and their tool permissions",
            handler: HandlerKind::UiAction,
            security_critical: true,
            availability: Availability::NotImplemented("connectors land in Phase 5"),
        },
        // ---- Diagnostics ---------------------------------------------
        Builtin {
            id: "doctor",
            path: "doctor",
            aliases: &[],
            category: "Diagnostics",
            description: "Report what this build can and cannot do, honestly",
            handler: HandlerKind::CoreOp,
            // A project command must never be able to fake a clean
            // diagnostic report.
            security_critical: true,
            availability: Availability::Ready,
        },
        Builtin {
            id: "keys",
            path: "keys",
            aliases: &[],
            category: "Diagnostics",
            description: "Show which keyboard shortcuts this terminal actually delivers",
            handler: HandlerKind::UiAction,
            security_critical: false,
            availability: Availability::Ready,
        },
    ]
}

/// A registry holding every builtin.
///
/// Both the TUI picker and the CLI's help output construct the registry
/// through this one function, which is what makes them projections of
/// the same metadata rather than two lists that drift (charter §12).
///
/// Panics if two builtins collide, which is a programming error in
/// [`builtins`] rather than a runtime condition -- and is covered by a
/// test, so it cannot reach a release.
pub fn registry_with_builtins() -> crate::Registry {
    let mut reg = crate::Registry::new();
    for b in builtins() {
        reg.register(b.to_spec())
            .expect("builtins must not collide with each other");
    }
    reg
}

/// The availability of a builtin, by id. Separate from the registry
/// because availability is a runtime question (is a provider
/// configured?) while the registry is static metadata.
pub fn availability(id: &str) -> Option<Availability> {
    builtins()
        .into_iter()
        .find(|b| b.id == id)
        .map(|b| b.availability)
}

impl Builtin {
    pub fn to_spec(&self) -> CommandSpec {
        CommandSpec {
            id: self.id.to_string(),
            path: self.path.to_string(),
            aliases: self.aliases.iter().map(|a| a.to_string()).collect(),
            category: self.category.to_string(),
            description: self.description.to_string(),
            provenance: Provenance::Builtin,
            handler: self.handler,
            security_critical: self.security_critical,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_registers_without_colliding_with_another_builtin() {
        let reg = registry_with_builtins();
        assert_eq!(reg.live_len(), builtins().len());
    }

    #[test]
    fn every_builtin_belongs_to_a_declared_category() {
        // A command in an unlisted category would be silently dropped
        // by a picker that groups strictly by CATEGORIES.
        for b in builtins() {
            assert!(
                CATEGORIES.contains(&b.category),
                "{} has category {:?}, which is not in CATEGORIES",
                b.id,
                b.category
            );
        }
    }

    #[test]
    fn the_commands_that_must_not_be_shadowable_are_marked_security_critical() {
        // Pinned deliberately: if someone adds a credential, permission
        // or diagnostic surface later and forgets the flag, this fails.
        let expected = [
            "help",
            "commands",
            "mode",
            "exit",
            "connect",
            "permissions",
            "connectors",
            "doctor",
        ];
        let actual: Vec<&str> = builtins()
            .iter()
            .filter(|b| b.security_critical)
            .map(|b| b.id)
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn a_project_command_cannot_shadow_any_security_critical_builtin() {
        for critical in builtins().iter().filter(|b| b.security_critical) {
            let mut reg = registry_with_builtins();
            let hostile = CommandSpec {
                id: "hostile".to_string(),
                path: critical.path.to_string(),
                aliases: Vec::new(),
                category: "Session".to_string(),
                description: "intercepts a security-critical path".to_string(),
                provenance: Provenance::Project,
                handler: HandlerKind::PromptTemplate,
                security_critical: false,
            };
            assert!(
                reg.register(hostile).is_err(),
                "/{} must not be shadowable",
                critical.path
            );
            assert_eq!(reg.resolve(critical.path).unwrap().id, critical.id);
        }
    }

    #[test]
    fn an_unimplemented_builtin_says_why_rather_than_claiming_to_work() {
        // Charter §1: listing a feature that does not work is a defect.
        // Every not-yet-built command must carry a reason a user can
        // read, and it must name something, not be empty.
        for b in builtins() {
            if let Availability::NotImplemented(reason) | Availability::Unconfigured(reason) =
                b.availability
            {
                assert!(
                    reason.len() > 10,
                    "{} needs a real reason, got {reason:?}",
                    b.id
                );
            }
        }
    }

    #[test]
    fn every_builtin_has_an_availability_record() {
        // Both the TUI picker and `se commands` look availability up
        // by id. A builtin added to `builtins()` without one would
        // render as "[no availability record]" in both surfaces, so
        // this is the anti-drift guard for charter §12's single
        // metadata source.
        for b in builtins() {
            assert!(
                availability(b.id).is_some(),
                "{} has no availability record",
                b.id
            );
        }
    }

    #[test]
    fn the_registry_and_the_builtin_list_describe_the_same_set() {
        // If these ever diverge, one surface shows a command the other
        // cannot resolve.
        let reg = registry_with_builtins();
        for b in builtins() {
            let spec = reg
                .resolve(b.path)
                .unwrap_or_else(|_| panic!("/{} is listed but does not resolve", b.path));
            assert_eq!(spec.id, b.id);
            assert_eq!(spec.description, b.description);
        }
        assert_eq!(reg.live_len(), builtins().len());
    }

    #[test]
    fn typing_mod_finds_the_models_command() {
        // Charter §13's named slash-command test.
        let reg = registry_with_builtins();
        let results = reg.query("mod");
        assert!(
            results.iter().any(|c| c.id == "models"),
            "got {:?}",
            results.iter().map(|c| &c.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn typing_agent_finds_the_agent_commands() {
        let reg = registry_with_builtins();
        let results = reg.query("agent");
        assert!(results.iter().any(|c| c.id == "agents"));
    }

    #[test]
    fn the_help_command_can_see_every_other_command() {
        // /help is a projection of the registry, so it is only correct
        // if the registry is the thing it reads.
        let reg = registry_with_builtins();
        assert_eq!(reg.query("").len(), builtins().len());
    }
}
