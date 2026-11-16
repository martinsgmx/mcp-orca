//! Small workspace path helper.

use std::path::PathBuf;

#[must_use]
pub fn workspace_path(root: &str) -> PathBuf {
    if let Some(rest) = root.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(root)
}
