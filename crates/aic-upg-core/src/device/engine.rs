use std::{
    thread,
    time::{Duration, Instant},
};

use super::commands::UpgDevice;
use super::events::{emit, BurnCallback, BurnEvent, BurnReport, ResetOutcome};
use super::options::{is_cancelled, BurnOptions};
use super::plan::{
    classify_components, default_burn_parts, format_plan_lines, plan_burn, BurnPlan, ComponentKind,
    FirmwareComponent,
};
use crate::error::FlashError;
use crate::image::parser::FwHeader;
use crate::log_verbose;
use crate::protocol::commands::{
    FwcMeta, CMD_GET_FWC_BURN_RESULT, CMD_GET_FWC_CRC, CMD_GET_FWC_RUN_RESULT,
};
use crate::transport::{CswPolicy, UpgTransport};

const CHUNK_SIZE: u32 = 1024 * 1024;
const UPDATER_PROBE_DELAY: Duration = Duration::from_millis(30);

fn check_cancelled(
    options: &BurnOptions,
    callback: &mut Option<&mut BurnCallback<'_>>,
) -> Result<(), FlashError> {
    if is_cancelled(options) {
        emit(
            callback,
            BurnEvent::Log("Burn cancelled by user".to_string()),
        );
        return Err(FlashError::Cancelled);
    }
    Ok(())
}

impl<T: UpgTransport> UpgDevice<T> {
    /// Burn the default selection from previously parsed image bytes.
    pub fn burn_image(
        &mut self,
        img_data: &[u8],
        metas: &[FwcMeta],
        _header: &FwHeader,
    ) -> Result<(), String> {
        let options = BurnOptions {
            selected_parts: default_burn_parts(metas),
            ..BurnOptions::default()
        };
        self.burn_image_with_options(img_data, metas, &options, None)
    }

    /// Plan and burn previously parsed components. Use the prepared image API for container validation.
    pub fn burn_image_with_options(
        &mut self,
        img_data: &[u8],
        metas: &[FwcMeta],
        options: &BurnOptions,
        mut callback: Option<&mut BurnCallback<'_>>,
    ) -> Result<(), String> {
        check_cancelled(options, &mut callback).map_err(|error| error.to_string())?;
        let plan = plan_burn(img_data, metas, options)?;
        self.burn_planned(
            img_data,
            metas,
            options,
            &plan,
            options.no_updater,
            callback,
        )
        .map_err(|error| error.to_string())
    }

