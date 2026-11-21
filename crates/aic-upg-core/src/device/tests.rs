use super::commands::OFFICIAL_UPG_CFG_RESERVED;
use super::plan::{ComponentKind, FirmwareComponent};
use super::*;
use crate::protocol::cbw_csw::{AicCsw, AIC_UPG_SIGN_UPGR, AIC_USB_SIGN_USBS};
use crate::protocol::cbw_csw::{CmdHeader, AIC_UPG_SIGN_UPGC, CMD_HDR_LEN};
use crate::protocol::commands::*;
use crate::transport::CswPolicy;
use crate::transport::UpgTransport;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Default)]
struct MockTransport {
    writes: usize,
    reads: usize,
    panic_on_use: bool,
    max_chunk: usize,
    block_size: u32,
    boot_stage: u8,
    reconnects: usize,
    fail_hwinfo_once: bool,
    fail_command: Option<(u8, &'static str)>,
    corrupt_crc: bool,
    malformed_response_at: Option<u8>,
    dropped: ReleaseMarker,
    /// `SET_UPG_CFG` mode bytes observed on the wire (32-byte cfg payload).
    upg_modes: Vec<u8>,
    /// Whether any write payload contained the `reset` shell command.
    saw_reset: bool,
    /// Every payload passed to `write_txn`, in order (cmd header first,
    /// then the command payload), so tests can assert exact wire bytes.
    payloads: Vec<Vec<u8>>,
}

#[derive(Default)]
struct ReleaseMarker(Option<Arc<AtomicBool>>);

impl Drop for ReleaseMarker {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

impl MockTransport {
    fn ok_csw() -> AicCsw {
        let mut bytes = [0u8; 13];
        bytes[0..4].copy_from_slice(&AIC_USB_SIGN_USBS.to_le_bytes());
        AicCsw::from_bytes(&bytes).unwrap()
    }

    fn ok_resp_header() -> Vec<u8> {
        let mut hdr = vec![0u8; 16];
        hdr[0..4].copy_from_slice(&AIC_UPG_SIGN_UPGR.to_le_bytes());
        hdr[4] = 1;
        hdr[5] = 1;
        hdr[6] = 0; // wildcard command: accepted for any request
        hdr[7] = 0; // status OK
        hdr
    }
}

impl UpgTransport for MockTransport {
    fn write_txn(&mut self, payload: &[u8], _policy: CswPolicy) -> Result<Option<AicCsw>, String> {
        if self.panic_on_use {
            panic!("mock transport must not be touched after cancellation");
        }
        if self.fail_command.is_some_and(|(command, _)| {
            payload.len() == CMD_HDR_LEN && payload.starts_with(b"UPGC") && payload[6] == command
        }) {
            return Err(self.fail_command.take().unwrap().1.into());
        }
        self.writes += 1;
        self.payloads.push(payload.to_vec());
        if payload.len() == 32 && payload[1..] == OFFICIAL_UPG_CFG_RESERVED {
            self.upg_modes.push(payload[0]);
        }
        if payload.windows(5).any(|w| w == b"reset") {
            self.saw_reset = true;
        }
        Ok(Some(Self::ok_csw()))
    }

    fn read_txn(&mut self, read_len: u32, _policy: CswPolicy) -> Result<Vec<u8>, String> {
        if self.panic_on_use {
            panic!("mock transport must not be touched after cancellation");
        }
        self.reads += 1;
        if read_len as usize == 16 {
            let mut header = Self::ok_resp_header();
            let command = self
                .payloads
                .iter()
                .rev()
                .find(|payload| payload.starts_with(b"UPGC"))
                .map(|payload| payload[6]);
            if command.is_some() && command == self.malformed_response_at {
                header[0] ^= 1;
            }
            let size = match command {
                Some(CMD_GET_STORAGE_MEDIA) => STORAGE_MEDIA_SIZE as u32,
                Some(CMD_GET_STORAGE_GEOME) => STORAGE_GEOMETRY_SIZE as u32,
                Some(CMD_GET_PARTITION_TABLE) => 128,
                _ => 0,
            };
            header[8..12].copy_from_slice(&size.to_le_bytes());
            return Ok(header);
        }
        if read_len == STORAGE_MEDIA_SIZE as u32 {
            return Ok(StorageMedia::new("spi-nand", 0).to_bytes());
        }
        if read_len == 128
            && self.payloads.iter().any(|payload| {
                payload.starts_with(b"UPGC") && payload.get(6) == Some(&CMD_GET_PARTITION_TABLE)
            })
        {
            let mut data = vec![0; 128];
            data[..9].copy_from_slice(b"128k(env)");
            return Ok(data);
        }
        if read_len == 104 {
            if std::mem::take(&mut self.fail_hwinfo_once) {
                return Err("simulated HWINFO timeout".into());
            }
            let mut hwinfo = vec![0; 104];
            hwinfo[..8].copy_from_slice(b"HWINFO\0\0");
            hwinfo[38] = 1;
            hwinfo[40] = self.boot_stage;
            return Ok(hwinfo);
        }
        if read_len as usize == 4 {
            let cmd = self.payloads.last().and_then(|payload| payload.get(6));
            let value = match cmd.copied() {
                Some(CMD_GET_FWC_CRC) => {
                    self.payloads
                        .iter()
                        .rev()
                        .find_map(|payload| {
                            FwcMeta::from_bytes(payload).filter(|meta| meta.magic_str() == "META")
                        })
                        .unwrap()
                        .crc_val()
                        ^ u32::from(self.corrupt_crc)
                }
                Some(CMD_GET_FWC_BURN_RESULT | CMD_GET_FWC_RUN_RESULT) => 0,
                _ => self.block_size,
            };
            return Ok(value.to_le_bytes().to_vec());
        }
        Ok(vec![0u8; read_len as usize])
    }

