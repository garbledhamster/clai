use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The user's real home directory. On a native install this is
/// `dirs::home_dir()`. Inside Flatpak `$HOME` is the sandboxed per-app
/// home, so we resolve the *real* host home (via `flatpak-spawn`) and
/// cache it — this is what makes `~/.clai` (and any `~/...` path) point at
/// the same location the native `.deb` install uses, rather than an
/// isolated copy under `~/.var/app/run.clai.CLAI/`. The host
/// home is reachable inside the sandbox because the Flatpak is granted
/// `--filesystem=home`. Falls back to `dirs::home_dir()` if resolution
/// fails, so a missing host-spawn permission degrades to isolated-but-
/// working rather than broken.
///
/// This is also the home the shell filesystem policy is compiled against
/// (`EffectiveFilesystemPolicy::from_config`) and the base for `~` in
/// `fs_request_grant`. Use it instead of `providers::get_home_dir` so every
/// caller shares one cached host lookup and one answer.
pub(crate) fn real_home() -> Option<PathBuf> {
    if crate::providers::is_flatpak() {
        static REAL_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
        return REAL_HOME
            .get_or_init(|| {
                crate::providers::get_home_dir()
                    .map(PathBuf::from)
                    .or_else(dirs::home_dir)
            })
            .clone();
    }
    dirs::home_dir()
}

pub fn clai_home() -> PathBuf {
    std::env::var_os("CLAI_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| real_home().map(|home| home.join(".clai")))
        .unwrap_or_else(|| PathBuf::from(".clai"))
}

/// Ephemeral MCP files share the explicit profile override. Without an
/// override, preserve the legacy location used by installed CLAI builds.
pub fn runtime_tmp_dir() -> Option<PathBuf> {
    runtime_tmp_dir_for(std::env::var_os("CLAI_HOME").as_deref(), dirs::config_dir())
}

fn runtime_tmp_dir_for(home: Option<&OsStr>, config_dir: Option<PathBuf>) -> Option<PathBuf> {
    match home.filter(|value| !value.is_empty()) {
        Some(home) => Some(PathBuf::from(home).join("tmp")),
        None => config_dir.map(|dir| dir.join("clai").join("tmp")),
    }
}

pub fn clai_skills_root() -> PathBuf {
    clai_home().join("skills")
}

pub fn clai_cache_root() -> PathBuf {
    clai_home().join("cache")
}

pub fn clai_cache_bundled_root() -> PathBuf {
    clai_cache_root().join("bundled")
}

pub fn clai_cache_skill_sources_root() -> PathBuf {
    clai_cache_root().join("skill-sources")
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    if value == "~" {
        return real_home().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = real_home() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_temp_preserves_legacy_path_without_nonempty_override() {
        let config = PathBuf::from("/legacy/config");
        let expected = Some(config.join("clai/tmp"));
        assert_eq!(runtime_tmp_dir_for(None, Some(config.clone())), expected);
        assert_eq!(
            runtime_tmp_dir_for(Some(OsStr::new("")), Some(config)),
            expected
        );
        assert_eq!(runtime_tmp_dir_for(None, None), None);
    }

    #[test]
    fn runtime_temp_uses_explicit_profile_even_without_platform_config() {
        assert_eq!(
            runtime_tmp_dir_for(Some(OsStr::new("/preview")), None),
            Some(PathBuf::from("/preview/tmp"))
        );
    }

    #[test]
    fn expand_tilde_passes_absolute_and_relative_through() {
        // Non-tilde paths are returned unchanged regardless of home.
        assert_eq!(expand_tilde(Path::new("/abs/x")), PathBuf::from("/abs/x"));
        assert_eq!(expand_tilde(Path::new("rel/x")), PathBuf::from("rel/x"));
    }

    #[test]
    fn expand_tilde_joins_under_home_when_resolvable() {
        // Tests run on the host (not Flatpak), so real_home() == home_dir().
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde(Path::new("~/.clai")), home.join(".clai"));
            assert_eq!(expand_tilde(Path::new("~")), home);
        }
    }
}
