//! An SE skill definition: `<root>/<skill-name>/SKILL.md`, frontmatter
//! only `name` and `description` -- much simpler than the agent
//! schema, no nested maps or lists.
//!
//! Bodies are loaded lazily: scanning a root to list skills (e.g. for
//! a picker) must not pay the cost of reading and retaining every
//! skill's full instructions, only to show a name and a description.
//! See [`load_skill_body`].

use crate::loader::Skipped;
use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
}

/// A loaded skill: its frontmatter, where it came from, which root it
/// was found under, and a content hash of the whole file (same
/// reasoning and mechanism as [`crate::AgentDefinition::content_hash`]
/// -- an accidental-change/pinning check within one trusted machine,
/// not a cryptographic guarantee).
///
/// Deliberately has no `body` field. The scan does read every byte of
/// the file once (a whole-file hash cannot be computed any other
/// way), but it is read as raw bytes, never decoded or validated as
/// UTF-8 past the frontmatter's closing `---`, and no `body` `String`
/// is ever allocated during a scan. A caller that wants a specific
/// skill's instructions calls [`load_skill_body`] on its
/// `source_path`, on demand -- that is the only place a skill's body
/// is read, parsed as UTF-8, and held as a `String`.
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    pub frontmatter: SkillFrontmatter,
    pub source_path: std::path::PathBuf,
    pub root: std::path::PathBuf,
    pub content_hash: u64,
}

/// Reads `path` and returns just the body (the text after the closing
/// `---` of the frontmatter block), verbatim, never reformatted. This
/// re-reads the file -- the scan that produced the [`SkillDefinition`]
/// deliberately did not keep the body around for every skill it saw.
pub fn load_skill_body(path: &std::path::Path) -> std::io::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    let (_, body) = crate::loader::split_frontmatter(&raw).map_err(std::io::Error::other)?;
    Ok(body)
}

#[derive(Debug, Default)]
pub struct SkillLoadReport {
    pub skills: Vec<SkillDefinition>,
    pub skipped: Vec<Skipped>,
}

/// Scans every root in `roots` for `<root>/<skill-name>/SKILL.md`.
///
/// A directory directly under a root that has no `SKILL.md` inside it
/// (e.g. `.claude\skills\synced`, which holds unrelated bucket/manifest
/// files, not a skill) is not a candidate at all and is silently
/// skipped -- not reported, the same distinction `loader::load` draws
/// for a non-`.md` file. A directory that DOES contain a `SKILL.md`
/// but which fails to parse is reported as skipped-with-a-reason.
///
/// A skill with the same name found under two different roots is not
/// an error here: both are reported, each tagged with its own `root`.
/// Resolving which one wins is a precedence decision for whatever
/// calls this loader, not this crate's job.
pub fn load_skills(roots: &[PathBuf]) -> SkillLoadReport {
    let mut report = SkillLoadReport::default();
    for root in roots {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(e) => {
                report.skipped.push(Skipped {
                    path: root.clone(),
                    reason: format!("could not read skill root: {e}"),
                });
                continue;
            }
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let skill_md = dir.join("SKILL.md");
            if !skill_md.is_file() {
                // No `SKILL.md` inside: not a skill candidate at all,
                // e.g. `synced/<bucket>/...`. Not reported as skipped.
                continue;
            }
            load_one(&skill_md, root, &mut report);
        }
    }
    report
}

