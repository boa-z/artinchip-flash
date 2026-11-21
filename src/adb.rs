use std::path::{Path, PathBuf};
use std::process::Command;

pub fn enter_upgrade(
    configured: Option<&Path>,
    compatibility_dir: &Path,
) -> Result<String, String> {
    let executable = resolve(configured, compatibility_dir)?;
    let output = Command::new(&executable)
        .args(["shell", "aicupg"])
        .output()
        .map_err(|error| format!("Failed to start {}: {error}", executable.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!(
            "ADB exited with {}\n{}{}",
            output.status, stdout, stderr
        ));
    }
    Ok(format!("{stdout}{stderr}"))
}

fn resolve(configured: Option<&Path>, compatibility_dir: &Path) -> Result<PathBuf, String> {
    if let Some(configured) = configured {
        if !configured.is_file() {
            return Err(format!(
                "Configured ADB executable does not exist: {}",
                configured.display()
            ));
        }
        return configured.canonicalize().map_err(|error| error.to_string());
    }
    let filename = if cfg!(windows) { "adb.exe" } else { "adb" };
    if let Some(paths) = std::env::var_os("PATH") {
        if let Some(executable) = std::env::split_paths(&paths)
            .filter(|directory| directory.is_absolute())
            .map(|directory| directory.join(filename))
            .find(|path| path.is_file())
        {
            return Ok(executable);
        }
    }
    let bundled = compatibility_dir.join("adb").join(filename);
    if !compatibility_dir.as_os_str().is_empty() && bundled.is_file() {
        return bundled.canonicalize().map_err(|error| error.to_string());
    }
    Err(
        "ADB not found. Install Android Platform Tools or choose the ADB executable in Settings"
            .into(),
    )
}
