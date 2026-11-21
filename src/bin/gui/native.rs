use super::*;
use burn::Target;
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Command {
    #[default]
    Read,
    Write,
    Readl,
    Writel,
    Hexdump,
    Fill,
    Clear,
    Memtest,
    Exec,
    Shcmd,
    Log,
}

impl Command {
    const ALL: [Self; 11] = [
        Self::Read,
        Self::Write,
        Self::Readl,
        Self::Writel,
        Self::Hexdump,
        Self::Fill,
        Self::Clear,
        Self::Memtest,
        Self::Exec,
        Self::Shcmd,
        Self::Log,
    ];

    fn label(self, lang: Language) -> &'static str {
        let (en, zh) = match self {
            Self::Read => ("read · Save memory", "read · 保存内存"),
            Self::Write => ("write · Write memory file", "write · 写入文件到内存"),
            Self::Readl => ("readl · Read word", "readl · 读取 32 位字"),
            Self::Writel => ("writel · Write word", "writel · 写入 32 位字"),
            Self::Hexdump => ("hexdump · View memory", "hexdump · 查看内存"),
            Self::Fill => ("fill · Fill bytes", "fill · 填充字节"),
            Self::Clear => ("clear · Zero memory", "clear · 清零内存"),
            Self::Memtest => ("memtest · Test and restore RAM", "memtest · 测试并恢复 RAM"),
            Self::Exec => ("exec · Execute address", "exec · 执行指定地址"),
            Self::Shcmd => ("shcmd · Bootloader shell", "shcmd · Bootloader 命令"),
            Self::Log => ("log · Read device log", "log · 读取设备日志"),
        };
        bilingual(lang, en, zh)
    }

    fn mutates(self) -> bool {
        matches!(
            self,
            Self::Write
                | Self::Writel
                | Self::Fill
                | Self::Clear
                | Self::Memtest
                | Self::Exec
                | Self::Shcmd
        )
    }
}

#[derive(Clone, Default)]
pub(super) struct NativeForm {
    command: Command,
    address: String,
    length: String,
    value: String,
    skip: String,
    rounds: String,
    shell: String,
    file: PathBuf,
    pub(super) pending: Option<Box<Pending>>,
}

#[derive(Clone)]
pub(super) struct Pending {
    target: Target,
    form: NativeForm,
    address: u32,
    length: u32,
    value: u32,
    rounds: u32,
    payload: Vec<u8>,
}

fn range_end(address: u32, length: u32) -> Result<u32, String> {
    let last = length
        .checked_sub(1)
        .ok_or("Length must be greater than zero")?;
    address
        .checked_add(last)
        .ok_or_else(|| "Address range overflows 32 bits".into())
}

fn optional_number(input: &str, default: u32) -> Result<u32, String> {
    if input.trim().is_empty() {
        Ok(default)
    } else {
        services::parse_u32(input)
    }
}

impl Pending {
    fn prepare(form: NativeForm, target: Target) -> Result<Self, String> {
        let command = form.command;
        let address = if matches!(command, Command::Shcmd | Command::Log) {
            0
        } else {
            services::parse_u32(&form.address)?
        };
        let value = if matches!(command, Command::Fill | Command::Writel) {
            services::parse_u32(&form.value)?
        } else {
            0
        };
        if command == Command::Fill && value > u8::MAX.into() {
            return Err("Fill byte must be 0–255".into());
        }
        let rounds = if command == Command::Memtest {
            optional_number(&form.rounds, 1)?
        } else {
            1
        };
        if rounds == 0 {
            return Err("Rounds must be greater than zero".into());
        }
        let mut payload = Vec::new();
        let length = match command {
            Command::Read
            | Command::Hexdump
            | Command::Fill
            | Command::Clear
            | Command::Memtest => services::parse_u32(&form.length)?,
            Command::Readl | Command::Writel => 4,
            Command::Write => {
                let mut input = std::fs::File::open(&form.file)
                    .map_err(|error| format!("{}: {error}", form.file.display()))?;
                let size = input.metadata().map_err(|error| error.to_string())?.len();
                let skip = u64::from(optional_number(&form.skip, 0)?);
                let available = size
                    .checked_sub(skip)
                    .ok_or("Skip exceeds input file size")?;
                let count = available.min(u64::from(optional_number(&form.length, u32::MAX)?));
                let length = u32::try_from(count).map_err(|error| error.to_string())?;
                range_end(address, length)?;
                input
                    .seek(SeekFrom::Start(skip))
                    .map_err(|error| error.to_string())?;
                input
                    .take(count)
                    .read_to_end(&mut payload)
                    .map_err(|error| error.to_string())?;
                if payload.len() as u64 != count {
                    return Err("Input file changed during preview".into());
                }
                length
            }
            Command::Exec | Command::Shcmd | Command::Log => 0,
        };
        if !matches!(command, Command::Exec | Command::Shcmd | Command::Log) {
            range_end(address, length)?;
        }
        if command == Command::Read && form.file.as_os_str().is_empty() {
            return Err("Choose an output file".into());
        }
        if command == Command::Shcmd {
            if form.shell.trim().is_empty() || form.shell.contains('\0') {
                return Err("Enter a nonempty shell command without NUL bytes".into());
            }
            artinchip_flash_app::protocol::commands::shell_request(&form.shell)?;
        }
        Ok(Self {
            target,
            form,
            address,
            length,
            value,
            rounds,
            payload,
        })
    }

