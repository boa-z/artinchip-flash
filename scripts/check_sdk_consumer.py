"""Exercise the public SDK API in a temporary external Cargo package."""

import json
from pathlib import Path
import shutil
import subprocess
from tempfile import TemporaryDirectory


repo = Path(__file__).resolve().parent.parent
sdk = repo / "crates" / "artinchip-flash"
with TemporaryDirectory(prefix="aic-sdk-") as directory:
    project = Path(directory)
    (project / "src").mkdir()
    shutil.copyfile(repo / "tests" / "sdk-consumer" / "consumer.rs", project / "src" / "main.rs")
    shutil.copytree(repo / "tests" / "support", project / "support")
    shutil.copyfile(repo / "Cargo.lock", project / "Cargo.lock")
    shutil.copyfile(repo / "rust-toolchain.toml", project / "rust-toolchain.toml")
    manifest = f"""[workspace]

[package]
name = "offline-sdk-consumer"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
artinchip-flash = {{ path = {json.dumps(sdk.as_posix(), ensure_ascii=False)}, default-features = false }}
"""
    (project / "Cargo.toml").write_text(manifest, encoding="utf-8")
    subprocess.run(["cargo", "fmt", "--", "--check"], cwd=project, check=True)
    tree = subprocess.check_output(["cargo", "tree", "--offline", "--prefix", "none", "--edges", "normal"], cwd=project, text=True)
    dependencies = {line.split()[0] for line in tree.splitlines() if line}
    if unexpected := dependencies.intersection({"rusb", "serialport", "egui", "eframe", "rfd", "ureq", "rustls"}):
        raise SystemExit(f"Unexpected device or GUI dependencies: {sorted(unexpected)}")
    subprocess.run(["cargo", "run", "--locked"], cwd=project, check=True)
