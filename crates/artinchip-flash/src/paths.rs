//! Shared per-user configuration and USB lock location.

use std::path::PathBuf;

/// Resolve the configuration directory using the platform’s existing conventions.
pub fn config_dir(app_name: &str) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join(app_name);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_dir() {
            return home
                .join("Library")
                .join("Application Support")
                .join(app_name);
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg).join(app_name);
        }
        if let Some(home) = home_dir() {
            return home.join(".config").join(app_name);
        }
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join(app_name);
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(home).join(format!(".{app_name}"));
    }
    PathBuf::from(format!(".{app_name}"))
}

#[cfg(any(unix, target_os = "macos"))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}
