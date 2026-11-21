use super::*;
use artinchip_flash::FirmwareImage;
use artinchip_flash_app::device::BurnOptions;
use artinchip_flash_app::flash_job::{self, EraseRequest, FlashJob, JobError, JobEvent};
pub(super) use artinchip_flash_app::services::DeviceTarget as Target;

pub(super) fn usb_selector(device: &DeviceInfo) -> Result<DeviceSelector, String> {
    if device.port_path.is_empty() {
        return Err("Selected USB device has no stable physical port path".into());
    }
    Ok(DeviceSelector::PhysicalPort(
        device.bus_number,
        device.port_path.clone(),
    ))
}

pub(super) fn bilingual(lang: Language, en: &'static str, zh: &'static str) -> &'static str {
    match lang {
        Language::En => en,
        Language::ZhCn => zh,
    }
}

#[derive(Default)]
pub(super) struct BurnForm {
    partition: bool,
    no_updater: bool,
    no_reset: bool,
    capture_json: bool,
    log_file: Option<PathBuf>,
}

impl BurnForm {
    pub(super) fn skips_updater(&self) -> bool {
        self.no_updater
    }
    fn options(&self, parts: Vec<String>, force: bool, timeout: u64) -> BurnOptions {
        BurnOptions {
            selected_parts: parts,
            force_upgrade: force,
            partition_upgrade: self.partition,
            no_updater: self.no_updater,
            reset_after_burn: !self.no_reset && !force,
            burn_timeout: Duration::from_secs(timeout.max(1)),
            ..Default::default()
        }
    }
}

pub(super) struct BurnRequest {
    pub(super) path: PathBuf,
    pub(super) log_file: Option<PathBuf>,
    pub(super) capture_json: bool,
    target: Target,
    options: BurnOptions,
    erase: Option<(PathBuf, String)>,
}

impl BurnRequest {
    pub(super) fn execute(
        mut self,
        cancel: Arc<AtomicBool>,
        tx: &mpsc::Sender<WorkerEvent>,
    ) -> Result<(), JobError> {
        self.options.cancel = Some(cancel);
        let image = FirmwareImage::open(&self.path).map_err(JobError::Upgrade)?;
        let flash = flash_job::prepare(&image, self.options, self.erase.is_some())
            .map_err(JobError::Upgrade)?;
        let erase = self.erase.map(|(tool, media)| EraseRequest {
            tool,
            media: Some(media),
            image: None,
            uart_baudrate: match &self.target {
                Target::Uart(spec) => Some(spec.baud),
                Target::Usb(_) => None,
            },
        });
        let job = FlashJob {
            flash: &flash,
            target: self.target,
            erase,
            report_device_info: false,
        };
        job.run(|event| {
            let _ = tx.send(match event {
                JobEvent::Output(message) => WorkerEvent::ToolOutput(message),
                JobEvent::Progress(progress) => WorkerEvent::Burn(progress),
            });
        })
    }
}

impl GuiApp {
    pub(super) fn inspect_burn_plan(&mut self) {
        let Some(path) = self.config.image_path.clone() else {
            return;
        };
        let options = self.burn_form.options(
            self.selected_parts.clone(),
            self.config.force_upgrade,
            self.config.burn_timeout_secs,
        );
        let erase_all = self.config.erase_all;
        self.start_task(move || {
            let image = FirmwareImage::open(&path).map_err(|error| error.to_string())?;
            let flash = flash_job::prepare(&image, options, erase_all)
                .map_err(|error| error.to_string())?;
            Ok(WorkerEvent::Structured(flash.plan().to_json()))
        });
    }

    pub(super) fn target(&self, pin: bool) -> Result<Target, String> {
        if self.transport == TransportKind::Uart {
            let port = self.selected_serial_port_name();
            let port = if pin && (port.eq_ignore_ascii_case("auto") || port.trim().is_empty()) {
                match self.serial_ports.as_slice() {
                    [only] => only.port_name.clone(),
                    _ => return Err("Select a specific UART port before a write operation".into()),
                }
            } else {
                port
            };
            return Ok(Target::Uart(UartSpec::new(
                &port,
                self.config.serial_baud,
                Some(self.config.serial_speed),
                self.config.serial_auto_enter,
            )));
        }
        let device = self
            .selected_device
            .and_then(|index| self.devices.get(index));
        match device {
            // Enumeration only makes the device selectable.  The real
            // libusb open/claim decides whether it is usable, and reports
            // the raw error if macOS IOKit is not ready.
            Some(device) => Ok(Target::Usb(usb_selector(device)?)),
            None if !pin && self.devices.is_empty() => Ok(Target::Usb(DeviceSelector::Unique)),
            None => Err("Select an ArtInChip USB device".into()),
        }
    }

