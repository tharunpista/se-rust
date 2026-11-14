//! Read-only scan over the agent catalogue tree
//! (`<root>/<department>/<id>.md`). Never writes to a source it
//! scanned, per `compatibility.ts@9bbb8ce`'s scan-don't-move model.

use crate::agent::{AgentDefinition, AgentFrontmatter};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Why one file in the tree did not become an [`AgentDefinition`].
/// A loader that silently drops a file it could not parse is the thing
/// this type exists to rule out -- every skip is reported, not hidden.
#[derive(Debug)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct LoadReport {
    pub agents: Vec<AgentDefinition>,
    pub skipped: Vec<Skipped>,
}

/// Scans every `*.md` file directly under each department directory of
/// `root`. Non-`.md` entries and nested directories other than one
/// level of department are ignored, not reported as skipped -- they
/// are not candidate agent files at all, which is a different thing
/// from a candidate that failed to parse.
pub fn load(root: &Path) -> std::io::Result<LoadReport> {
    let mut report = LoadReport::default();
    let mut dept_dirs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            dept_dirs.push(path);
        } else if path.extension().is_some_and(|e| e == "md") {
            // A stray `.md` directly under the catalogue root (e.g.
            // `ownership-matrix.md`, `role-inventory.md`): a real
            // candidate, reported as skipped rather than silently
            // ignored, since it does share the file extension every
            // real agent definition has.
            load_one(&path, &mut report);
        }
    }
    for dir in dept_dirs {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                report.skipped.push(Skipped {
                    path: dir,
                    reason: format!("could not read department directory: {e}"),
                });
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md") {
                load_one(&path, &mut report);
            }
        }
    }
    Ok(report)
}

fn load_one(path: &Path, report: &mut LoadReport) {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            report.skipped.push(Skipped {
                path: path.to_path_buf(),
                reason: format!("could not read file: {e}"),
            });
            return;
        }
    };
    match parse(&raw) {
        Ok((frontmatter, body)) => {
            let mut hasher = DefaultHasher::new();
            raw.hash(&mut hasher);
            report.agents.push(AgentDefinition {
                frontmatter,
                body,
                source_path: path.to_path_buf(),
                content_hash: hasher.finish(),
            });
        }
        Err(reason) => report.skipped.push(Skipped {
            path: path.to_path_buf(),
            reason,
        }),
    }
}

/// Splits `---\n<yaml>\n---\n<body>` and parses the YAML half. A file
/// with no leading `---` frontmatter block (e.g. `role-inventory.md`,
/// which opens with an HTML comment, not frontmatter) is reported as
/// "no frontmatter block", not silently treated as a bodyless agent.
fn parse(raw: &str) -> Result<(AgentFrontmatter, String), String> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .ok_or_else(|| "no frontmatter block (file does not start with `---`)".to_string())?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| "frontmatter block is never closed with `---`".to_string())?;
    let yaml = &rest[..end];
    // Skip the closing `---` and the newline after it, if present.
    let after = &rest[end + 4..];
    let body = after.strip_prefix('\n').unwrap_or(after).to_string();
    let frontmatter: AgentFrontmatter =
        serde_yaml_ng::from_str(yaml).map_err(|e| format!("invalid frontmatter: {e}"))?;
    Ok((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).unwrap();
    }

    const MINIMAL: &str =
        "---\nschema_version: 1\nid: a\nname: A\ndepartment: d\n---\n\n# A\n\nbody text\n";

    #[test]
    fn loads_a_minimal_valid_definition() {
        let dir = std::env::temp_dir().join(format!("se-catalogue-min-{}", std::process::id()));
        let dept = dir.join("d");
        fs::create_dir_all(&dept).unwrap();
        write(&dept, "a.md", MINIMAL);

        let report = load(&dir).unwrap();
        assert_eq!(report.agents.len(), 1, "{:?}", report.skipped);
        assert!(report.skipped.is_empty());
        let a = &report.agents[0];
        assert_eq!(a.frontmatter.id, "a");
        assert_eq!(a.frontmatter.name, "A");
        assert!(a.body.contains("body text"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_loads_of_an_unchanged_file_produce_the_same_hash() {
        let dir = std::env::temp_dir().join(format!("se-catalogue-hash-{}", std::process::id()));
        let dept = dir.join("d");
        fs::create_dir_all(&dept).unwrap();
        write(&dept, "a.md", MINIMAL);

        let first = load(&dir).unwrap().agents[0].content_hash;
        let second = load(&dir).unwrap().agents[0].content_hash;
        assert_eq!(first, second);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_file_produces_a_different_hash() {
        let dir = std::env::temp_dir().join(format!("se-catalogue-hash2-{}", std::process::id()));
        let dept = dir.join("d");
        fs::create_dir_all(&dept).unwrap();
        write(&dept, "a.md", MINIMAL);
        let before = load(&dir).unwrap().agents[0].content_hash;

        write(&dept, "a.md", &MINIMAL.replace("body text", "edited body"));
        let after = load(&dir).unwrap().agents[0].content_hash;
        assert_ne!(before, after);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_with_no_frontmatter_block_is_skipped_with_a_reason_not_dropped_silently() {
        let dir = std::env::temp_dir().join(format!("se-catalogue-skip-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        write(
            &dir,
            "role-inventory.md",
            "<!-- GENERATED -->\n\n# Role inventory\n",
        );

        let report = load(&dir).unwrap();
        assert!(report.agents.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.contains("no frontmatter"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_with_malformed_yaml_is_skipped_with_a_reason() {
        let dir = std::env::temp_dir().join(format!("se-catalogue-badyaml-{}", std::process::id()));
        let dept = dir.join("d");
        fs::create_dir_all(&dept).unwrap();
        write(&dept, "bad.md", "---\nid: [unterminated\n---\nbody\n");

        let report = load(&dir).unwrap();
        assert!(report.agents.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.contains("invalid frontmatter"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scanning_never_writes_to_the_tree_it_scans() {
        let dir = std::env::temp_dir().join(format!("se-catalogue-ro-{}", std::process::id()));
        let dept = dir.join("d");
        fs::create_dir_all(&dept).unwrap();
        write(&dept, "a.md", MINIMAL);
        let before = fs::read_to_string(dept.join("a.md")).unwrap();

        let _ = load(&dir).unwrap();

        let after = fs::read_to_string(dept.join("a.md")).unwrap();
        assert_eq!(before, after, "the loader must never modify a source file");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failure_modes_and_memory_scopes_parse_as_the_real_nested_shape() {
        let content = r#"---
schema_version: 1
id: a
name: A
department: d
memory_scopes:
  read: [session, project]
  write: [session]
failure_modes:
  - mode: >-
      a trace breaks at the boundary
    guard: >-
      propagate a stable id
---

body
"#;
        let dir = std::env::temp_dir().join(format!("se-catalogue-nested-{}", std::process::id()));
        let dept = dir.join("d");
        fs::create_dir_all(&dept).unwrap();
        write(&dept, "a.md", content);

        let report = load(&dir).unwrap();
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let a = &report.agents[0];
        assert_eq!(a.frontmatter.memory_scopes.read, vec!["session", "project"]);
        assert_eq!(a.frontmatter.memory_scopes.write, vec!["session"]);
        assert_eq!(a.frontmatter.failure_modes.len(), 1);
        assert_eq!(
            a.frontmatter.failure_modes[0].mode,
            "a trace breaks at the boundary"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
