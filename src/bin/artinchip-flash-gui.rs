#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use artinchip_flash_app::app_config::{
    append_image_history, compat_tool_path, load_image_history, AppConfig,
};
use artinchip_flash_app::build_info;
use artinchip_flash_app::burn_stats::{self, BurnOutcome};
use artinchip_flash_app::i18n::{command_label, tr, Language, Msg};
use artinchip_flash_app::image::parser::{self, ImageSummary, MetaSummary};
use artinchip_flash_app::official::{self, OfficialArgs, OfficialCommand};
use artinchip_flash_app::sdcard;
use artinchip_flash_app::services::{self, UartSpec};
use artinchip_flash_app::standalone;
use artinchip_flash_app::uart::{SerialPortInfo, UartDevice, UartMonitor, UartOptions};
use artinchip_flash_app::update::{self, UpdateChannel};
use artinchip_flash_app::usb::device::{
    AicDevice, BurnEvent, BurnOptions, DeviceInfo, DeviceSelector,
};
use artinchip_flash_app::verbosity;
use eframe::egui;

#[path = "gui/burn.rs"]
mod burn;
#[path = "gui/diagnostics.rs"]
mod diagnostics;
#[path = "gui/native.rs"]
mod native;
#[path = "gui/storage_tools.rs"]
mod storage_tools;
#[path = "gui/theme.rs"]
mod theme;
use burn::{bilingual, usb_selector};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tab {
    Burn,
    Debug,
    Tools,
    Settings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransportKind {
    Usb,
    Uart,
}

impl TransportKind {
    fn from_config(value: &str) -> Self {
        if value.eq_ignore_ascii_case("uart") {
            TransportKind::Uart
        } else {
            TransportKind::Usb
        }
    }

    fn config_value(self) -> &'static str {
        match self {
            TransportKind::Usb => "usb",
            TransportKind::Uart => "uart",
        }
    }
}

enum WorkerEvent {
    Burn(BurnEvent),
    BurnCancelled,
    NativePreview(Box<native::Pending>),
    NativeResult(String),
    ImageLoaded(Box<ImageSummary>, bool),
    ImageLoadFailed(String),
    ImageValidated(Result<(), String>),
    Structured(serde_json::Value),
    UsbScan(Vec<DeviceInfo>),
    ToolOutput(String),
    Error(String),
    Done,
}

#[derive(Clone, Copy)]
enum BurnDetails {
    Device,
    Image,
    Partitions,
}

#[derive(Clone, Copy, Debug)]
enum NotReadyContext {
    Scan,
}

type DeviceScanResult = Result<Vec<DeviceInfo>, String>;

struct GuiApp {
    config: AppConfig,
    tab: Tab,
    transport: TransportKind,
    devices: Vec<DeviceInfo>,
    selected_device: Option<usize>,
    usb_port: Option<(u8, String)>,
    serial_ports: Vec<SerialPortInfo>,
    selected_serial_port: Option<usize>,
    image_summary: Option<ImageSummary>,
    image_input: String,
    image_error: Option<String>,
    image_validation: Option<Result<(), String>>,
    burn_details: Option<BurnDetails>,
    confirm_risky_burn: Option<burn::Target>,
    logs_open: bool,
    selected_parts: Vec<String>,
    image_history: Vec<(PathBuf, String)>,
    log_lines: Vec<String>,
    burn_progress: f32,
    component_progress: f32,
    active_component: String,
    busy: bool,
    cancel_flag: Option<Arc<AtomicBool>>,
    burn_started_at: Option<Instant>,
    overall_sent: usize,
    overall_total: usize,
    burn_rate_bps: f64,
    burn_failed: bool,
    burn_cancelled: bool,
    burn_error: Option<String>,
    /// Elapsed time frozen at burn completion/cancel, so the status line
    /// stops ticking. `None` before the first burn of this session.
    burn_finished_elapsed: Option<Duration>,
    worker_is_burn: bool,
    burn_form: burn::BurnForm,
    burn_log_file: Option<(PathBuf, Vec<String>)>,
    burn_json_events: Option<Vec<serde_json::Value>>,
    verbose_rx: Receiver<String>,
    native_form: native::NativeForm,
    storage_form: storage_tools::StorageForm,
    native_result: Option<String>,
    official_pending: Option<(PathBuf, Vec<String>)>,
    update_url: Option<String>,
    structured: Option<serde_json::Value>,
    last_scan: Instant,
    rx: Option<Receiver<WorkerEvent>>,
    scan_rx: Option<Receiver<DeviceScanResult>>,
    serial_scan_rx: Option<Receiver<Result<Vec<SerialPortInfo>, String>>>,
    device_scan_in_progress: bool,
    monitor: Option<UartMonitor>,
    monitor_input: String,
    monitor_enter_on_connect: bool,
    official_args: OfficialArgs,
    settings_path: PathBuf,
    log_started_at: Instant,
    update_rx: Option<Receiver<Result<update::UpdateStatus, String>>>,
    update_status: String,
    update_check_in_progress: bool,
}