    fn reconnect(&mut self, _timeout: Duration) -> Result<(), String> {
        self.reconnects += 1;
        self.boot_stage = 1;
        Ok(())
    }

    fn transport_name(&self) -> &'static str {
        "mock"
    }

    fn max_write_chunk(&self, _block_size: u32) -> usize {
        self.max_chunk
    }
}

fn make_meta(name: &str, partition: &str, offset: u32, size: u32, crc: u32) -> FwcMeta {
    let mut bytes = vec![0u8; 512];
    bytes[0..4].copy_from_slice(b"META");
    bytes[8..8 + name.len().min(64)].copy_from_slice(&name.as_bytes()[..name.len().min(64)]);
    bytes[72..72 + partition.len().min(64)]
        .copy_from_slice(&partition.as_bytes()[..partition.len().min(64)]);
    let media = if name.starts_with("image.target.") {
        "spi-nand"
    } else {
        "ram"
    };
    bytes[112..112 + media.len()].copy_from_slice(media.as_bytes());
    bytes[136..140].copy_from_slice(&offset.to_le_bytes());
    bytes[140..144].copy_from_slice(&size.to_le_bytes());
    bytes[144..148].copy_from_slice(&crc.to_le_bytes());
    FwcMeta::from_bytes(&bytes).unwrap()
}

#[test]
fn is_cancelled_reflects_flag() {
    let plain = BurnOptions::default();
    assert!(!is_cancelled(&plain));
    let flag = Arc::new(AtomicBool::new(false));
    let armed = BurnOptions {
        cancel: Some(flag.clone()),
        ..Default::default()
    };
    assert!(!is_cancelled(&armed));
    flag.store(true, Ordering::SeqCst);
    assert!(is_cancelled(&armed));
}

#[test]
fn pre_cancelled_burn_does_not_touch_transport() {
    let img = vec![0xABu8; 4096];
    let crc = crc32fast::hash(&img[0..2048]);
    let info_crc = crc32fast::hash(&img[2048..4096]);
    let metas = vec![
        make_meta("image.target.os", "os", 0, 2048, crc),
        make_meta("image.info", "", 2048, 2048, info_crc),
    ];
    let flag = Arc::new(AtomicBool::new(true));
    let options = BurnOptions {
        selected_parts: vec!["os".to_string()],
        reset_after_burn: false,
        burn_timeout: Duration::from_secs(5),
        cancel: Some(flag),
        force_upgrade: false,
        partition_upgrade: false,
        no_updater: false,
    };
    let mut dev = UpgDevice::new(MockTransport {
        writes: 0,
        reads: 0,
        panic_on_use: true,
        max_chunk: 512,
        block_size: 512,
        boot_stage: 0,
        reconnects: 0,
        fail_hwinfo_once: false,
        ..Default::default()
    });
    let mut events = Vec::new();
    let mut cb = |e: BurnEvent| events.push(format!("{:?}", e));
    let err = dev
        .burn_image_with_options(&img, &metas, &options, Some(&mut cb))
        .expect_err("pre-cancelled burn must fail");
    assert!(err.contains("cancelled"), "unexpected error: {}", err);
}

#[test]
fn cancel_mid_chunk_aborts_burn() {
    let img = vec![0xABu8; 4096];
    let crc = crc32fast::hash(&img[0..2048]);
    let info_crc = crc32fast::hash(&img[2048..4096]);
    let metas = vec![
        make_meta("image.target.os", "os", 0, 2048, crc),
        make_meta("image.info", "", 2048, 2048, info_crc),
    ];
    let flag = Arc::new(AtomicBool::new(false));
    let options = BurnOptions {
        selected_parts: vec!["os".to_string()],
        reset_after_burn: false,
        burn_timeout: Duration::from_secs(5),
        cancel: Some(flag.clone()),
        force_upgrade: false,
        partition_upgrade: false,
        no_updater: false,
    };
    let mut dev = UpgDevice::new(MockTransport {
        writes: 0,
        reads: 0,
        panic_on_use: false,
        max_chunk: 512,
        block_size: 512,
        boot_stage: 0,
        reconnects: 0,
        fail_hwinfo_once: false,
        ..Default::default()
    });
    let mut progress_events = 0usize;
    let mut cb = |event: BurnEvent| {
        if matches!(event, BurnEvent::ComponentProgress { .. }) {
            progress_events += 1;
            if progress_events >= 1 {
                flag.store(true, Ordering::SeqCst);
            }
        }
    };
    let err = dev
        .burn_image_with_options(&img, &metas, &options, Some(&mut cb))
        .expect_err("mid-burn cancel must fail");
    assert!(err.contains("cancelled"), "unexpected error: {}", err);
    assert!(
        dev.transport_mut().writes >= 1,
        "expected at least one chunk before cancel"
    );
}

#[test]
fn upg_mode_selects_force_upgrade_byte() {
    assert_eq!(
        upg_mode(&BurnOptions::default()),
        UPG_MODE_FULL_DISK_UPGRADE
    );
    let force = BurnOptions {
        force_upgrade: true,
        partition_upgrade: false,
        no_updater: false,
        ..Default::default()
    };
    assert_eq!(upg_mode(&force), UPG_MODE_BURN_IMG_FORCE);
}

#[test]
fn default_burn_uses_full_disk_mode_and_resets() {
    let img = vec![0xABu8; 2048];
    let crc = crc32fast::hash(&img[0..1024]);
    let info_crc = crc32fast::hash(&img[1024..2048]);
    let metas = vec![
        make_meta("image.target.os", "os", 0, 1024, crc),
        make_meta("image.info", "", 1024, 1024, info_crc),
    ];
    let options = BurnOptions {
        selected_parts: vec!["os".to_string()],
        reset_after_burn: true,
        burn_timeout: Duration::from_secs(5),
        cancel: None,
        force_upgrade: false,
        partition_upgrade: false,
        no_updater: false,
    };
    let mut dev = UpgDevice::new(MockTransport {
        writes: 0,
        reads: 0,
        panic_on_use: false,
        max_chunk: 4096,
        block_size: 512,
        boot_stage: 0,
        reconnects: 0,
        fail_hwinfo_once: false,
        ..Default::default()
    });
    dev.burn_image_with_options(&img, &metas, &options, None)
        .expect("mock burn must succeed");
    assert_eq!(
        dev.transport_mut().upg_modes,
        vec![UPG_MODE_FULL_DISK_UPGRADE]
    );
    assert!(
        dev.transport_mut().saw_reset,
        "default burn must reset the device"
    );
}

#[test]
fn force_upgrade_uses_force_mode_and_skips_reset() {
    let img = vec![0xABu8; 2048];
    let crc = crc32fast::hash(&img[0..1024]);
    let info_crc = crc32fast::hash(&img[1024..2048]);
    let metas = vec![
        make_meta("image.target.os", "os", 0, 1024, crc),
        make_meta("image.info", "", 1024, 1024, info_crc),
    ];
    // Even with reset_after_burn=true, force upgrade wins (mutually exclusive).
    let options = BurnOptions {
        selected_parts: vec!["os".to_string()],
        reset_after_burn: true,
        burn_timeout: Duration::from_secs(5),
        cancel: None,
        force_upgrade: true,
        partition_upgrade: false,
        no_updater: false,
    };
    let mut dev = UpgDevice::new(MockTransport {
        writes: 0,
        reads: 0,
        panic_on_use: false,
        max_chunk: 4096,
        block_size: 512,
        boot_stage: 0,
        reconnects: 0,
        fail_hwinfo_once: false,
        ..Default::default()
    });
    let mut logs = Vec::new();
    let mut cb = |e: BurnEvent| {
        if let BurnEvent::Log(line) = e {
            logs.push(line);
        }
    };
    dev.burn_image_with_options(&img, &metas, &options, Some(&mut cb))
        .expect("mock force burn must succeed");
    assert_eq!(dev.transport_mut().upg_modes, vec![UPG_MODE_BURN_IMG_FORCE]);
    assert!(
        !dev.transport_mut().saw_reset,
        "force upgrade must skip the post-burn reset"
    );
    assert!(
        logs.iter().any(|l| l.contains("skipping reset")),
        "expected a skipping-reset note, got: {:?}",
        logs
    );
}

fn mock_device() -> UpgDevice<MockTransport> {
    UpgDevice::new(MockTransport {
        max_chunk: 4096,
        block_size: 512,
        ..Default::default()
    })
}

#[test]
fn full_upgrade_downloads_updater_reconnects_and_finishes_in_order() {
    let (mut image, mut metas) = plan_image_parts();
    for (index, bytes) in image.as_chunks_mut::<512>().0.iter_mut().enumerate() {
        bytes.fill(0xa0 + index as u8);
        metas[index].bytes[144..148].copy_from_slice(&crc32fast::hash(bytes).to_le_bytes());
    }
    let mut dev = mock_device();
    let mut events = Vec::new();
    dev.burn_image_with_options(
        &image,
        &metas,
        &BurnOptions {
            selected_parts: vec!["spl".into(), "os".into()],
            ..Default::default()
        },
        Some(&mut |event| events.push(event)),
    )
    .unwrap();
    let wire = dev.transport_mut();
    assert_eq!(wire.reconnects, 1);
    assert_eq!(wire.boot_stage, 1);
    let commands: Vec<_> = wire
        .payloads
        .iter()
        .filter(|payload| payload.starts_with(b"UPGC"))
        .map(|payload| payload[6])
        .collect();
    let component = [CMD_SET_FWC_META, CMD_GET_BLOCK_SIZE, CMD_SEND_FWC_DATA];
    assert_eq!(
        commands,
        [
            component.as_slice(),
            &[CMD_GET_HWINFO, CMD_SET_UPG_CFG],
            &component,
            &component,
            &component,
            &[CMD_SET_UPG_END, CMD_RUN_SHELL_STR]
        ]
        .concat()
    );
    let data: Vec<_> = wire
        .payloads
        .iter()
        .filter(|payload| payload.len() == 512 && !payload.starts_with(b"META"))
        .flatten()
        .copied()
        .collect();
    assert_eq!(data, image);
    let stages: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            BurnEvent::Stage(stage) => Some(stage.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        [
            "Build component plan",
            "Send updater components",
            "Wait for bootloader reconnect",
            "Probe bootloader after reconnect",
            "Set full-disk upgrade mode",
            "End upgrade",
            "Reset device"
        ]
    );
    assert!(matches!(events.last(), Some(BurnEvent::Finished)));
}

#[test]
fn disconnected_or_timed_out_write_never_reports_completion() {
    let (image, metas) = plan_image_parts();
    for failure in [
        "simulated device disconnected",
        "simulated transaction timeout",
    ] {
        let mut dev = mock_device();
        let dropped = Arc::new(AtomicBool::new(false));
        dev.transport_mut().dropped.0 = Some(dropped.clone());
        dev.transport_mut().fail_command = Some((CMD_SEND_FWC_DATA, failure));
        let mut events = Vec::new();
        let options = BurnOptions {
            selected_parts: vec!["os".into()],
            ..Default::default()
        };
        let plan = plan_burn(&image, &metas, &options).unwrap();
        let error = dev
            .burn_planned(
                &image,
                &metas,
                &options,
                &plan,
                true,
                Some(&mut |event| events.push(event)),
            )
            .unwrap_err();
        assert!(
            matches!(error, crate::error::FlashError::Device {operation: "start component transfer", message} if message == failure)
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            BurnEvent::Finished | BurnEvent::ComponentFinished { .. }
        )));
        assert!(!dev.transport_mut().saw_reset);
        drop(dev);
        assert!(dropped.load(Ordering::SeqCst));
    }
}

