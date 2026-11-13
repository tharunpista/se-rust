//! Reads project and global command directories from disk.
//!
//! This is the crate's one I/O surface -- see the crate doc. It never
//! executes anything; it walks a directory, hands each `.md` file's
//! text to [`crate::markdown::parse`], and turns a successful parse
//! into a [`CommandSpec`]. A failure is reported, not fatal: per the
//! lesson in `docs/reference/OPENCODE_SOURCE_AUDIT.md` §4.6
//! (`compatibility.ts@9bbb8ce`'s "skip one, don't fail all"), one
//! malformed file must not hide every other command in the directory.

use std::path::{Path, PathBuf};

use crate::markdown::{parse, CommandDef};
use crate::{loaded_id, CommandSpec, HandlerKind, Provenance};

/// A command loaded from disk: its registry entry, and the parsed
/// definition a caller needs to actually render and run it. Kept as
/// one pair rather than two parallel vectors, so a caller cannot index
/// them out of sync.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub spec: CommandSpec,
    pub def: CommandDef,
}

/// The category every discovered command is filed under. A constant,
/// not a literal repeated at each call site, so `builtins::CATEGORIES`
/// and this module cannot drift to two different spellings.
pub const CATEGORY: &str = "Custom";

/// One file that failed to load, and why. Carries the real path so a
/// user can go fix it, which a bare error string would not let them do.
#[derive(Debug, Clone)]
pub struct DiscoveryError {
    pub file: PathBuf,
    pub reason: String,
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.file.display(), self.reason)
    }
}

/// Recursively loads every `.md` file under `root` as a command.
///
/// A missing `root` is not an error -- most projects have no
/// `.se/commands` directory, and that is the ordinary case, not a
/// failure to report. A subdirectory becomes a namespace segment
/// joined with `:`, so `review/security.md` becomes `/review:security`
/// (the same separator `CLAUDE_CODE_PUBLIC_UX_MAP.md` §3 documents for
/// Claude Code's own subdirectory commands).
///
/// Returns every command that parsed, and every file that did not,
/// separately -- never mixed into one `Result`, because a directory
/// with nine good files and one bad one has nine real commands to
/// register, not zero.
pub fn scan(root: &Path, provenance: Provenance) -> (Vec<Loaded>, Vec<DiscoveryError>) {
    let mut specs = Vec::new();
    let mut errors = Vec::new();
    if !root.is_dir() {
        return (specs, errors);
    }

    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                errors.push(DiscoveryError {
                    file: dir,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            match load_one(root, &path, provenance) {
                Ok(loaded) => specs.push(loaded),
                Err(reason) => errors.push(DiscoveryError { file: path, reason }),
            }
        }
    }
    (specs, errors)
}