impl GuiApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::with_config(&cc.egui_ctx, AppConfig::load_default());
        if let Some(path) = app.config.image_path.clone() {
            app.load_image_summary(path, false);
        }
        app.start_device_scan(cc.egui_ctx.clone());
        if app.config.auto_check_update
            && !update_recently_checked(app.config.last_update_check_unix)
        {
            app.start_update_check(cc.egui_ctx.clone());
        }
        app
    }

    fn with_config(ctx: &egui::Context, mut config: AppConfig) -> Self {
        install_cjk_font(ctx);
        theme::apply_preference(ctx, &config.gui_theme);
        config.erase_all = false;
        config.force_upgrade = false;
        let verbose_rx = verbosity::gui_output();
        verbosity::set_verbose(config.verbose);
        let settings_path = config.app_dir.join("config.ini");
        let image_history = load_image_history(&config.app_dir);
        let transport = TransportKind::from_config(&config.transport);
        Self {
            selected_parts: config.selected_parts.clone(),
            official_args: OfficialArgs {
                image: config.image_path.clone(),
                ..Default::default()
            },
            config,
            tab: Tab::Burn,
            transport,
            devices: Vec::new(),
            selected_device: None,
            usb_port: None,
            serial_ports: Vec::new(),
            selected_serial_port: None,
            image_summary: None,
            image_input: String::new(),
            image_error: None,
            image_validation: None,
            burn_details: None,
            confirm_risky_burn: None,
            logs_open: false,
            image_history,
            log_lines: Vec::new(),
            burn_progress: 0.0,
            component_progress: 0.0,
            active_component: String::new(),
            busy: false,
            cancel_flag: None,
            burn_started_at: None,
            overall_sent: 0,
            overall_total: 0,
            burn_rate_bps: 0.0,
            burn_failed: false,
            burn_cancelled: false,
            burn_error: None,
            burn_finished_elapsed: None,
            worker_is_burn: false,
            burn_form: burn::BurnForm::default(),
            burn_log_file: None,
            burn_json_events: None,
            verbose_rx,
            native_form: native::NativeForm::default(),
            storage_form: storage_tools::StorageForm::default(),
            native_result: None,
            official_pending: None,
            update_url: None,
            structured: None,
            last_scan: Instant::now(),
            rx: None,
            scan_rx: None,
            serial_scan_rx: None,
            device_scan_in_progress: false,
            monitor: None,
            monitor_input: String::new(),
            monitor_enter_on_connect: false,
            settings_path,
            log_started_at: Instant::now(),
            update_rx: None,
            update_status: String::new(),
            update_check_in_progress: false,
        }
    }

    fn lang(&self) -> Language {
        Language::from_code(&self.config.language)
    }

    fn t(&self, msg: Msg) -> &'static str {
        tr(self.lang(), msg)
    }

    fn start_device_scan(&mut self, ctx: egui::Context) {
        if self.device_scan_in_progress || self.busy {
            return;
        }
        if self.transport == TransportKind::Uart {
            self.start_serial_scan(ctx);
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        self.device_scan_in_progress = true;
        thread::spawn(move || {
            let result = AicDevice::scan_selectable_devices();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn start_serial_scan(&mut self, ctx: egui::Context) {
        if self.device_scan_in_progress {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.serial_scan_rx = Some(rx);
        self.device_scan_in_progress = true;
        thread::spawn(move || {
            let result = UartDevice::list_ports();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn start_update_check(&mut self, ctx: egui::Context) {
        if self.update_check_in_progress {
            return;
        }
        let channel = UpdateChannel::from_str(&self.config.update_channel);
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        self.update_check_in_progress = true;
        self.update_status = self.t(Msg::CheckingUpdate).to_string();
        thread::spawn(move || {
            let result = update::check(channel);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn apply_update_result(&mut self, result: Result<update::UpdateStatus, String>) {
        self.update_check_in_progress = false;
        match result {
            Ok(status) => {
                self.structured = serde_json::from_str(&status.to_json()).ok();
                self.update_url = Some(status.html_url.clone());
                self.config.last_update_check_unix = current_unix_secs();
                let _ = self.config.save_to(&self.settings_path);
                if status.update_available {
                    self.update_status = format!(
                        "{}: {} -> {}",
                        self.t(Msg::UpdateAvailable),
                        status.current_version,
                        status.latest_tag
                    );
                    self.log(format!(
                        "{}: {} ({}). {}: {}",
                        self.t(Msg::UpdateAvailable),
                        status.current_version,
                        status.latest_tag,
                        self.t(Msg::OpenReleasePage),
                        status.html_url
                    ));
                    if !status.notes_preview.is_empty() {
                        for line in status.notes_preview.lines().take(5) {
                            self.log(format!("  {}", line));
                        }
                    }
                } else if status.state == update::UpdateState::Current {
                    self.update_status =
                        format!("{} ({})", self.t(Msg::UpToDate), status.current_version);
                    self.log(format!(
                        "{}: {}",
                        self.t(Msg::UpToDate),
                        status.current_version
                    ));
                } else {
                    self.update_status = status.summary_line();
                    self.log(self.update_status.clone());
                }
            }
            Err(e) => {
                self.update_status = e.clone();
                self.log(format!("{}: {}", self.t(Msg::CheckUpdate), e));
            }
        }
    }

    fn apply_serial_scan(&mut self, result: Result<Vec<SerialPortInfo>, String>) {
        match result {
            Ok(ports) => {
                self.serial_ports = ports;
                if self.serial_ports.is_empty() {
                    self.selected_serial_port = None;
                    self.log(format!(
                        "{}: {}",
                        self.t(Msg::Scan),
                        self.t(Msg::NoSerialPorts)
                    ));
                } else {
                    let configured = self.config.serial_port.clone();
                    self.selected_serial_port = self
                        .serial_ports
                        .iter()
                        .position(|port| port.port_name == configured);
                    let summary = match self.lang() {
                        Language::ZhCn => {
                            format!("扫描完成：发现 {} 个串口", self.serial_ports.len())
                        }
                        Language::En => {
                            format!("Scan complete: {} serial port(s)", self.serial_ports.len())
                        }
                    };
                    self.log(summary);
                }
            }
            Err(e) => self.log(format!("{}: {}", self.t(Msg::SerialScanFailed), e)),
        }
    }

    fn apply_device_scan(&mut self, result: Result<Vec<DeviceInfo>, String>) {
        match result {
            Ok(devices) => {
                if self.devices == devices {
                    return;
                }
                self.selected_device = select_scanned_device(&devices, self.usb_port.as_ref());
                self.devices = devices;
                if self.devices.is_empty() {
                    self.log(format!(
                        "{}: {}",
                        self.t(Msg::Scan),
                        self.t(Msg::NoDeviceAvailable)
                    ));
                } else {
                    let enumerated_count =
                        self.devices.iter().filter(|device| device.ready).count();
                    let protocol_ready_count = self
                        .devices
                        .iter()
                        .filter(|device| device.protocol_ready == Some(true))
                        .count();
                    let protocol_failed_count = self
                        .devices
                        .iter()
                        .filter(|device| device.protocol_ready == Some(false))
                        .count();
                    let protocol_unchecked_count = self
                        .devices
                        .iter()
                        .filter(|device| device.protocol_ready.is_none())
                        .count();
                    self.log(scan_summary(
                        self.lang(),
                        self.devices.len(),
                        enumerated_count,
                        protocol_ready_count,
                        protocol_failed_count,
                        protocol_unchecked_count,
                    ));
                    let diagnostics = self
                        .devices
                        .iter()
                        .filter(|device| device.status.is_some())
                        .cloned()
                        .collect::<Vec<_>>();
                    for device in &diagnostics {
                        self.log_not_ready_device(device, NotReadyContext::Scan);
                    }
                }
            }
            Err(e) => self.log(format!("{}: {}", self.t(Msg::ScanFailed), e)),
        }
    }

    fn load_image(&mut self, path: PathBuf) {
        self.load_image_summary(path, true);
    }

    fn load_image_summary(&mut self, path: PathBuf, update_history: bool) {
        if self.busy {
            return;
        }
        self.image_input = path.display().to_string();
        self.image_error = None;
        self.image_validation = None;
        let error_label = self.t(Msg::ImageParseFailed);
        self.start_task(move || {
            Ok(match services::load_image_summary(&path) {
                Ok(mut summary) => {
                    summary.path = Some(path);
                    WorkerEvent::ImageLoaded(Box::new(summary), update_history)
                }
                Err(error) => WorkerEvent::ImageLoadFailed(format!("{error_label}: {error}")),
            })
        });
    }

    fn apply_image_summary(&mut self, summary: ImageSummary, update_history: bool) {
        let Some(path) = summary.path.clone() else {
            return;
        };
        self.config.image_path = Some(path.clone());
        self.official_args.image = Some(path.clone());
        self.image_summary = Some(summary);
        self.sync_selected_parts_from_image();
        self.log(format!(
            "{} {}",
            self.t(Msg::ParseImageHeaderFrom),
            path.display()
        ));
        if update_history {
            let _ = append_image_history(&self.config.app_dir, &path);
            self.image_history = load_image_history(&self.config.app_dir);
        }
    }

    fn sync_selected_parts_from_image(&mut self) {
        let Some(summary) = &self.image_summary else {
            return;
        };
        if self.config.selected_parts == BurnOptions::default().selected_parts {
            self.selected_parts = target_metas(summary)
                .filter(|meta| {
                    artinchip_flash_app::device::default_burn_target(
                        &meta.name,
                        &meta.partition_media,
                    )
                })
                .map(part_key)
                .collect();
        } else {
            self.selected_parts = self.config.selected_parts.clone();
            self.selected_parts
                .retain(|part| target_metas(summary).any(|meta| partition_matches(part, meta)));
        }
    }

    fn start_monitor(&mut self) {
        if self.monitor.is_some() {
            return;
        }
        let selected = self.selected_serial_port_name();
        let resolved = if selected.is_empty() || selected.eq_ignore_ascii_case("auto") {
            self.serial_ports
                .iter()
                .find(|port| port.port_type == "usb")
                .or_else(|| self.serial_ports.first())
                .map(|port| port.port_name.clone())
        } else {
            Some(selected)
        };
        let Some(path) = resolved else {
            self.log(self.t(Msg::NoSerialPorts));
            return;
        };
        match UartMonitor::start(&path, self.config.serial_baud.max(1200)) {
            Ok(monitor) => {
                self.monitor = Some(monitor);
                self.log(match self.lang() {
                    Language::ZhCn => format!("已连接串口监视：{}", path),
                    Language::En => format!("UART monitor connected: {}", path),
                });
            }
            Err(e) => self.log_error(e),
        }
    }

    fn stop_monitor(&mut self) {
        if let Some(mut monitor) = self.monitor.take() {
            monitor.close();
            self.log(match self.lang() {
                Language::ZhCn => "串口监视已断开".to_string(),
                Language::En => "UART monitor disconnected".to_string(),
            });
        }
    }

    fn monitor_trigger_upgrade(&mut self) {
        if let Some(monitor) = &self.monitor {
            monitor.trigger_upgrade();
            self.log(match self.lang() {
                Language::ZhCn => {
                    "已发送 `aicupg gotobl` / `aicupg uart 0`；若设备未自动重启，请手动复位"
                        .to_string()
                }
                Language::En => {
                    "Sent `aicupg gotobl` / `aicupg uart 0`; reset the board if it does not reboot"
                        .to_string()
                }
            });
        }
    }

    fn monitor_send(&mut self, text: &str) {
        if let Some(monitor) = &self.monitor {
            monitor.send_line(text);
            self.log(format!("> {}", text));
        }
    }

    fn start_burn(&mut self, request: burn::BurnRequest) {
        if self.busy {
            return;
        }
        self.stop_monitor();
        self.burn_json_events = request.capture_json.then(Vec::new);
        self.burn_log_file = request.log_file.clone().map(|path| (path, Vec::new()));
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.busy = true;
        self.burn_progress = 0.0;
        self.component_progress = 0.0;
        self.active_component.clear();
        self.overall_sent = 0;
        self.overall_total = 0;
        self.burn_rate_bps = 0.0;
        self.burn_failed = false;
        self.burn_cancelled = false;
        self.burn_error = None;
        self.burn_finished_elapsed = None;
        self.worker_is_burn = true;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(cancel.clone());
        self.burn_started_at = Some(Instant::now());
        self.log(format!(
            "{} {} ...",
            self.t(Msg::BurnImageFile),
            request.path.display()
        ));

        thread::spawn(move || {
            if let Err(error) = request.execute(cancel, &tx) {
                let event = if matches!(
                    error,
                    artinchip_flash_app::flash_job::JobError::Upgrade(
                        artinchip_flash::FlashError::Cancelled
                    )
                ) {
                    WorkerEvent::BurnCancelled
                } else {
                    WorkerEvent::Error(error.to_string())
                };
                let _ = tx.send(event);
            }
            let _ = tx.send(WorkerEvent::Done);
        });
    }

    fn stop_burn(&mut self) {
        if let Some(flag) = &self.cancel_flag {
            flag.store(true, Ordering::SeqCst);
            self.log(match self.lang() {
                Language::ZhCn => "已请求停止烧录，等待当前数据块完成后中止…",
                Language::En => "Stop requested; aborting after the current chunk…",
            });
        }
    }

    fn start_task(&mut self, task: impl FnOnce() -> Result<WorkerEvent, String> + Send + 'static) {
        if self.busy {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.busy = true;
        self.worker_is_burn = false;
        thread::spawn(move || {
            let _ = tx.send(task().unwrap_or_else(WorkerEvent::Error));
            let _ = tx.send(WorkerEvent::Done);
        });
    }

    fn start_read_device_info(&mut self) {
        if self.busy {
            return;
        }
        if self.transport == TransportKind::Uart {
            self.stop_monitor();
        }
        let selected_device = self
            .selected_device
            .and_then(|idx| self.devices.get(idx).cloned());
        let use_uart = self.transport == TransportKind::Uart;
        let read_log = self.config.read_device_log;
        let lang = self.lang();
        let uart_port = self.selected_serial_port_name();
        let uart_options = UartOptions {
            baudrate: self.config.serial_baud.max(1200),
            max_baudrate: (self.config.serial_speed > self.config.serial_baud)
                .then_some(self.config.serial_speed),
            auto_enter: self.config.serial_auto_enter,
            ..Default::default()
        };
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.busy = true;
        self.worker_is_burn = false;
        thread::spawn(move || {
            let result = (|| -> Result<String, String> {
                if use_uart {
                    let mut dev = open_uart_backend(&uart_port, uart_options)?;
                    let mut text = dev.device_info_text()?;
                    if read_log {
                        text.push_str("\n\n");
                        text.push_str(tr(lang, Msg::DeviceLogDivider));
                        text.push('\n');
                        text.push_str(&dev.get_device_log()?);
                    }
                    return Ok(text);
                }
                let mut dev = if let Some(device) = selected_device {
                    AicDevice::open_selector(usb_selector(&device)?)?
                } else {
                    AicDevice::open_first()?
                };
                let mut text = dev.device_info_text()?;
                if read_log {
                    text.push_str("\n\n");
                    text.push_str(tr(lang, Msg::DeviceLogDivider));
                    text.push('\n');
                    text.push_str(&dev.get_device_log()?);
                }
                Ok(text)
            })();
            match result {
                Ok(text) => {
                    let _ = tx.send(WorkerEvent::ToolOutput(text));
                }
                Err(e) => {
                    let _ = tx.send(WorkerEvent::Error(e));
                }
            }
            let _ = tx.send(WorkerEvent::Done);
        });
    }

    fn start_official_command(&mut self) {
        if self.busy {
            return;
        }
        let upgcmd = self.config.upgcmd_path.clone();
        let args = match official::build_args(&self.official_args) {
            Ok(args) => args,
            Err(e) => {
                self.log(e);
                return;
            }
        };
        self.official_pending = Some((upgcmd, args));
    }

    fn start_sd_list(&mut self) {
        if self.busy {
            return;
        }
        self.start_task(|| {
            Ok(WorkerEvent::Structured(diagnostics::disks_json(
                &sdcard::list_disks()?,
            )))
        });
    }

    fn record_burn_stats(&mut self) {
        let outcome = if !self.burn_failed {
            BurnOutcome::Success
        } else if let Some(err) = &self.burn_error {
            burn_stats::classify_error(err)
        } else {
            BurnOutcome::Failure
        };
        if let Err(e) = burn_stats::record(&self.config.app_dir, outcome) {
            self.log(format!("Warning: could not record burn stats: {}", e));
        }
    }

    fn poll_worker(&mut self, ctx: &egui::Context) {
        let verbose: Vec<_> = self.verbose_rx.try_iter().take(2048).collect();
        for message in verbose {
            self.log(message);
        }
        if verbosity::take_dropped_output() {
            self.log("Verbose log queue filled; some transport messages were omitted");
        }
        self.poll_device_scan(ctx);
        self.poll_update_check(ctx);

        let monitor_lines: Vec<String> = self
            .monitor
            .as_ref()
            .map(|monitor| monitor.poll())
            .unwrap_or_default();
        if !monitor_lines.is_empty() {
            for line in monitor_lines {
                self.log(line);
            }
            ctx.request_repaint();
        }

        let mut done = false;
        if let Some(rx) = self.rx.take() {
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if done {
                            break;
                        }
                        self.burn_failed = self.worker_is_burn;
                        let error = "Background worker stopped unexpectedly".to_owned();
                        self.burn_error = Some(error.clone());
                        self.log_error(error);
                        WorkerEvent::Done
                    }
                };
                match event {
                    WorkerEvent::Burn(event) => {
                        if let Some(events) = &mut self.burn_json_events {
                            let elapsed = self
                                .burn_started_at
                                .map_or(Duration::ZERO, |start| start.elapsed());
                            events.push(event.to_json(elapsed));
                        }
                        self.apply_burn_event(event);
                    }
                    WorkerEvent::Structured(value) => self.structured = Some(value),
                    WorkerEvent::UsbScan(devices) => {
                        self.structured = Some(diagnostics::scan_json(&devices));
                        self.apply_device_scan(Ok(devices));
                    }
                    WorkerEvent::NativePreview(task) => self.native_form.pending = Some(task),
                    WorkerEvent::NativeResult(result) => self.native_result = Some(result),
                    WorkerEvent::ImageLoaded(summary, history) => {
                        self.apply_image_summary(*summary, history)
                    }
                    WorkerEvent::ImageLoadFailed(error) => {
                        self.image_error = Some(error.clone());
                        self.log_error(error);
                    }
                    WorkerEvent::ImageValidated(result) => {
                        if let Err(error) = &result {
                            self.log_error(error.clone());
                        }
                        self.image_validation = Some(result);
                    }
                    WorkerEvent::BurnCancelled => {
                        let error = artinchip_flash::FlashError::Cancelled.to_string();
                        self.burn_failed = true;
                        self.burn_cancelled = true;
                        self.burn_error = Some(error.clone());
                        self.log_error(error);
                    }
                    WorkerEvent::ToolOutput(text) => {
                        if !self.worker_is_burn {
                            self.logs_open = true;
                        }
                        for line in text.lines() {
                            self.log(line);
                        }
                    }
                    WorkerEvent::Error(e) => {
                        if self.worker_is_burn {
                            self.burn_failed = true;
                            self.burn_error = Some(e.clone());
                        } else {
                            self.logs_open = true;
                        }
                        self.log_error(e);
                    }
                    WorkerEvent::Done => {
                        let was_burn = self.worker_is_burn;
                        self.busy = false;
                        self.cancel_flag = None;
                        self.worker_is_burn = false;
                        done = true;
                        if was_burn {
                            // Freeze the elapsed counter at completion/cancel
                            // so the status line stops ticking on later
                            // repaints; then record the outcome.
                            self.burn_finished_elapsed = self.burn_started_at.map(|s| s.elapsed());
                            self.burn_started_at = None;
                            if let Some(events) = self.burn_json_events.take() {
                                self.structured = Some(
                                    serde_json::json!({ "events": events, "success": !self.burn_failed, "error": self.burn_error }),
                                );
                            }
                            self.record_burn_stats();
                            if let Some((path, lines)) = self.burn_log_file.take() {
                                match services::write_log_file(&path, &lines) {
                                    Ok(()) => {
                                        self.log(format!("Burn log saved: {}", path.display()))
                                    }
                                    Err(error) => self.log_error(error),
                                }
                            }
                        }
                    }
                }
                ctx.request_repaint();
            }
            if !done {
                self.rx = Some(rx);
            }
        }
    }

    fn poll_device_scan(&mut self, ctx: &egui::Context) {
        let mut done = false;
        if let Some(rx) = self.scan_rx.take() {
            while let Ok(result) = rx.try_recv() {
                self.apply_device_scan(result);
                self.device_scan_in_progress = false;
                done = true;
                ctx.request_repaint();
            }
            if !done {
                self.scan_rx = Some(rx);
            }
        }
        let mut serial_done = false;
        if let Some(rx) = self.serial_scan_rx.take() {
            while let Ok(result) = rx.try_recv() {
                self.apply_serial_scan(result);
                self.device_scan_in_progress = false;
                serial_done = true;
                ctx.request_repaint();
            }
            if !serial_done {
                self.serial_scan_rx = Some(rx);
            }
        }
    }

    fn poll_update_check(&mut self, ctx: &egui::Context) {
        let mut done = false;
        if let Some(rx) = self.update_rx.take() {
            while let Ok(result) = rx.try_recv() {
                self.apply_update_result(result);
                done = true;
                ctx.request_repaint();
            }
            if !done {
                self.update_rx = Some(rx);
            }
        }
    }

    fn apply_burn_event(&mut self, event: BurnEvent) {
        match event {
            BurnEvent::Log(line) => self.log(line),
            BurnEvent::Stage(stage) => self.log(stage),
            BurnEvent::ComponentStarted {
                name,
                partition,
                size,
            } => {
                self.active_component = name.clone();
                self.component_progress = 0.0;
                self.log(format!(
                    "{} {} {}={} {}={} ...",
                    self.t(Msg::Meta),
                    name,
                    self.t(Msg::PartitionField),
                    partition,
                    self.t(Msg::SizeField),
                    size
                ));
            }
            BurnEvent::ComponentProgress { name, sent, total } => {
                self.active_component = name;
                self.component_progress = progress(sent, total);
            }
            BurnEvent::OverallProgress { sent, total } => {
                self.burn_progress = progress(sent, total);
                self.overall_sent = sent;
                self.overall_total = total;
                self.refresh_burn_rate();
            }
            BurnEvent::ComponentFinished { name } => {
                self.log(format!("{}: {}", self.t(Msg::BurnComponentSuccess), name))
            }
            BurnEvent::Finished => {
                self.burn_progress = 1.0;
                self.refresh_burn_rate();
                self.log(self.t(Msg::BurnOnlineSuccess));
            }
        }
    }

    fn refresh_burn_rate(&mut self) {
        if let Some(started) = self.burn_started_at {
            let elapsed = started.elapsed().as_secs_f64();
            if elapsed > 0.0 && self.overall_sent > 0 {
                self.burn_rate_bps = self.overall_sent as f64 / elapsed;
            }
        }
    }

    fn burn_status_line(&self) -> String {
        // Live while burning; frozen at the completion value afterwards.
        let elapsed = self
            .burn_started_at
            .map(|started| started.elapsed())
            .or(self.burn_finished_elapsed)
            .unwrap_or_default();
        format!(
            "{}: {}  {}: {}  {}: {}/{}",
            self.t(Msg::Elapsed),
            format_elapsed(elapsed),
            self.t(Msg::Rate),
            format_rate(self.burn_rate_bps),
            self.t(Msg::Overall),
            self.overall_sent,
            self.overall_total
        )
    }

    fn log(&mut self, line: impl Into<String>) {
        let elapsed = self.log_started_at.elapsed();
        let minutes = elapsed.as_secs() / 60;
        let seconds = elapsed.as_secs() % 60;
        let line = format!("[{:02}:{:02}] {}", minutes, seconds, line.into());
        if let Some((_, burn_log)) = &mut self.burn_log_file {
            burn_log.push(line.clone());
        }
        self.log_lines.push(line);
        if self.log_lines.len() > 1500 {
            let overflow = self.log_lines.len() - 1500;
            self.log_lines.drain(0..overflow);
        }
    }

    fn log_error(&mut self, error: String) {
        self.log(format!("{}: {}", self.t(Msg::ErrorPrefix), error));
        if error.contains("macOS has not configured it for USB transfers")
            || error.contains("kUSBCurrentConfiguration")
            || error.contains("IOUSBHostInterface")
        {
            self.log(macos_recovery_hint(self.lang()));
        }
    }

    fn log_not_ready_device(&mut self, device: &DeviceInfo, context: NotReadyContext) {
        let status = device
            .status
            .as_deref()
            .map(|status| localize_device_status(self.lang(), status))
            .unwrap_or_else(|| "no additional diagnostic".to_string());
        self.log(format!(
            "{}: {} {} ({})",
            not_ready_context_label(self.lang(), context),
            format_device_ref(device),
            device_readiness_label(self.lang(), device),
            status
        ));
        self.log(macos_recovery_hint(self.lang()));
    }

    fn ui_top_bar(&mut self, ui: &mut egui::Ui) {
        let burn = self.t(Msg::TabBurn);
        let tools = self.t(Msg::TabTools);
        let settings = self.t(Msg::TabSettings);
        let debug = bilingual(self.lang(), "Debug", "调试");
        let previous = self.tab;
        ui.horizontal_wrapped(|ui| {
            selectable_tab(ui, &mut self.tab, Tab::Burn, burn);
            selectable_tab(ui, &mut self.tab, Tab::Debug, debug);
            selectable_tab(ui, &mut self.tab, Tab::Tools, tools);
            selectable_tab(ui, &mut self.tab, Tab::Settings, settings);
        });
        if self.tab != previous {
            self.burn_details = None;
        }
    }

    fn ui_burn(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        ui.set_max_width(ui.available_width().min(720.0));
        ui.label(egui::RichText::new(self.t(Msg::Device)).strong());
        self.ui_device_selection(ui);
        ui.add_space(10.0);
        ui.label(egui::RichText::new(text("Firmware", "固件")).strong());
        self.ui_image_section(ui);
        ui.add_space(10.0);
        ui.label(egui::RichText::new(self.t(Msg::Partitions)).strong());
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.image_matches_input() && !self.busy,
                    egui::Button::new(text("Configure…", "配置…")),
                )
                .clicked()
            {
                self.burn_details = Some(BurnDetails::Partitions);
            }
            let summary = self
                .image_summary
                .as_ref()
                .filter(|_| self.image_matches_input())
                .map(|image| {
                    target_metas(image)
                        .filter(|meta| partition_selected(&self.selected_parts, meta))
                        .map(part_key)
                        .collect::<Vec<_>>()
                        .join(" · ")
                });
            let label = match summary.as_deref() {
                Some("") => text("No target partitions selected", "未选择目标分区"),
                Some(summary) => summary,
                None => self.t(Msg::NoImageLoaded),
            };
            ui.add(egui::Label::new(label).truncate())
                .on_hover_text(label);
        });
        ui.add_space(10.0);
        egui::CollapsingHeader::new(text("Advanced options", "高级选项"))
            .id_salt("burn_control_section")
            .show(ui, |ui| {
                ui.add_enabled_ui(!self.busy, |ui| {
                    self.ui_burn_options(ui);
                    if self.transport == TransportKind::Uart {
                        self.ui_uart_options(ui);
                    }
                });
            });
        if self.transport == TransportKind::Uart || self.monitor.is_some() {
            egui::CollapsingHeader::new(text("UART monitor", "串口监视器"))
                .show(ui, |ui| self.ui_monitor(ui));
        }
    }

    fn ui_device_selection(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        ui.add_enabled_ui(!self.busy && !self.device_scan_in_progress, |ui| {
            ui.horizontal_wrapped(|ui| {
                self.ui_transport(ui);
                if self.transport == TransportKind::Usb {
                    egui::ComboBox::from_id_salt("device_select")
                        .width(126.0)
                        .truncate()
                        .selected_text(self.selected_device_label())
                        .show_ui(ui, |ui| {
                            for (idx, device) in self.devices.iter().enumerate() {
                                if ui
                                    .selectable_label(
                                        self.selected_device == Some(idx),
                                        usb_port_label(device),
                                    )
                                    .clicked()
                                {
                                    self.selected_device = Some(idx);
                                    self.usb_port = (!device.port_path.is_empty())
                                        .then(|| (device.bus_number, device.port_path.clone()));
                                }
                            }
                        });
                } else {
                    self.ui_serial_selector(ui);
                }
                if ui.button(self.t(Msg::Refresh)).clicked() {
                    self.start_device_scan(ui.ctx().clone());
                }
                if ui.button(text("Details…", "详情…")).clicked() {
                    self.burn_details = Some(BurnDetails::Device);
                }
            });
        });
        if self.transport == TransportKind::Usb {
            if let Some(device) = self
                .selected_device
                .and_then(|index| self.devices.get(index))
            {
                ui.weak(device_readiness_label(lang, device));
            }
        }
    }

    fn ui_image_section(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        ui.add_enabled_ui(!self.busy, |ui| {
            let hint = self.t(Msg::SelectImageFile);
            let browse_label = self.t(Msg::Browse);
            let (response, browse) =
                theme::path_field_with_hint(ui, "", &mut self.image_input, browse_label, hint);
            if response.changed() {
                self.image_error = None;
                self.image_validation = None;
            }
            if response.lost_focus()
                && ui.input(|input| input.key_pressed(egui::Key::Enter))
                && !self.image_input.trim().is_empty()
                && !self.image_matches_input()
            {
                self.load_image(PathBuf::from(self.image_input.trim()));
            }
            if browse.clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter(self.t(Msg::ArtInChipImageFilter), &["img"])
                    .pick_file()
                {
                    self.load_image(path);
                }
            }
        });
        if let Some(error) = &self.image_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        } else if let Some(image) = self
            .image_summary
            .as_ref()
            .filter(|_| self.image_matches_input())
        {
            let summary = [
                &image.platform,
                &image.product,
                &image.version,
                &image.media_type,
            ]
            .into_iter()
            .filter(|value| !value.is_empty())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" · ");
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Label::new(summary.as_str()).truncate())
                    .on_hover_text(summary);
                match &self.image_validation {
                    Some(Ok(())) => {
                        ui.label(text("Validation passed", "校验通过"));
                    }
                    Some(Err(error)) => {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            text("Validation failed", "校验失败"),
                        )
                        .on_hover_text(error);
                    }
                    None => {}
                }
            });
        } else if !self.busy && !self.image_input.trim().is_empty() {
            ui.weak(text("Press Enter to load this path", "按 Enter 加载此路径"));
        }
        if self.image_matches_input() || !self.image_history.is_empty() {
            ui.add_enabled_ui(!self.busy, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if self.image_matches_input() && ui.button(text("Details…", "详情…")).clicked()
                    {
                        self.burn_details = Some(BurnDetails::Image);
                    }
                    self.ui_image_history(ui);
                });
            });
        }
    }

    fn image_matches_input(&self) -> bool {
        self.image_error.is_none()
            && self
                .image_summary
                .as_ref()
                .and_then(|summary| summary.path.as_deref())
                .is_some_and(|path| {
                    let displayed = path.to_string_lossy();
                    displayed == self.image_input || displayed == self.image_input.trim()
                })
    }

    fn ui_image_history(&mut self, ui: &mut egui::Ui) {
        if !self.image_history.is_empty() {
            egui::ComboBox::from_id_salt("history")
                .selected_text(self.t(Msg::ImageHistory))
                .show_ui(ui, |ui| {
                    let mut selected = None;
                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .show(ui, |ui| {
                            for (path, ts) in &self.image_history {
                                let label = format!("{}  {}", ts, path.display());
                                if ui
                                    .add_sized([350.0, 0.0], egui::Button::new(&label).truncate())
                                    .on_hover_text(label)
                                    .clicked()
                                {
                                    selected = Some(path.clone());
                                }
                            }
                        });
                    if let Some(path) = selected {
                        self.load_image(path);
                    }
                });
        }
    }

    fn ui_image_details(&mut self, ui: &mut egui::Ui) {
        if !self.image_matches_input() {
            ui.label(self.t(Msg::NoImageLoaded));
            return;
        }
        let lang = self.lang();
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(self.config.image_path.is_some() && !self.busy, |ui| {
                if ui
                    .button(bilingual(lang, "Validate image", "校验镜像"))
                    .clicked()
                {
                    if let Some(path) = self.config.image_path.clone() {
                        self.image_validation = None;
                        self.start_task(move || {
                            let result = artinchip_flash::FirmwareImage::open(path)
                                .and_then(|image| image.validate())
                                .map_err(|error| error.to_string());
                            Ok(WorkerEvent::ImageValidated(result))
                        });
                    }
                }
                if ui.button(self.t(Msg::ExtractComponents)).clicked() {
                    if let Some(image) = self.config.image_path.clone() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            let success = self.t(Msg::ExtractedComponentsTo);
                            let failure = self.t(Msg::ExtractFailed);
                            self.start_task(move || {
                                let files = parser::extract_components(&image, &dir)
                                    .map_err(|error| format!("{failure}: {error}"))?;
                                Ok(WorkerEvent::ToolOutput(format!(
                                    "{success}: {} ({})",
                                    dir.display(),
                                    files.len()
                                )))
                            });
                        }
                    }
                }
            });
        });

        if let Some(summary) = &self.image_summary {
            egui::Grid::new("image_header")
                .num_columns(2)
                .striped(true)
                .show(ui, |ui| {
                    row(ui, self.t(Msg::Magic), &summary.magic);
                    row(ui, self.t(Msg::Platform), &summary.platform);
                    row(ui, self.t(Msg::Product), &summary.product);
                    row(ui, self.t(Msg::Version), &summary.version);
                    row(ui, self.t(Msg::MediaType), &summary.media_type);
                    row(ui, self.t(Msg::MediaId), &summary.media_id);
                    row(
                        ui,
                        self.t(Msg::MediaDev),
                        &format!("{:#x}", summary.media_dev_id),
                    );
                    row(
                        ui,
                        self.t(Msg::MetaOffset),
                        &format!("{:#x}", summary.meta_offset),
                    );
                    row(ui, self.t(Msg::MetaSize), &summary.meta_size.to_string());
                    row(
                        ui,
                        self.t(Msg::FileOffset),
                        &format!("{:#x}", summary.file_offset),
                    );
                    row(ui, self.t(Msg::FileSize), &summary.file_size.to_string());
                });
        }
    }

    fn ui_burn_actions(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        let start_label = if self.config.erase_all {
            text("Erase all and burn…", "全片擦除并烧录…")
        } else if self.config.force_upgrade {
            text("Force upgrade…", "强制升级…")
        } else {
            text("Start burn", "开始烧录")
        };
        let target = self.target(true).ok();
        let can_start = !self.busy
            && !self.device_scan_in_progress
            && self.image_matches_input()
            && !self.selected_parts.is_empty()
            && target.is_some();
        let cancelling = self
            .cancel_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed));
        let status = if self.worker_is_burn {
            if cancelling {
                text("Cancelling…", "正在取消…")
            } else {
                text("Burning…", "正在烧录…")
            }
        } else if self.busy {
            text("Working…", "正在处理…")
        } else if self.burn_cancelled {
            text("Burn cancelled", "烧录已取消")
        } else if self.burn_failed {
            text("Burn failed", "烧录失败")
        } else if self.burn_finished_elapsed.is_some() {
            text("Burn complete", "烧录完成")
        } else if self.device_scan_in_progress {
            text("Scanning…", "正在扫描…")
        } else if !self.image_matches_input() {
            text("Select firmware", "请选择固件")
        } else if self.selected_parts.is_empty() {
            text("Select partitions", "请选择分区")
        } else if target.is_none() {
            text("Select a target device", "请选择目标设备")
        } else {
            text("Ready to start", "可以开始")
        };
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(can_start, egui::Button::new(start_label))
                .clicked()
            {
                if let Some(target) = target {
                    if self.config.erase_all || self.config.force_upgrade {
                        self.confirm_risky_burn = Some(target);
                    } else {
                        self.request_burn(target);
                    }
                }
            }
            if self.worker_is_burn
                && ui
                    .add_enabled(
                        self.cancel_flag.is_some() && !cancelling,
                        egui::Button::new(text("Cancel", "取消")),
                    )
                    .clicked()
            {
                self.stop_burn();
            }
            ui.label(status);
        });
        if self.worker_is_burn || self.burn_finished_elapsed.is_some() {
            ui.add(egui::ProgressBar::new(self.burn_progress).show_percentage());
            ui.horizontal_wrapped(|ui| {
                ui.label(self.burn_status_line());
                if !self.active_component.is_empty() {
                    let component = format!(
                        "{} ({:.0}%)",
                        self.active_component,
                        self.component_progress * 100.0
                    );
                    ui.add(egui::Label::new(&component).truncate())
                        .on_hover_text(component);
                }
            });
        }
        if let Some(error) = self.burn_error.as_ref().filter(|_| !self.burn_cancelled) {
            ui.add(
                egui::Label::new(egui::RichText::new(error).color(ui.visuals().error_fg_color))
                    .truncate(),
            )
            .on_hover_text(error);
        }
    }

    fn ui_transport(&mut self, ui: &mut egui::Ui) {
        let mut transport = self.transport;
        egui::ComboBox::from_id_salt("transport_select")
            .width(84.0)
            .selected_text(match transport {
                TransportKind::Usb => self.t(Msg::TransportUsb),
                TransportKind::Uart => self.t(Msg::TransportUart),
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut transport,
                    TransportKind::Usb,
                    self.t(Msg::TransportUsb),
                );
                ui.selectable_value(
                    &mut transport,
                    TransportKind::Uart,
                    self.t(Msg::TransportUart),
                );
            });
        if transport != self.transport {
            if self.transport == TransportKind::Uart {
                self.stop_monitor();
            }
            self.transport = transport;
            self.config.transport = transport.config_value().to_string();
            self.start_device_scan(ui.ctx().clone());
        }
    }

    fn ui_uart_options(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(self.t(Msg::BaudRate));
            ui.add(
                egui::DragValue::new(&mut self.config.serial_baud)
                    .range(1200..=6_000_000)
                    .speed(100.0),
            );
            ui.label(self.t(Msg::MaxBaudRate));
            ui.add(
                egui::DragValue::new(&mut self.config.serial_speed)
                    .range(0..=6_000_000)
                    .speed(100.0),
            );
        });
        let auto_label = self.t(Msg::AutoEnterUpgrade);
        ui.checkbox(&mut self.config.serial_auto_enter, auto_label);
        ui.label(
            egui::RichText::new(self.t(Msg::UartModeHint))
                .small()
                .weak(),
        );
    }

    fn ui_monitor(&mut self, ui: &mut egui::Ui) {
        let connect_label = self.t(Msg::ConnectMonitor);
        let disconnect_label = self.t(Msg::DisconnectMonitor);
        let trigger_label = self.t(Msg::EnterUpgradeMode);
        let send_label = self.t(Msg::Send);
        let input_hint = self.t(Msg::MonitorInputHint);
        let monitor_active = self.monitor.is_some();

        let mut connect = false;
        let mut disconnect = false;
        let mut trigger = false;
        let mut send_text: Option<String> = None;

        ui.horizontal_wrapped(|ui| {
            if monitor_active {
                if ui.button(disconnect_label).clicked() {
                    disconnect = true;
                }
                if ui.button(trigger_label).clicked() {
                    trigger = true;
                }
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.monitor_input)
                        .hint_text(input_hint)
                        .desired_width(200.0),
                );
                let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button(send_label).clicked() || enter {
                    let text = self.monitor_input.trim().to_string();
                    if !text.is_empty() {
                        self.monitor_input.clear();
                        send_text = Some(text);
                    }
                }
            } else if ui
                .add_enabled(!self.busy, egui::Button::new(connect_label))
                .clicked()
            {
                connect = true;
            }
        });
        ui.label(egui::RichText::new(self.t(Msg::MonitorHint)).small().weak());

        let lang = self.lang();
        ui.checkbox(
            &mut self.monitor_enter_on_connect,
            bilingual(
                lang,
                "Enter upgrade on monitor connect",
                "连接监视器时进入升级模式",
            ),
        );
        if connect {
            self.logs_open = true;
            self.start_monitor();
            if self.monitor_enter_on_connect {
                self.monitor_trigger_upgrade();
            }
        }
        if disconnect {
            self.stop_monitor();
        }
        if trigger {
            self.monitor_trigger_upgrade();
        }
        if let Some(text) = send_text {
            self.monitor_send(&text);
        }
        if self
            .monitor
            .as_ref()
            .is_some_and(|monitor| monitor.is_finished())
        {
            self.stop_monitor();
        }
    }

    fn ui_serial_selector(&mut self, ui: &mut egui::Ui) {
        let selected_text = self.selected_serial_port_label();
        let auto_label = self.t(Msg::SerialPortAuto);
        let mut changed = false;
        egui::ComboBox::from_id_salt("serial_port_select")
            .width(126.0)
            .truncate()
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                changed |= ui
                    .selectable_value(&mut self.selected_serial_port, None, auto_label)
                    .clicked();
                for (idx, port) in self.serial_ports.iter().enumerate() {
                    changed |= ui
                        .selectable_value(
                            &mut self.selected_serial_port,
                            Some(idx),
                            &port.port_name,
                        )
                        .clicked();
                }
            });
        let port_name = self
            .selected_serial_port
            .and_then(|idx| self.serial_ports.get(idx))
            .map(|port| port.port_name.clone())
            .unwrap_or_default();
        if changed {
            self.config.serial_port = port_name;
        }
    }

    fn selected_serial_port_name(&self) -> String {
        match self.selected_serial_port {
            Some(idx) => self
                .serial_ports
                .get(idx)
                .map(|port| port.port_name.clone())
                .unwrap_or_default(),
            None => self.config.serial_port.clone(),
        }
    }

    fn selected_serial_port_label(&self) -> String {
        match self.selected_serial_port {
            Some(idx) => self
                .serial_ports
                .get(idx)
                .map(|port| port.port_name.clone())
                .unwrap_or_else(|| self.t(Msg::SerialPortAuto).to_string()),
            None if !self.config.serial_port.is_empty() => self.config.serial_port.clone(),
            None => self.t(Msg::SerialPortAuto).to_string(),
        }
    }

    fn ui_partition_selector(&mut self, ui: &mut egui::Ui) {
        let Some(summary) = self
            .image_summary
            .as_ref()
            .filter(|_| self.image_matches_input())
        else {
            ui.label(self.t(Msg::NoImageLoaded));
            return;
        };
        let lang = self.lang();
        let fixed = bilingual(
            lang,
            "Fixed components depend on the selected boot stage.",
            "固定组件取决于所选启动阶段。",
        );
        let size = self.t(Msg::ColumnSize);
        let offset = self.t(Msg::ColumnOffset);
        ui.weak(fixed);
        let mut changed = false;
        for meta in &summary.metas {
            let key = part_key(meta);
            let is_target = services::is_target_component(&meta.name);
            let mut checked = if is_target {
                partition_selected(&self.selected_parts, meta)
            } else {
                meta.name == "image.info"
                    || (meta.name.starts_with("image.updater.") && !self.burn_form.skips_updater())
            };
            if ui
                .add_enabled(
                    is_target && !self.busy,
                    egui::Checkbox::new(&mut checked, &meta.name),
                )
                .changed()
            {
                changed = true;
                if checked {
                    self.selected_parts.push(key);
                } else {
                    self.selected_parts
                        .retain(|part| !partition_matches(part, meta));
                }
            }
            let partition = meta.partition_display();
            let partition = if partition.is_empty() {
                summary.media_type.as_str()
            } else {
                &partition
            };
            ui.indent(meta.index, |ui| {
                ui.weak(format!(
                    "{partition} · {size} {} B · {offset} {:#x} · CRC {:08x}",
                    meta.size, meta.offset, meta.crc
                ));
            });
        }
        if changed {
            self.config.selected_parts = self.selected_parts.clone();
        }
    }

    fn ui_burn_dialogs(&mut self, ctx: &egui::Context) {
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        if let Some(details) = self.burn_details {
            let (title, height) = match details {
                BurnDetails::Device => (text("Device details", "设备详情"), 150.0),
                BurnDetails::Image => (text("Firmware details", "固件详情"), 300.0),
                BurnDetails::Partitions => (text("Configure partitions", "配置分区"), 240.0),
            };
            let mut open = true;
            let available = ctx.content_rect().size();
            egui::Window::new(title)
                .open(&mut open)
                .collapsible(false)
                .default_width(420.0_f32.min(available.x - 40.0))
                .default_height(height)
                .max_size(available - egui::vec2(40.0, 70.0))
                .vscroll(true)
                .show(ctx, |ui| match details {
                    BurnDetails::Device => self.ui_device_details(ui),
                    BurnDetails::Image => self.ui_image_details(ui),
                    BurnDetails::Partitions => self.ui_partition_selector(ui),
                });
            if !open {
                self.burn_details = None;
            }
        }
        if let Some(target) = self.confirm_risky_burn.as_ref() {
            let response =
                egui::Modal::new(egui::Id::unique("confirm_risky_burn")).show(ctx, |ui| {
                    ui.set_width(355.0);
                    ui.heading(text("Confirm destructive operation", "确认高风险操作"));
                    ui.monospace(target.label());
                    if self.config.erase_all {
                        ui.label(text(
                        "All data on the selected storage device will be erased before burning.",
                        "烧录前将擦除所选存储介质上的全部数据。",
                    ));
                    }
                    if self.config.force_upgrade {
                        ui.label(self.t(Msg::ForceUpgradeNote));
                    }
                    ui.horizontal_wrapped(|ui| {
                        let cancel = ui.button(text("Cancel", "取消")).clicked();
                        let confirm = ui
                            .add_enabled(
                                !self.busy && self.image_matches_input(),
                                egui::Button::new(text("Confirm and burn", "确认并烧录")),
                            )
                            .clicked();
                        (cancel, confirm)
                    })
                    .inner
                });
            let (cancel, confirm) = response.inner;
            if confirm {
                if let Some(target) = self.confirm_risky_burn.take() {
                    self.request_burn(target);
                }
            } else if cancel || response.should_close() {
                self.confirm_risky_burn = None;
            }
        }
    }

    fn ui_device_details(&mut self, ui: &mut egui::Ui) {
        if self.transport == TransportKind::Usb {
            if let Some(device) = self
                .selected_device
                .and_then(|index| self.devices.get(index))
            {
                ui.label(device_readiness_label(self.lang(), device));
                ui.monospace(format_device_ref(device));
                if let Some(status) = &device.status {
                    ui.label(localize_device_status(self.lang(), status));
                }
            } else {
                ui.label(self.t(Msg::NoDevice));
            }
        } else {
            ui.label(self.selected_serial_port_label());
        }
        if ui
            .add_enabled(!self.busy, egui::Button::new(self.t(Msg::DeviceInfo)))
            .clicked()
        {
            self.start_read_device_info();
            self.logs_open = true;
            self.burn_details = None;
        }
    }

    fn ui_tools(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        self.ui_diagnostics(ui);
        ui.separator();
        ui.heading(self.t(Msg::OfficialTools));
        ui.scope(|ui| {
            let mut path_text = self.config.upgcmd_path.display().to_string();
            let (response, browse) =
                theme::path_field(ui, "upgcmd", &mut path_text, self.t(Msg::Browse));
            if response.lost_focus() {
                self.config.upgcmd_path = PathBuf::from(path_text.trim());
            }
            if browse.clicked() {
                let dialog = rfd::FileDialog::new();
                #[cfg(windows)]
                let dialog = dialog.add_filter("upgcmd", &["exe"]);
                if let Some(path) = dialog.pick_file() {
                    self.config.upgcmd_path = path;
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label(self.t(Msg::Command));
            egui::ComboBox::from_id_salt("official_command")
                .selected_text(command_label(lang, self.official_args.command))
                .show_ui(ui, |ui| {
                    for command in OfficialCommand::ALL {
                        ui.selectable_value(
                            &mut self.official_args.command,
                            command,
                            command_label(lang, command),
                        );
                    }
                });
            let verbose = tr(lang, Msg::Verbose);
            let device_log = tr(lang, Msg::DeviceLog);
            let progress = tr(lang, Msg::Progress);
            ui.checkbox(&mut self.official_args.verbose, verbose);
            ui.checkbox(&mut self.official_args.device_log, device_log);
            ui.checkbox(&mut self.official_args.progress, progress);
        });
        // Transport selectors mirror official `upgcmd -d/-u/-b`; empty = default.
        let target_device = tr(lang, Msg::Device);
        let serial_port_label = tr(lang, Msg::SerialPort);
        let baud_rate_label = tr(lang, Msg::BaudRate);
        ui.horizontal(|ui| {
            ui.label(target_device);
            ui.add(
                egui::TextEdit::singleline(&mut self.official_args.device)
                    .hint_text("bus:port")
                    .desired_width(80.0),
            );
            ui.label(serial_port_label);
            ui.add(
                egui::TextEdit::singleline(&mut self.official_args.uart_port)
                    .hint_text("COMx")
                    .desired_width(80.0),
            );
            ui.label(baud_rate_label);
            ui.add(
                egui::TextEdit::singleline(&mut self.official_args.baudrate)
                    .hint_text("921600")
                    .desired_width(80.0),
            );
        });

        self.ui_official_args(ui);
        ui.separator();
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.busy, |ui| {
                if ui.button(self.t(Msg::Run)).clicked() {
                    self.start_official_command();
                }
            });
            match official::build_args(&self.official_args) {
                Ok(args) => {
                    ui.label(format!("{}: {}", self.t(Msg::ArgsPrefix), args.join(" ")));
                }
                Err(e) => {
                    ui.colored_label(egui::Color32::from_rgb(180, 40, 40), e);
                }
            }
        });
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui.button(self.t(Msg::Manual)).clicked() {
                match official::open_manual(&self.config.aiburn_dir) {
                    Ok(()) => self.log(self.t(Msg::OpenedManual)),
                    Err(e) => self.log(e),
                }
            }
        });
    }

    fn ui_official_confirmation(&mut self, ctx: &egui::Context) {
        let Some((tool, args)) = &self.official_pending else {
            return;
        };
        let lang = self.lang();
        let mut confirm = false;
        let mut open = true;
        egui::Window::new(bilingual(lang, "Confirm AiBurn command", "确认 AiBurn 命令"))
            .open(&mut open).show(ctx, |ui| {
                ui.colored_label(egui::Color32::RED, bilingual(lang, "Check the target. Erase, eFuse and unlock commands can permanently change the device.", "请核对目标。擦除、eFuse 和解锁命令可能永久改变设备。"));
                ui.label(tool.display().to_string());
                ui.monospace(args.join(" "));
                confirm = ui.add_enabled(!self.busy, egui::Button::new(bilingual(lang, "Confirm and run", "确认并运行"))).clicked();
            });
        if confirm {
            if let Some((tool, args)) = self.official_pending.take() {
                self.stop_monitor();
                self.start_task(move || {
                    official::run_upgcmd(&tool, &args).map(WorkerEvent::ToolOutput)
                });
            }
        } else if !open {
            self.official_pending = None;
        }
    }

    fn ui_official_args(&mut self, ui: &mut egui::Ui) {
        match self.official_args.command {
            OfficialCommand::ListDevices
            | OfficialCommand::DeviceLog
            | OfficialCommand::ContinueBoot
            | OfficialCommand::GoToBootloader => {}
            OfficialCommand::ImageInfo
            | OfficialCommand::ExtractImage
            | OfficialCommand::UpgradeImage
            | OfficialCommand::ListMedia
            | OfficialCommand::FlashErase
            | OfficialCommand::RamBoot => {
                let label = self.t(Msg::Image);
                let browse = self.t(Msg::Browse);
                path_picker(ui, label, browse, &mut self.official_args.image, false);
            }
            _ => {}
        }
        match self.official_args.command {
            OfficialCommand::WriteMemory
            | OfficialCommand::BdefuseWrite
            | OfficialCommand::AuzWriteFuse => {
                let label = self.t(Msg::Input);
                let browse = self.t(Msg::Browse);
                path_picker(ui, label, browse, &mut self.official_args.input, false);
            }
            OfficialCommand::DumpPartition
            | OfficialCommand::ReadMemory
            | OfficialCommand::BdefuseRead
            | OfficialCommand::JtagUnlockData => {
                let label = self.t(Msg::Output);
                let browse = self.t(Msg::Browse);
                path_picker(ui, label, browse, &mut self.official_args.output, true);
            }
            _ => {}
        }
        match self.official_args.command {
            OfficialCommand::DumpPartition
            | OfficialCommand::ListPartitions
            | OfficialCommand::FlashErase => {
                ui.horizontal(|ui| {
                    ui.label(self.t(Msg::Media));
                    ui.text_edit_singleline(&mut self.official_args.media);
                });
            }
            _ => {}
        }
        if self.official_args.command == OfficialCommand::DumpPartition {
            ui.horizontal(|ui| {
                ui.label(self.t(Msg::ColumnPartition));
                ui.text_edit_singleline(&mut self.official_args.partition);
            });
        }
        if self.official_args.command == OfficialCommand::ShellCommand {
            ui.horizontal(|ui| {
                ui.label(self.t(Msg::Shell));
                ui.text_edit_singleline(&mut self.official_args.shell);
            });
        }
        if self.official_args.command == OfficialCommand::UpgradeImage {
            let hint = self.t(Msg::FwcListHint);
            ui.horizontal(|ui| {
                ui.label(self.t(Msg::FwcList));
                ui.add(
                    egui::TextEdit::singleline(&mut self.official_args.fwc_list)
                        .hint_text(hint)
                        .desired_width(f32::INFINITY),
                );
            });
        }
        if matches!(
            self.official_args.command,
            OfficialCommand::WriteMemory
                | OfficialCommand::ReadMemory
                | OfficialCommand::WriteLong
                | OfficialCommand::ReadLong
                | OfficialCommand::MemTest
                | OfficialCommand::Exec
                | OfficialCommand::HexDump
                | OfficialCommand::Fill
                | OfficialCommand::Clear
                | OfficialCommand::BdefuseRead
                | OfficialCommand::BdefuseDump
                | OfficialCommand::BdefuseWrite
                | OfficialCommand::BdefuseWriteHex
        ) {
            ui.horizontal(|ui| {
                ui.label(self.t(Msg::Address));
                ui.text_edit_singleline(&mut self.official_args.address);
                if matches!(
                    self.official_args.command,
                    OfficialCommand::ReadMemory
                        | OfficialCommand::MemTest
                        | OfficialCommand::HexDump
                        | OfficialCommand::Fill
                        | OfficialCommand::Clear
                        | OfficialCommand::WriteMemory
                        | OfficialCommand::BdefuseRead
                        | OfficialCommand::BdefuseDump
                        | OfficialCommand::BdefuseWrite
                ) {
                    ui.label(self.t(Msg::Length));
                    ui.text_edit_singleline(&mut self.official_args.length);
                }
                if matches!(
                    self.official_args.command,
                    OfficialCommand::WriteLong
                        | OfficialCommand::Fill
                        | OfficialCommand::BdefuseWriteHex
                ) {
                    ui.label(self.t(Msg::Value));
                    ui.text_edit_singleline(&mut self.official_args.value);
                }
                if self.official_args.command == OfficialCommand::MemTest {
                    ui.label(self.t(Msg::Round));
                    ui.text_edit_singleline(&mut self.official_args.round);
                }
            });
        }
        if self.official_args.command == OfficialCommand::BdefuseSelect {
            ui.horizontal(|ui| {
                ui.label(self.t(Msg::EfuseId));
                ui.text_edit_singleline(&mut self.official_args.efuse_id);
            });
        }
        if self.official_args.command == OfficialCommand::WriteMemory {
            let optional = self.t(Msg::Optional);
            ui.horizontal(|ui| {
                ui.label(self.t(Msg::Skip));
                ui.add(
                    egui::TextEdit::singleline(&mut self.official_args.skip)
                        .hint_text(optional)
                        .desired_width(120.0),
                );
            });
        }
        if self.official_args.command == OfficialCommand::RamBoot {
            ui.horizontal(|ui| {
                ui.label(self.t(Msg::Fwc));
                ui.text_edit_singleline(&mut self.official_args.fwc_name);
                ui.label(self.t(Msg::Ram));
                ui.text_edit_singleline(&mut self.official_args.ram_address);
            });
        }
        if matches!(
            self.official_args.command,
            OfficialCommand::Raw | OfficialCommand::JtagUnlock
        ) {
            ui.horizontal(|ui| {
                ui.label(self.t(Msg::Args));
                ui.add(
                    egui::TextEdit::singleline(&mut self.official_args.raw_args)
                        .desired_width(f32::INFINITY),
                );
            });
        }
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading(self.t(Msg::TabSettings));
        ui.scope(|ui| {
            let mut text = self.config.app_dir.display().to_string();
            let (response, browse) =
                theme::path_field(ui, self.t(Msg::AppDataDir), &mut text, self.t(Msg::Browse));
            if response.lost_focus() && !text.trim().is_empty() {
                let path = PathBuf::from(text.trim());
                if path != self.config.app_dir {
                    self.config.app_dir = path.clone();
                    self.settings_path = path.join("config.ini");
                    self.image_history = load_image_history(&self.config.app_dir);
                }
            }
            if browse.clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.config.app_dir = path.clone();
                    self.settings_path = path.join("config.ini");
                    self.image_history = load_image_history(&self.config.app_dir);
                }
            }
        });
        ui.scope(|ui| {
            let mut text = self.config.aiburn_dir.display().to_string();
            let (response, browse) =
                theme::path_field(ui, self.t(Msg::AiBurnDir), &mut text, self.t(Msg::Browse));
            if response.lost_focus() {
                let path = PathBuf::from(text.trim());
                if path != self.config.aiburn_dir {
                    self.config.aiburn_dir = path.clone();
                    self.config.upgcmd_path = compat_tool_path(&path);
                }
            }
            if browse.clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.config.aiburn_dir = path.clone();
                    self.config.upgcmd_path = compat_tool_path(&path);
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label(self.t(Msg::Language));
            let mut lang = self.lang();
            egui::ComboBox::from_id_salt("language_select")
                .selected_text(lang.native_name())
                .show_ui(ui, |ui| {
                    for candidate in Language::ALL {
                        ui.selectable_value(&mut lang, candidate, candidate.native_name());
                    }
                });
            self.config.language = lang.code().to_string();
            ui.label(self.t(Msg::TimeoutSeconds));
            ui.add(egui::DragValue::new(&mut self.config.burn_timeout_secs).range(1..=3600));
            ui.label(self.t(Msg::Retry));
            ui.add(egui::DragValue::new(&mut self.config.retry_count).range(1..=20));
        });
        let verbose_log = self.t(Msg::VerboseLog);
        let block_error_log = self.t(Msg::BlockErrorLog);
        let read_device_log = self.t(Msg::ReadDeviceLog);
        ui.checkbox(&mut self.config.verbose, verbose_log);
        ui.checkbox(&mut self.config.block_error_log, block_error_log);
        ui.checkbox(&mut self.config.read_device_log, read_device_log);
        verbosity::set_verbose(self.config.verbose);
        ui.separator();
        self.ui_burn_stats(ui);
        let lang = self.lang();
        ui.horizontal_wrapped(|ui| {
            ui.label(bilingual(lang, "ADB executable", "ADB 可执行文件"));
            if ui.button(self.t(Msg::Browse)).clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    self.config.adb_executable = Some(path);
                }
            }
            if let Some(path) = &self.config.adb_executable {
                ui.label(path.display().to_string());
                if ui
                    .button(bilingual(lang, "Use PATH", "使用 PATH"))
                    .clicked()
                {
                    self.config.adb_executable = None;
                }
            } else {
                ui.monospace("PATH");
            }
        });
        ui.horizontal_wrapped(|ui| {
            let lang = self.lang();
            ui.label(bilingual(lang, "Appearance", "外观"));
            let label = |preference: &str| match preference {
                "dark" => bilingual(lang, "Dark", "深色"),
                "light" => bilingual(lang, "Light", "浅色"),
                _ => bilingual(lang, "System", "跟随系统"),
            };
            egui::ComboBox::from_id_salt("gui_theme")
                .selected_text(label(&self.config.gui_theme))
                .show_ui(ui, |ui| {
                    for preference in theme::THEMES {
                        if ui
                            .selectable_value(
                                &mut self.config.gui_theme,
                                preference.to_string(),
                                label(preference),
                            )
                            .changed()
                        {
                            theme::apply_preference(ui.ctx(), preference);
                        }
                    }
                });
        });
        ui.separator();
        self.ui_update_settings(ui);
        self.ui_structured(ui);
        ui.horizontal(|ui| {
            if ui.button(self.t(Msg::LoadAiBurnIni)).clicked() {
                match AppConfig::load_from(&self.settings_path) {
                    Ok(config) => {
                        self.config = config;
                        theme::apply_preference(ui.ctx(), &self.config.gui_theme);
                        self.config.erase_all = false;
                        self.config.force_upgrade = false;
                        self.transport = TransportKind::from_config(&self.config.transport);
                        self.image_summary = None;
                        self.image_input.clear();
                        self.selected_serial_port = None;
                        self.stop_monitor();
                        self.selected_parts = self.config.selected_parts.clone();
                        if let Some(path) = self.config.image_path.clone() {
                            self.load_image_summary(path, false);
                        }
                        self.start_device_scan(ui.ctx().clone());
                        self.log(self.t(Msg::LoadedAiBurnIni));
                    }
                    Err(e) => self.log(e),
                }
            }
            if ui.button(self.t(Msg::SaveAiBurnIni)).clicked() {
                self.config.selected_parts = self.selected_parts.clone();
                match self.config.save_to(&self.settings_path) {
                    Ok(()) => self.log(format!(
                        "{} {}",
                        self.t(Msg::Saved),
                        self.settings_path.display()
                    )),
                    Err(e) => self.log(e),
                }
            }
        });
    }

    fn ui_burn_stats(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(self.t(Msg::BurnStats));
            if ui.button(self.t(Msg::Refresh)).clicked() {
                // Stats are read fresh on every frame; just repaint.
                ui.ctx().request_repaint();
            }
            if ui
                .button(bilingual(
                    self.lang(),
                    "View statistics JSON",
                    "查看统计 JSON",
                ))
                .clicked()
            {
                self.structured = Some(burn_stats::load(&self.config.app_dir).to_json());
            }
            if ui.button(self.t(Msg::ClearStats)).clicked() {
                match burn_stats::clear(&self.config.app_dir) {
                    Ok(()) => self.log(self.t(Msg::ClearStats)),
                    Err(e) => self.log_error(e),
                }
            }
        });
        let stats = burn_stats::load(&self.config.app_dir);
        if stats.days.is_empty() {
            ui.label(match self.lang() {
                Language::ZhCn => "暂无烧写统计，完成一次烧录后自动计数。",
                Language::En => "No burn statistics yet; counted automatically after a burn.",
            });
            return;
        }
        egui::Grid::new("burn_stats_grid")
            .striped(true)
            .min_col_width(70.0)
            .show(ui, |ui| {
                ui.label(match self.lang() {
                    Language::ZhCn => "日期",
                    Language::En => "date",
                });
                ui.label(match self.lang() {
                    Language::ZhCn => "成功",
                    Language::En => "success",
                });
                ui.label(match self.lang() {
                    Language::ZhCn => "失败",
                    Language::En => "failure",
                });
                ui.label(match self.lang() {
                    Language::ZhCn => "取消",
                    Language::En => "cancelled",
                });
                ui.label(match self.lang() {
                    Language::ZhCn => "成功率",
                    Language::En => "rate",
                });
                ui.end_row();
                for (date, day) in &stats.days {
                    ui.monospace(date);
                    ui.label(day.success.to_string());
                    ui.label(day.failure.to_string());
                    ui.label(day.cancelled.to_string());
                    ui.label(
                        day.success_rate()
                            .map(|r| format!("{:.1}%", r * 100.0))
                            .unwrap_or_else(|| "--".to_string()),
                    );
                    ui.end_row();
                }
            });
        let total = stats.total();
        ui.label(format!(
            "{}: {} / {} ({})",
            match self.lang() {
                Language::ZhCn => "累计",
                Language::En => "total",
            },
            total.success,
            total.success + total.failure,
            total
                .success_rate()
                .map(|r| format!("{:.1}%", r * 100.0))
                .unwrap_or_else(|| "--".to_string()),
        ));
    }

    fn ui_update_settings(&mut self, ui: &mut egui::Ui) {
        let channel_label = self.t(Msg::UpdateChannel);
        let auto_label = self.t(Msg::AutoCheckUpdate);
        let check_label = self.t(Msg::CheckUpdate);
        let open_label = self.t(Msg::OpenReleasePage);
        ui.horizontal(|ui| {
            ui.label(channel_label);
            let mut channel = self.config.update_channel.clone();
            egui::ComboBox::from_id_salt("update_channel_select")
                .selected_text(channel.clone())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut channel, "stable".to_string(), "stable");
                    ui.selectable_value(&mut channel, "nightly".to_string(), "nightly");
                });
            if self.config.update_channel != channel {
                self.update_url = None;
                self.config.update_channel = channel;
            }
            ui.checkbox(&mut self.config.auto_check_update, auto_label);
        });
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.update_check_in_progress, |ui| {
                if ui.button(check_label).clicked() {
                    self.start_update_check(ui.ctx().clone());
                }
            });
            if ui.button(open_label).clicked() {
                let channel = UpdateChannel::from_str(&self.config.update_channel);
                let url = match channel {
                    UpdateChannel::Nightly => update::nightly_url(),
                    UpdateChannel::Stable => update::releases_url(),
                };
                if let Err(e) = update::open_url(self.update_url.as_deref().unwrap_or(&url)) {
                    self.log(e);
                }
            }
            if !self.update_status.is_empty() {
                ui.label(&self.update_status);
            }
        });
        ui.label(format!(
            "{} {}",
            build_info::VERSION_WITH_BUILD,
            if update::is_portable_install() {
                "(portable)"
            } else {
                "(managed install)"
            }
        ));
    }

    fn ui_log(&mut self, ui: &mut egui::Ui) {
        let mut copy_requested = false;
        let mut save_requested = false;
        ui.horizontal_wrapped(|ui| {
            if ui.button(self.t(Msg::CopyLog)).clicked() {
                copy_requested = true;
            }
            if ui.button(self.t(Msg::SaveLog)).clicked() {
                save_requested = true;
            }
            if ui.button(self.t(Msg::Clear)).clicked() {
                self.log_lines.clear();
            }
        });
        if copy_requested {
            self.copy_log(ui.ctx());
        }
        if save_requested {
            self.save_log_to_file();
        }
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(ui.available_height())
            .show(ui, |ui| {
                for line in &self.log_lines {
                    ui.add(
                        egui::Label::new(egui::RichText::new(line).monospace()).selectable(true),
                    );
                }
            });
    }

    fn copy_log(&mut self, ctx: &egui::Context) {
        let text = self.log_lines.join("\n");
        ctx.copy_text(text);
        self.log(format!(
            "{} ({} lines)",
            self.t(Msg::LogCopied),
            self.log_lines.len()
        ));
    }

    fn save_log_to_file(&mut self) {
        let default = services::default_log_path(&self.config.app_dir);
        let dialog = rfd::FileDialog::new().set_file_name(
            default
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("artinchip-flash.log"),
        );
        let dialog = match default.parent() {
            Some(dir) => dialog.set_directory(dir),
            None => dialog,
        };
        let Some(path) = dialog.save_file() else {
            return;
        };
        match services::write_log_file(&path, &self.log_lines) {
            Ok(()) => self.log(format!("{} {}", self.t(Msg::LogSavedTo), path.display())),
            Err(e) => self.log_error(e),
        }
    }

    fn selected_device_label(&self) -> String {
        self.selected_device
            .and_then(|idx| self.devices.get(idx))
            .map(usb_port_label)
            .unwrap_or_else(|| {
                if self.devices.is_empty() {
                    self.t(Msg::NoDevice).to_string()
                } else {
                    bilingual(self.lang(), "Select device", "选择设备").to_string()
                }
            })
    }

    fn ui_page(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| self.ui_top_bar(ui));
        egui::Panel::bottom("log_toggle").show(ui, |ui| {
            let label = match self.logs_open {
                true => bilingual(self.lang(), "Hide logs", "隐藏日志"),
                false => bilingual(self.lang(), "Show logs", "显示日志"),
            };
            if ui.button(label).clicked() {
                self.logs_open = !self.logs_open;
            }
        });
        if self.logs_open {
            let max_log_height = (ui.available_height() * 0.42).max(90.0);
            egui::Panel::bottom("log_panel")
                .resizable(true)
                .default_size(144.0)
                .size_range(70.0..=max_log_height)
                .show(ui, |ui| self.ui_log(ui));
        }
        if self.tab == Tab::Burn || self.worker_is_burn {
            egui::Panel::bottom("burn_actions").show(ui, |ui| self.ui_burn_actions(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            let scroll = if self.tab == Tab::Burn {
                egui::ScrollArea::vertical()
            } else {
                egui::ScrollArea::both()
            };
            scroll
                .id_salt(self.tab as u8)
                .auto_shrink([false, false])
                .show(ui, |ui| match self.tab {
                    Tab::Burn => self.ui_burn(ui),
                    Tab::Debug => self.ui_native(ui),
                    Tab::Tools => self.ui_tools(ui),
                    Tab::Settings => self.ui_settings(ui),
                });
        });
        self.ui_burn_dialogs(ui.ctx());
        self.ui_native_preview(ui.ctx());
        self.ui_official_confirmation(ui.ctx());
    }
}

