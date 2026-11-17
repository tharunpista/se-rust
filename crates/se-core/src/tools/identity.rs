//! OS-level file identity, the thing `se-permission` deliberately does
//! not resolve itself (see that crate's module doc). A `ResourceIdentity`
//! built from a path *string* would be exactly the thing
//! `SE_TARGET_ARCHITECTURE.md` §3a rejects the first-draft token design
//! for: a symlink (or a junction, on Windows) swapped between approval
//! and execution keeps the string identical while the real target
//! changes underneath it. Resolving by volume+file-id (Windows) or
//! dev+inode (Unix) is what makes that defense real.

use se_permission::ResourceIdentity;
use std::io;
use std::path::Path;

#[cfg(windows)]
#[allow(unsafe_code)] // The one FFI call; its SAFETY argument is inline below.
pub fn resolve_resource_identity(path: &Path) -> io::Result<ResourceIdentity> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
    };

    // FILE_FLAG_BACKUP_SEMANTICS is what lets this open a directory
    // handle at all (plain `File::open` refuses directories on
    // Windows) — the shell tool needs to resolve a cwd, not just files.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;

    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `file` stays alive for the call, and `info` is a plain
    // `#[repr(C)]` struct the API fills in; both pointers are valid for
    // the duration of this single FFI call.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(ResourceIdentity(format!(
        "win:{:08x}:{:08x}{:08x}",
        info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
    )))
}

#[cfg(unix)]
pub fn resolve_resource_identity(path: &Path) -> io::Result<ResourceIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok(ResourceIdentity(format!(
        "unix:{:x}:{:x}",
        meta.dev(),
        meta.ino()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_symlink_and_its_target_resolve_to_the_same_identity() {
        let dir = std::env::temp_dir().join(format!(
            "se-core-identity-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.txt");
        fs::write(&real, b"hello").unwrap();
        let link = dir.join("link.txt");

        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&real, &link)
            // Creating a file symlink needs a privilege a sandboxed CI
            // user may not have; fall back to a hard link, which shares
            // the same file-id/volume-serial identity just as well for
            // what this test proves (two distinct path strings, one
            // underlying file).
            .or_else(|_| fs::hard_link(&real, &link))
            .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let id_real = resolve_resource_identity(&real).unwrap();
        let id_link = resolve_resource_identity(&link).unwrap();
        assert_eq!(id_real, id_link);

        // Delete-and-recreate at the same path string: the classic
        // TOCTOU swap. The new file is a different underlying object,
        // so its identity must differ even though the path is identical.
        fs::remove_file(&real).unwrap();
        fs::write(&real, b"different content, same path").unwrap();
        let id_swapped = resolve_resource_identity(&real).unwrap();
        assert_ne!(id_real, id_swapped);

        let _ = fs::remove_dir_all(&dir);
    }
}
