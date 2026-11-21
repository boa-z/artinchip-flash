use super::*;
use serde_json::{json, Value};

pub(super) fn scan_json(devices: &[DeviceInfo]) -> Value {
    json!({"devices": devices.iter().map(|device| json!({
        "bus": device.bus_number, "address": device.address, "path": device.port_path,
        "vid": format!("{:04x}", device.vendor_id), "pid": format!("{:04x}", device.product_id),
        "speed": device.speed, "ready": device.ready, "status": device.status,
        "ioreg_ready": device.ioreg_ready, "protocol_ready": device.protocol_ready,
    })).collect::<Vec<_>>()})
}

pub(super) fn serial_json(ports: &[SerialPortInfo]) -> Value {
    json!({"ports": ports.iter().map(|port| json!({
        "port": port.port_name, "type": port.port_type, "vid": port.vid,
        "pid": port.pid, "product": port.product,
    })).collect::<Vec<_>>()})
}

pub(super) fn disks_json(disks: &[sdcard::PhysicalDisk]) -> Value {
    json!({"disks": disks.iter().map(|disk| json!({
        "id": disk.id, "model": disk.model, "size_bytes": disk.size_bytes,
        "bus_type": disk.bus_type, "removable": disk.removable,
    })).collect::<Vec<_>>()})
}

impl GuiApp {
    pub(super) fn ui_diagnostics(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        ui.heading(text("Device discovery and diagnostics", "设备发现与诊断"));
        ui.add_enabled_ui(!self.busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!self.device_scan_in_progress, egui::Button::new(text("Scan ArtInChip USB", "扫描 ArtInChip USB"))).clicked() {
                    self.start_task(|| AicDevice::scan_selectable_devices().map(WorkerEvent::UsbScan));
                }
                if ui.button(text("List all USB devices", "列出所有 USB 设备")).clicked() {
                    self.start_task(|| {
                        let devices = AicDevice::list_usb_devices()?;
                        Ok(WorkerEvent::Structured(json!({"devices": devices.iter().map(|device| json!({
                            "bus": device.bus_number, "address": device.address, "path": device.port_path,
                            "vid": format!("{:04x}", device.vendor_id), "pid": format!("{:04x}", device.product_id),
                            "speed": device.speed, "class": device.class_code, "subclass": device.subclass_code,
                            "protocol": device.protocol_code,
                        })).collect::<Vec<_>>()})))
                    });
                }
                if ui.button(text("List serial ports", "列出串口")).clicked() {
                    self.start_task(|| Ok(WorkerEvent::Structured(serial_json(&UartDevice::list_ports()?))));
                }
                if ui.button(self.t(Msg::EnvCheck)).clicked() {
                    let image = self.config.image_path.clone();
                    self.start_task(move || Ok(WorkerEvent::ToolOutput(standalone::environment_report(image.as_deref()))));
                }
                if ui.button(self.t(Msg::Driver)).clicked() {
                    let message = self.t(Msg::StartedDriverInstaller);
                    self.start_task(move || {
                        standalone::install_driver()?;
                        Ok(WorkerEvent::ToolOutput(message.into()))
                    });
                }
                if ui.button(self.t(Msg::SdList)).clicked() {
                    self.start_sd_list();
                }
                if ui.add_enabled(self.image_summary.is_some(), egui::Button::new(text("Offline burn plan", "离线烧录计划"))).clicked() {
                    self.inspect_burn_plan();
                }
                if ui.button(text("Request ADB upgrade mode", "通过 ADB 请求进入升级模式")).clicked() {
                    let directory = self.config.aiburn_dir.clone();
                    let executable = self.config.adb_executable.clone();
                    self.start_task(move || artinchip_flash_app::adb::enter_upgrade(executable.as_deref(), &directory).map(WorkerEvent::ToolOutput));
                }
            });
        });
        ui.label(text(
            "Physical disk enumeration is read-only.",
            "物理磁盘枚举为只读操作。",
        ));
        self.ui_device_selection(ui);
        self.ui_storage_tools(ui);
        self.ui_structured(ui);
    }

    pub(super) fn ui_structured(&mut self, ui: &mut egui::Ui) {
        let Some(value) = self.structured.as_ref() else {
            return;
        };
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        let mut save = false;
        egui::CollapsingHeader::new(text("Latest structured result", "最近一次结构化结果"))
            .default_open(true)
            .show(ui, |ui| {
                let json = serde_json::to_string_pretty(value).expect("JSON value is serializable");
                ui.horizontal_wrapped(|ui| {
                    if ui.button(text("Copy JSON", "复制 JSON")).clicked() {
                        ui.ctx().copy_text(json.clone());
                    }
                    save = ui
                        .add_enabled(
                            !self.busy,
                            egui::Button::new(text("Export JSON", "导出 JSON")),
                        )
                        .clicked();
                });
                egui::ScrollArea::both().max_height(220.0).show(ui, |ui| {
                    ui.monospace(json);
                });
            });
        if save {
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("result.json")
                .save_file()
            {
                let value = value.clone();
                self.start_task(move || {
                    std::fs::write(
                        &path,
                        serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                    Ok(WorkerEvent::ToolOutput(path.display().to_string()))
                });
            }
        }
    }
}
