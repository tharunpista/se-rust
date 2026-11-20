//! Loads the real, on-disk skill roots, not a synthetic fixture.
//!
//! Skipped (not failed) if none of the real roots are present on this
//! machine -- these are real local install locations, not
//! workspace-relative fixtures, so a clone of this repo without the
//! user's `SE`/`.claude` trees must not fail its test suite over a
//! missing external directory.

use std::path::PathBuf;

fn home() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default(),
    )
}

fn real_roots() -> Vec<PathBuf> {
    let h = home();
    vec![
        h.join("SE/se/skills"),
        h.join("SE/.agents/skills"),
        h.join(".claude/skills"),
    ]
}

#[test]
fn the_real_skill_roots_load_atina_and_tolerate_the_claude_skills_synced_junk() {
    let roots = real_roots();
    let present: Vec<PathBuf> = roots.into_iter().filter(|r| r.exists()).collect();
    if present.is_empty() {
        eprintln!("skipping: none of the real skill roots are present on this machine");
        return;
    }

    let report = se_catalogue::load_skills(&present);
    eprintln!(
        "loaded {} skill(s), skipped {}: {:?}",
        report.skills.len(),
        report.skipped.len(),
        report.skipped
    );

    let se_skills_buf = home().join("SE/se/skills");
    let se_skills_root = se_skills_buf.as_path();
    if se_skills_root.exists() {
        assert!(
            report
                .skills
                .iter()
                .any(|s| s.frontmatter.name == "atina" && s.root == se_skills_root),
            "expected `atina` specifically under {se_skills_root:?}, got {:?}",
            report
                .skills
                .iter()
                .map(|s| (&s.frontmatter.name, &s.root))
                .collect::<Vec<_>>()
        );
    }

    // `~\.claude\skills\synced\...` holds unrelated bucket/manifest
    // files with no `SKILL.md` directly inside it -- not a skill
    // candidate, so it must appear in neither list.
    let claude_skills_buf = home().join(".claude/skills");
    let claude_skills_root = claude_skills_buf.as_path();
    if claude_skills_root.exists() {
        assert!(
            !report.skills.iter().any(|s| s
                .source_path
                .components()
                .any(|c| c.as_os_str() == "synced")),
            "the `synced` junk directory must never be loaded as a skill: {:?}",
            report.skills
        );
        assert!(
            !report
                .skipped
                .iter()
                .any(|s| s.path.components().any(|c| c.as_os_str() == "synced")),
            "the `synced` junk directory has no SKILL.md, so it is not a candidate at all -- \
             it must not appear in the skipped list either: {:?}",
            report.skipped
        );
    }
}
