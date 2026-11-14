use super::{require_fresh_identity, Granted, ToolError};
use std::path::Path;

/// Writes `content` to `path`, replacing it entirely. Refuses if an
/// existing file at `path` no longer matches the identity `granted`
/// was approved for; a path that does not exist yet is a new-file
/// creation and has no prior identity to check.
pub fn write_file(granted: &Granted, path: &Path, content: &str) -> Result<(), ToolError> {
    require_fresh_identity(granted, path)?;
    Ok(std::fs::write(path, content)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::granted_for;
    use se_permission::NonceLedger;
    use std::fs;

    #[test]
    fn creates_a_new_file() {
        let dir = std::env::temp_dir().join(format!("se-core-write-new-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("new.txt");

        let ledger = NonceLedger::new();
        // Approved before the file exists: resource identity is bound
        // to the containing directory, since there is nothing else yet.
        let granted = granted_for("write", &dir, &dir, vec![], &ledger);
        write_file(&granted, &file, "new content").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "new content");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrites_the_file_the_token_was_approved_for() {
        let dir =
            std::env::temp_dir().join(format!("se-core-write-overwrite-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "old").unwrap();

        let ledger = NonceLedger::new();
        let granted = granted_for("write", &file, &dir, vec![], &ledger);
        write_file(&granted, &file, "new").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_file_swapped_after_approval() {
        let dir = std::env::temp_dir().join(format!("se-core-write-swap-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "original").unwrap();

        let ledger = NonceLedger::new();
        let granted = granted_for("write", &file, &dir, vec![], &ledger);

        fs::remove_file(&file).unwrap();
        fs::write(&file, "swapped").unwrap();

        let err = write_file(&granted, &file, "attempted overwrite").unwrap_err();
        assert!(matches!(err, ToolError::ResourceMismatch));
        // The swap-detection must have refused the write, not merely
        // reported an error after the fact.
        assert_eq!(fs::read_to_string(&file).unwrap(), "swapped");

        let _ = fs::remove_dir_all(&dir);
    }
}