impl eframe::App for GuiApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(app_window_title(self.lang())));
        self.poll_worker(ctx);
        if self.monitor.as_ref().is_some_and(UartMonitor::is_finished) {
            self.stop_monitor();
        }
        if !self.busy && self.last_scan.elapsed() >= Duration::from_secs(5) {
            if self.transport == TransportKind::Usb {
                self.start_device_scan(ctx.clone());
            }
            self.last_scan = Instant::now();
        }
        ctx.request_repaint_after(Duration::from_secs(1));
        // Keep the rate/elapsed label ticking while a burn is in flight,
        // even between chunk progress events.
        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui_page(ui);
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title(app_window_title(Language::from_code("zh_cn")))
            .with_icon(app_icon())
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([480.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        &app_window_title(Language::from_code("zh_cn")),
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc)))),
    )
}

fn app_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../../assets/artinchip-flash.png"))
        .expect("embedded application icon must be a valid PNG")
        .into_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

fn app_window_title(lang: Language) -> String {
    format!(
        "{} {}",
        tr(lang, Msg::AppTitle),
        build_info::VERSION_WITH_BUILD
    )
}

fn selectable_tab(ui: &mut egui::Ui, current: &mut Tab, tab: Tab, label: &str) {
    if ui.selectable_label(*current == tab, label).clicked() {
        *current = tab;
    }
}