    fn execute(self) -> Result<WorkerEvent, String> {
        let mut dev = self.target.open()?;
        let address = self.address;
        let len = self.length;
        let output = match self.form.command {
            Command::Read => {
                let bytes = dev.read_memory(address, len)?;
                std::fs::write(&self.form.file, &bytes)
                    .map_err(|error| format!("{}: {error}", self.form.file.display()))?;
                format!(
                    "Read {} bytes from {address:#010x} → {}",
                    bytes.len(),
                    self.form.file.display()
                )
            }
            Command::Write => {
                dev.write_memory(address, &self.payload)?;
                format!("Wrote {len} bytes at {address:#010x}")
            }
            Command::Readl => {
                let bytes = dev.read_memory(address, 4)?;
                let word =
                    u32::from_le_bytes(bytes.try_into().map_err(|_| "Expected exactly 4 bytes")?);
                format!("{address:#010x}: {word:#010x}")
            }
            Command::Writel => {
                dev.write_memory(address, &self.value.to_le_bytes())?;
                format!("Wrote {:#010x} at {address:#010x}", self.value)
            }
            Command::Hexdump => services::format_hexdump(address, &dev.read_memory(address, len)?),
            Command::Fill | Command::Clear => {
                dev.fill_memory(address, len, self.value as u8)?;
                format!(
                    "Filled {len} bytes at {address:#010x} with {:#04x}",
                    self.value
                )
            }
            Command::Memtest => {
                dev.memtest_memory(address, len, self.rounds)?;
                format!(
                    "Tested, restored and verified {len} bytes at {address:#010x} ({} rounds)",
                    self.rounds
                )
            }
            Command::Exec => {
                dev.exec_address(address)?;
                format!("EXEC accepted at {address:#010x}")
            }
            Command::Shcmd => {
                dev.run_shell(&self.form.shell)?;
                format!("Shell command accepted: {}", self.form.shell)
            }
            Command::Log => dev.get_device_log()?,
        };
        Ok(WorkerEvent::NativeResult(output))
    }
}

