# artinchip-flash

[English documentation](docs/en/README.md) · [简体中文文档](docs/zh-CN/README.md)

Cross-platform CLI and egui GUI for flashing ArtInChip SoCs over USB or UART, with a Rust SDK for firmware inspection, upgrade plans and device operations.

> [!IMPORTANT]
> **Known macOS compatibility issue.** Native USB flashing of the D213 in Boot ROM (BROM, `33C3:6677`) is unavailable on the tested Apple Silicon macOS setup. The device can appear in the USB registry while configuration and bulk access fail. Use a known-working Windows or Linux host for BROM recovery. [Research findings](docs/en/reference.md#macos-d213-brom-usb) · [简体中文](docs/zh-CN/reference.md#macos-d213-brom-usb)

## Quick start

On a headless Debian 13 x86-64 server, install `artinchip-flash-cli-linux-x64.deb`. Use `artinchip-flash-linux-x64.deb` for both the CLI and desktop GUI. See [Linux installation](docs/en/usage.md#linux-installation) or [简体中文](docs/zh-CN/usage.md#linux-安装).

CI also builds Intel macOS, Windows arm64 and Linux arm64 packages. These three variants are pending physical hardware testing. [Platform details](docs/en/development.md#hardware-validation) · [简体中文](docs/zh-CN/development.md#硬件验证)

Download packages from [GitHub Releases](https://github.com/boa-z/artinchip-flash/releases), or build with the repository's pinned Rust toolchain. See the [build prerequisites](docs/en/development.md#build).

```sh
cargo build --locked --release --bins
cargo build --locked --release --no-default-features --bin artinchip-flash
```

Put the board in upgrade mode, check the image and preview the plan before selecting the target. See the [user guide](docs/en/usage.md) for installation, UART and GUI instructions.

```sh
artinchip-flash scan
artinchip-flash info firmware.img
artinchip-flash burn firmware.img --plan --json
```

`artinchip-flash --version` reports the application version, commit, build channel and target. For scripting, see the [CLI output and exit codes](docs/en/reference.md#cli-contract) or [简体中文](docs/zh-CN/reference.md#cli-契约).

![Burn page with a validated synthetic image](docs/screenshots/burn.jpg)

## Rust SDK

Use the `artinchip-flash` package in `crates/artinchip-flash`. Enable USB or UART as needed, or disable default features for offline use. See the [SDK guide](docs/en/sdk.md).

## License

Project code and assets use the [Apache License 2.0](LICENSE). See [licensing and provenance](docs/en/development.md#licensing-and-provenance) for upstream sources and third-party terms.