fn row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.label(key);
    ui.monospace(value);
    ui.end_row();
}

fn path_picker(
    ui: &mut egui::Ui,
    label: &str,
    browse_label: &str,
    path: &mut Option<PathBuf>,
    save: bool,
) {
    let mut text = path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let (_, browse) = theme::path_field(ui, label, &mut text, browse_label);
    if browse.clicked() {
        let picked = if save {
            rfd::FileDialog::new().save_file()
        } else {
            rfd::FileDialog::new().pick_file()
        };
        if let Some(picked) = picked {
            *path = Some(picked);
        }
    }
}

fn target_metas(summary: &ImageSummary) -> impl Iterator<Item = &MetaSummary> {
    summary
        .metas
        .iter()
        .filter(|meta| services::is_target_component(&meta.name))
}

fn part_key(meta: &MetaSummary) -> String {
    services::partition_key(&meta.name).to_string()
}

fn partition_selected(parts: &[String], meta: &MetaSummary) -> bool {
    parts.iter().any(|part| partition_matches(part, meta))
}

fn partition_matches(part: &str, meta: &MetaSummary) -> bool {
    let part = part.trim();
    part == services::partition_key(&meta.name) || part == meta.partition || part == meta.name
}

fn usb_port_label(device: &DeviceInfo) -> String {
    format!("{}:{}", device.bus_number, device.port_path_or_address())
}

