//! One-time moves from the old product name (Meeting Master) to Friston.
//!
//! Runs at startup, before any engine writes to disk. Folders are renamed in place (same
//! volume, so `rename` is atomic), never copied; when both the old and the new folder exist we
//! leave both alone rather than guess how to merge them.

use std::path::{Path, PathBuf};

use mm_store::Store;

use crate::ServiceError;
use crate::config::AppConfig;

/// Folder name the app used before it became Friston.
pub const LEGACY_APP_DIR_NAME: &str = "Meeting Master";

/// What [`move_folder`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderMove {
    /// The old folder now lives at the new path.
    Moved,
    /// There was no old folder (fresh install, or already migrated).
    NothingToMove,
    /// Both folders exist; nothing was touched.
    BothExist,
}

/// Renames `old` to `new` if only `old` exists.
///
/// Params: `old` – legacy folder; `new` – target path (its parent is created if missing).
/// Returns: what happened; an error carries both paths.
pub fn move_folder(old: &Path, new: &Path) -> Result<FolderMove, ServiceError> {
    if !old.exists() {
        return Ok(FolderMove::NothingToMove);
    }
    if new.exists() {
        return Ok(FolderMove::BothExist);
    }
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ServiceError::Config(format!("creating {}: {e}", parent.display())))?;
    }
    std::fs::rename(old, new).map_err(|e| {
        ServiceError::Config(format!(
            "moving {} to {}: {e}",
            old.display(),
            new.display()
        ))
    })?;
    Ok(FolderMove::Moved)
}

/// The data folder's location before the rename.
///
/// Params: `new` – the current data folder (`…/Application Support/Friston`).
/// Returns: its `Meeting Master` sibling.
pub fn legacy_sibling(new: &Path) -> PathBuf {
    new.with_file_name(LEGACY_APP_DIR_NAME)
}

/// Moves the default recordings folder and rewrites the paths the database stores for it.
///
/// Skipped when the user picked their own recordings folder, which the rename does not affect.
///
/// Params: `store` – open database; `cfg` – loaded settings; `old`/`new` – default folders.
/// Returns: what happened to the folder.
pub fn migrate_recordings(
    store: &Store,
    cfg: &AppConfig,
    old: &Path,
    new: &Path,
) -> Result<FolderMove, ServiceError> {
    if cfg.recordings_dir.is_some() {
        return Ok(FolderMove::NothingToMove);
    }
    let moved = move_folder(old, new)?;
    if moved == FolderMove::Moved {
        // A failure here leaves stale paths but no lost recordings; the error says which folders.
        store.rebase_paths(old, new).map_err(|e| {
            ServiceError::Config(format!(
                "updating recording paths from {} to {}: {e}",
                old.display(),
                new.display()
            ))
        })?;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_folder_renames_only_when_target_is_free() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("Meeting Master");
        let new = tmp.path().join("Friston");

        assert_eq!(move_folder(&old, &new).unwrap(), FolderMove::NothingToMove);

        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("config.json"), "{}").unwrap();
        assert_eq!(move_folder(&old, &new).unwrap(), FolderMove::Moved);
        assert!(!old.exists());
        assert_eq!(
            std::fs::read_to_string(new.join("config.json")).unwrap(),
            "{}"
        );

        std::fs::create_dir(&old).unwrap();
        assert_eq!(move_folder(&old, &new).unwrap(), FolderMove::BothExist);
        assert!(old.exists() && new.exists());
    }

    #[test]
    fn legacy_sibling_swaps_the_last_component() {
        assert_eq!(
            legacy_sibling(Path::new("/u/Library/Application Support/Friston")),
            Path::new("/u/Library/Application Support/Meeting Master")
        );
    }

    #[test]
    fn migrate_recordings_moves_folder_and_rewrites_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("Meeting Master");
        let new = tmp.path().join("Friston");
        std::fs::create_dir_all(old.join("2026 a")).unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut m = mm_store::new_meeting("a", chrono::Utc::now());
        m.recording_dir = Some(old.join("2026 a").display().to_string());
        store.upsert_meeting(&m).unwrap();

        let moved = migrate_recordings(&store, &AppConfig::default(), &old, &new).unwrap();

        assert_eq!(moved, FolderMove::Moved);
        assert!(new.join("2026 a").is_dir());
        assert_eq!(
            store.meeting(m.id).unwrap().unwrap().recording_dir,
            Some(new.join("2026 a").display().to_string())
        );
    }

    #[test]
    fn migrate_recordings_leaves_a_custom_folder_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("Meeting Master");
        std::fs::create_dir(&old).unwrap();
        let cfg = AppConfig {
            recordings_dir: Some(tmp.path().join("mine")),
            ..AppConfig::default()
        };
        let store = Store::open_in_memory().unwrap();

        let moved = migrate_recordings(&store, &cfg, &old, &tmp.path().join("Friston")).unwrap();

        assert_eq!(moved, FolderMove::NothingToMove);
        assert!(old.exists());
    }
}
