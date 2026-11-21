# Rust SDK

[Documentation](README.md) · [简体中文](../zh-CN/sdk.md)

## Install and prepare a plan

Add the SDK with a path to `crates/artinchip-flash` in a checkout. A Git dependency can also select the `artinchip-flash` package at a reviewed commit. Disable default features for offline use.

```toml
[dependencies]
artinchip-flash = { path = "/path/to/artinchip-flash/crates/artinchip-flash", default-features = false }
```

```rust
use artinchip_flash::{FirmwareImage, FlashError};

fn plan(path: &std::path::Path) -> Result<(), FlashError> {
    let image = FirmwareImage::open(path)?;
    println!("{} {}", image.summary().product, image.summary().version);
    let mut options = image.default_options();
    options.selected_parts = vec!["os".to_owned()];
    options.partition_upgrade = true;
    let prepared = image.prepare(options)?;
    println!("{}", prepared.plan().to_json());
    Ok(())
}
```

`FirmwareImage::validate` checks the whole image. `prepare` performs those integrity checks while applying a particular partition selection. An alias must identify exactly one target. `BurnOptions::no_updater` selects stage 1, which is the U-Boot upgrade mode. The default expects stage 0, or ROM/BROM. `FlashError` distinguishes image I/O, malformed images, invalid plans, device operations, device CRC mismatches and cancellation. Match it with a wildcard for future variants. The command-level and transport APIs retain their descriptive `String` errors.

## Select a transport and execute

Enable `usb` to use `AicDevice::scan_selectable_devices` and `AicDevice::open_selector`. Select `DeviceSelector::PhysicalPort` to pin a USB target or `DeviceSelector::Unique` to reject ambiguous discovery. Enable `uart` for `UartDevice::list_ports`, `UartDevice::open_port` and `UartDevice::open_auto`. Use an explicit port when multiple boards are connected. Both features are on by default.

Use `prepared.execute(&mut device, Some(&mut callback))` for a synchronous upgrade over a native device or `UpgDevice::new(custom_transport)`. The callback receives `BurnEvent` values on the executing thread. Configure `BurnOptions::cancel` with an `Arc<AtomicBool>` before preparing the job. The engine checks the flag at transaction boundaries and leaves completed writes in place. `UpgDevice::get_hwinfo` and other command methods are available for device queries.

## Workspace layout

| Package | Role |
| --- | --- |
| `aic-upg-core` | Firmware parsing, validation, UPG commands, planning and execution via `UpgTransport` |
| `artinchip-flash` | Public Rust SDK, optional USB/UART adapters and storage range calculation |
| `artinchip-flash-app` | CLI and egui GUI, shared `FlashJob`, configuration, vendor compatibility and updates |

The application validates and prepares the plan before opening the target. Both frontends consume the same burn events. The SDK has no AiBurn dependency. See [software verification](development.md#software-verification) for the external SDK integration test.
