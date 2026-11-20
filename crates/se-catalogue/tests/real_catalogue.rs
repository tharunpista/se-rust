//! Loads the real, on-disk SE catalogue, not a synthetic fixture.
//! This is the Phase 3 gate claim itself: "194 canonical definitions
//! load; the 2 non-agent files are skipped with a reason." A loader
//! that only works against hand-built test fixtures has not actually
//! proven that claim.
//!
//! Skipped (not failed) if the catalogue is not present on this
//! machine -- the path is a real local install location, not a
//! workspace-relative fixture, so a clone of this repo without the
//! user's `SE` tree must not fail its test suite over a missing
//! external directory.

fn home() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default(),
    )
}

fn catalogue_root() -> String {
    std::env::var("SE_CATALOGUE_ROOT")
        .unwrap_or_else(|_| home().join("SE/se/agents").to_string_lossy().into_owned())
}

#[test]
fn the_real_catalogue_loads_194_agents_and_skips_exactly_the_2_known_non_agent_files() {
    let root_buf = std::path::PathBuf::from(catalogue_root());
    let root = root_buf.as_path();
    if !root.exists() {
        eprintln!("skipping: {} not present on this machine", catalogue_root());
        return;
    }

    let report = se_catalogue::load(root).expect("the real catalogue tree must be readable");

    assert_eq!(
        report.agents.len(),
        194,
        "expected 194 real agent definitions, got {} (skipped: {:?})",
        report.agents.len(),
        report.skipped
    );
    assert_eq!(
        report.skipped.len(),
        2,
        "expected exactly the 2 known non-agent files to be skipped, got {:?}",
        report.skipped
    );

    let skipped_names: Vec<String> = report
        .skipped
        .iter()
        .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        skipped_names.contains(&"ownership-matrix.md".to_string()),
        "{skipped_names:?}"
    );
    assert!(
        skipped_names.contains(&"role-inventory.md".to_string()),
        "{skipped_names:?}"
    );

    // Every real agent id is unique and the schema this crate expects.
    let mut ids: Vec<&str> = report
        .agents
        .iter()
        .map(|a| a.frontmatter.id.as_str())
        .collect();
    ids.sort_unstable();
    let unique_count = {
        let mut dedup = ids.clone();
        dedup.dedup();
        dedup.len()
    };
    assert_eq!(unique_count, ids.len(), "every agent id must be unique");
    assert!(report
        .agents
        .iter()
        .all(|a| a.frontmatter.schema_version == 1));
    assert!(report.agents.iter().all(|a| !a.body.trim().is_empty()));
}

#[test]
fn loading_the_real_catalogue_twice_in_a_row_never_modifies_it() {
    let root_buf = std::path::PathBuf::from(catalogue_root());
    let root = root_buf.as_path();
    if !root.exists() {
        eprintln!("skipping: {} not present on this machine", catalogue_root());
        return;
    }

    let first = se_catalogue::load(root).unwrap();
    let second = se_catalogue::load(root).unwrap();

    let mut first_hashes: Vec<(String, u64)> = first
        .agents
        .iter()
        .map(|a| (a.frontmatter.id.clone(), a.content_hash))
        .collect();
    let mut second_hashes: Vec<(String, u64)> = second
        .agents
        .iter()
        .map(|a| (a.frontmatter.id.clone(), a.content_hash))
        .collect();
    first_hashes.sort();
    second_hashes.sort();
    assert_eq!(
        first_hashes, second_hashes,
        "hashing the tree before and after must be identical -- the catalogue is unmodified"
    );
}