#[test]
fn malformed_upgrade_configuration_response_stops_before_component_writes() {
    let (image, metas) = plan_image_parts();
    let mut dev = mock_device();
    dev.transport_mut().boot_stage = 1;
    dev.transport_mut().malformed_response_at = Some(CMD_SET_UPG_CFG);
    let options = BurnOptions {
        selected_parts: vec!["os".into()],
        no_updater: true,
        ..Default::default()
    };
    let plan = plan_burn(&image, &metas, &options).unwrap();
    let error = dev
        .burn_planned(&image, &metas, &options, &plan, true, None)
        .unwrap_err();
    assert!(matches!(
        error,
        crate::error::FlashError::Device {
            operation: "set upgrade configuration",
            ..
        }
    ));
    assert!(!dev
        .transport_mut()
        .payloads
        .iter()
        .any(|payload| payload.starts_with(b"META")));
}

#[test]
fn write_memory_sends_addr_len_data() {
    let mut dev = mock_device();
    dev.write_memory(0x41000000, &[0xAA, 0xBB]).unwrap();
    // writes[0] = 16-byte cmd header, writes[1] = command payload.
    assert_eq!(dev.transport_mut().payloads.len(), 2);
    assert_eq!(
        dev.transport_mut().payloads[1],
        vec![0x00, 0x00, 0x00, 0x41, 0x02, 0x00, 0x00, 0x00, 0xAA, 0xBB]
    );
}

