use super::{require_fresh_identity, Granted, ToolError};
use std::path::Path;

/// Reads a file's full contents. Refuses if the path's current
/// identity does not match what `granted` was approved for.
pub fn read_file(granted: &Granted, path: &Path) -> Result<String, ToolError> {
    require_fresh_identity(granted, path)?;
    Ok(std::fs::read_to_string(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::granted_for;
    use se_permission::NonceLedger;
    use std::fs;

    #[test]
    fn reads_the_file_the_token_was_approved_for() {
        let dir = std::env::temp_dir().join(format!("se-core-read-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "hello world").unwrap();

        let ledger = NonceLedger::new();
        let granted = granted_for("read", &file, &dir, vec![], &ledger);
        let content = read_file(&granted, &file).unwrap();
        assert_eq!(content, "hello world");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_file_swapped_after_approval() {
        let dir = std::env::temp_dir().join(format!("se-core-read-swap-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "original").unwrap();

        let ledger = NonceLedger::new();
        let granted = granted_for("read", &file, &dir, vec![], &ledger);

        // TOCTOU: the file at the same path is now a different object.
        fs::remove_file(&file).unwrap();
        fs::write(&file, "swapped").unwrap();

        let err = read_file(&granted, &file).unwrap_err();
        assert!(matches!(err, ToolError::ResourceMismatch));

        let _ = fs::remove_dir_all(&dir);
    }
}
