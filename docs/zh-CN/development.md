# 开发指南

[文档目录](README.md) · [English](../en/development.md)

## 构建

安装 [rustup](https://rustup.rs) 以使用 `rust-toolchain.toml` 锁定的工具链。在 Debian 和 Ubuntu 上，纯 CLI 构建需要 C 工具链、`libusb-1.0-0-dev`、`libudev-dev` 和 `pkg-config`。默认启用 `gui` 特性，还需要 [CI](../../.github/workflows/ci.yml) 中列出的 Linux 桌面依赖。macOS 使用 `brew install libusb`。Windows 发布构建通过 vcpkg 链接共享 libusb。egui 界面使用 Glow，Linux 文件对话框使用桌面门户。Linux 缺少中文字形时可安装 `fonts-noto-cjk`。

```sh
cargo build --locked --release --bins
cargo build --locked --release --no-default-features --bin artinchip-flash
```

CLI 和 GUI 可执行文件位于 `target/release/`。

## 软件验证

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

在 Linux x86-64 上，运行 `qemu-x86_64 -cpu Nehalem target/release/artinchip-flash --help`，再用同一命令运行 `--version`，即可在无 AVX 的 CPU 模型下测试发布版 CLI。CI 检查 `ldd` 和 CLI DEB 的 `Depends`，并在 Debian 13 上验证安装、QEMU 下的 CLI 启动、Xvfb 下的 GUI 启动和包切换。更新检查使用 Rustls、ring 和系统证书库。

测试使用合成镜像和脚本化传输，覆盖固定协议向量、固件校验、命令顺序、故障处理和 CLI 输出。外部 SDK 检查会构建一个独立 Cargo 项目，确认其依赖不含 USB、UART 或 GUI，再通过公共 API 执行模拟分区升级。

## 硬件验证

D213 硬件验证待执行。使用可恢复的开发板和已知可用的固件。记录主机系统和构建 SHA、板卡和 Bootloader 版本、存储介质及固件 SHA-256。保存离线计划、退出码、事件日志及串口输出。

1. 检查多 USB 设备的发现和显式选择，验证设备能在同一物理端口重连。检查离线计划和损坏镜像拒绝行为。
2. 验证阶段 1 分区升级选择 `image.info` 和指定目标。核对写入分区，并确认其他分区内容保持一致。单独验证阶段 0 的 USB 完整烧录、updater 重连、设备 CRC 和复位。
3. 验证 UART 握手、波特率协商、updater 交接和复位。核对 GUI 进度、取消、失败及无设备状态，并比较 GUI 与 CLI 结果。
4. 在测试板上验证取消和断连后的恢复，单独验证介质与分区查询。厂商擦除验证仅使用可牺牲的板卡，并核对介质与锁定目标。

## 发版

更新根目录 `Cargo.toml` 版本和 `Cargo.lock`，为发布提交创建 `vX.Y.Z` 标签。推送标签会触发 Windows、Linux 和 macOS 的构建与发布流程。推送到 `main` 会刷新 `nightly`，提交说明带 `[skip nightly]` 时跳过刷新。

## 授权与来源

项目源码、文档和自制资产采用 Apache-2.0，见 [LICENSE](../../LICENSE) 和 [NOTICE](../../NOTICE)。Rust 代码实现了 [Luban-Lite Bootloader 与 UPG 源码](https://github.com/artinchip/luban-lite/tree/77fbe30dd2de366fae702ece84e1694153d94591) 所记录的线格式。解析器参考 Luban-Lite 的 `mk_image.py`（GPL-2.0-or-later）中的固件布局。发行包包含独立的 Rust 实现，支持调用另行安装的 AiBurn 工具。

原生 libusb 使用 LGPL-2.1-or-later。Windows 包提供可替换 DLL 及包含上游源码、许可证和 vcpkg port 配方的 `libusb-source.zip`。Linux 和 macOS 使用共享库。CI 审计依赖许可证，并随包提供包含 Rust 依赖和内嵌字体授权的 `THIRD-PARTY-LICENSES.txt`。可用 `python3 scripts/dependency_licenses.py --target <rust-triple> --output THIRD-PARTY-LICENSES.txt` 在本地生成授权清单。