fn load_one(path: &Path, root: &Path, report: &mut SkillLoadReport) {
    // Read bytes, not a UTF-8-validated `String`: the content hash
    // needs the raw bytes regardless, but a skill body that happens
    // not to be valid UTF-8 must not stop the scan from seeing the
    // frontmatter, since the scan never decodes or even looks at the
    // body at all. `frontmatter_yaml_bytes`, not `split_frontmatter`:
    // only the YAML slice is decoded to UTF-8 and parsed; no `body`
    // `String` is ever allocated for a file nothing has asked to read
    // the instructions of yet.
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            report.skipped.push(Skipped {
                path: path.to_path_buf(),
                reason: format!("could not read file: {e}"),
            });
            return;
        }
    };
    let (yaml, _body_start) = match crate::loader::frontmatter_yaml_bytes(&raw) {
        Ok(v) => v,
        Err(reason) => {
            report.skipped.push(Skipped {
                path: path.to_path_buf(),
                reason,
            });
            return;
        }
    };
    let frontmatter: SkillFrontmatter = match serde_yaml_ng::from_str(yaml) {
        Ok(f) => f,
        Err(e) => {
            report.skipped.push(Skipped {
                path: path.to_path_buf(),
                reason: format!("invalid frontmatter: {e}"),
            });
            return;
        }
    };
    let mut hasher = DefaultHasher::new();
    raw.hash(&mut hasher);
    report.skills.push(SkillDefinition {
        frontmatter,
        source_path: path.to_path_buf(),
        root: root.to_path_buf(),
        content_hash: hasher.finish(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const MINIMAL: &str = "---\nname: a\ndescription: does a thing\n---\n\nbody text\n";

    fn write(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("se-catalogue-skill-{tag}-{}", std::process::id()))
    }

    #[test]
    fn loads_a_minimal_valid_skill() {
        let root = tmp("min");
        let skill_dir = root.join("a");
        fs::create_dir_all(&skill_dir).unwrap();
        write(&skill_dir, "SKILL.md", MINIMAL);

        let report = load_skills(std::slice::from_ref(&root));
        assert_eq!(report.skills.len(), 1, "{:?}", report.skipped);
        assert!(report.skipped.is_empty());
        let s = &report.skills[0];
        assert_eq!(s.frontmatter.name, "a");
        assert_eq!(s.frontmatter.description, "does a thing");
        assert_eq!(s.root, root);

        let body = load_skill_body(&s.source_path).unwrap();
        assert!(body.contains("body text"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_with_no_skill_md_is_not_a_candidate_and_is_not_reported() {
        let root = tmp("nocandidate");
        let junk_dir = root.join("synced");
        fs::create_dir_all(junk_dir.join("some-bucket")).unwrap();
        write(&junk_dir, "manifest.json", "{}");

        let report = load_skills(std::slice::from_ref(&root));
        assert!(report.skills.is_empty());
        assert!(
            report.skipped.is_empty(),
            "a directory with no SKILL.md is not a candidate, not a skipped one: {:?}",
            report.skipped
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_skill_md_that_fails_to_parse_is_skipped_with_a_reason() {
        let root = tmp("badyaml");
        let skill_dir = root.join("bad");
        fs::create_dir_all(&skill_dir).unwrap();
        write(
            &skill_dir,
            "SKILL.md",
            "---\nname: [unterminated\n---\nbody\n",
        );

        let report = load_skills(std::slice::from_ref(&root));
        assert!(report.skills.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.contains("invalid frontmatter"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn two_loads_of_an_unchanged_file_produce_the_same_hash() {
        let root = tmp("hash");
        let skill_dir = root.join("a");
        fs::create_dir_all(&skill_dir).unwrap();
        write(&skill_dir, "SKILL.md", MINIMAL);

        let first = load_skills(std::slice::from_ref(&root)).skills[0].content_hash;
        let second = load_skills(std::slice::from_ref(&root)).skills[0].content_hash;
        assert_eq!(first, second);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_changed_file_produces_a_different_hash() {
        let root = tmp("hash2");
        let skill_dir = root.join("a");
        fs::create_dir_all(&skill_dir).unwrap();
        write(&skill_dir, "SKILL.md", MINIMAL);
        let before = load_skills(std::slice::from_ref(&root)).skills[0].content_hash;

        write(
            &skill_dir,
            "SKILL.md",
            &MINIMAL.replace("body text", "edited body"),
        );
        let after = load_skills(std::slice::from_ref(&root)).skills[0].content_hash;
        assert_ne!(before, after);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scanning_never_writes_to_the_tree_it_scans() {
        let root = tmp("ro");
        let skill_dir = root.join("a");
        fs::create_dir_all(&skill_dir).unwrap();
        write(&skill_dir, "SKILL.md", MINIMAL);
        let before = fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();

        let _ = load_skills(std::slice::from_ref(&root));

        let after = fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(before, after, "the loader must never modify a source file");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn multiple_roots_merge_and_a_name_collision_across_roots_is_reported_as_two_entries() {
        let root_a = tmp("multi-a");
        let root_b = tmp("multi-b");
        let dir_a = root_a.join("shared");
        let dir_b = root_b.join("shared");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        write(&dir_a, "SKILL.md", MINIMAL);
        write(&dir_b, "SKILL.md", MINIMAL);

        let report = load_skills(&[root_a.clone(), root_b.clone()]);
        assert_eq!(report.skills.len(), 2, "{:?}", report.skipped);
        let roots: Vec<&PathBuf> = report.skills.iter().map(|s| &s.root).collect();
        assert!(roots.contains(&&root_a));
        assert!(roots.contains(&&root_b));

        let _ = fs::remove_dir_all(&root_a);
        let _ = fs::remove_dir_all(&root_b);
    }

    #[test]
    fn a_body_with_invalid_utf8_still_scans_but_fails_only_when_the_body_is_actually_read() {
        // Proves the scan is genuinely lazy, not eager-with-a-missing-field:
        // a scan that decoded the whole file as UTF-8 (`read_to_string`)
        // could not even see this frontmatter, since the body after it is
        // not valid UTF-8.
        let root = tmp("invalid-utf8-body");
        let skill_dir = root.join("a");
        fs::create_dir_all(&skill_dir).unwrap();
        let mut bytes = b"---\nname: a\ndescription: does a thing\n---\n\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, 0x00, 0xff]);
        fs::write(skill_dir.join("SKILL.md"), &bytes).unwrap();

        let report = load_skills(std::slice::from_ref(&root));
        assert_eq!(report.skills.len(), 1, "{:?}", report.skipped);
        assert!(report.skipped.is_empty());
        let s = &report.skills[0];
        assert_eq!(s.frontmatter.name, "a");

        let body_result = load_skill_body(&s.source_path);
        assert!(
            body_result.is_err(),
            "reading the body of a file with invalid UTF-8 past the frontmatter must fail"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
