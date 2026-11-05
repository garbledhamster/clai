//! The local scheduler preview is opt-in at build time; normal builds do not
//! select a profile or change startup behavior through this module.

use std::path::{Path, PathBuf};

pub(crate) const PREVIEW_HOME: Option<&str> = option_env!("CLAI_PREVIEW_HOME");

pub(crate) fn is_preview() -> bool {
    PREVIEW_HOME.is_some()
}

pub(crate) fn prepare_startup_or_exit() {
    if let Err(error) = prepare_startup() {
        eprintln!("CLAI preview refused to start: {error}");
        std::process::exit(1);
    }
}

pub(crate) fn configure_updater<R: tauri::Runtime>(
    builder: tauri::Builder<R>,
) -> tauri::Builder<R> {
    // Preview builds must also reject direct updater-plugin IPC commands.
    if is_preview() {
        builder
    } else {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    }
}

fn prepare_startup() -> Result<(), String> {
    if let Some(home) = startup_home(PREVIEW_HOME)? {
        // This runs before spawning threads or initializing any application
        // state. Finder launches and shell launches therefore use one profile.
        std::env::set_var("CLAI_HOME", home);
        std::env::remove_var("CLAI_CC_DEBUG");
    }
    Ok(())
}

fn startup_home(baked_home: Option<&str>) -> Result<Option<PathBuf>, String> {
    baked_home
        .map(|home| validate_profile(Path::new(home)))
        .transpose()
}

fn validate_profile(home: &Path) -> Result<PathBuf, String> {
    if !home.is_absolute() {
        return Err("the build-time preview home must be an absolute path".into());
    }
    let home = home
        .canonicalize()
        .map_err(|error| format!("preview home: {error}"))?;
    let config_path = home.join("config.json");
    let resolved_config = config_path
        .canonicalize()
        .map_err(|error| format!("preview config: {error}"))?;
    if !resolved_config.starts_with(&home) {
        return Err("preview config resolves outside the preview home".into());
    }
    let bytes = std::fs::read(&config_path).map_err(|error| format!("preview config: {error}"))?;
    let raw: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid preview config: {error}"))?;
    // Check presence before deserialization: AppConfig otherwise defaults this
    // field to the live ~/.clai/workspaces directory.
    let roots = raw
        .get("workspaceDirs")
        .and_then(serde_json::Value::as_array)
        .filter(|roots| !roots.is_empty())
        .ok_or("preview config needs explicit, nonempty workspaceDirs")?;
    for root in roots {
        let root = Path::new(root.as_str().ok_or("workspaceDirs entries must be paths")?);
        if !root.is_absolute() {
            return Err("preview workspace roots must be absolute paths".into());
        }
        let resolved = root
            .canonicalize()
            .map_err(|error| format!("preview workspace root: {error}"))?;
        if !resolved.starts_with(&home) || !resolved.is_dir() {
            return Err(
                "preview workspace roots must be directories inside the preview home".into(),
            );
        }
    }
    serde_json::from_slice::<crate::config::ClaiConfig>(&bytes)
        .map_err(|error| format!("invalid preview config: {error}"))?;
    Ok(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(home: &Path, roots: serde_json::Value) {
        std::fs::write(
            home.join("config.json"),
            serde_json::to_vec(&serde_json::json!({
                "workspaceDirs": roots,
                "schedulerPaused": true,
                "mcpServers": [],
                "skillSources": [],
                "providerConnections": []
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn normal_build_does_not_require_or_select_a_preview_profile() {
        assert_eq!(startup_home(None).unwrap(), None);
    }

    #[test]
    fn preview_requires_an_absolute_home_and_a_valid_explicit_seed() {
        assert!(startup_home(Some("")).is_err());
        assert!(startup_home(Some("relative")).is_err());
        let home = tempfile::tempdir().unwrap();
        let home_text = home.path().to_str().unwrap();
        assert!(startup_home(Some(home_text)).is_err());
        std::fs::write(home.path().join("config.json"), "invalid").unwrap();
        assert!(startup_home(Some(home_text)).is_err());
        std::fs::write(home.path().join("config.json"), "{}").unwrap();
        assert!(startup_home(Some(home_text)).is_err());
        seed(home.path(), serde_json::json!([]));
        assert!(startup_home(Some(home_text)).is_err());
    }

    #[test]
    fn preview_selects_baked_home_with_explicit_contained_workspace() {
        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().join("workspaces");
        std::fs::create_dir(&workspace).unwrap();
        seed(home.path(), serde_json::json!([workspace]));
        assert_eq!(
            startup_home(Some(home.path().to_str().unwrap())).unwrap(),
            Some(home.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn rejected_seed_does_not_cleanup_or_write_any_files() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("preview");
        let live = root.path().join("live");
        std::fs::create_dir_all(home.join("tmp")).unwrap();
        std::fs::create_dir(&live).unwrap();
        let sentinel = home.join("tmp/sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        seed(&home, serde_json::json!([live]));
        let before = std::fs::read(home.join("config.json")).unwrap();
        assert!(startup_home(Some(home.to_str().unwrap())).is_err());
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep");
        assert_eq!(std::fs::read(home.join("config.json")).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn preview_rejects_symlinks_and_parent_traversal_outside_home() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("preview");
        let live = root.path().join("live");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&live).unwrap();
        std::os::unix::fs::symlink(&live, home.join("linked")).unwrap();
        for path in [
            home.join("linked"),
            home.join("../live"),
            PathBuf::from("~/.clai/workspaces"),
        ] {
            seed(&home, serde_json::json!([path]));
            assert!(validate_profile(&home).is_err());
        }
    }
}
