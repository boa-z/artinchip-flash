use std::fs;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use artinchip_flash::{FirmwareImage, FlashError};
use artinchip_flash_app::build_info;
use artinchip_flash_app::burn_stats::{self, BurnOutcome};
use artinchip_flash_app::device::{BurnOptions, BurnReport, UpgDevice};
use artinchip_flash_app::flash_job::{self, EraseRequest, FlashJob, JobError};
use artinchip_flash_app::image;
use artinchip_flash_app::sdcard;
use artinchip_flash_app::services::{self, DeviceTarget, UartSpec};
use artinchip_flash_app::standalone;
use artinchip_flash_app::transport::UpgTransport;
use artinchip_flash_app::update::{self, UpdateChannel};
use artinchip_flash_app::usb;
use artinchip_flash_app::verbosity;
use clap::{Args, Parser, Subcommand, ValueEnum};

#[path = "cli/burn_output.rs"]
mod burn_output;
use burn_output::BurnOutput;

#[derive(Parser)]
#[command(
    name = "artinchip-flash",
    version = build_info::VERSION,
    long_version = build_info::LONG_VERSION,
    about = "Cross-platform flasher for ArtInChip SoCs"
)]
struct Cli {
    /// Verbose transport-level logging (CBW/CSW bytes, UART framing)
    #[arg(long, global = true)]
    verbose: bool,
    /// Machine-readable output (scan, usb-list, serial-list, info, env-check, burn, stats, sd-list, update)
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}

#[cfg(test)]
mod gui_parity_tests {
    use super::Cli;
    use clap::{error::ErrorKind, CommandFactory, Parser};