    pub(crate) fn burn_planned(
        &mut self,
        img_data: &[u8],
        metas: &[FwcMeta],
        options: &BurnOptions,
        plan: &BurnPlan,
        verify_stage: bool,
        mut callback: Option<&mut BurnCallback<'_>>,
    ) -> Result<(), FlashError> {
        self.burn_report = BurnReport::default();
        check_cancelled(options, &mut callback)?;
        if verify_stage {
            let stage = self
                .verify_upgrade_stage(options.no_updater)
                .map_err(|message| FlashError::device("verify upgrade stage", message))?;
            emit(&mut callback, BurnEvent::Log(stage));
            check_cancelled(options, &mut callback)?;
        }
        let classified = classify_components(metas, &options.selected_parts);
        for line in format_plan_lines(plan) {
            emit(&mut callback, BurnEvent::Log(line));
        }
        log_verbose!("{}", format_plan_lines(plan).join("\n"));
        emit(
            &mut callback,
            BurnEvent::Stage("Build component plan".to_string()),
        );

        let total_bytes = plan.total_bytes;
        let mut overall_sent = 0usize;

        // In bootloader upgrade mode the updater payload is neither needed
        // nor accepted reliably; the caller opts in with --no-updater.
        let need_updater = !options.no_updater;
        let updater_count = classified
            .iter()
            .filter(|c| c.kind == ComponentKind::Updater)
            .count();
        if updater_count > 0 && need_updater {
            emit(
                &mut callback,
                BurnEvent::Stage("Send updater components".to_string()),
            );
            let updater_components: Vec<_> = classified
                .iter()
                .filter(|c| c.kind == ComponentKind::Updater)
                .collect();
            let updater_last_index = updater_components.len().saturating_sub(1);
            for (index, component) in updater_components.iter().enumerate() {
                check_cancelled(options, &mut callback)?;
                let executable = component
                    .meta
                    .attr_str()
                    .split(';')
                    .any(|attr| attr == "run");
                let allow_final_response_no_csw =
                    index == updater_last_index || (plan.uses_header_media && executable);
                self.send_component(
                    img_data,
                    options,
                    component,
                    allow_final_response_no_csw,
                    plan.uses_header_media,
                    &mut overall_sent,
                    total_bytes,
                    &mut callback,
                )?;
                if index < updater_last_index {
                    thread::sleep(UPDATER_PROBE_DELAY);
                    emit(
                        &mut callback,
                        BurnEvent::Stage("Probe bootloader between updater components".to_string()),
                    );
                    let probe = self.get_hwinfo().or_else(|error| {
                        if plan.uses_header_media && executable {
                            self.burn_reconnect(options.burn_timeout)?;
                            self.get_hwinfo()
                        } else {
                            Err(error)
                        }
                    });
                    if let Err(e) = probe {
                        return Err(FlashError::device(
                            "probe updater",
                            format!("Bootloader probe between updater components failed: {}", e),
                        ));
                    }
                }
            }
            emit(
                &mut callback,
                BurnEvent::Stage("Wait for bootloader reconnect".to_string()),
            );
            if let Err(e) = self.burn_reconnect(options.burn_timeout) {
                emit(
                    &mut callback,
                    BurnEvent::Log(format!(
                        "Warning: updater reconnect was not observed: {}",
                        e
                    )),
                );
            }
            emit(
                &mut callback,
                BurnEvent::Stage("Probe bootloader after reconnect".to_string()),
            );
            let hwinfo = self.get_hwinfo().map_err(|error| {
                FlashError::device(
                    "probe after reconnect",
                    format!("Bootloader probe after reconnect failed: {}", error),
                )
            })?;
            if plan.uses_header_media && hwinfo.boot_stage() != 1 {
                return Err(FlashError::device(
                    "probe after reconnect",
                    format!(
                        "Expected U-Boot upgrade mode after SPL, got stage {}",
                        hwinfo.boot_stage()
                    ),
                ));
            }
        } else if updater_count > 0 {
            emit(
                &mut callback,
                BurnEvent::Log(
                    "Device is already in bootloader upgrade mode; skipping updater components."
                        .to_string(),
                ),
            );
        } else {
            emit(
                &mut callback,
                BurnEvent::Log(
                    "No updater components found; continuing with target stage on current connection."
                        .to_string(),
                ),
            );
        }

        check_cancelled(options, &mut callback)?;
        emit(
            &mut callback,
            BurnEvent::Stage(format!("Set {}", plan.mode_name())),
        );
        self.set_upg_cfg(plan.mode)
            .map_err(|message| FlashError::device("set upgrade configuration", message))?;

        if let Some(info) = classified
            .iter()
            .find(|c| c.kind == ComponentKind::ImageInfo)
        {
            self.send_component(
                img_data,
                options,
                info,
                false,
                plan.uses_header_media,
                &mut overall_sent,
                total_bytes,
                &mut callback,
            )?;
        } else {
            emit(
                &mut callback,
                BurnEvent::Log("Warning: no image.info component found".to_string()),
            );
        }

        let selected: Vec<_> = classified
            .iter()
            .filter(|c| c.kind == ComponentKind::Target && c.selected)
            .collect();
        if selected.is_empty() {
            return Err(FlashError::InvalidPlan(
                "No selected target components to burn".to_string(),
            ));
        }
        for component in selected {
            check_cancelled(options, &mut callback)?;
            self.send_component(
                img_data,
                options,
                component,
                false,
                plan.uses_header_media,
                &mut overall_sent,
                total_bytes,
                &mut callback,
            )?;
        }

        check_cancelled(options, &mut callback)?;
        emit(&mut callback, BurnEvent::Stage("End upgrade".to_string()));
        self.set_upg_end()
            .map_err(|message| FlashError::device("end upgrade", message))?;
        self.burn_report.upgrade_ended = true;
        // Force upgrade and post-burn reset are mutually exclusive: the
        // device stays in upgrade mode so the forced image can be verified.
        let do_reset = options.reset_after_burn && !options.force_upgrade;
        if options.force_upgrade {
            emit(
                &mut callback,
                BurnEvent::Log("Force upgrade: skipping reset (mutually exclusive)".to_string()),
            );
        }
        if do_reset {
            emit(&mut callback, BurnEvent::Stage("Reset device".to_string()));
            self.burn_report.reset = self.reset_for_burn();
            let warning = match &self.burn_report.reset {
                ResetOutcome::Failed(error) => Some(format!("Warning: reset failed: {error}")),
                ResetOutcome::Unconfirmed(error) => {
                    Some(format!("Warning: reset unconfirmed: {error}"))
                }
                _ => None,
            };
            if let Some(warning) = warning {
                emit(&mut callback, BurnEvent::Log(warning));
            }
        } else {
            self.burn_report.reset = ResetOutcome::Skipped;
        }
        emit(&mut callback, BurnEvent::Finished);

        Ok(())
    }

