//! Firmware preparation and native burn orchestration shared by both frontends.

use std::path::PathBuf;
use std::time::Instant;

use artinchip_flash::{FirmwareImage, FlashError, PreparedFlash};

use crate::device::{validate_erase_plan, BurnEvent, BurnOptions, BurnReport, UpgDevice};
use crate::official;
use crate::services::DeviceTarget;
use crate::transport::UpgTransport;
use crate::usb::device::{AicDevice, DeviceSelector};

pub fn prepare(
    image: &FirmwareImage,
    options: BurnOptions,
    erase_all: bool,
) -> Result<PreparedFlash<'_>, FlashError> {
    let flash = image.prepare(options)?;
    validate_erase_plan(flash.plan(), erase_all).map_err(FlashError::InvalidPlan)?;
    Ok(flash)
}

pub struct EraseRequest {
    pub tool: PathBuf,
    pub media: Option<String>,
    pub image: Option<PathBuf>,
    pub uart_baudrate: Option<u32>,
}

pub enum JobEvent {
    Output(String),
    Progress(BurnEvent),
}

#[derive(Debug)]
pub enum JobError {
    PreErase(String),
    Connect(String),
    Upgrade(FlashError),
}

impl std::fmt::Display for JobError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreErase(message) | Self::Connect(message) => formatter.write_str(message),
            Self::Upgrade(error) => error.fmt(formatter),
        }
    }
}

pub struct FlashJob<'job, 'image> {
    pub flash: &'job PreparedFlash<'image>,
    pub target: DeviceTarget,
    pub erase: Option<EraseRequest>,
    pub report_device_info: bool,
}

impl FlashJob<'_, '_> {
    pub fn run(self, emit: impl FnMut(JobEvent) + Send) -> Result<(), JobError> {
        self.run_reported(&mut BurnReport::default(), emit)
    }

    pub fn run_reported(
        self,
        report: &mut BurnReport,
        mut emit: impl FnMut(JobEvent) + Send,
    ) -> Result<(), JobError> {
        *report = BurnReport::default();
        self.flash.check_cancelled().map_err(JobError::Upgrade)?;
        let target = match &self.erase {
            Some(erase) => {
                validate_erase_plan(self.flash.plan(), true).map_err(JobError::PreErase)?;
                if !erase.tool.is_file() {
                    return Err(JobError::PreErase(format!(
                        "Pre-burn erase needs an existing upgcmd executable: {}",
                        erase.tool.display()
                    )));
                }
                pin_erase_target(self.target).map_err(JobError::Connect)?
            }
            None => self.target,
        };

        self.flash.check_cancelled().map_err(JobError::Upgrade)?;
        if !self.report_device_info {
            emit(JobEvent::Output(target.label()));
        } else {
            emit(JobEvent::Progress(BurnEvent::Stage(
                "Connect to target".into(),
            )));
        }
        let mut device = Self::open(self.flash, &target, report)?;
        if let Some(erase) = self.erase {
            let stage = device
                .verify_upgrade_stage(self.flash.options().no_updater)
                .map_err(|message| {
                    JobError::Upgrade(FlashError::Device {
                        operation: "verify upgrade stage before erase",
                        message,
                    })
                })?;
            emit(JobEvent::Output(stage));
            let media_type = self.flash.header().media_type_str();
            let media_id = erase
                .media
                .as_deref()
                .map(str::trim)
                .filter(|media| !media.is_empty())
                .map_or(Ok(self.flash.header().media_dev_id()), |media| {
                    media.parse::<u32>()
                })
                .map_err(|_| {
                    JobError::PreErase("Erase media ID must be a decimal integer".into())
                })?;
            if !device
                .list_storage_media()
                .map_err(|message| {
                    JobError::Upgrade(FlashError::Device {
                        operation: "query storage media before erase",
                        message,
                    })
                })?
                .iter()
                .any(|entry| entry.media_dev_id == media_id && entry.media_type == media_type)
            {
                return Err(JobError::PreErase(format!(
                    "Erase target not reported by device: {media_type} {media_id}"
                )));
            }
            self.flash.check_cancelled().map_err(JobError::Upgrade)?;
            drop(device);
            let (usb, uart) = erase_selectors(&target).map_err(JobError::PreErase)?;
            let baud = erase
                .uart_baudrate
                .map(|rate| rate.to_string())
                .unwrap_or_default();
            emit(JobEvent::Output(format!(
                "Full-chip erase, media {media_id}"
            )));
            self.flash.check_cancelled().map_err(JobError::Upgrade)?;
            let output = official::run_pre_burn_erase(
                &erase.tool,
                &media_id.to_string(),
                erase.image.as_deref(),
                &usb,
                &uart,
                &baud,
            )
            .map_err(|error| JobError::PreErase(format!("Pre-burn erase failed: {error}")))?;
            emit(JobEvent::Output(output));
            self.flash.check_cancelled().map_err(JobError::Upgrade)?;
            if self.report_device_info {
                emit(JobEvent::Progress(BurnEvent::Stage(
                    "Reconnect after pre-burn erase".into(),
                )));
            }
            device = Self::open(self.flash, &target, report)?;
        }
        self.flash.check_cancelled().map_err(JobError::Upgrade)?;
        if self.report_device_info {
            emit(JobEvent::Output(format!(
                "Transport: {}",
                device.transport_name()
            )));
            match device.device_info_lines() {
                Ok(lines) => lines
                    .into_iter()
                    .for_each(|line| emit(JobEvent::Output(format!("  {line}")))),
                Err(error) => emit(JobEvent::Output(format!(
                    "Warning: could not read device info: {error}"
                ))),
            }
        }
        let result = self
            .flash
            .execute(
                &mut device,
                Some(&mut |event| emit(JobEvent::Progress(event))),
            )
            .map_err(JobError::Upgrade);
        let connection_time = report.connection_time;
        *report = device.burn_report().clone();
        report.connection_time = connection_time;
        result
    }

    fn open(
        flash: &PreparedFlash<'_>,
        target: &DeviceTarget,
        report: &mut BurnReport,
    ) -> Result<UpgDevice<Box<dyn UpgTransport>>, JobError> {
        let started = Instant::now();
        let result = target.open();
        report.connection_time += started.elapsed();
        flash.check_cancelled().map_err(JobError::Upgrade)?;
        result.map_err(JobError::Connect)
    }
}

fn pin_erase_target(target: DeviceTarget) -> Result<DeviceTarget, String> {
    match &target {
        DeviceTarget::Usb(DeviceSelector::Unique) => {
            Err("Full-chip erase requires an explicit physical target".into())
        }
        DeviceTarget::Usb(selector) => AicDevice::pin_selector(selector).map(DeviceTarget::Usb),
        DeviceTarget::Uart(spec)
            if spec.port.trim().is_empty() || spec.port.trim().eq_ignore_ascii_case("auto") =>
        {
            Err("Full-chip erase requires an explicit UART port".into())
        }
        DeviceTarget::Uart(_) => Ok(target),
    }
}

fn erase_selectors(target: &DeviceTarget) -> Result<(String, String), String> {
    match target {
        DeviceTarget::Usb(DeviceSelector::PhysicalPort(bus, path)) => {
            Ok((format!("{bus}:{path}"), String::new()))
        }
        DeviceTarget::Uart(spec) => Ok((String::new(), spec.port.clone())),
        DeviceTarget::Usb(_) => Err("Full-chip erase requires an explicit physical target".into()),
    }
}
