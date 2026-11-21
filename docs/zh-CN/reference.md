# 命令与兼容性参考

[文档目录](README.md) · [English](../en/reference.md)

## CLI 与 GUI 入口

用 `artinchip-flash <command> --help` 查看参数和选项。GUI 与 CLI 使用同一组 Rust 服务。诊断结果可复制或导出为 JSON。需要导出烧录事件时，在烧录前通过「高级选项」启用 JSON 记录，完成后在「工具」或「设置」中打开「最近一次结构化结果」。

| CLI | GUI 入口 |
| --- | --- |
| `scan` | 烧录或调试 → USB 选择框，或工具 → 扫描 ArtInChip USB |
| `usb-list` | 工具 → 列出所有 USB 设备 |
| `serial-list` | 烧录或调试 → UART 选择框，或工具 → 列出串口 |
| `burn` | 烧录 → 开始烧录，或工具 → 离线烧录计划 |
| `info` | 烧录 → 固件详情，或设备详情 → 设备信息 |
| `uart-monitor` | 烧录 → UART → 串口监视器 |
| `env-check` | 工具 → 环境检查 |
| `install-usb-access` | 工具 → 驱动 |
| `write` | 调试 → 写入文件到内存 |
| `read` | 调试 → 保存内存 |
| `writel` | 调试 → 写入 32 位字 |
| `readl` | 调试 → 读取 32 位字 |
| `exec` | 调试 → 执行指定地址 |
| `hexdump` | 调试 → 查看内存 |
| `fill` | 调试 → 填充字节 |
| `clear` | 调试 → 清零内存 |
| `memtest` | 调试 → 测试并恢复 RAM |
| `shcmd` | 调试 → Bootloader 命令 |
| `log` | 调试 → 读取设备日志 |
| `stats` | 设置 → 烧录统计 |
| `sd-list` | 工具 → 列出物理磁盘（只读） |
| `update` | 设置 → 检查更新 |

![原生调试操作](../screenshots/debug.png)

## 存储查询与擦除预览

「工具」通过原生 UPG 查询介质和分区表文本。几何查询为实验性功能，需要 U-Boot 阶段 1，且依赖 Bootloader 支持。擦除范围在离线计算器中计算。Flash 几何使用字节，MMC 几何使用逻辑扇区，须核对每扇区字节数。偏移和长度始终使用字节。按分区计算时需要物理边界。`ubiroot:rootfs` 等 UBI 卷是升级目标，其逻辑范围不能用作原始 NAND 擦除范围。

![存储查询与离线擦除预览](../screenshots/storage.png)

## AiBurn 兼容性与限制

- 常规 USB/UART 烧录、镜像检查、内存和 Bootloader 调试使用原生实现。eFuse 和 JTAG 等可选厂商命令需要单独安装 `upgcmd` 并在设置中配置。ADB 升级入口使用已配置的可执行程序，或 `PATH` 中的 Android Platform Tools。
- 烧录前全片擦除使用 `upgcmd flasherase`，要求选择全部目标组件、明确锁定设备并核对设备报告的介质。擦除或强制升级前，GUI 会显示选中目标并要求确认。强制升级为实验性功能，完成后跳过复位。
- 工具支持只读枚举物理磁盘。当前不支持制作 SD 启动卡、原生 bulk 擦除或原生 eFuse 操作。待验证项目见[硬件验证](development.md#硬件验证)。