    fn burn_reconnect(&mut self, timeout: Duration) -> Result<(), String> {
        let started = Instant::now();
        let result = self.transport.reconnect(timeout);
        self.burn_report.reconnect_time += started.elapsed();
        result
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_component(
        &mut self,
        img_data: &[u8],
        options: &BurnOptions,
        component: &FirmwareComponent<'_>,
        allow_final_no_csw: bool,
        uses_header_media: bool,
        overall_sent: &mut usize,
        overall_total: usize,
        callback: &mut Option<&mut BurnCallback<'_>>,
    ) -> Result<(), FlashError> {
        let meta = component.meta;
        let name = meta.name_str();
        let size = meta.size_val() as usize;
        let offset = meta.offset_val() as usize;
        let crc_expected = meta.crc_val();
        let end = offset
            .checked_add(size)
            .ok_or_else(|| FlashError::InvalidPlan(format!("{} offset/size overflow", name)))?;
        if end > img_data.len() {
            return Err(FlashError::InvalidPlan(format!(
                "{} image range out of bounds: offset={:#x}, size={}, image_len={}",
                name,
                offset,
                size,
                img_data.len()
            )));
        }

        emit(
            callback,
            BurnEvent::ComponentStarted {
                name: name.to_string(),
                partition: meta.partition_str().to_string(),
                size,
            },
        );
        emit(
            callback,
            BurnEvent::Log(format!(
                "Meta: {} (partition={}, offset={:#x}, size={}, crc=0x{:08x})",
                name,
                meta.partition_str(),
                offset,
                size,
                crc_expected
            )),
        );

        self.set_fwc_meta(meta)
            .map_err(|message| FlashError::device("set component META", message))?;

        let block_size = if uses_header_media {
            self.get_block_size()
                .map_err(|message| FlashError::device("get block size", message))?
        } else {
            self.get_block_size().unwrap_or(2048)
        };
        if uses_header_media && !(1..=CHUNK_SIZE).contains(&block_size) {
            return Err(FlashError::device(
                "get block size",
                format!("{}: invalid device block size {}", name, block_size),
            ));
        }
        emit(
            callback,
            BurnEvent::Log(format!("Block size: {}", block_size)),
        );

        let mut data_sent = 0usize;
        let base_chunk = if component.kind == ComponentKind::Updater {
            (block_size as usize).saturating_mul(512).max(512)
        } else {
            CHUNK_SIZE as usize
        };
        let transport_chunk = self.transport.max_write_chunk(block_size).max(1);
        let streams_blocks = uses_header_media
            && component.kind == ComponentKind::Target
            && meta.attr_str().split(';').any(|attr| attr == "ubi");
        let alignment = if uses_header_media && !streams_blocks {
            block_size as usize
        } else {
            1
        };
        let mut chunk_max = base_chunk.min(transport_chunk);
        chunk_max -= chunk_max % alignment;
        if chunk_max == 0 {
            return Err(FlashError::device(
                "calculate chunk size",
                format!(
                    "{}: device block size exceeds the transport chunk limit",
                    name
                ),
            ));
        }
        log_verbose!(
            "    Write chunk: {} bytes ({})",
            chunk_max,
            self.transport.transport_name()
        );
        self.start_fwc_data(size)
            .map_err(|message| FlashError::device("start component transfer", message))?;
        while data_sent < size {
            check_cancelled(options, callback)?;
            let chunk_end = (data_sent + chunk_max).min(size);
            let chunk_offset = offset + data_sent;
            let chunk_size = chunk_end - data_sent;
            let chunk_data = &img_data[chunk_offset..chunk_offset + chunk_size];

            let started = Instant::now();
            let result = if chunk_size.is_multiple_of(alignment) {
                self.write_fwc_data_chunk(chunk_data, CswPolicy::Required)
            } else {
                let mut padded = chunk_data.to_vec();
                padded.resize(chunk_size.next_multiple_of(alignment), 0);
                self.write_fwc_data_chunk(&padded, CswPolicy::Required)
            };
            self.burn_report.transfer_time += started.elapsed();
            result.map_err(|message| FlashError::device("write component data", message))?;
            self.burn_report.bytes_sent += chunk_size;
            data_sent += chunk_size;
            *overall_sent += chunk_size;
            log_verbose!(
                "    {}: {}/{} ({:.1}%)",
                name,
                data_sent,
                size,
                (data_sent as f64 / size as f64) * 100.0
            );
            emit(
                callback,
                BurnEvent::ComponentProgress {
                    name: name.to_string(),
                    sent: data_sent,
                    total: size,
                },
            );
            emit(
                callback,
                BurnEvent::OverallProgress {
                    sent: *overall_sent,
                    total: overall_total,
                },
            );
        }

        let finish_policy = if allow_final_no_csw {
            CswPolicy::AllowMissing
        } else {
            CswPolicy::Required
        };
        self.finish_fwc_data(finish_policy)
            .map_err(|message| FlashError::device("finish component transfer", message))?;

        let actual_crc = crc32fast::hash(&img_data[offset..end]);
        if actual_crc != crc_expected {
            return Err(FlashError::InvalidPlan(format!(
                "{}: CRC mismatch (expected 0x{:08x}, actual 0x{:08x})",
                name, crc_expected, actual_crc
            )));
        }
        if uses_header_media && !allow_final_no_csw {
            let device_crc = self
                .fwc_result(CMD_GET_FWC_CRC)
                .map_err(|message| FlashError::device("read component CRC", message))?;
            if device_crc != crc_expected {
                return Err(FlashError::DeviceCrc {
                    component: name.into(),
                    expected: crc_expected,
                    actual: device_crc,
                });
            }
            self.burn_report.device_crcs_verified += 1;
            for (cmd, action) in [
                (CMD_GET_FWC_BURN_RESULT, "burn"),
                (CMD_GET_FWC_RUN_RESULT, "run"),
            ] {
                let result = self
                    .fwc_result(cmd)
                    .map_err(|message| FlashError::device("read component result", message))?;
                if result != 0 {
                    return Err(FlashError::device(
                        "check component result",
                        format!("{}: device {} result {:#x}", name, action, result),
                    ));
                }
            }
        }
        self.burn_report.components_completed += 1;
        emit(
            callback,
            BurnEvent::Log(format!("CRC OK (0x{:08x})", actual_crc)),
        );
        emit(
            callback,
            BurnEvent::ComponentFinished {
                name: name.to_string(),
            },
        );
        Ok(())
    }

    fn fwc_result(&mut self, cmd: u8) -> Result<u32, String> {
        let response = self.cmd_hdr_resp(cmd, 4)?;
        let bytes: [u8; 4] = response.payload.as_slice().try_into().map_err(|_| {
            format!(
                "Command 0x{:02x} returned an invalid FWC result length",
                cmd
            )
        })?;
        Ok(u32::from_le_bytes(bytes))
    }
}
