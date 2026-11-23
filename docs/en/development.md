# Development

[Documentation](README.md) · [简体中文](../zh-CN/development.md)

## Build

Install [rustup](https://rustup.rs) to use the toolchain pinned in `rust-toolchain.toml`. On Debian and Ubuntu the CLI needs a C toolchain, `libusb-1.0-0-dev`, `libudev-dev` and `pkg-config`. The default `gui` feature also needs the Linux desktop dependencies from [CI](../../.github/workflows/ci.yml). On macOS use `brew install libusb`. Windows release builds use shared libusb through vcpkg. The egui GUI uses Glow and the Linux file dialog uses the desktop portal. Install `fonts-noto-cjk` if Chinese glyphs are missing on Linux.

```sh
cargo build --locked --release --bins
cargo build --locked --release --no-default-features --bin artinchip-flash
```

The CLI and GUI binaries are under `target/release/`.

## Software verification

```sh
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo test --locked --no-default-features --all-targets
cargo check --locked -p artinchip-flash --no-default-features
cargo check --locked -p artinchip-flash --no-default-features --features usb
cargo check --locked -p artinchip-flash --no-default-features --features uart
python3 scripts/check_sdk_consumer.py
```

On Linux x86-64, `qemu-x86_64 -cpu Nehalem target/release/artinchip-flash --help` and the same command with `--version` exercise the release CLI on a non-AVX CPU model. CI checks `ldd` and the CLI DEB's `Depends`, then checks package installation, CLI startup under QEMU, GUI startup under Xvfb and package switching on Debian 13. Update checks use Rustls with ring and the platform trust store.

The suite exercises fixed protocol vectors, firmware validation, command ordering, failure handling and CLI output using synthetic images and scripted transports. The external SDK check builds a separate Cargo project without USB, UART or GUI dependencies and runs a simulated partition upgrade through the public API.

## Hardware validation

The D213 hardware checklist is pending. Use a recoverable board and known-good firmware. Record the host OS and build SHA, board and bootloader revisions, storage medium and firmware SHA-256. Collect the offline plan, exit status, event log and any serial console output.

1. Check USB discovery and explicit target selection with multiple devices. Verify reconnect at the same physical port. Check the offline plan and rejection of a corrupted image.
2. Verify the stage-1 partition upgrade selects `image.info` and the requested target. Verify the written partition and check that unrelated partitions retain their contents. Separately confirm the stage-0 USB burn, updater reconnect, device CRC and reboot.
3. Verify UART handshake, baud negotiation, updater handoff and reboot. Check GUI progress, cancellation, errors and the no-device state. Compare GUI and CLI results.
4. Verify cancellation and recovery after disconnect on the test board. Validate storage media and partition queries separately. Test vendor pre-erase only on a sacrificial board with a verified medium and pinned target.

## Release

Update the root `Cargo.toml` version and `Cargo.lock`, then tag the release commit as `vX.Y.Z`. Publishing the tag triggers the Windows, Linux and macOS build and release workflow. A push to `main` refreshes `nightly` unless the commit message contains `[skip nightly]`.

## Licensing and provenance

Project source, docs and authored assets use Apache-2.0. See [LICENSE](../../LICENSE) and [NOTICE](../../NOTICE). The Rust code implements wire layouts referenced in the [Luban-Lite bootloader and UPG sources](https://github.com/artinchip/luban-lite/tree/77fbe30dd2de366fae702ece84e1694153d94591). The parser uses firmware layout information from Luban-Lite's GPL-2.0-or-later `mk_image.py`. The distribution contains the independent Rust implementation and supports separately installed AiBurn tools.

Native libusb is LGPL-2.1-or-later. Windows packages ship the replaceable DLL and `libusb-source.zip` with upstream source, license and the vcpkg port recipe. Linux and macOS use shared libraries. CI audits dependency licenses and ships `THIRD-PARTY-LICENSES.txt`, including Rust dependencies and embedded font terms. Use `python3 scripts/dependency_licenses.py --target <rust-triple> --output THIRD-PARTY-LICENSES.txt` to generate the attributions locally.