fn progress(sent: usize, total: usize) -> f32 {
    services::progress_ratio(sent, total)
}

fn format_rate(bps: f64) -> String {
    if bps <= 0.0 {
        return "--".to_string();
    }
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    if bps >= MIB {
        format!("{:.2} MiB/s", bps / MIB)
    } else if bps >= KIB {
        format!("{:.1} KiB/s", bps / KIB)
    } else {
        format!("{:.0} B/s", bps)
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

trait DeviceLabel {
    fn port_path_or_address(&self) -> String;
}

fn select_scanned_device(devices: &[DeviceInfo], pinned: Option<&(u8, String)>) -> Option<usize> {
    match pinned {
        Some((bus, port)) => devices
            .iter()
            .position(|device| device.bus_number == *bus && device.port_path == *port),
        None => (devices.len() == 1).then_some(0),
    }
}

#[cfg(test)]
#[test]
fn usb_scan_selects_single_device_and_tracks_manually_selected_port() {
    let first = DeviceInfo {
        bus_number: 1,
        address: 2,
        vendor_id: 0x33c3,
        product_id: 0x6677,
        port_path: "3".into(),
        speed: "High".into(),
        ready: true,
        ioreg_ready: None,
        protocol_ready: None,
        status: None,
    };
    let second = DeviceInfo {
        port_path: "4".into(),
        ..first.clone()
    };
    let reconnected = DeviceInfo {
        address: 9,
        ..first.clone()
    };
    let pinned = (first.bus_number, first.port_path.clone());
    assert_eq!(select_scanned_device(&[], None), None);
    assert_eq!(
        select_scanned_device(std::slice::from_ref(&first), None),
        Some(0)
    );
    assert_eq!(select_scanned_device(&[first, second.clone()], None), None);
    assert_eq!(
        select_scanned_device(std::slice::from_ref(&second), Some(&pinned)),
        None
    );
    assert_eq!(
        select_scanned_device(&[second, reconnected], Some(&pinned)),
        Some(1)
    );
}

impl DeviceLabel for DeviceInfo {
    fn port_path_or_address(&self) -> String {
        if self.port_path.is_empty() {
            self.address.to_string()
        } else {
            self.port_path.clone()
        }
    }
}

fn open_uart_backend(port: &str, options: UartOptions) -> Result<UartDevice, String> {
    // Shared implementation lives in `services`.
    services::open_uart(&UartSpec::new(
        port,
        options.baudrate,
        options.max_baudrate,
        options.auto_enter,
    ))
}

fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn update_recently_checked(last_unix: u64) -> bool {
    const DAY_SECS: u64 = 24 * 3600;
    last_unix > 0 && current_unix_secs().saturating_sub(last_unix) < DAY_SECS
}

fn scan_summary(
    lang: Language,
    detected: usize,
    enumerated: usize,
    protocol_ready: usize,
    protocol_failed: usize,
    protocol_unchecked: usize,
) -> String {
    match lang {
        Language::ZhCn => format!(
            "扫描完成：检测到 {} 个 ArtInChip 设备，{} 个已枚举，协议就绪 {}，协议失败 {}，未检测 {}",
            detected, enumerated, protocol_ready, protocol_failed, protocol_unchecked
        ),
        Language::En => format!(
            "Scan complete: detected {} ArtInChip device(s), {} enumerated, {} protocol-ready, {} protocol-failed, {} protocol-unchecked",
            detected, enumerated, protocol_ready, protocol_failed, protocol_unchecked
        ),
    }
}

fn device_readiness_label(lang: Language, device: &DeviceInfo) -> String {
    let enumerated = match (lang, device.ready) {
        (Language::ZhCn, true) => "已枚举",
        (Language::ZhCn, false) => "未枚举",
        (Language::En, true) => "enumerated",
        (Language::En, false) => "not-enumerated",
    };
    let protocol = match (lang, device.protocol_ready) {
        (Language::ZhCn, Some(true)) => "协议就绪",
        (Language::ZhCn, Some(false)) => "协议失败",
        (Language::ZhCn, None) => "协议未检测",
        (Language::En, Some(true)) => "protocol-ready",
        (Language::En, Some(false)) => "protocol-failed",
        (Language::En, None) => "protocol-unchecked",
    };
    format!("{}, {}", enumerated, protocol)
}

fn not_ready_context_label(lang: Language, context: NotReadyContext) -> &'static str {
    match (lang, context) {
        (Language::ZhCn, NotReadyContext::Scan) => "扫描诊断",
        (Language::En, NotReadyContext::Scan) => "Scan diagnostic",
    }
}

fn format_device_ref(device: &DeviceInfo) -> String {
    format!(
        "bus={} path={} address={} vid=0x{:04x} pid=0x{:04x} speed={}",
        device.bus_number,
        device.port_path_or_address(),
        device.address,
        device.vendor_id,
        device.product_id,
        device.speed
    )
}

fn localize_device_status(lang: Language, status: &str) -> String {
    if lang == Language::ZhCn
        && (status.contains("kUSBCurrentConfiguration") || status.contains("IOUSBHostInterface"))
    {
        return format!(
            "macOS 已枚举 VID/PID，但还没有为设备创建 USB 配置或接口；原始状态：{}",
            status
        );
    }
    status.to_string()
}

fn macos_recovery_hint(lang: Language) -> &'static str {
    match lang {
        Language::ZhCn => {
            "处理建议：关闭其它 artinchip-flash/AiBurn 实例，拔插或断电重启板子，重新进入升级模式；优先直连 Mac，先避开 hub/dock。"
        }
        Language::En => {
            "Recovery hint: close other artinchip-flash/AiBurn instances, unplug or power-cycle the board, enter upgrade mode again, and test with a direct Mac connection before using hubs/docks."
        }
    }
}

fn install_cjk_font(ctx: &egui::Context) {
    let Some((_, font_data)) = load_cjk_font() else {
        eprintln!(
            "Warning: No CJK font found on this system. Chinese characters might not display correctly."
        );
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk_font".to_owned(),
        egui::FontData::from_owned(font_data).into(),
    );

    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .unwrap()
        .push("cjk_font".to_owned());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .unwrap()
        .push("cjk_font".to_owned());

    ctx.set_fonts(fonts);
}

fn load_cjk_font() -> Option<(&'static str, Vec<u8>)> {
    let candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\NotoSansSC-VF.ttf",
        r"C:\Windows\Fonts\Deng.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/wenquanyi/wqy-zenhei.ttc",
    ];

    candidates.iter().find_map(|path| {
        if Path::new(path).exists() {
            std::fs::read(path).ok().map(|data| (*path, data))
        } else {
            None
        }
    })
}

#[allow(dead_code)]
fn _path_exists(path: &Path) -> bool {
    path.exists()
}
