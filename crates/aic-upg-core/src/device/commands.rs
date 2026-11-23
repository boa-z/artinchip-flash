use std::time::Duration;

use super::events::{BurnReport, ResetOutcome};
use crate::log_verbose;
use crate::protocol::cbw_csw::*;
use crate::protocol::commands::*;
use crate::transport::{CswPolicy, UpgTransport};

/// Host-side memory-op chunk (fill/memtest/restore): one USB transfer each.
const MEM_FILL_CHUNK: u32 = 1024 * 1024;
pub(super) const OFFICIAL_UPG_CFG_RESERVED: [u8; 31] = [
    0xea, 0x00, 0x00, 0xbc, 0xf5, 0x44, 0x04, 0x50, 0xf5, 0x44, 0x04, 0x01, 0x00, 0x00, 0x00, 0x18,
    0x73, 0xdf, 0x05, 0x50, 0xf5, 0x44, 0x04, 0x40, 0xfe, 0xf1, 0x00, 0x18, 0x73, 0xdf, 0x05,
];

#[derive(Clone, Debug)]
pub(super) struct UpgResponse {
    pub(super) payload: Vec<u8>,
}

/// Transport independent UPG device.
///
/// Holds the command/burn protocol; the transport only moves CBW/CSW
/// transactions between host and device.
pub struct UpgDevice<T: UpgTransport> {
    pub(super) transport: T,
    pub(super) burn_report: BurnReport,
}