#[test]
fn read_memory_sends_addr_len_and_returns_len_bytes() {
    // NOTE: len 0x10 would collide with the mock's 16-byte RESP-header
    // special case; 0x20 exercises the generic zero-fill path.
    let mut dev = mock_device();
    let data = dev.read_memory(0x40000000, 0x20).unwrap();
    assert_eq!(data, vec![0u8; 0x20]);
    assert_eq!(
        dev.transport_mut().payloads[1],
        vec![0x00, 0x00, 0x00, 0x40, 0x20, 0x00, 0x00, 0x00]
    );
}

#[test]
fn exec_address_sends_four_byte_addr() {
    let mut dev = mock_device();
    dev.exec_address(0x41000100).unwrap();
    assert_eq!(
        dev.transport_mut().payloads[1],
        vec![0x00, 0x01, 0x00, 0x41]
    );
}

#[test]
fn run_shell_rejects_overlong_commands() {
    let mut dev = mock_device();
    assert!(dev.run_shell(&"x".repeat(128)).is_err());
    // Rejected before touching the transport.
    assert!(dev.transport_mut().payloads.is_empty());
}

#[test]
fn storage_geometry_and_erase_wire_shapes() {
    let mut dev = mock_device();
    let media = StorageMedia::new("spi-nand", 0);
    let geo = dev.get_storage_geometry(&media).unwrap();
    assert_eq!(geo.total_size, 0);
    let payloads = &dev.transport_mut().payloads;
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[1].len(), STORAGE_MEDIA_SIZE);
    assert_eq!(&payloads[1][..9], b"spi-nand\0");

    let mut dev = mock_device();
    dev.erase_storage(&StorageEraseArgs {
        media: StorageMedia::new("spi-nand", 0),
        start: 0,
        size: 0x0800_0000,
        flag: 0,
    })
    .unwrap();
    let payloads = &dev.transport_mut().payloads;
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[1].len(), STORAGE_ERASE_ARGS_SIZE);
    assert_eq!(
        u64::from_le_bytes(payloads[1][80..88].try_into().unwrap()),
        0x0800_0000
    );
}