    #[test]
    fn every_cli_subcommand_has_one_documented_gui_entry() {
        let matrices = [
            ("en", include_str!("../docs/en/reference.md")),
            ("zh-CN", include_str!("../docs/zh-CN/reference.md")),
        ];
        let command = Cli::command();
        let names = command
            .get_subcommands()
            .map(|command| command.get_name())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 22);
        for name in &names {
            let result = Cli::command().try_get_matches_from(["artinchip-flash", name, "--help"]);
            assert_eq!(result.unwrap_err().kind(), ErrorKind::DisplayHelp, "{name}");
        }
        for (locale, matrix) in matrices {
            let entries = matrix
                .lines()
                .skip_while(|line| !line.starts_with("| CLI |"))
                .take_while(|line| line.starts_with('|'))
                .collect::<Vec<_>>();
            for name in &names {
                let row = format!("| `{name}` |");
                assert_eq!(
                    entries.iter().filter(|line| line.starts_with(&row)).count(),
                    1,
                    "{locale}: {name}"
                );
            }
        }
    }

    #[test]
    fn all_subcommands_accept_their_documented_arguments() {
        for command in [
            "scan --require-device",
            "usb-list --json",
            "serial-list --json",
            "burn image.img --plan --uart COM4 --baud 115200",
            "info image.img",
            "uart-monitor /dev/cu.serial",
            "env-check image.img",
            "install-usb-access",
            "write 0x41000000 payload.bin --device 9-2",
            "read 0x41000000 8 dump.bin",
            "writel 0x41000000 1",
            "readl 0x41000000 --uart /dev/ttyUSB0",
            "exec 0x41000000",
            "hexdump 0x41000000 16",
            "fill 0x41000000 16 0xff",
            "clear 0x41000000 16",
            "memtest 0x41000000 16",
            "shcmd version",
            "log --device 1:2",
            "stats",
            "sd-list",
            "update --channel nightly",
        ] {
            assert!(
                Cli::try_parse_from(
                    std::iter::once("artinchip-flash").chain(command.split_whitespace())
                )
                .is_ok(),
                "{command}"
            );
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Scan for connected ArtInChip USB devices
    Scan {
        /// Return exit code 3 if the result is empty
        #[arg(long)]
        require_device: bool,
    },
    /// List all USB devices visible through libusb
    UsbList,
    /// List serial ports that can be used for UART firmware updates
    SerialList,
    /// Burn firmware image to device
    Burn {
        /// Path to the firmware image file (.img)
        #[arg(value_name = "IMAGE")]
        image: PathBuf,
        /// Do not reset device after burn
        #[arg(long)]
        no_reset: bool,
        /// Experimental force upgrade (BURN_IMG_FORCE, skips post-burn reset).
        /// Requires the device-side force-upgrade switch; unverified hardware.
        #[arg(long)]
        force_upgrade: bool,
        /// Target partitions (repeatable or comma-separated).  Selects
        /// partition-upgrade mode and only sends those image.target.*
        /// components; defaults to spl,env,os for RT-Thread and all targets for Linux.
        #[arg(
            long,
            value_name = "PART",
            value_delimiter = ',',
            action = clap::ArgAction::Append
        )]
        partition: Vec<String>,
        /// Print the validated burn plan and exit without touching a device
        #[arg(long)]
        plan: bool,
        /// Skip updater components (device already in bootloader upgrade mode)
        #[arg(long)]
        no_updater: bool,
        /// Erase the whole chip via `upgcmd flasherase` before burning.
        /// Requires a specific --device or --uart port and an installed upgcmd.
        #[arg(long)]
        erase_all: bool,
        /// Media id for --erase-all; defaults to the image media_dev_id
        #[arg(long, value_name = "ID")]
        erase_media: Option<String>,
        /// Path to official upgcmd(.exe) used by --erase-all
        #[arg(long, value_name = "PATH")]
        upgcmd_path: Option<PathBuf>,
        /// Save the burn event log to a file (in addition to console output)
        #[arg(long, value_name = "PATH")]
        log_file: Option<PathBuf>,
        /// Console progress style (JSON output always uses NDJSON)
        #[arg(long, value_enum, default_value = "auto")]
        progress: ProgressMode,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Show device information, or parse an .img file
    Info {
        /// Optional .img file to parse instead of querying a device
        #[arg(value_name = "IMAGE", conflicts_with_all = ["device", "uart"])]
        image: Option<PathBuf>,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Interactive UART monitor: view device output and send console commands
    UartMonitor {
        /// Serial port, or "auto" to use the only USB serial port (or only serial port)
        #[arg(value_name = "PORT", default_value = "auto")]
        port: String,
        /// Baudrate of the device console
        #[arg(long, default_value_t = 115200)]
        baud: u32,
        /// Immediately send `aicupg gotobl` / `aicupg uart 0`
        #[arg(long)]
        enter_upg: bool,
    },
    /// Check local config, USB access, and optional image parsing
    EnvCheck {
        /// Optional .img file to parse during the check
        #[arg(value_name = "IMAGE")]
        image: Option<PathBuf>,
    },
    /// Install platform USB access support (WinUSB INF or Linux udev rule)
    InstallUsbAccess,
    /// Write a file to device memory (native UPG WRITE, no upgcmd needed)
    Write {
        /// Memory address (hex like 0x41000000, decimal, or 4k/1m suffix)
        #[arg(value_name = "ADDR")]
        address: String,
        /// Input file whose bytes are written
        #[arg(value_name = "FILE")]
        input: PathBuf,
        /// Skip this many input bytes before writing
        #[arg(long, value_name = "N")]
        skip: Option<String>,
        /// Write at most this many bytes
        #[arg(long, value_name = "N")]
        length: Option<String>,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Read device memory to a file (native UPG READ)
    Read {
        /// Memory address
        #[arg(value_name = "ADDR")]
        address: String,
        /// Byte count
        #[arg(value_name = "LEN")]
        length: String,
        /// Output file
        #[arg(value_name = "FILE")]
        output: PathBuf,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Write one 32-bit little-endian word (native)
    Writel {
        /// Memory address
        #[arg(value_name = "ADDR")]
        address: String,
        /// 32-bit value
        #[arg(value_name = "VALUE")]
        value: String,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Read one 32-bit little-endian word (native)
    Readl {
        /// Memory address
        #[arg(value_name = "ADDR")]
        address: String,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Call the function at an address with no arguments (native UPG EXEC)
    Exec {
        /// Function address
        #[arg(value_name = "ADDR")]
        address: String,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Hexdump device memory (native UPG READ plus local formatting)
    Hexdump {
        /// Memory address
        #[arg(value_name = "ADDR")]
        address: String,
        /// Byte count
        #[arg(value_name = "LEN")]
        length: String,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Fill memory with a byte value (native, host-side loop)
    Fill {
        /// Memory address
        #[arg(value_name = "ADDR")]
        address: String,
        /// Byte count
        #[arg(value_name = "LEN")]
        length: String,
        /// Byte fill value (0-255, like memset)
        #[arg(value_name = "VALUE")]
        value: String,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Zero device memory (native, fill with 0)
    Clear {
        /// Memory address
        #[arg(value_name = "ADDR")]
        address: String,
        /// Byte count
        #[arg(value_name = "LEN")]
        length: String,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Host-side RAM test: save, pattern-check, restore, re-verify (native).
    /// Destructive by nature; avoid bootloader-reserved RAM.
    Memtest {
        /// Memory address
        #[arg(value_name = "ADDR")]
        address: String,
        /// Byte count
        #[arg(value_name = "SIZE")]
        size: String,
        /// Pattern rounds (default 1)
        #[arg(long, default_value_t = 1)]
        rounds: u32,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Run a bootloader shell command (native UPG RUN_SHELL_STR, max 127 B)
    #[command(visible_alias = "sh")]
    Shcmd {
        /// Shell words, joined with spaces (e.g. shcmd md 0x40000000 4)
        #[arg(value_name = "SHELL", required = true)]
        shell: Vec<String>,
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Print the device log buffer (native UPG GET_LOG_*)
    Log {
        #[command(flatten)]
        transport: TransportArgs,
    },
    /// Show per-day burn statistics (success/failure/cancelled)
    Stats {
        /// Clear all recorded statistics instead of showing them
        #[arg(long)]
        clear: bool,
    },
    /// List physical disks (read-only; for boot-card target confirmation).
    /// Writing boot cards is not implemented — use official AiBurn for that.
    SdList,
    /// Check for updates from GitHub Releases
    Update {
        /// Update channel: stable (default, semver `v*` releases) or nightly
        #[arg(long, default_value = "stable", value_parser = ["stable", "nightly"], ignore_case = true)]
        channel: String,
        /// Open the release page in the default browser when an update exists
        #[arg(long)]
        open: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ProgressMode {
    Auto,
    Plain,
    Off,
}

/// Transport selectors shared by every native device command (USB default,
/// UART opt-in with the same flags as `burn`/`info`).
#[derive(Args, Clone, Debug)]
struct TransportArgs {
    /// Select a USB physical port path (9-2) or a bus:address pair (1:45)
    #[arg(long, value_name = "SELECTOR", conflicts_with = "uart")]
    device: Option<String>,
    /// Query over UART: port name, or "auto" to probe every serial port
    #[arg(
        long,
        value_name = "PORT",
        num_args = 0..=1,
        default_missing_value = "auto"
    )]
    uart: Option<String>,
    /// Initial UART baudrate used to reach the bootloader
    #[arg(long, default_value_t = 115200, value_parser = clap::value_parser!(u32).range(1..), requires = "uart")]
    baud: u32,
    /// Negotiate a higher UART baudrate before the command
    #[arg(long, value_name = "BAUD", value_parser = clap::value_parser!(u32).range(1..), requires = "uart")]
    speed: Option<u32>,
    /// Do not try to trigger UART upgrade mode when the protocol does not answer
    #[arg(long, requires = "uart")]
    no_enter_upg: bool,
}

impl TransportArgs {
    fn target(&self) -> Result<DeviceTarget, String> {
        if let Some(port) = &self.uart {
            if port.trim().is_empty() || self.speed.is_some_and(|speed| speed < self.baud) {
                return Err(
                    "Specify a nonempty UART port and a speed at least as high as --baud".into(),
                );
            }
            Ok(DeviceTarget::Uart(UartSpec::new(
                port.trim(),
                self.baud,
                self.speed,
                !self.no_enter_upg,
            )))
        } else {
            if self
                .device
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(
                    "USB selector is empty. Use a port path or bus:address from scan.".into(),
                );
            }
            let selector = usb::device::DeviceSelector::parse(self.device.as_deref())?;
            if let usb::device::DeviceSelector::PortPath(path) = &selector {
                if path.split('-').any(|part| part.parse::<u8>().is_err()) {
                    return Err(
                        "Invalid USB port path. Use a port path or bus:address from scan.".into(),
                    );
                }
            }
            Ok(DeviceTarget::Usb(selector))
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let verbose = cli.verbose || std::env::var_os("ARTINCHIP_FLASH_VERBOSE").is_some();
    verbosity::set_verbose(verbose);
    if cli.json
        && !matches!(
            cli.command,
            Commands::Scan { .. }
                | Commands::UsbList
                | Commands::SerialList
                | Commands::Info { .. }
                | Commands::EnvCheck { .. }
                | Commands::Burn { .. }
                | Commands::Stats { .. }
                | Commands::SdList
                | Commands::Update { .. }
        )
    {
        eprintln!("Error: --json is unavailable for this command");
        std::process::exit(2);
    }

    match cli.command {
        Commands::Scan { require_device } => cmd_scan(cli.json, require_device),
        Commands::UsbList => cmd_usb_list(cli.json),
        Commands::SerialList => cmd_serial_list(cli.json),
        Commands::Info { image, transport } => cmd_info(image, transport, cli.json),
        Commands::Burn {
            image,
            no_reset,
            force_upgrade,
            partition,
            plan,
            no_updater,
            erase_all,
            erase_media,
            upgcmd_path,
            log_file,
            transport,
            progress,
        } => cmd_burn(BurnFlags {
            image,
            no_reset,
            force_upgrade,
            partition,
            plan,
            no_updater,
            erase_all,
            erase_media,
            upgcmd_path,
            log_file,
            transport,
            progress,
            verbose,
            json: cli.json,
        }),
        Commands::UartMonitor {
            port,
            baud,
            enter_upg,
        } => cmd_uart_monitor(port, baud, enter_upg),
        Commands::EnvCheck { image } => cmd_env_check(image, cli.json),
        Commands::InstallUsbAccess => cmd_install_usb_access(),
        Commands::Write {
            address,
            input,
            skip,
            length,
            transport,
        } => cmd_write(address, input, skip, length, transport),
        Commands::Read {
            address,
            length,
            output,
            transport,
        } => cmd_read(address, length, output, transport),
        Commands::Writel {
            address,
            value,
            transport,
        } => cmd_writel(address, value, transport),
        Commands::Readl { address, transport } => cmd_readl(address, transport),
        Commands::Exec { address, transport } => cmd_exec(address, transport),
        Commands::Hexdump {
            address,
            length,
            transport,
        } => cmd_hexdump(address, length, transport),
        Commands::Fill {
            address,
            length,
            value,
            transport,
        } => cmd_fill(address, length, value, transport),
        Commands::Clear {
            address,
            length,
            transport,
        } => cmd_clear(address, length, transport),
        Commands::Memtest {
            address,
            size,
            rounds,
            transport,
        } => cmd_memtest(address, size, rounds, transport),
        Commands::Shcmd { shell, transport } => cmd_shcmd(shell, transport),
        Commands::Log { transport } => cmd_log(transport),
        Commands::Stats { clear } => cmd_stats(clear, cli.json),
        Commands::SdList => cmd_sd_list(cli.json),
        Commands::Update { channel, open } => cmd_update(&channel, open, cli.json),
    }
}

/// Default official `upgcmd` path used by `--erase-all` when the caller did
/// not pass `--upgcmd-path`. Mirrors the GUI compat default: Windows tries
/// `C:\ArtInChip\AiBurn\upgcmd.exe`, other platforms have no default and the
/// caller must pass the flag explicitly.
fn default_upgcmd_path() -> PathBuf {
    #[cfg(windows)]
    {
        artinchip_flash_app::app_config::compat_tool_path(std::path::Path::new(
            r"C:\ArtInChip\AiBurn",
        ))
    }
    #[cfg(not(windows))]
    {
        PathBuf::new()
    }
}

fn cmd_uart_monitor(port: String, baud: u32, enter_upg: bool) {
    if let Err(e) = artinchip_flash_app::uart::run_cli_monitor(&port, baud, enter_upg) {
        eprintln!("{}", e);
        std::process::exit(3);
    }
}

fn cmd_serial_list(json: bool) {
    match artinchip_flash_app::uart::UartDevice::list_ports() {
        Ok(ports) => {
            if json {
                let items: Vec<serde_json::Value> = ports
                    .iter()
                    .map(|port| {
                        serde_json::json!({
                            "port": port.port_name,
                            "type": port.port_type,
                            "vid": port.vid,
                            "pid": port.pid,
                            "product": port.product,
                        })
                    })
                    .collect();
                println!("{}", serde_json::json!({ "ports": items }));
                return;
            }
            if ports.is_empty() {
                println!("No serial ports found.");
                return;
            }
            println!("Serial ports:");
            for port in ports {
                let usb = match (port.vid, port.pid) {
                    (Some(vid), Some(pid)) => format!(" usb={:04x}:{:04x}", vid, pid),
                    _ => String::new(),
                };
                let product = port
                    .product
                    .as_deref()
                    .map(|product| format!(" {}", product))
                    .unwrap_or_default();
                println!(
                    "  {:<32} type={:<9}{}{}",
                    port.port_name, port.port_type, usb, product
                );
            }
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_usb_list(json: bool) {
    match usb::device::AicDevice::list_usb_devices() {
        Ok(devices) => {
            if json {
                let items: Vec<serde_json::Value> = devices
                    .iter()
                    .map(|device| {
                        serde_json::json!({
                            "bus": device.bus_number,
                            "address": device.address,
                            "vid": format!("{:04x}", device.vendor_id),
                            "pid": format!("{:04x}", device.product_id),
                            "class": device.class_code,
                            "speed": device.speed,
                            "path": device.port_path,
                        })
                    })
                    .collect();
                println!("{}", serde_json::json!({ "devices": items }));
                return;
            }
            if devices.is_empty() {
                println!("No USB devices found.");
                return;
            }
            println!("Visible USB devices:");
            for device in devices {
                let marker = if device.vendor_id == 0x33c3 && device.product_id == 0x6677 {
                    "  <-- ArtInChip upgrade"
                } else {
                    ""
                };
                println!(
                    "  bus={:<3} address={:<3} {:04x}:{:04x} class={:02x}/{:02x}/{:02x} speed={:<10} path={}{}",
                    device.bus_number,
                    device.address,
                    device.vendor_id,
                    device.product_id,
                    device.class_code,
                    device.subclass_code,
                    device.protocol_code,
                    device.speed,
                    device.port_path,
                    marker
                );
            }
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_scan(json: bool, require_device: bool) {
    match usb::device::AicDevice::scan_devices() {
        Ok(devices) if devices.is_empty() => {
            if json {
                println!("{}", serde_json::json!({ "devices": [] }));
            } else {
                println!("No ArtInChip device found (VID=0x33C3, PID=0x6677)");
            }
            if require_device {
                std::process::exit(3);
            }
        }
        Ok(devices) => {
            if json {
                let items: Vec<serde_json::Value> = devices
                    .iter()
                    .map(|device| {
                        serde_json::json!({
                            "bus": device.bus_number,
                            "address": device.address,
                            "path": device.port_path,
                            "vid": format!("{:04x}", device.vendor_id),
                            "pid": format!("{:04x}", device.product_id),
                            "speed": device.speed,
                            "ready": device.ready,
                            "enumerated": device.ready,
                            "ioreg_ready": device.ioreg_ready,
                            "protocol_ready": device.protocol_ready,
                            "status": device.status,
                        })
                    })
                    .collect();
                println!("{}", serde_json::json!({ "devices": items }));
                return;
            }
            println!("Detected {} ArtInChip device(s):", devices.len());
            for device in &devices {
                println!(
                    "  bus={} address={} path={} vid=0x{:04x} pid=0x{:04x} speed={} enumerated={} ioreg={} protocol={}",
                    device.bus_number,
                    device.address,
                    device.port_path,
                    device.vendor_id,
                    device.product_id,
                    device.speed,
                    if device.ready { "yes" } else { "no" },
                    match device.ioreg_ready {
                        Some(true) => "ready",
                        Some(false) => "not-ready",
                        None => "unknown",
                    },
                    match device.protocol_ready {
                        Some(true) => "ready",
                        Some(false) => "failed",
                        None => "unchecked",
                    }
                );
                if let Some(status) = &device.status {
                    println!("    {}", status);
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to scan USB devices: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_info(image: Option<PathBuf>, transport: TransportArgs, json: bool) {
    if let Some(path) = image {
        let bytes = fs::read(&path).unwrap_or_else(|error| {
            cli_error(2, format!("Error reading '{}': {error}", path.display()))
        });
        if json {
            let image =
                FirmwareImage::from_bytes(bytes).unwrap_or_else(|error| cli_error(2, error));
            let summary = image.summary();
            let components: Vec<_> = summary.metas.iter().map(|meta| serde_json::json!({
                "name":meta.name, "partition":meta.partition, "partition_media":meta.partition_media,
                "offset":meta.offset, "size":meta.size, "crc":meta.crc, "ram":meta.ram, "attr":meta.attr,
            })).collect();
            println!(
                "{}",
                serde_json::json!({
                    "kind":"image", "platform":summary.platform, "product":summary.product,
                    "version":summary.version, "total_size":summary.total_size, "media_type":summary.media_type,
                    "media_dev_id":summary.media_dev_id, "components":components,
                })
            );
        } else {
            let text = image::parser::format_image_info(&bytes)
                .unwrap_or_else(|error| cli_error(2, error));
            println!("{text}");
        }
        return;
    }
    let mut device = open_target(&transport);
    let lines = device
        .device_info_lines()
        .unwrap_or_else(|error| cli_error(4, error));
    let media = device.get_storage_media().ok();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "kind":"device", "transport":device.transport_name(), "lines":lines, "storage_media":media,
            })
        );
    } else {
        println!("=== Device Info ({}) ===", device.transport_name());
        for line in lines {
            println!("  {line}");
        }
        if let Some(media) = media {
            println!("  Storage media: {media}");
        }
    }
}

/// CLI `burn` flags bundled so `cmd_burn` stays under the argument limit.
struct BurnFlags {
    image: PathBuf,
    no_reset: bool,
    force_upgrade: bool,
    partition: Vec<String>,
    plan: bool,
    no_updater: bool,
    erase_all: bool,
    erase_media: Option<String>,
    upgcmd_path: Option<PathBuf>,
    log_file: Option<PathBuf>,
    transport: TransportArgs,
    progress: ProgressMode,
    verbose: bool,
    json: bool,
}

fn cmd_burn(flags: BurnFlags) {
    let BurnFlags {
        image: image_path,
        no_reset,
        force_upgrade,
        partition,
        plan,
        no_updater,
        erase_all,
        erase_media,
        upgcmd_path,
        log_file,
        transport,
        progress,
        verbose,
        json,
    } = flags;
    let mut output = BurnOutput::new(
        json && !plan,
        progress,
        verbose,
        log_file.as_deref().filter(|_| !plan),
    );
    let reject = |output: &mut BurnOutput, error: &dyn std::fmt::Display| -> ! {
        output.finish(&BurnReport::default(), 2, Some(&error.to_string()));
        std::process::exit(2)
    };
    let target = transport
        .target()
        .unwrap_or_else(|error| reject(&mut output, &error));
    if erase_media
        .as_deref()
        .is_some_and(|value| value.trim().parse::<u32>().is_err())
    {
        reject(&mut output, &"--erase-media requires a decimal media ID");
    }
    let image =
        FirmwareImage::open(&image_path).unwrap_or_else(|error| reject(&mut output, &error));
    let summary = image.summary();
    let image_description = format!(
        "Image: {} {} v{} ({} components, {} bytes total)",
        summary.platform,
        summary.product,
        summary.version,
        summary.metas.len(),
        summary.total_size
    );
    if json {
        eprintln!("{image_description}");
    } else {
        println!("{image_description}");
    }

    if !partition.is_empty() && (erase_all || force_upgrade) {
        reject(
            &mut output,
            &"--partition cannot be combined with --erase-all or --force-upgrade",
        );
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let mut options = BurnOptions {
        partition_upgrade: !partition.is_empty(),
        no_updater,
        force_upgrade,
        reset_after_burn: !no_reset && !force_upgrade,
        cancel: Some(cancel.clone()),
        ..image.default_options()
    };
    if !partition.is_empty() {
        options.selected_parts = partition;
    }
    let flash = flash_job::prepare(&image, options, erase_all)
        .unwrap_or_else(|error| reject(&mut output, &error));
    let burn_plan = flash.plan();
    if plan {
        if json {
            println!("{}", burn_plan.to_json());
        } else {
            for line in artinchip_flash_app::device::format_plan_lines(burn_plan) {
                println!("{}", line);
            }
        }
        return;
    }
    if erase_all
        && (matches!(
            target,
            DeviceTarget::Usb(usb::device::DeviceSelector::Unique)
        ) || transport
            .uart
            .as_deref()
            .is_some_and(|port| port.trim().eq_ignore_ascii_case("auto")))
    {
        reject(
            &mut output,
            &"--erase-all requires an explicit --device or UART port",
        );
    }

    if force_upgrade {
        eprintln!(
            "Note: --force-upgrade is experimental (BURN_IMG_FORCE, no post-burn reset); \
             the device must enable force upgrade (official manual §2.1.4)."
        );
    }

    let erase = erase_all.then(|| EraseRequest {
        tool: upgcmd_path.unwrap_or_else(default_upgcmd_path),
        media: erase_media,
        image: Some(image_path.clone()),
        uart_baudrate: transport.uart.as_ref().and(transport.speed),
    });

    if let Err(error) = ctrlc::set_handler(move || cancel.store(true, Ordering::SeqCst)) {
        eprintln!("Warning: could not register cancellation handler: {error}");
    }
    eprintln!("Press Ctrl+C to cancel the burn (aborts at the next chunk).");
    let job = FlashJob {
        flash: &flash,
        target,
        erase,
        report_device_info: true,
    };
    let mut report = BurnReport::default();
    let result = job.run_reported(&mut report, |event| output.event(event));
    let (code, outcome) = match &result {
        Ok(()) => (0, BurnOutcome::Success),
        Err(JobError::Connect(_)) => (3, BurnOutcome::Failure),
        Err(JobError::PreErase(_)) => (1, BurnOutcome::Failure),
        Err(JobError::Upgrade(FlashError::Cancelled)) => (130, BurnOutcome::Cancelled),
        Err(JobError::Upgrade(
            FlashError::ImageIo { .. } | FlashError::InvalidImage(_) | FlashError::InvalidPlan(_),
        )) => (2, BurnOutcome::Failure),
        Err(JobError::Upgrade(_)) => (4, BurnOutcome::Failure),
    };
    if let Err(e) = burn_stats::record(&standalone::default_app_dir(), outcome) {
        eprintln!("Warning: could not record burn stats: {}", e);
    }
    let error = result.err().map(|error| error.to_string());
    output.finish(&report, code, error.as_deref());
    if code != 0 {
        std::process::exit(code);
    }
}

fn cli_error(code: i32, error: impl std::fmt::Display) -> ! {
    eprintln!("Error: {error}");
    std::process::exit(code)
}

fn open_target(transport: &TransportArgs) -> UpgDevice<Box<dyn UpgTransport>> {
    let target = transport
        .target()
        .unwrap_or_else(|error| cli_error(2, error));
    target.open().unwrap_or_else(|error| {
        cli_error(
            3,
            format!(
                "{error}\nCheck the target and permissions with scan, serial-list or env-check."
            ),
        )
    })
}

/// Run `f` against the selected native device.
fn with_native_device(
    transport: &TransportArgs,
    f: impl FnOnce(&mut UpgDevice<Box<dyn UpgTransport>>) -> Result<(), String>,
) {
    let mut dev = open_target(transport);
    eprintln!("Transport: {}", dev.transport_name());
    if let Err(error) = f(&mut dev) {
        cli_error(4, error);
    }
}

fn must_parse_u32(label: &str, text: &str) -> u32 {
    match services::parse_u32(text) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("Invalid {} ('{}'): {}", label, text, e);
            std::process::exit(2);
        }
    }
}

fn cmd_write(
    address: String,
    input: PathBuf,
    skip: Option<String>,
    length: Option<String>,
    transport: TransportArgs,
) {
    let addr = must_parse_u32("address", &address);
    let skip_bytes = skip
        .as_deref()
        .map(|s| must_parse_u32("skip", s))
        .unwrap_or(0) as usize;
    let file = match fs::read(&input) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Error reading '{}': {}", input.display(), e);
            std::process::exit(1);
        }
    };
    if skip_bytes > file.len() {
        eprintln!("Skip {} exceeds input size {}", skip_bytes, file.len());
        std::process::exit(2);
    }
    let mut data = &file[skip_bytes..];
    if let Some(len_text) = length.as_deref() {
        let max = must_parse_u32("length", len_text) as usize;
        data = &data[..data.len().min(max)];
    }
    if data.is_empty() {
        eprintln!("Nothing to write (input empty after skip/length)");
        std::process::exit(2);
    }
    let len = data.len();
    with_native_device(&transport, |dev| {
        dev.write_memory(addr, data)?;
        println!("Wrote {} bytes to {:#x}", len, addr);
        Ok(())
    });
}

fn cmd_read(address: String, length: String, output: PathBuf, transport: TransportArgs) {
    let addr = must_parse_u32("address", &address);
    let len = must_parse_u32("length", &length);
    if len == 0 {
        eprintln!("Length must be > 0");
        std::process::exit(2);
    }
    with_native_device(&transport, |dev| {
        let data = dev.read_memory(addr, len)?;
        match fs::write(&output, &data) {
            Ok(()) => println!(
                "Read {} bytes from {:#x} to {}",
                data.len(),
                addr,
                output.display()
            ),
            Err(e) => cli_error(1, format!("Error writing '{}': {}", output.display(), e)),
        }
        Ok(())
    });
}

fn cmd_writel(address: String, value: String, transport: TransportArgs) {
    let addr = must_parse_u32("address", &address);
    let val = must_parse_u32("value", &value);
    with_native_device(&transport, |dev| {
        dev.write_memory(addr, &val.to_le_bytes())?;
        println!("Wrote {:#x} to {:#x}", val, addr);
        Ok(())
    });
}

fn cmd_readl(address: String, transport: TransportArgs) {
    let addr = must_parse_u32("address", &address);
    with_native_device(&transport, |dev| {
        let data = dev.read_memory(addr, 4)?;
        let val = u32::from_le_bytes(data[..4].try_into().unwrap());
        println!("{:#x}: {:#x} ({})", addr, val, val);
        Ok(())
    });
}

fn cmd_exec(address: String, transport: TransportArgs) {
    let addr = must_parse_u32("address", &address);
    with_native_device(&transport, |dev| {
        dev.exec_address(addr)?;
        println!("Executed {:#x}", addr);
        Ok(())
    });
}

fn cmd_hexdump(address: String, length: String, transport: TransportArgs) {
    let addr = must_parse_u32("address", &address);
    let len = must_parse_u32("length", &length);
    if len == 0 {
        eprintln!("Length must be > 0");
        std::process::exit(2);
    }
    with_native_device(&transport, |dev| {
        let data = dev.read_memory(addr, len)?;
        print!("{}", services::format_hexdump(addr, &data));
        Ok(())
    });
}

fn cmd_fill(address: String, length: String, value: String, transport: TransportArgs) {
    let addr = must_parse_u32("address", &address);
    let len = must_parse_u32("length", &length);
    let val = must_parse_u32("value", &value);
    if val > 0xFF {
        eprintln!("Fill value must be a byte (0-255), got {:#x}", val);
        std::process::exit(2);
    }
    with_native_device(&transport, |dev| {
        dev.fill_memory(addr, len, val as u8)?;
        println!("Filled {} bytes at {:#x} with {:#x}", len, addr, val);
        Ok(())
    });
}

fn cmd_clear(address: String, length: String, transport: TransportArgs) {
    let addr = must_parse_u32("address", &address);
    let len = must_parse_u32("length", &length);
    with_native_device(&transport, |dev| {
        dev.fill_memory(addr, len, 0)?;
        println!("Cleared {} bytes at {:#x}", len, addr);
        Ok(())
    });
}

fn cmd_memtest(address: String, size: String, rounds: u32, transport: TransportArgs) {
    let addr = must_parse_u32("address", &address);
    let len = must_parse_u32("size", &size);
    eprintln!(
        "Warning: memtest writes patterns in place at {:#x} ({} bytes, {} rounds); avoid bootloader-reserved RAM.",
        addr, len, rounds
    );
    with_native_device(&transport, |dev| {
        dev.memtest_memory(addr, len, rounds)?;
        println!("memtest {:#x}+{:#x} rounds={}: PASS", addr, len, rounds);
        Ok(())
    });
}

fn cmd_shcmd(shell: Vec<String>, transport: TransportArgs) {
    let line = shell.join(" ");
    if line.trim().is_empty() {
        eprintln!("Shell command must not be empty");
        std::process::exit(2);
    }
    with_native_device(&transport, |dev| {
        dev.run_shell(&line)?;
        println!("OK");
        Ok(())
    });
}

fn cmd_log(transport: TransportArgs) {
    with_native_device(&transport, |dev| {
        let log = dev.get_device_log()?;
        if log.is_empty() {
            println!("(device log empty)");
        } else {
            print!("{}", log);
            if !log.ends_with('\n') {
                println!();
            }
        }
        Ok(())
    });
}

fn cmd_env_check(image: Option<PathBuf>, json: bool) {
    let (text, structured) = standalone::environment_reports(image.as_deref());
    if json {
        println!("{structured}");
    } else {
        println!("{text}");
    }
}

fn cmd_install_usb_access() {
    match standalone::install_driver() {
        Ok(()) => println!("USB access setup completed."),
        Err(e) => {
            eprintln!("USB access setup failed: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_stats(clear: bool, json: bool) {
    let app_dir = standalone::default_app_dir();
    if clear {
        match burn_stats::clear(&app_dir) {
            Ok(()) if json => println!("{}", serde_json::json!({"cleared":true})),
            Ok(()) => println!("Burn statistics cleared."),
            Err(e) => {
                eprintln!("Failed to clear burn statistics: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }
    let stats = burn_stats::load(&app_dir);
    if json {
        println!("{}", stats.to_json());
    } else {
        println!("{}", burn_stats::format_table(&stats));
        println!("File: {}", burn_stats::stats_path(&app_dir).display());
    }
}

fn cmd_sd_list(json: bool) {
    match sdcard::list_disks() {
        Ok(disks) => {
            if json {
                let items: Vec<serde_json::Value> = disks
                    .iter()
                    .map(|disk| {
                        serde_json::json!({
                            "id": disk.id,
                            "model": disk.model,
                            "size_bytes": disk.size_bytes,
                            "bus_type": disk.bus_type,
                            "removable": disk.removable,
                        })
                    })
                    .collect();
                println!("{}", serde_json::json!({ "disks": items }));
                return;
            }
            if disks.is_empty() {
                println!("No physical disks found.");
                return;
            }
            println!("Physical disks (read-only; writing boot cards is not implemented):");
            for disk in &disks {
                println!("{}", disk.summary());
            }
            println!(
                "Note: use official AiBurn to write a boot card; double-check the target id first."
            );
        }
        Err(e) => {
            eprintln!("Failed to list physical disks: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_update(channel: &str, open: bool, json: bool) {
    let channel = UpdateChannel::from_str(channel);
    match update::check(channel) {
        Ok(status) => {
            if json {
                println!("{}", status.to_json());
            } else {
                println!("{}", status.summary_line());
                println!("Channel: {}", status.channel.as_str());
                println!(
                    "Current: {} ({})",
                    status.current_version, status.current_commit
                );
                println!("Latest:  {}", status.latest_tag);
                println!("Page:    {}", status.html_url);
                if !status.notes_preview.is_empty() {
                    println!("--- release notes (preview) ---");
                    println!("{}", status.notes_preview);
                }
                if status.update_available {
                    if update::is_portable_install() {
                        println!("This looks like a portable checkout; download the matching archive from the page above.");
                    } else {
                        println!("Installer-managed location detected; re-run the matching installer (msi/setup/deb/pkg) instead of replacing binaries.");
                    }
                }
            }
            if open && status.update_available {
                if let Err(e) = update::open_url(&status.html_url) {
                    eprintln!("{}", e);
                }
            }
            if status.update_available {
                std::process::exit(10);
            }
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "error": e, "channel": channel.as_str() })
                );
            } else {
                eprintln!("{}", e);
            }
            std::process::exit(1);
        }
    }
}
