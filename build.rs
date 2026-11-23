use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-env-changed=ARTINCHIP_FLASH_CHANNEL");
    println!("cargo:rerun-if-changed=assets/artinchip-flash.ico");
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        ".github",
        "crates",
        "docs",
        "scripts",
        "src",
        "tests",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    #[cfg(windows)]
    {
        let mut resources = winres::WindowsResource::new();
        resources.set_icon("assets/artinchip-flash.ico");
        if let Err(error) = resources.compile() {
            panic!("failed to compile Windows icon resource: {error}");
        }
    }

    let commit =
        git_output(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let full_commit = git_output(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    if let Some(reference) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed=.git/{reference}");
    }
    let dirty = Command::new("git")
        .args([
            "diff",
            "--quiet",
            "--ignore-submodules",
            "HEAD",
            "--",
            ".",
            ":(exclude)tools",
        ])
        .output()
        .is_ok_and(|output| output.status.code() == Some(1));
    let build = if dirty {
        format!("{}-dirty", commit)
    } else {
        commit
    };

    println!("cargo:rustc-env=ARTINCHIP_FLASH_BUILD={}", build);
    println!("cargo:rustc-env=ARTINCHIP_FLASH_COMMIT={}", full_commit);
    let channel = std::env::var("ARTINCHIP_FLASH_CHANNEL").unwrap_or_else(|_| "development".into());
    assert!(matches!(
        channel.as_str(),
        "development" | "stable" | "nightly"
    ));
    println!("cargo:rustc-env=ARTINCHIP_FLASH_BUILD_CHANNEL={channel}");
    println!(
        "cargo:rustc-env=ARTINCHIP_FLASH_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}