#[test]
fn list_storage_media_parses_68_byte_chunks() {
    let mut dev = mock_device();
    let nand = StorageMedia::new("spi-nand", 0);
    assert_eq!(dev.list_storage_media().unwrap(), vec![nand.clone()]);
    assert_eq!(dev.read_partition_table(&nand).unwrap(), "128k(env)");
}

/// Fake transport backed by real RAM: applies WRITE payloads to a map
/// and answers READs from it, with correct RESP `data_length`, so
/// fill/memtest logic is verified for real instead of against zeros.
struct RamTransport {
    mem: std::collections::HashMap<u32, u8>,
    /// When true, WRITE data is dropped (reads return prior content),
    /// exercising the memtest mismatch path.
    corrupt_writes: bool,
    pending_cmd: u8,
    pending_read: Option<(u32, u32)>,
}

impl RamTransport {
    fn resp_header(cmd: u8, data_len: u32) -> Vec<u8> {
        let mut hdr = vec![0u8; 16];
        hdr[0..4].copy_from_slice(&AIC_UPG_SIGN_UPGR.to_le_bytes());
        hdr[4] = 1;
        hdr[5] = 1;
        hdr[6] = cmd;
        hdr[7] = 0;
        hdr[8..12].copy_from_slice(&data_len.to_le_bytes());
        hdr
    }
}

impl UpgTransport for RamTransport {
    fn write_txn(&mut self, payload: &[u8], _policy: CswPolicy) -> Result<Option<AicCsw>, String> {
        if payload.len() == 16 && payload[0..4] == AIC_UPG_SIGN_UPGC.to_le_bytes() {
            // Command header (magic "UPGC"): record the command, the
            // next write is its payload even when also 16 bytes long
            // (e.g. an 8-byte WRITE of 8 data bytes).
            self.pending_cmd = payload[6];
            self.pending_read = None;
            return Ok(Some(MockTransport::ok_csw()));
        }
        match self.pending_cmd {
            CMD_WRITE => {
                let addr = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                let len = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
                if !self.corrupt_writes {
                    for (i, byte) in payload[8..8 + len].iter().enumerate() {
                        self.mem.insert(addr + i as u32, *byte);
                    }
                }
            }
            CMD_READ => {
                let addr = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                let len = u32::from_le_bytes(payload[4..8].try_into().unwrap());
                self.pending_read = Some((addr, len));
            }
            _ => {}
        }
        self.pending_cmd = 0;
        Ok(Some(MockTransport::ok_csw()))
    }

    fn read_txn(&mut self, read_len: u32, _policy: CswPolicy) -> Result<Vec<u8>, String> {
        if read_len as usize == 16 {
            let data_len = self.pending_read.map(|(_, len)| len).unwrap_or(0);
            return Ok(Self::resp_header(self.pending_cmd, data_len));
        }
        if let Some((addr, len)) = self.pending_read.take() {
            assert_eq!(len, read_len, "device answers exactly the READ length");
            return Ok((0..len)
                .map(|i| self.mem.get(&(addr + i)).copied().unwrap_or(0))
                .collect());
        }
        Ok(vec![0u8; read_len as usize])
    }

    fn reconnect(&mut self, _timeout: Duration) -> Result<(), String> {
        Ok(())
    }

    fn transport_name(&self) -> &'static str {
        "ram-fake"
    }

    fn max_write_chunk(&self, _block_size: u32) -> usize {
        usize::MAX
    }
}

#[test]
fn ram_fake_round_trips_write_then_read() {
    let mut dev = UpgDevice::new(RamTransport {
        mem: std::collections::HashMap::new(),
        corrupt_writes: false,
        pending_cmd: 0,
        pending_read: None,
    });
    dev.write_memory(0x4000_0000, &[1, 2, 3, 4, 5]).unwrap();
    assert_eq!(
        dev.read_memory(0x4000_0000, 5).unwrap(),
        vec![1, 2, 3, 4, 5]
    );
    // Untouched regions read back as zeros.
    assert_eq!(dev.read_memory(0x5000_0000, 4).unwrap(), vec![0, 0, 0, 0]);
}

#[test]
fn fill_memory_writes_repeating_byte() {
    let mut dev = UpgDevice::new(RamTransport {
        mem: std::collections::HashMap::new(),
        corrupt_writes: false,
        pending_cmd: 0,
        pending_read: None,
    });
    dev.fill_memory(0x4000_0000, 10, 0xAB).unwrap();
    assert_eq!(dev.read_memory(0x4000_0000, 10).unwrap(), vec![0xAB; 10]);
    assert!(dev.fill_memory(0x4000_0000, 0, 1).is_err());
    assert!(dev.fill_memory(u32::MAX, 16, 1).is_err());
}

