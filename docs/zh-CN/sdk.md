# Rust SDK

[文档目录](README.md) · [English](../en/sdk.md)

## 引入 SDK 并生成计划

通过本地检出目录中的 `crates/artinchip-flash` 引入 SDK，也可使用 Git 依赖，在已核对的提交上选择 `artinchip-flash` 包。仅做离线处理时可禁用默认特性。

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

`FirmwareImage::validate` 检查整个镜像。`prepare` 在应用指定分区选择的同时执行相同的完整性检查。分区别名必须唯一对应一个目标。`BurnOptions::no_updater` 选择阶段 1，即 U-Boot 升级模式。默认使用阶段 0，即 ROM/BROM。`FlashError` 区分镜像 I/O、镜像格式、无效计划、设备操作、设备 CRC 不匹配和用户取消。匹配枚举时保留通配分支，以兼容未来新增的变体。命令层和传输层 API 继续返回描述性的 `String` 错误。

## 选择传输方式并执行

启用 `usb` 后可用 `AicDevice::scan_selectable_devices` 和 `AicDevice::open_selector`。使用 `DeviceSelector::PhysicalPort` 锁定 USB 设备，或使用 `DeviceSelector::Unique` 拒绝设备选择不明确的情况。启用 `uart` 后可用 `UartDevice::list_ports`、`UartDevice::open_port` 和 `UartDevice::open_auto`。连接多台开发板时，明确选择串口。两个特性默认均开启。

用 `prepared.execute(&mut device, Some(&mut callback))` 同步执行升级，设备既可使用原生适配器，也可由 `UpgDevice::new(custom_transport)` 创建。回调在执行线程上接收 `BurnEvent`。在生成计划前为 `BurnOptions::cancel` 配置 `Arc<AtomicBool>`。执行器在事务边界检查取消标志，已完成的写入会保留。可用 `UpgDevice::get_hwinfo` 等命令查询设备。

## 工作区结构

| 包 | 职责 |
| --- | --- |
| `aic-upg-core` | 镜像解析与校验、UPG 命令、升级计划及基于 `UpgTransport` 的执行 |
| `artinchip-flash` | 公共 Rust SDK、可选 USB/UART 适配器及存储范围计算 |
| `artinchip-flash-app` | CLI 和 egui GUI、共用的 `FlashJob`、配置、厂商兼容和更新 |

应用先验证并生成计划，再打开目标设备。CLI 与 GUI 消费相同的烧录事件。SDK 可以独立于 AiBurn 使用。外部 SDK 集成测试见[软件验证](development.md#软件验证)。