impl<T: UpgTransport> UpgDevice<T> {
    /// Bind a transport to the UPG command client.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            burn_report: BurnReport::default(),
        }
    }

    /// Measurements and outcomes from the most recent prepared upgrade attempt.
    pub fn burn_report(&self) -> &BurnReport {
        &self.burn_report
    }

    /// Return the diagnostic name supplied by the transport.
    pub fn transport_name(&self) -> &'static str {
        self.transport.transport_name()
    }

    /// Borrow the transport for transport-specific operations.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Box the transport so USB/UART devices share one type
    /// (`UpgDevice<Box<dyn UpgTransport>>`) at CLI call sites.
    pub fn into_boxed(self) -> UpgDevice<Box<dyn UpgTransport>>
    where
        T: 'static,
    {
        UpgDevice {
            transport: Box::new(self.transport),
            burn_report: self.burn_report,
        }
    }

    // ── High-level protocol helpers ────────────────────────────────────

    /// Send only a command header (no payload, no response read).
    /// Used as first step in multi-transaction commands.
    fn send_hdr(&mut self, cmd: u8, data_len: u32) -> Result<(), String> {
        let hdr = CmdHeader::new(cmd, data_len);
        self.transport
            .write_txn(hdr.to_bytes(), CswPolicy::Required)?;
        Ok(())
    }

    fn cmd_hdr_data_resp(
        &mut self,
        cmd: u8,
        payload: &[u8],
        resp_extra: usize,
    ) -> Result<UpgResponse, String> {
        self.cmd_hdr_data_resp_policy(cmd, payload, resp_extra, CswPolicy::Required)
    }

    fn cmd_hdr_data_resp_policy(
        &mut self,
        cmd: u8,
        payload: &[u8],
        resp_extra: usize,
        policy: CswPolicy,
    ) -> Result<UpgResponse, String> {
        self.send_hdr(cmd, payload.len() as u32)?;
        let csw = self.transport.write_txn(payload, policy)?;
        if policy == CswPolicy::AllowMissing && csw.is_none() {
            return Ok(UpgResponse {
                payload: Vec::new(),
            });
        }
        self.read_upg_response(cmd, resp_extra, policy)
    }

    pub(super) fn cmd_hdr_resp(
        &mut self,
        cmd: u8,
        resp_extra: usize,
    ) -> Result<UpgResponse, String> {
        self.send_hdr(cmd, 0)?;
        self.read_upg_response(cmd, resp_extra, CswPolicy::Required)
    }

    pub(super) fn read_upg_response(
        &mut self,
        cmd: u8,
        expected_payload_len: usize,
        policy: CswPolicy,
    ) -> Result<UpgResponse, String> {
        // Official AiBurn reads the 16-byte RESP packet first, then reads the
        // optional data packet separately. Reading header+payload in one CBW
        // makes some bootloaders answer the READ CBW with a failed CSW only.
        let header_data = match self.read_txn_policy(RESP_MIN_HDR_LEN as u32, policy) {
            Ok(data) => data,
            Err(e) if policy == CswPolicy::AllowMissing => {
                log_verbose!("  << No UPG response accepted by transaction policy: {}", e);
                return Ok(UpgResponse {
                    payload: Vec::new(),
                });
            }
            Err(e) => return Err(e),
        };
        let header = self.parse_resp_header(cmd, &header_data)?;
        let declared_len = header.data_length_val() as usize;
        let payload_len = if declared_len > 0 {
            declared_len
        } else {
            expected_payload_len
        };
        let payload = if payload_len > 0 {
            match self.read_txn_policy(payload_len as u32, policy) {
                Ok(data) => data,
                Err(e) if policy == CswPolicy::AllowMissing => {
                    log_verbose!("  << No UPG payload accepted by transaction policy: {}", e);
                    Vec::new()
                }
                Err(e) => return Err(e),
            }
        } else {
            Vec::new()
        };
        Ok(UpgResponse { payload })
    }

    fn read_txn_policy(&mut self, read_len: u32, policy: CswPolicy) -> Result<Vec<u8>, String> {
        self.transport.read_txn(read_len, policy)
    }

    /// Parse the 16-byte UPG response packet.
    fn parse_resp_header(&self, expected_cmd: u8, data: &[u8]) -> Result<RespHeader, String> {
        if data.len() < RESP_MIN_HDR_LEN {
            return Err("Response too short".to_string());
        }
        let resp = RespHeader::from_bytes(data)
            .ok_or_else(|| "Failed to parse response header".to_string())?;
        if !resp.is_ok() {
            return Err(format!(
                "Command 0x{:02x} failed, status: {}, magic=0x{:08x}",
                expected_cmd,
                resp.status_val(),
                resp.magic()
            ));
        }
        if resp.command() != 0 && resp.command() != expected_cmd {
            log_verbose!(
                "  << Warning: response command 0x{:02x} does not match request 0x{:02x}",
                resp.command(),
                expected_cmd
            );
        }
        Ok(resp)
    }

    // ── Public API ─────────────────────────────────────────────────────

    /// Query the UPG identity and current boot stage.
    pub fn get_hwinfo(&mut self) -> Result<HwInfo, String> {
        let resp = self.cmd_hdr_resp(CMD_GET_HWINFO, 104)?;
        HwInfo::from_bytes(&resp.payload).ok_or_else(|| {
            format!(
                "Failed to parse HWINFO ({} payload bytes)",
                resp.payload.len()
            )
        })
    }

    /// Structured device info lines shared by CLI printing and GUI display.
    ///
    /// Previously `show_info()` printed directly, forcing GUI to capture
    /// stdout. Callers now render these lines themselves.
    /// Query the hardware and format each field for display.
    pub fn device_info_lines(&mut self) -> Result<Vec<String>, String> {
        let hwinfo = self.get_hwinfo()?;
        let stage = match hwinfo.boot_stage() {
            0 => "ROM/BROM",
            1 => "U-Boot upgrade",
            _ => "unknown",
        };
        let chipid = hwinfo.chipid_val();
        let mut lines = vec![
            format!("Magic:        {}", hwinfo.magic_str()),
            format!("Init mode:    {:#x}", hwinfo.init_mode()),
            format!("Current mode: {:#x}", hwinfo.curr_mode()),
            format!("Boot stage:   {}", hwinfo.boot_stage()),
            format!("Boot phase:   {}", stage),
            format!(
                "Chip ID:      {:08x} {:08x} {:08x} {:08x}",
                chipid[0], chipid[1], chipid[2], chipid[3]
            ),
        ];
        if let Ok(media) = self.get_storage_media() {
            lines.push(format!("Storage media: {}", media));
        }
        Ok(lines)
    }

    /// Query the hardware and return a multiline summary.
    pub fn device_info_text(&mut self) -> Result<String, String> {
        Ok(self.device_info_lines()?.join("\n"))
    }

    /// Require stage 1 for no-updater operation or stage 0 for ROM operation.
    pub fn verify_upgrade_stage(&mut self, no_updater: bool) -> Result<String, String> {
        let info = self
            .get_hwinfo()
            .map_err(|error| format!("HWINFO probe failed: {error}"))?;
        if info.magic_str() != "HWINFO" {
            return Err(format!(
                "Unrecognized HWINFO signature {:?}; no firmware sent",
                info.magic_str()
            ));
        }
        let expected = u32::from(no_updater);
        let phase = if no_updater {
            "U-Boot upgrade"
        } else {
            "ROM/BROM"
        };
        if info.boot_stage() != expected {
            return Err(format!(
                "Expected {phase} (Boot stage={expected}), got Boot stage={}; no firmware sent",
                info.boot_stage()
            ));
        }
        Ok(format!(
            "{phase}, Boot stage={}, Init mode={:#x}",
            info.boot_stage(),
            info.init_mode()
        ))
    }

    /// Send the upgrade configuration and validate its acknowledgement.
    pub fn set_upg_cfg(&mut self, mode: u8) -> Result<(), String> {
        let mut cfg = [0u8; 32];
        cfg[0] = mode;
        cfg[1..].copy_from_slice(&OFFICIAL_UPG_CFG_RESERVED);
        self.send_hdr(CMD_SET_UPG_CFG, (cfg.len() + 4) as u32)?;
        self.transport
            .write_txn(&(cfg.len() as u32).to_le_bytes(), CswPolicy::Required)?;
        let write_csw = self.transport.write_txn(&cfg, CswPolicy::Required)?;
        let response = self.read_txn_policy(RESP_MIN_HDR_LEN as u32, CswPolicy::Required)?;
        if let Some(csw) = write_csw {
            if csw.matches_padded_ack(&response) {
                log_verbose!("SET_UPG_CFG acknowledged with a copy of the configuration write CSW");
                return Ok(());
            }
        }
        let header = self.parse_resp_header(CMD_SET_UPG_CFG, &response)?;
        if header.data_length_val() != 0 {
            return Err("SET_UPG_CFG returned unexpected response data".to_string());
        }
        Ok(())
    }

    /// Send component metadata to the device.
    pub fn set_fwc_meta(&mut self, meta: &FwcMeta) -> Result<(), String> {
        let _resp = self.cmd_hdr_data_resp(CMD_SET_FWC_META, meta.to_bytes(), 0)?;
        Ok(())
    }

    /// Query the block size required for subsequent component transactions.
    pub fn get_block_size(&mut self) -> Result<u32, String> {
        let resp = self.cmd_hdr_resp(CMD_GET_BLOCK_SIZE, 4)?;
        if resp.payload.len() < 4 {
            return Err(format!(
                "Block size response too short: {} bytes",
                resp.payload.len()
            ));
        }
        let block_size = u32::from_le_bytes(resp.payload[0..4].try_into().unwrap());
        Ok(block_size)
    }

    pub(super) fn start_fwc_data(&mut self, total_len: usize) -> Result<(), String> {
        self.send_hdr(CMD_SEND_FWC_DATA, total_len as u32)
    }

    pub(super) fn write_fwc_data_chunk(
        &mut self,
        chunk: &[u8],
        policy: CswPolicy,
    ) -> Result<(), String> {
        let csw = self.transport.write_txn(chunk, policy)?;
        if policy == CswPolicy::AllowMissing && csw.is_none() {
            let _ = self
                .transport
                .drain_rx(Duration::from_millis(50), 64 * 1024);
        }
        Ok(())
    }

    pub(super) fn finish_fwc_data(&mut self, policy: CswPolicy) -> Result<(), String> {
        let _resp = self.read_upg_response(CMD_SEND_FWC_DATA, 0, policy)?;
        Ok(())
    }

    /// End the upgrade and allow the bootloader to leave the protocol before acknowledging.
    pub fn set_upg_end(&mut self) -> Result<(), String> {
        let mut payload = [0u8; 36];
        payload[0..4].copy_from_slice(&32u32.to_le_bytes());
        let _resp =
            self.cmd_hdr_data_resp_policy(CMD_SET_UPG_END, &payload, 0, CswPolicy::AllowMissing)?;
        Ok(())
    }

    /// Send one command to the bootloader shell.
    pub fn run_shell(&mut self, cmd_str: &str) -> Result<(), String> {
        // Device shell buffer is 128 B and the string must arrive in one
        // packet (basic_cmd.c run_shell_str_cmd_write_input_data).
        let payload = shell_request(cmd_str)?;
        let _resp = self.cmd_hdr_data_resp(CMD_RUN_SHELL_STR, &payload, 0)?;
        Ok(())
    }

    /// Ask the bootloader to reset the device.
    pub fn reset(&mut self) -> Result<(), String> {
        self.run_shell("reset")
    }

    pub(super) fn reset_for_burn(&mut self) -> ResetOutcome {
        let payload = match shell_request("reset") {
            Ok(payload) => payload,
            Err(error) => return ResetOutcome::Failed(error),
        };
        if let Err(error) = self.send_hdr(CMD_RUN_SHELL_STR, payload.len() as u32) {
            return ResetOutcome::Failed(error);
        }
        let response = self
            .transport
            .write_txn(&payload, CswPolicy::Required)
            .and_then(|_| self.read_upg_response(CMD_RUN_SHELL_STR, 0, CswPolicy::Required));
        match response {
            Ok(_) => ResetOutcome::Acknowledged,
            Err(error) => ResetOutcome::Unconfirmed(error),
        }
    }

    /// Query the legacy storage-media text response.
    pub fn get_storage_media(&mut self) -> Result<String, String> {
        let resp = self.cmd_hdr_resp(CMD_GET_STORAGE_MEDIA, 64)?;
        let media = String::from_utf8_lossy(&resp.payload)
            .trim_end_matches('\0')
            .to_string();
        Ok(media)
    }

    /// Parsed `GET_STORAGE_MEDIA` list (68-byte `struct storage_media`
    /// entries). The legacy [`UpgDevice::get_storage_media`] string form
    /// stays for existing callers.
    pub fn list_storage_media(&mut self) -> Result<Vec<StorageMedia>, String> {
        let bytes =
            self.read_storage_response(CMD_GET_STORAGE_MEDIA, &[], 5 * STORAGE_MEDIA_SIZE)?;
        if !bytes.len().is_multiple_of(STORAGE_MEDIA_SIZE) {
            return Err(format!(
                "Invalid storage list length: {} bytes",
                bytes.len()
            ));
        }
        let media = StorageMedia::parse_list(&bytes);
        if media.iter().any(|entry| entry.media_type.is_empty()) {
            return Err("Storage list contains an invalid media type".into());
        }
        Ok(media)
    }

    fn read_storage_response(
        &mut self,
        cmd: u8,
        request: &[u8],
        max: usize,
    ) -> Result<Vec<u8>, String> {
        self.send_hdr(cmd, request.len() as u32)?;
        if !request.is_empty() {
            self.transport.write_txn(request, CswPolicy::Required)?;
        }
        let header = self
            .transport
            .read_txn(RESP_MIN_HDR_LEN as u32, CswPolicy::Required)?;
        let declared = self.parse_resp_header(cmd, &header)?.data_length_val();
        if declared as usize > max {
            return Err(format!(
                "Storage command {cmd:#04x} returned {declared} bytes, maximum {max}"
            ));
        }
        if declared == 0 {
            return Ok(Vec::new());
        }
        let bytes = self.transport.read_txn(declared, CswPolicy::Required)?;
        if bytes.len() != declared as usize {
            return Err(format!(
                "Storage command {cmd:#04x} returned {} bytes, expected {declared}",
                bytes.len()
            ));
        }
        Ok(bytes)
    }

    pub fn read_partition_table(&mut self, media: &StorageMedia) -> Result<String, String> {
        if !matches!(media.media_type.as_str(), "mmc" | "spi-nor" | "spi-nand") {
            return Err("Partition table query requires mmc, spi-nor or spi-nand".into());
        }
        let bytes = self.read_storage_response(CMD_GET_PARTITION_TABLE, &media.to_bytes(), 128)?;
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        if end == 0 {
            return Err("Bootloader reported an empty partition table".into());
        }
        String::from_utf8(bytes[..end].to_vec())
            .map_err(|error| format!("Invalid partition table text: {error}"))
    }

    /// `WRITE` (0x02): write `data` to device memory at `addr`.
    pub fn write_memory(&mut self, addr: u32, data: &[u8]) -> Result<(), String> {
        let payload = write_mem_request(addr, data);
        let _resp = self.cmd_hdr_data_resp(CMD_WRITE, &payload, 0)?;
        Ok(())
    }

    /// `READ` (0x03): read `len` bytes of device memory at `addr`.
    pub fn read_memory(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, String> {
        let payload = read_mem_request(addr, len);
        let resp = self.cmd_hdr_data_resp(CMD_READ, &payload, len as usize)?;
        if resp.payload.len() != len as usize {
            return Err(format!(
                "READ returned {} bytes, expected {}",
                resp.payload.len(),
                len
            ));
        }
        Ok(resp.payload)
    }

    /// `EXEC` (0x04): call the function at `addr` (no arguments).
    pub fn exec_address(&mut self, addr: u32) -> Result<(), String> {
        let payload = exec_request(addr);
        let _resp = self.cmd_hdr_data_resp(CMD_EXEC, &payload, 0)?;
        Ok(())
    }

    /// Fill `[addr, addr+len)` with the byte `value` (official `fill`
    /// semantics: "memory set the area with byte value").
    ///
    /// No dedicated UPG fill command exists in the public SDK, so this loops
    /// `WRITE` (0x02) in 1 MiB chunks (single USB transfer each; the device
    /// accumulates multi-packet writes).
    pub fn fill_memory(&mut self, addr: u32, len: u32, value: u8) -> Result<(), String> {
        let end = addr
            .checked_add(len)
            .ok_or_else(|| format!("fill range overflow: {:#x} + {:#x}", addr, len))?;
        if len == 0 {
            return Err("fill length must be > 0".to_string());
        }
        let mut offset = addr;
        while offset < end {
            let chunk = (end - offset).min(MEM_FILL_CHUNK) as usize;
            self.write_memory(offset, &vec![value; chunk])?;
            offset += chunk as u32;
        }
        Ok(())
    }

    /// Fill `[addr, addr+len)` with the 4-byte LE `value` pattern, chunked.
    /// Word-pattern counterpart of [`UpgDevice::fill_memory`], used by the
    /// memory test (official `fill` itself is byte-oriented).
    fn fill_words(&mut self, addr: u32, len: u32, value: u32) -> Result<(), String> {
        let end = addr
            .checked_add(len)
            .ok_or_else(|| format!("fill range overflow: {:#x} + {:#x}", addr, len))?;
        let pattern = value.to_le_bytes();
        let mut offset = addr;
        while offset < end {
            let chunk = (end - offset).min(MEM_FILL_CHUNK) as usize;
            let mut data = Vec::with_capacity(chunk);
            while data.len() < chunk {
                let take = (chunk - data.len()).min(4);
                data.extend_from_slice(&pattern[..take]);
            }
            self.write_memory(offset, &data)?;
            offset += chunk as u32;
        }
        Ok(())
    }

    /// Host-side memory test over `READ`/`WRITE`: saves the original content,
    /// writes + verifies each pattern (`FFFFFFFF/00000000/AAAAAAAA/55555555`,
    /// repeated `rounds` times), then restores and re-verifies the original.
    ///
    /// There is no `memtest` UPG command in the public SDK (`mtest` exists
    /// only as a U-Boot shell word, not a protocol handler), so the pattern
    /// loop runs on the host. Destructive by nature: avoid bootloader-reserved
    /// RAM, and expect a reboot if the device crashes mid-test.
    pub fn memtest_memory(&mut self, addr: u32, len: u32, rounds: u32) -> Result<(), String> {
        let _end = addr
            .checked_add(len)
            .ok_or_else(|| format!("memtest range overflow: {:#x} + {:#x}", addr, len))?;
        if len == 0 {
            return Err("memtest length must be > 0".to_string());
        }
        if rounds == 0 {
            return Err("memtest rounds must be >= 1".to_string());
        }
        const PATTERNS: [u32; 4] = [0xFFFF_FFFF, 0x0000_0000, 0xAAAA_AAAA, 0x5555_5555];
        let original = self.read_memory(addr, len)?;
        for _ in 0..rounds {
            for pattern in PATTERNS {
                self.fill_words(addr, len, pattern)?;
                let back = self.read_memory(addr, len)?;
                verify_pattern(addr, pattern, &back)?;
            }
        }
        self.write_chunks(addr, &original)?;
        let restored = self.read_memory(addr, len)?;
        if restored != original {
            return Err(format!(
                "memtest restore mismatch at {:#x} ({} bytes)",
                addr, len
            ));
        }
        Ok(())
    }

    /// Write `data` at `addr` in bounded chunks (shared by fill/restore).
    fn write_chunks(&mut self, addr: u32, data: &[u8]) -> Result<(), String> {
        let mut offset = 0usize;
        while offset < data.len() {
            let chunk = (data.len() - offset).min(MEM_FILL_CHUNK as usize);
            self.write_memory(
                addr.checked_add(offset as u32).ok_or_else(|| {
                    format!("write range overflow at {:#x} + {:#x}", addr, offset)
                })?,
                &data[offset..offset + chunk],
            )?;
            offset += chunk;
        }
        Ok(())
    }

    /// `GET_STORAGE_GEOME` (0x1A): 24-byte geometry for `media`.
    pub fn get_storage_geometry(
        &mut self,
        media: &StorageMedia,
    ) -> Result<StorageGeometry, String> {
        let bytes = self.read_storage_response(
            CMD_GET_STORAGE_GEOME,
            &media.to_bytes(),
            STORAGE_GEOMETRY_SIZE,
        )?;
        StorageGeometry::from_bytes(&bytes)
            .ok_or_else(|| format!("Geometry response too short: {} bytes", bytes.len()))
    }

    /// `ERASE_STORAGE` (0x1B): synchronous erase described by `args`
    /// (units follow the geometry command: bytes on Flash, sectors on MMC).
    pub fn erase_storage(&mut self, args: &StorageEraseArgs) -> Result<(), String> {
        let _resp = self.cmd_hdr_data_resp(CMD_ERASE_STORAGE, &args.to_bytes(), 0)?;
        Ok(())
    }

    pub fn get_device_log(&mut self) -> Result<String, String> {
        let size_resp = self.cmd_hdr_resp(CMD_GET_LOG_SIZE, 4)?;
        if size_resp.payload.len() < 4 {
            return Err(format!(
                "Log size response too short: {} bytes",
                size_resp.payload.len()
            ));
        }
        let size = u32::from_le_bytes(size_resp.payload[0..4].try_into().unwrap()) as usize;
        if size == 0 {
            return Ok(String::new());
        }
        let data_resp = self.cmd_hdr_resp(CMD_GET_LOG_DATA, size)?;
        Ok(String::from_utf8_lossy(&data_resp.payload).to_string())
    }

    pub fn show_info(&mut self) -> Result<(), String> {
        for line in self.device_info_lines()? {
            println!("  {}", line);
        }
        Ok(())
    }
}

/// Check `data` against the 4-byte LE `pattern` repeated from `base_addr`.
/// Reports the first mismatching word address on failure.
fn verify_pattern(base_addr: u32, pattern: u32, data: &[u8]) -> Result<(), String> {
    let expected = pattern.to_le_bytes();
    for (i, chunk) in data.chunks(4).enumerate() {
        for (j, byte) in chunk.iter().enumerate() {
            if *byte != expected[(i * 4 + j) % 4] {
                return Err(format!(
                    "memtest mismatch at {:#x}: expected {:#x}, got {:#x}",
                    base_addr + (i * 4 + j) as u32,
                    pattern,
                    u32::from_le_bytes(padded_word(data, i * 4))
                ));
            }
        }
    }
    Ok(())
}

/// First up-to-4 bytes at `offset` as a u32 (zero-padded tail for reporting).
fn padded_word(data: &[u8], offset: usize) -> [u8; 4] {
    let mut word = [0u8; 4];
    let take = (data.len().saturating_sub(offset)).min(4);
    word[..take].copy_from_slice(&data[offset..offset + take]);
    word
}