#[test]
fn memtest_passes_and_restores_original() {
    let mut mem = std::collections::HashMap::new();
    mem.insert(0x4000_0000, 0xAB);
    mem.insert(0x4000_0001, 0xCD);
    let mut dev = UpgDevice::new(RamTransport {
        mem,
        corrupt_writes: false,
        pending_cmd: 0,
        pending_read: None,
    });
    dev.memtest_memory(0x4000_0000, 8, 1).unwrap();
    // Original content restored after the pattern loop.
    assert_eq!(
        dev.read_memory(0x4000_0000, 8).unwrap(),
        vec![0xAB, 0xCD, 0, 0, 0, 0, 0, 0]
    );
    assert!(dev.memtest_memory(0x4000_0000, 8, 0).is_err());
    assert!(dev.memtest_memory(0x4000_0000, 0, 1).is_err());
}

#[test]
fn memtest_reports_first_mismatch_address() {
    let mut dev = UpgDevice::new(RamTransport {
        mem: std::collections::HashMap::new(),
        corrupt_writes: true, // writes dropped: read-back never matches
        pending_cmd: 0,
        pending_read: None,
    });
    let err = dev.memtest_memory(0x4000_0000, 8, 1).unwrap_err();
    assert!(
        err.contains("0x40000000"),
        "mismatch must name the address, got: {}",
        err
    );
}

#[test]
fn partition_upgrade_selects_mode() {
    let opts = BurnOptions {
        partition_upgrade: true,
        ..Default::default()
    };
    assert_eq!(upg_mode(&opts), UPG_MODE_PARTITION_UPGRADE);
    assert_eq!(upg_mode_name(&opts), "partition upgrade mode");
    assert_eq!(
        upg_mode(&BurnOptions::default()),
        UPG_MODE_FULL_DISK_UPGRADE
    );
}

fn plan_image_parts() -> (Vec<u8>, Vec<FwcMeta>) {
    let img = vec![0xABu8; 4 * 512];
    let names = [
        ("image.updater.ddr", ""),
        ("image.info", ""),
        ("image.target.spl", "spl"),
        ("image.target.os", "os"),
    ];
    let metas = names
        .iter()
        .enumerate()
        .map(|(index, (name, partition))| {
            let offset = (index * 512) as u32;
            let crc = crc32fast::hash(&img[(index * 512)..(index * 512 + 512)]);
            make_meta(name, partition, offset, 512, crc)
        })
        .collect();
    (img, metas)
}

#[test]
fn linux_plan_inherits_header_media_and_selects_component_aliases() {
    let header_size = crate::image::parser::FW_HEADER_SIZE;
    let mut img = vec![0; header_size + 9];
    img[..6].copy_from_slice(b"AIC.FW");
    img[200..208].copy_from_slice(b"spi-nand");
    let info = make_meta(
        "image.info",
        "",
        0,
        header_size as u32,
        crc32fast::hash(&img[..header_size]),
    );
    let mut metas = vec![info];
    for (index, (name, partition)) in [
        ("image.target.spl", "spl"),
        ("image.target.env", "env;env_r"),
        ("image.target.rootfs", "ubiroot:rootfs"),
    ]
    .into_iter()
    .enumerate()
    {
        let offset = header_size + index * 3;
        let mut meta = make_meta(
            name,
            partition,
            offset as u32,
            3,
            crc32fast::hash(&img[offset..offset + 3]),
        );
        meta.bytes[112..136].fill(0);
        metas.push(meta);
    }
    let options = BurnOptions {
        selected_parts: default_burn_parts(&metas),
        ..Default::default()
    };
    let full = plan_burn(&img, &metas, &options).unwrap();
    assert_eq!(full.mode, UPG_MODE_FULL_DISK_UPGRADE);
    assert!(full.uses_header_media);
    assert_eq!(full.total_bytes, img.len());
    assert!(full.components[1..]
        .iter()
        .all(|entry| entry.selected && entry.media == "spi-nand"));
    assert_eq!(full.components[2].partition, "env;env_r");
    for alias in ["rootfs", "image.target.rootfs", "ubiroot:rootfs"] {
        let options = BurnOptions {
            selected_parts: vec![alias.to_string()],
            ..Default::default()
        };
        let partial = plan_burn(&img, &metas, &options).unwrap();
        assert_eq!(partial.mode, UPG_MODE_PARTITION_UPGRADE);
        assert_eq!(partial.total_bytes, header_size + 3);
        assert!(partial.components[3].selected);
    }
    metas[1].bytes[112..115].copy_from_slice(b"ram");
    assert!(plan_burn(&img, &metas, &options)
        .unwrap_err()
        .contains("target has no storage media"));
}

#[test]
fn linux_component_preserves_meta_and_checks_device_results_after_aligned_write() {
    let img = [0x11, 0x22, 0x33];
    let mut meta = make_meta("image.target.env", "env;env_r", 0, 3, crc32fast::hash(&img));
    meta.bytes[112..136].fill(0);
    let component = FirmwareComponent {
        meta: &meta,
        kind: ComponentKind::Target,
        selected: true,
    };
    let mut dev = mock_device();
    let mut sent = 0;
    dev.send_component(
        &img,
        &BurnOptions::default(),
        &component,
        false,
        true,
        &mut sent,
        img.len(),
        &mut None,
    )
    .unwrap();
    assert_eq!(sent, img.len());
    let writes = &dev.transport_mut().payloads;
    assert_eq!(writes[1], meta.to_bytes());
    assert_eq!(writes[3], CmdHeader::new(CMD_SEND_FWC_DATA, 3).to_bytes());
    assert_eq!(writes[4].len(), 512);
    assert_eq!(writes[4][..3], img);
    assert!(writes[4][3..].iter().all(|&byte| byte == 0));
    assert_eq!(writes[5], CmdHeader::new(CMD_GET_FWC_CRC, 0).to_bytes());
    assert_eq!(
        writes[6],
        CmdHeader::new(CMD_GET_FWC_BURN_RESULT, 0).to_bytes()
    );
    assert_eq!(
        writes[7],
        CmdHeader::new(CMD_GET_FWC_RUN_RESULT, 0).to_bytes()
    );
}