    pub(super) fn request_burn(&mut self, target: Target) {
        let Some(path) = self.config.image_path.clone() else {
            self.log(self.t(Msg::SelectImageFirst));
            return;
        };
        let options = self.burn_form.options(
            self.selected_parts.clone(),
            self.config.force_upgrade,
            self.config.burn_timeout_secs,
        );
        let log_file = self.burn_form.log_file.clone();
        if log_file.as_ref().is_some_and(|log| {
            std::fs::canonicalize(log)
                .ok()
                .zip(std::fs::canonicalize(&path).ok())
                .is_some_and(|(log, image)| log == image)
        }) {
            self.logs_open = true;
            self.log_error("Burn log cannot overwrite the firmware image".to_owned());
            return;
        }
        let erase = std::mem::take(&mut self.config.erase_all).then(|| {
            (
                self.config.upgcmd_path.clone(),
                self.config.erase_media.trim().to_owned(),
            )
        });
        self.start_burn(BurnRequest {
            path,
            target,
            options,
            erase,
            log_file,
            capture_json: self.burn_form.capture_json,
        });
    }

    pub(super) fn ui_burn_options(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        let erase_all = self.t(Msg::EraseAll);
        let force_upgrade = self.t(Msg::ForceUpgrade);
        let media_label = self.t(Msg::Media);
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(
                &mut self.burn_form.partition,
                false,
                text("Normal upgrade", "普通升级"),
            );
            ui.radio_value(
                &mut self.burn_form.partition,
                true,
                text("Partition upgrade", "分区升级"),
            );
        });
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(
                &mut self.burn_form.no_updater,
                false,
                text("ROM/BROM (stage 0)", "ROM/BROM（阶段 0）"),
            );
            ui.radio_value(
                &mut self.burn_form.no_updater,
                true,
                text(
                    "U-Boot (stage 1): skip RAM updater",
                    "U-Boot（阶段 1）：跳过 RAM updater",
                ),
            );
            ui.checkbox(
                &mut self.burn_form.no_reset,
                text("Do not reset", "烧录后不复位"),
            );
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(
                &mut self.burn_form.capture_json,
                text("Capture burn events as JSON", "记录烧录 JSON 事件"),
            );
            if ui
                .button(text("Choose burn log…", "选择烧录日志…"))
                .clicked()
            {
                self.burn_form.log_file =
                    rfd::FileDialog::new().set_file_name("burn.log").save_file();
            }
            if let Some(path) = &self.burn_form.log_file {
                if ui.button(text("Remove", "移除")).clicked() {
                    self.burn_form.log_file = None;
                } else {
                    let path = path.display().to_string();
                    ui.add(egui::Label::new(&path).truncate())
                        .on_hover_text(path);
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.config.erase_all, erase_all);
            ui.checkbox(&mut self.config.force_upgrade, force_upgrade);
            if self.config.erase_all {
                ui.label(media_label);
                ui.add(
                    egui::TextEdit::singleline(&mut self.config.erase_media)
                        .hint_text("auto")
                        .desired_width(64.0),
                );
            }
        });
        if self.config.erase_all {
            ui.weak(self.t(Msg::EraseAllNote));
        }
        if self.config.force_upgrade {
            ui.weak(self.t(Msg::ForceUpgradeNote));
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/support/firmware.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d213_form_maps_explicit_partition_and_bootloader_mode() {
        let form = BurnForm {
            partition: true,
            no_updater: true,
            no_reset: true,
            capture_json: false,
            ..Default::default()
        };
        let options = form.options(vec!["ubiroot:rootfs".into()], false, 60);
        assert_eq!(options.selected_parts, ["ubiroot:rootfs"]);
        assert_eq!(artinchip_flash_app::device::upg_mode(&options), 0x01);
        assert!(options.no_updater);
        assert!(!options.reset_after_burn);
    }

    #[test]
    fn loads_and_plans_image_offline_and_cancels_worker_before_open() {
        let path = std::env::temp_dir().join(format!("aic-gui-worker-{}.img", std::process::id()));
        std::fs::write(&path, fixture::firmware()).unwrap();
        let summary = services::load_image_summary(&path).unwrap();
        assert_eq!(summary.metas.len(), 4);
        let options = BurnForm {
            partition: true,
            no_updater: true,
            ..Default::default()
        }
        .options(vec!["os".into()], false, 60);
        let image = FirmwareImage::open(&path).unwrap();
        assert_eq!(
            flash_job::prepare(&image, options.clone(), false)
                .unwrap()
                .plan()
                .total_bytes,
            2055
        );

        let request = BurnRequest {
            path: path.clone(),
            log_file: None,
            capture_json: false,
            target: Target::Usb(DeviceSelector::Unique),
            options,
            erase: None,
        };
        let (sender, receiver) = mpsc::channel();
        let worker =
            thread::spawn(move || request.execute(Arc::new(AtomicBool::new(true)), &sender));
        let result = worker.join().unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(matches!(
            result,
            Err(JobError::Upgrade(artinchip_flash::FlashError::Cancelled))
        ));
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            tr(Language::En, Msg::NoDeviceAvailable),
            "No device available"
        );
        assert_eq!(tr(Language::ZhCn, Msg::NoDeviceAvailable), "没有可用设备");
    }

    #[test]
    fn narrow_burn_home_keeps_browse_and_primary_controls_visible() {
        let folder = std::env::temp_dir().join(format!("aic-gui-layout-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("long-firmware-path-".repeat(8) + ".img");
        std::fs::write(&path, fixture::firmware()).unwrap();
        let ctx = egui::Context::default();
        let mut app = GuiApp::with_config(
            &ctx,
            AppConfig {
                language: "en".into(),
                app_dir: folder.clone(),
                ..Default::default()
            },
        );
        app.image_input = path.display().to_string();
        app.apply_image_summary(services::load_image_summary(&path).unwrap(), false);
        std::fs::remove_dir_all(folder).unwrap();

        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 420.0));
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| app.ui_page(ui),
        );
        output.textures_delta.clear();
        let visible_text: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text)
                    if screen
                        .intersect(shape.clip_rect)
                        .contains_rect(text.visual_bounding_rect()) =>
                {
                    Some(text.galley.text())
                }
                _ => None,
            })
            .collect();
        for expected in [
            "Browse",
            "Configure…",
            "Start burn",
            "Show logs",
            "spl · os",
        ] {
            assert!(
                visible_text.contains(&expected),
                "{expected:?} missing from {visible_text:?}"
            );
        }
        assert!(!visible_text.contains(&"ROM/BROM (stage 0)"));
        assert!(!visible_text.contains(&"image.target.os"));
        assert!(!visible_text.contains(&"Validation passed"));
        assert!(!visible_text.contains(&"Cancel"));
        assert!(app.image_matches_input());
        app.image_input.push_str(".other");
        assert!(!app.image_matches_input());
    }

    #[test]
    fn switching_tabs_preserves_progress_and_cancellation_events() {
        let folder = std::env::temp_dir().join(format!("aic-gui-events-{}", std::process::id()));
        let ctx = egui::Context::default();
        let mut app = GuiApp::with_config(
            &ctx,
            AppConfig {
                app_dir: folder.clone(),
                ..Default::default()
            },
        );
        let (sender, receiver) = mpsc::channel();
        app.rx = Some(receiver);
        app.busy = true;
        app.worker_is_burn = true;
        app.burn_started_at = Some(Instant::now());
        app.burn_json_events = Some(Vec::new());
        let cancel = Arc::new(AtomicBool::new(false));
        app.cancel_flag = Some(cancel.clone());
        app.tab = Tab::Tools;
        sender
            .send(WorkerEvent::Burn(BurnEvent::OverallProgress {
                sent: 3,
                total: 4,
            }))
            .unwrap();
        app.poll_worker(&ctx);
        assert_eq!(app.burn_progress, 0.75);
        assert!(app.worker_is_burn);
        app.stop_burn();
        assert!(cancel.load(Ordering::Relaxed));
        sender.send(WorkerEvent::BurnCancelled).unwrap();
        sender.send(WorkerEvent::Done).unwrap();
        app.poll_worker(&ctx);
        assert!(!app.busy);
        assert!(app.burn_cancelled && app.burn_failed);
        assert!(app.burn_finished_elapsed.is_some());
        let json = app.structured.unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["events"].as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(folder).unwrap();
    }
}
