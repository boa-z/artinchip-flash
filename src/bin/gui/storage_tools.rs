use super::*;
use artinchip_flash_app::protocol::commands::{StorageGeometry, StorageMedia};
use artinchip_flash_app::storage::{preview_erase, EraseScope};
use serde_json::json;

#[derive(Clone, Default)]
pub(super) struct StorageForm {
    media_type: String,
    media_id: u32,
    total: u64,
    alignment: u32,
    sector_bytes: u32,
    offset: u64,
    length: u64,
    scope: Scope,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Scope {
    #[default]
    Region,
    Partition,
    Whole,
}

#[derive(Clone, Copy)]
enum Query {
    Media,
    Geometry,
    Partitions,
}

impl StorageForm {
    fn media(&self) -> Result<StorageMedia, String> {
        if self.media_type.trim().is_empty()
            || self.media_type.len() >= 64
            || self.media_type.contains('\0')
        {
            return Err("Enter a storage media type shorter than 64 bytes".into());
        }
        Ok(StorageMedia::new(self.media_type.trim(), self.media_id))
    }
}

impl GuiApp {
    pub(super) fn ui_storage_tools(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        egui::CollapsingHeader::new(text("Storage inspection and erase preview", "存储查询与擦除预览"))
            .show(ui, |ui| {
                ui.label(text("Media listing is read-only. Geometry probing is experimental and requires U-Boot stage 1.", "介质枚举为只读操作。几何查询为实验功能，要求 U-Boot 阶段 1。"));
                ui.horizontal_wrapped(|ui| {
                    ui.label(text("Media type / ID", "介质类型 / 编号"));
                    ui.add(egui::TextEdit::singleline(&mut self.storage_form.media_type).hint_text("spi-nand / spi-nor / mmc").desired_width(150.0));
                    ui.add(egui::DragValue::new(&mut self.storage_form.media_id));
                });
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(!self.busy, egui::Button::new(text("List device storage", "枚举设备存储"))).clicked() {
                        self.start_storage_query(Query::Media);
                    }
                    if ui.add_enabled(!self.busy, egui::Button::new(text("Probe geometry", "查询存储几何"))).clicked() {
                        self.start_storage_query(Query::Geometry);
                    }
                    if ui.add_enabled(!self.busy, egui::Button::new(text("Read partition table", "读取分区表"))).clicked() {
                        self.start_storage_query(Query::Partitions);
                    }
                });
                ui.separator();
                ui.label(text("Offline calculation. Enter capacity and erase group in bytes for Flash, or sectors for MMC. Use physical partition boundaries for partition mode. UBI volume offsets are not physical erase ranges.", "离线计算。Flash 容量与擦除粒度使用字节，MMC 使用扇区。分区模式须填写物理分区边界。UBI 卷偏移不能作为物理擦除范围。"));
                let form = &mut self.storage_form;
                ui.horizontal_wrapped(|ui| {
                    ui.label(text("Capacity", "容量"));
                    ui.add(egui::DragValue::new(&mut form.total));
                    ui.label(text("Erase group", "擦除粒度"));
                    ui.add(egui::DragValue::new(&mut form.alignment));
                    if form.media_type.trim() == "mmc" {
                        ui.label(text("Bytes per sector", "每扇区字节数"));
                        ui.add(egui::DragValue::new(&mut form.sector_bytes));
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.radio_value(&mut form.scope, Scope::Region, text("Region", "区域"));
                    ui.radio_value(&mut form.scope, Scope::Partition, text("Physical partition", "物理分区"));
                    ui.radio_value(&mut form.scope, Scope::Whole, text("Whole media", "整片介质"));
                });
                if form.scope != Scope::Whole {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(text("Offset (bytes)", "偏移（字节）"));
                        ui.add(egui::DragValue::new(&mut form.offset));
                        ui.label(text("Length (bytes)", "长度（字节）"));
                        ui.add(egui::DragValue::new(&mut form.length));
                    });
                }
                if ui.button(text("Calculate range (dry run)", "计算范围（仅预览）")).clicked() {
                    let geometry = StorageGeometry { total_size: form.total, erase_size: form.alignment, write_size: 0, oob_size: 0 };
                    let scope = match form.scope {
                        Scope::Region => EraseScope::Region { offset: form.offset, length: form.length },
                        Scope::Partition => EraseScope::Partition { offset: form.offset, length: form.length },
                        Scope::Whole => EraseScope::WholeMedia,
                    };
                    match form.media().and_then(|media| preview_erase(media, &geometry, Some(form.sector_bytes), scope)) {
                        Ok(plan) => self.structured = Some(json!({
                            "mode": "dry-run", "scope": format!("{:?}", plan.scope), "media": plan.media.media_type,
                            "id": plan.media.media_dev_id, "offset_bytes": plan.offset_bytes, "length_bytes": plan.length_bytes,
                            "capacity_bytes": plan.capacity_bytes, "alignment_bytes": plan.alignment_bytes,
                        })),
                        Err(error) => {
                            self.logs_open = true;
                            self.log_error(error);
                        }
                    }
                }
            });
    }

    fn start_storage_query(&mut self, query: Query) {
        let target = match self.target(false) {
            Ok(target) => target,
            Err(error) => {
                self.logs_open = true;
                self.log_error(error);
                return;
            }
        };
        let requested = if !matches!(query, Query::Media) {
            match self.storage_form.media() {
                Ok(media) => Some(media),
                Err(error) => {
                    self.logs_open = true;
                    self.log_error(error);
                    return;
                }
            }
        } else {
            None
        };
        self.stop_monitor();
        self.start_task(move || {
            let mut dev = target.open()?;
            let stage = dev.verify_upgrade_stage(true)?;
            let media = dev.list_storage_media()?;
            let storage = media.iter().map(|entry| json!({"type": entry.media_type, "id": entry.media_dev_id})).collect::<Vec<_>>();
            let mut result = json!({"target": target.label(), "stage": stage, "storage": storage});
            if let Some(requested) = requested {
                if !media.contains(&requested) {
                    return Err("The selected type and ID were not reported by the target".into());
                }
                if matches!(query, Query::Partitions) {
                    let table = dev.read_partition_table(&requested).map_err(|error| format!("GET_PARTITION_TABLE (0x17): {error}"))?;
                    result["partition_table"] = json!({"type": requested.media_type, "id": requested.media_dev_id, "text": table});
                    return Ok(WorkerEvent::Structured(result));
                }
                let geometry = dev.get_storage_geometry(&requested).map_err(|error| format!("GET_STORAGE_GEOME (0x1A), {} {}: {error}", requested.media_type, requested.media_dev_id))?;
                let unit = if requested.media_type == "mmc" { "sectors (experimental)" } else { "bytes (experimental)" };
                result["geometry"] = json!({"type": requested.media_type, "id": requested.media_dev_id, "unit": unit,
                    "total_size": geometry.total_size, "erase_size": geometry.erase_size, "write_size": geometry.write_size, "oob_size": geometry.oob_size});
            }
            Ok(WorkerEvent::Structured(result))
        });
    }
}