#[test]
fn device_crc_error_has_component_and_both_checksums() {
    let data = b"123456789";
    let meta = make_meta("image.target.os", "os", 0, 9, 0xcbf43926);
    let component = FirmwareComponent {
        meta: &meta,
        kind: ComponentKind::Target,
        selected: true,
    };
    let mut dev = mock_device();
    dev.transport_mut().corrupt_crc = true;
    let mut events = Vec::new();
    let error = dev
        .send_component(
            data,
            &BurnOptions::default(),
            &component,
            false,
            true,
            &mut 0,
            data.len(),
            &mut Some(&mut |event| events.push(event)),
        )
        .unwrap_err();
    assert!(matches!(error, crate::error::FlashError::DeviceCrc {
        component, expected: 0xcbf43926, actual: 0xcbf43927,
    } if component == "image.target.os"));
    assert!(!events
        .iter()
        .any(|event| matches!(event, BurnEvent::ComponentFinished { .. })));
}

#[test]
fn linux_nand_sends_separate_usb_transactions_at_the_gadget_limit() {
    for (name, partition, attrs, block_size, size, expected) in [
        (
            "spl",
            "spl",
            "mtd;required",
            2048,
            142_864,
            &[65_536, 65_536, 12_288][..],
        ),
        (
            "rootfs",
            "ubiroot:rootfs",
            "ubi;required",
            126_976,
            126_976 + 1,
            &[65_536, 61_441][..],
        ),
    ] {
        let img = vec![0xab; size];
        let mut meta = make_meta(
            &format!("image.target.{name}"),
            partition,
            0,
            size as u32,
            crc32fast::hash(&img),
        );
        meta.bytes[112..136].fill(0);
        meta.bytes[152..152 + attrs.len()].copy_from_slice(attrs.as_bytes());
        let component = FirmwareComponent {
            meta: &meta,
            kind: ComponentKind::Target,
            selected: true,
        };
        let mut dev = mock_device();
        dev.transport_mut().max_chunk = 65_536;
        dev.transport_mut().block_size = block_size;
        let mut sent = 0;
        dev.send_component(
            &img,
            &BurnOptions::default(),
            &component,
            false,
            true,
            &mut sent,
            size,
            &mut None,
        )
        .unwrap();
        assert_eq!(sent, size);
        let writes = &dev.transport_mut().payloads;
        assert_eq!(
            writes[3],
            CmdHeader::new(CMD_SEND_FWC_DATA, size as u32).to_bytes()
        );
        let packets = &writes[4..writes.len() - 3];
        let lengths: Vec<_> = packets.iter().map(Vec::len).collect();
        assert_eq!(lengths, expected);
        let data = packets.concat();
        assert_eq!(&data[..size], &img);
        assert!(data[size..].iter().all(|&byte| byte == 0));
    }
}

#[test]
fn plan_selects_only_os() {
    let (img, metas) = plan_image_parts();
    let opts = BurnOptions {
        selected_parts: vec!["os".to_string()],
        partition_upgrade: true,
        ..Default::default()
    };
    let plan = plan_burn(&img, &metas, &opts).unwrap();
    assert_eq!(plan.mode, UPG_MODE_PARTITION_UPGRADE);
    let selected: Vec<&str> = plan
        .components
        .iter()
        .filter(|c| c.selected)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        selected,
        vec!["image.updater.ddr", "image.info", "image.target.os"]
    );
    assert_eq!(plan.total_bytes, 3 * 512);
}

#[test]
fn plan_rejects_unknown_partition() {
    let (img, metas) = plan_image_parts();
    let opts = BurnOptions {
        selected_parts: vec!["boot".to_string()],
        ..Default::default()
    };
    let err = plan_burn(&img, &metas, &opts).unwrap_err();
    assert!(err.contains("Unknown target partition"), "{}", err);
}

#[test]
fn plan_rejects_duplicate_selection() {
    let (img, metas) = plan_image_parts();
    let opts = BurnOptions {
        selected_parts: vec!["os".to_string(), "os".to_string()],
        ..Default::default()
    };
    let err = plan_burn(&img, &metas, &opts).unwrap_err();
    assert!(err.contains("Duplicate partition"), "{}", err);
}

#[test]
fn plan_rejects_empty_selection() {
    let (img, metas) = plan_image_parts();
    let opts = BurnOptions {
        selected_parts: vec![],
        ..Default::default()
    };
    let err = plan_burn(&img, &metas, &opts).unwrap_err();
    assert!(err.contains("No target partitions selected"), "{}", err);
}