impl GuiApp {
    pub(super) fn ui_native(&mut self, ui: &mut egui::Ui) {
        self.ui_device_selection(ui);
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        let browse = self.t(Msg::Browse);
        ui.add_space(6.0);
        let mut run = false;
        ui.add_enabled_ui(!self.busy, |ui| {
            let form = &mut self.native_form;
            egui::ComboBox::from_id_salt("native_command")
                .selected_text(form.command.label(lang))
                .show_ui(ui, |ui| {
                    for command in Command::ALL {
                        if ui
                            .selectable_value(&mut form.command, command, command.label(lang))
                            .changed()
                        {
                            form.file.clear();
                        }
                    }
                });
            let command = form.command;
            if !matches!(command, Command::Shcmd | Command::Log) {
                ui.label(text(
                    "Address (hex, decimal, k/m)",
                    "地址（十六进制、十进制、k/m）",
                ));
                ui.text_edit_singleline(&mut form.address);
            }
            if matches!(
                command,
                Command::Read
                    | Command::Write
                    | Command::Hexdump
                    | Command::Fill
                    | Command::Clear
                    | Command::Memtest
            ) {
                ui.label(if command == Command::Write {
                    text("Maximum byte count (optional)", "最大字节数（可选）")
                } else {
                    text("Byte count", "字节数")
                });
                ui.text_edit_singleline(&mut form.length);
            }
            if matches!(command, Command::Fill | Command::Writel) {
                ui.label(text("Value", "写入值"));
                ui.text_edit_singleline(&mut form.value);
            }
            if command == Command::Write {
                ui.label(text(
                    "Input skip in bytes (optional)",
                    "跳过输入字节数（可选）",
                ));
                ui.text_edit_singleline(&mut form.skip);
            }
            if matches!(command, Command::Read | Command::Write) {
                if ui.button(browse).clicked() {
                    let dialog = rfd::FileDialog::new();
                    if let Some(path) = if command == Command::Read {
                        dialog.save_file()
                    } else {
                        dialog.pick_file()
                    } {
                        form.file = path;
                    }
                }
                let file = form.file.display().to_string();
                ui.add(egui::Label::new(&file).truncate())
                    .on_hover_text(file);
            }
            if command == Command::Memtest {
                ui.label(text(
                    "Rounds (default 1). Keep bootloader and reserved RAM out of the range.",
                    "轮次（默认 1）。测试范围须避开 Bootloader 和保留 RAM。",
                ));
                ui.text_edit_singleline(&mut form.rounds);
            }
            if command == Command::Shcmd {
                ui.label(text(
                    "Bootloader shell command (1–127 bytes)",
                    "Bootloader 命令（1–127 字节）",
                ));
                ui.text_edit_singleline(&mut form.shell);
            }
            run = ui
                .button(if command.mutates() {
                    text("Review operation", "预览操作")
                } else {
                    text("Run", "运行")
                })
                .clicked();
        });
        if run {
            self.native_form.pending = None;
            let form = self.native_form.clone();
            match self.target(form.command.mutates()) {
                Ok(target) => {
                    self.stop_monitor();
                    self.start_task(move || {
                        let pending = Pending::prepare(form, target)?;
                        if pending.form.command.mutates() {
                            Ok(WorkerEvent::NativePreview(Box::new(pending)))
                        } else {
                            pending.execute()
                        }
                    });
                }
                Err(error) => {
                    self.logs_open = true;
                    self.log_error(error);
                }
            }
        }
        if let Some(result) = &self.native_result {
            ui.separator();
            let mut save = false;
            ui.horizontal_wrapped(|ui| {
                if ui.button(text("Copy result", "复制结果")).clicked() {
                    ui.ctx().copy_text(result.clone());
                }
                save = ui
                    .add_enabled(
                        !self.busy,
                        egui::Button::new(text("Export result", "导出结果")),
                    )
                    .clicked();
            });
            if save {
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name("result.txt")
                    .save_file()
                {
                    let result = result.clone();
                    self.start_task(move || {
                        std::fs::write(&path, result)
                            .map_err(|error| format!("{}: {error}", path.display()))?;
                        Ok(WorkerEvent::ToolOutput(path.display().to_string()))
                    });
                }
            }
            if let Some(result) = &self.native_result {
                let limit = result.floor_char_boundary(result.len().min(64 * 1024));
                ui.monospace(&result[..limit]);
                if result.len() > limit {
                    ui.label(text(
                        "Preview truncated. Export for the full result.",
                        "预览已截断，可导出完整结果。",
                    ));
                }
            }
        }
    }

    pub(super) fn ui_native_preview(&mut self, ctx: &egui::Context) {
        let Some(task) = &self.native_form.pending else {
            return;
        };
        let lang = self.lang();
        let text = |en, zh| bilingual(lang, en, zh);
        let mut open = true;
        let mut confirm = false;
        egui::Window::new(text("Confirm device operation", "确认设备操作"))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.colored_label(egui::Color32::RED, task.form.command.label(lang));
                ui.label(task.target.label());
                if task.length > 0 {
                    let last = range_end(task.address, task.length).expect("validated range");
                    ui.monospace(format!(
                        "{:#010x} – {last:#010x}, {} B",
                        task.address, task.length
                    ));
                }
                match task.form.command {
                    Command::Shcmd => {
                        ui.monospace(&task.form.shell);
                    }
                    Command::Exec => {
                        ui.monospace(format!("{:#010x}", task.address));
                    }
                    Command::Fill | Command::Writel => {
                        ui.monospace(format!("{:#x}", task.value));
                    }
                    Command::Write => {
                        ui.label(task.form.file.display().to_string());
                    }
                    Command::Memtest => {
                        ui.label(format!("{}: {}", text("Rounds", "轮次"), task.rounds));
                    }
                    _ => {}
                }
                confirm = ui
                    .add_enabled(
                        !self.busy,
                        egui::Button::new(text("Confirm and run", "确认并运行")),
                    )
                    .clicked();
            });
        if confirm {
            if let Some(task) = self.native_form.pending.take() {
                self.stop_monitor();
                self.start_task(move || task.execute());
            }
        } else if !open {
            self.native_form.pending = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_range_checks_zero_and_overflow() {
        assert_eq!(range_end(u32::MAX, 1), Ok(u32::MAX));
        assert!(range_end(u32::MAX, 2).is_err());
        assert!(range_end(0, 0).is_err());
        assert_eq!(optional_number("1m", 0), Ok(1 << 20));
    }

    #[test]
    fn every_native_command_has_bilingual_labels() {
        for command in Command::ALL {
            assert_ne!(command.label(Language::En), command.label(Language::ZhCn));
        }
    }
}
