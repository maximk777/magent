use std::path::{Path, PathBuf};

/// Overrides the state directory. Tests set this so they never touch the real
/// profile; a test that corrupted working memory would be unrecoverable.
pub const STATE_DIR_ENV: &str = "MAGENT_STATE_DIR";

/// Where Magent keeps its state.
///
/// `magent.db` is canonical and small enough to back up. Derived indexes live
/// beside it in their own files so they can be deleted and rebuilt.
#[must_use]
pub fn state_dir() -> PathBuf {
    if let Some(explicit) = std::env::var_os(STATE_DIR_ENV) {
        return PathBuf::from(explicit);
    }

    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".magent"),
        |home| Path::new(&home).join(".magent"),
    )
}

#[must_use]
pub fn database_path(state_dir: &Path) -> PathBuf {
    state_dir.join("magent.db")
}