#[test]
fn plan_rejects_crc_mismatch() {
    let (mut img, metas) = plan_image_parts();
    img[0] ^= 0xFF;
    let opts = BurnOptions {
        selected_parts: vec!["os".to_string()],
        ..Default::default()
    };
    let err = plan_burn(&img, &metas, &opts).unwrap_err();
    assert!(err.contains("CRC mismatch"), "{}", err);
}

#[test]
fn partition_burn_sends_only_os_and_partition_mode() {
    let (img, metas) = plan_image_parts();
    let opts = BurnOptions {
        selected_parts: vec!["os".to_string()],
        partition_upgrade: true,
        reset_after_burn: false,
        ..Default::default()
    };
    let mut dev = mock_device();
    let mut started = Vec::new();
    let mut cb = |event: BurnEvent| {
        if let BurnEvent::ComponentStarted { name, .. } = event {
            started.push(name);
        }
    };
    dev.burn_image_with_options(&img, &metas, &opts, Some(&mut cb))
        .expect("partition burn must succeed");
    assert_eq!(
        dev.transport_mut().upg_modes,
        vec![UPG_MODE_PARTITION_UPGRADE]
    );
    assert_eq!(
        started,
        vec![
            "image.updater.ddr".to_string(),
            "image.info".to_string(),
            "image.target.os".to_string()
        ]
    );
    assert!(!dev.transport_mut().saw_reset);
}

#[test]
fn no_updater_skips_updater_components() {
    let header_size = crate::image::parser::FW_HEADER_SIZE;
    let mut img = vec![0u8; header_size + 4 * 512];
    img[..6].copy_from_slice(b"AIC.FW");
    img[200..208].copy_from_slice(b"spi-nand");
    let metas = [
        ("image.info", "", 0, header_size),
        ("image.updater.ddr", "", header_size, 512),
        ("image.updater.spl", "", header_size + 512, 512),
        ("image.updater.uboot", "", header_size + 1024, 512),
        (
            "image.target.rootfs",
            "ubiroot:rootfs",
            header_size + 1536,
            512,
        ),
    ]
    .map(|(name, partition, offset, size)| {
        let mut meta = make_meta(
            name,
            partition,
            offset as u32,
            size as u32,
            crc32fast::hash(&img[offset..offset + size]),
        );
        meta.bytes[112..136].fill(0);
        meta
    });
    let opts = BurnOptions {
        selected_parts: vec!["ubiroot:rootfs".to_string()],
        partition_upgrade: true,
        no_updater: true,
        reset_after_burn: false,
        ..Default::default()
    };
    let mut dev = mock_device();
    dev.transport_mut().boot_stage = 1;
    let mut started = Vec::new();
    let mut cb = |event: BurnEvent| {
        if let BurnEvent::ComponentStarted { name, .. } = event {
            started.push(name);
        }
    };
    dev.burn_image_with_options(&img, &metas, &opts, Some(&mut cb))
        .expect("no-updater burn must succeed");
    assert_eq!(
        started,
        vec!["image.info".to_string(), "image.target.rootfs".to_string()]
    );
    assert_eq!(
        dev.transport_mut().upg_modes,
        vec![UPG_MODE_PARTITION_UPGRADE]
    );
    assert_eq!(dev.transport_mut().reconnects, 0);
}

#[test]
fn failed_bootloader_probe_leaves_a_fresh_attempt_possible() {
    let (img, metas) = plan_image_parts();
    let options = BurnOptions {
        selected_parts: vec!["os".into()],
        partition_upgrade: true,
        no_updater: true,
        reset_after_burn: false,
        ..Default::default()
    };
    let mut dev = mock_device();
    dev.transport_mut().fail_hwinfo_once = true;
    dev.transport_mut().boot_stage = 1;
    let err = dev
        .burn_image_with_options(&img, &metas, &options, None)
        .unwrap_err();
    assert!(err.contains("HWINFO probe failed: simulated HWINFO timeout"));
    assert!(dev.transport_mut().upg_modes.is_empty());
    dev.burn_image_with_options(&img, &metas, &options, None)
        .unwrap();
    assert_eq!(dev.transport_mut().upg_modes, [UPG_MODE_PARTITION_UPGRADE]);
}

#[test]
fn stage_mismatch_stops_before_config_or_components() {
    let (img, metas) = plan_image_parts();
    let options = BurnOptions {
        selected_parts: vec!["os".into()],
        partition_upgrade: true,
        no_updater: true,
        ..Default::default()
    };
    for stage in [0, 2, u8::MAX] {
        let mut dev = mock_device();
        dev.transport_mut().boot_stage = stage;
        let err = dev
            .burn_image_with_options(&img, &metas, &options, None)
            .unwrap_err();
        assert!(err.contains(&format!("got Boot stage={stage}")));
        assert_eq!(dev.transport_mut().writes, 1);
        assert!(dev.transport_mut().upg_modes.is_empty());
    }
    let mut dev = mock_device();
    assert!(dev.verify_upgrade_stage(false).is_ok());
    dev.transport_mut().boot_stage = 1;
    assert!(dev.verify_upgrade_stage(false).is_err());
    let plan = plan_burn(&img, &metas, &options).unwrap();
    assert!(validate_erase_plan(&plan, true).is_err());
}