fn load_one(root: &Path, file: &Path, provenance: Provenance) -> Result<Loaded, String> {
    let relative = file.strip_prefix(root).unwrap_or(file).with_extension("");
    let command_path: String = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    let source = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let def = parse(&command_path, &source).map_err(|e| e.to_string())?;
    let spec = CommandSpec {
        id: loaded_id(provenance, &command_path),
        path: command_path,
        aliases: Vec::new(),
        category: CATEGORY.to_string(),
        description: def.description.clone(),
        provenance,
        handler: HandlerKind::PromptTemplate,
        security_critical: false,
    };
    Ok(Loaded { spec, def })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A directory under the OS temp dir, removed on drop so a failed
    /// assertion still cleans up. `std::env::temp_dir()` plus a counter
    /// is used instead of a `tempfile` dependency, which this crate
    /// does not have and a test-only need does not justify adding.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "se-command-discovery-test-{}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, contents: &str) {
            let full = self.0.join(relative);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let (specs, errors) = scan(Path::new("/does/not/exist/anywhere"), Provenance::Project);
        assert!(specs.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn a_valid_command_file_loads_with_the_custom_category() {
        let dir = TempDir::new();
        dir.write(
            "review.md",
            "---\ndescription: Review the diff\n---\nLook at $0.",
        );
        let (specs, errors) = scan(dir.path(), Provenance::Project);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].spec.path, "review");
        assert_eq!(specs[0].spec.category, CATEGORY);
        assert_eq!(specs[0].spec.description, "Review the diff");
        assert_eq!(specs[0].spec.handler, HandlerKind::PromptTemplate);
        assert!(!specs[0].spec.security_critical);
    }

    #[test]
    fn a_subdirectory_becomes_a_colon_namespaced_path() {
        let dir = TempDir::new();
        dir.write("frontend/component.md", "Scaffold a component.");
        let (specs, errors) = scan(dir.path(), Provenance::Project);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(specs[0].spec.path, "frontend:component");
    }

    #[test]
    fn one_malformed_file_does_not_hide_the_others() {
        let dir = TempDir::new();
        dir.write("good.md", "---\ndescription: Fine\n---\nBody");
        // Unterminated frontmatter: malformed per markdown::parse.
        dir.write("bad.md", "---\ndescription: Fine");
        dir.write("also-good.md", "No frontmatter at all, still valid.");
        let (specs, errors) = scan(dir.path(), Provenance::Project);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].file.ends_with("bad.md"));
        let paths: Vec<&str> = specs.iter().map(|s| s.spec.path.as_str()).collect();
        assert!(paths.contains(&"good"), "{paths:?}");
        assert!(paths.contains(&"also-good"), "{paths:?}");
    }

    #[test]
    fn non_markdown_files_are_ignored() {
        let dir = TempDir::new();
        dir.write("review.md", "A command.");
        dir.write("README.txt", "not a command");
        let (specs, _) = scan(dir.path(), Provenance::Project);
        assert_eq!(specs.len(), 1);
    }

    #[test]
    fn the_same_path_from_two_different_provenances_gets_two_different_ids() {
        // The trap flagged when this module was designed: a bare path
        // used as the id would let a global command and a project
        // command at the same path silently merge into "one command
        // being reloaded" instead of competing on provenance.
        let project = TempDir::new();
        project.write(
            "review.md",
            "---
description: Project version.
---
Body.",
        );
        let global = TempDir::new();
        global.write(
            "review.md",
            "---
description: Global version.
---
Body.",
        );

        let (project_specs, _) = scan(project.path(), Provenance::Project);
        let (global_specs, _) = scan(global.path(), Provenance::Global);

        assert_ne!(project_specs[0].spec.id, global_specs[0].spec.id);

        let mut reg = crate::Registry::new();
        reg.register(global_specs[0].spec.clone()).unwrap();
        let outcome = reg.register(project_specs[0].spec.clone()).unwrap();
        // Project outranks global, and this is a genuine precedence
        // win, not a same-id reload silently taking over.
        assert!(matches!(outcome, crate::RegisterOutcome::Replaced { .. }));
        assert_eq!(
            reg.resolve("review").unwrap().description,
            "Project version."
        );
    }

    #[test]
    fn rescanning_the_same_file_produces_the_same_id_so_a_reload_replaces_cleanly() {
        let dir = TempDir::new();
        dir.write(
            "review.md",
            "---
description: First version.
---
Body.",
        );
        let (first, _) = scan(dir.path(), Provenance::Project);
        dir.write(
            "review.md",
            "---
description: Edited version.
---
Body.",
        );
        let (second, _) = scan(dir.path(), Provenance::Project);
        assert_eq!(first[0].spec.id, second[0].spec.id);

        let mut reg = crate::Registry::new();
        reg.register(first[0].spec.clone()).unwrap();
        let outcome = reg.register(second[0].spec.clone()).unwrap();
        assert!(matches!(outcome, crate::RegisterOutcome::Replaced { .. }));
        assert_eq!(
            reg.resolve("review").unwrap().description,
            "Edited version."
        );
    }

    #[test]
    fn the_custom_category_is_declared_in_builtins_categories() {
        assert!(crate::builtins::CATEGORIES.contains(&CATEGORY));
    }
}
