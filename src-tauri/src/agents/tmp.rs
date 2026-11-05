//! Temporary-file helpers for agent runtime setup.

/// Clears the CLAI temp directory.
///
/// Safe to call on startup even if the directory does not exist.
pub fn clear_tmp_dir() {
    clear_tmp_dir_at(crate::paths::runtime_tmp_dir());
}

fn clear_tmp_dir_at(tmp_dir: Option<std::path::PathBuf>) {
    if let Some(tmp_dir) = tmp_dir {
        if tmp_dir.exists() {
            match std::fs::remove_dir_all(&tmp_dir) {
                Ok(_) => tracing::debug!(path = %tmp_dir.display(), "Cleared clai tmp directory"),
                Err(e) => {
                    tracing::warn!(error = %e, path = %tmp_dir.display(), "Failed to clear clai tmp directory");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cleanup_removes_preview_files_and_preserves_live_sibling() {
        let root = tempfile::tempdir().unwrap();
        let preview = root.path().join("preview/tmp");
        let live = root.path().join("live/tmp");
        std::fs::create_dir_all(&preview).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(preview.join("mcp.json"), "preview").unwrap();
        let sentinel = live.join("mcp.json");
        std::fs::write(&sentinel, "live").unwrap();
        super::clear_tmp_dir_at(Some(preview.clone()));
        assert!(!preview.exists());
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "live");
    }
}
