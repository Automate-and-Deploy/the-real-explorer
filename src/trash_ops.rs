//! Recycle bin / trash integration, backed by the `trash` crate.
//!
//! Platform behaviour:
//! - **Windows**: files/directories move to the Recycle Bin. `os_limited::list`
//!   and `os_limited::restore_all` are available, so [`restore`] works.
//! - **Linux**: files/directories move to the trash can per the freedesktop.org
//!   Trash spec (version 1.0). `os_limited` is also available here, so
//!   [`restore`] works, provided the desktop environment follows the spec
//!   (GNOME, KDE, XFCE do).
//! - **macOS**: files move to `~/.Trash` via the Finder API. The `trash` crate
//!   does not expose `os_limited::list`/`restore` on macOS, so [`restore`]
//!   always returns `Err`.
//!
//! All functions are thin, fallible wrappers around the `trash` crate that
//! collapse its `Error` type into a `String` for easy display in UI code.

use std::path::{Path, PathBuf};

/// Move `path` to the OS recycle bin / trash.
///
/// Windows: Recycle Bin. macOS: `~/.Trash` via `NSFileManager`, not Finder,
/// because the Finder route needs an Automation permission grant and hangs
/// for a minute without one. Linux: freedesktop.org trash spec.
///
/// Returns `Err` if the path does not exist or the underlying OS call fails
/// (e.g. permissions, unsupported filesystem).
pub fn delete_to_trash(path: &Path) -> Result<(), String> {
    context().delete(path).map_err(|e| e.to_string())
}

/// A trash context configured for the platform.
///
/// macOS needs this: the crate defaults to driving Finder over AppleScript,
/// which requires an Automation (Apple Events) permission grant. Without one
/// the call hangs for a minute and then fails with `-1712`, and the file is
/// still there. `NsFileManager` does the move directly, needs no grant and
/// works with Finder not running. The cost is no delete sound and no Finder
/// "Put Back" entry, and this module already documents restore as unsupported
/// on macOS.
fn context() -> trash::TrashContext {
    #[allow(unused_mut)]
    let mut ctx = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    ctx
}

/// Move several paths to the OS recycle bin / trash in one call.
///
/// On platforms that support it (Windows, freedesktop.org Linux), the moves
/// are grouped into a single undo group in the destination's own undo UI.
/// Returns `Err` on the first failure reported by the underlying crate; no
/// partial-success information is provided beyond that.
#[allow(dead_code)]
pub fn delete_many_to_trash(paths: &[PathBuf]) -> Result<(), String> {
    context().delete_all(paths).map_err(|e| e.to_string())
}

/// Human-readable name of the trash destination, for UI text.
///
/// "Recycle Bin" on Windows, "Trash" everywhere else.
pub fn bin_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "Recycle Bin"
    } else {
        "Trash"
    }
}

/// Restore the most recently trashed item whose original path equals
/// `original`.
///
/// Available on Windows and Linux via the `trash` crate's `os_limited`
/// module. Returns `Ok(false)` if no trashed item with that original path is
/// found. On macOS this always returns `Err`, since the `trash` crate does
/// not expose list/restore there.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub fn restore(original: &Path) -> Result<bool, String> {
    let items = trash::os_limited::list().map_err(|e| e.to_string())?;

    // Pick the newest match by deletion time in case the same path was
    // trashed more than once.
    let candidate = items
        .into_iter()
        .filter(|item| item.original_path() == original)
        .max_by_key(|item| item.time_deleted);

    match candidate {
        Some(item) => {
            trash::os_limited::restore_all(std::iter::once(item)).map_err(|e| e.to_string())?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Restore is not supported on macOS: the `trash` crate exposes no
/// list/restore API there. Always returns `Err`.
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn restore(_original: &Path) -> Result<bool, String> {
    Err("restore not supported on macOS".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Builds a unique path under the system temp dir so parallel test runs
    /// (and repeated local runs) never collide.
    fn unique_temp_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rust-explorer-trash-ops-{prefix}-{}-{}",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn delete_to_trash_removes_file_from_disk() {
        let path = unique_temp_path("file");
        fs::write(&path, b"trash me").unwrap();
        assert!(path.exists());

        delete_to_trash(&path).expect("delete_to_trash should succeed");

        assert!(!path.exists());
    }

    #[test]
    fn delete_to_trash_removes_directory_with_nested_file() {
        let dir = unique_temp_path("dir");
        fs::create_dir(&dir).unwrap();
        let nested = dir.join("nested.txt");
        fs::write(&nested, b"nested contents").unwrap();
        assert!(dir.exists());
        assert!(nested.exists());

        delete_to_trash(&dir).expect("delete_to_trash should succeed on directories");

        assert!(!dir.exists());
    }

    #[test]
    fn delete_to_trash_nonexistent_path_errors() {
        let path = unique_temp_path("does-not-exist");
        assert!(!path.exists());

        let result = delete_to_trash(&path);

        assert!(result.is_err());
    }

    #[cfg(windows)]
    #[test]
    fn restore_brings_back_a_trashed_file() {
        let path = unique_temp_path("restore");
        fs::write(&path, b"restore me").unwrap();

        delete_to_trash(&path).expect("delete_to_trash should succeed");
        assert!(!path.exists());

        let items = trash::os_limited::list().expect("list should succeed");
        assert!(
            items.iter().any(|item| item.original_path() == path),
            "trashed item with matching original path should be listed"
        );

        let restored = restore(&path).expect("restore should succeed");
        assert!(restored, "restore should report the item was found");
        assert!(path.exists(), "file should be back on disk after restore");

        // Clean up permanently; this is std, not trash.
        fs::remove_file(&path).unwrap();
    }
}
