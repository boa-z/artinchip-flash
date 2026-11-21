use std::{collections::VecDeque, error::Error, time::Duration};

use artinchip_flash::protocol::cbw_csw::{AicCbw, AicCsw, CmdHeader};
use artinchip_flash::protocol::commands::{
    CMD_GET_BLOCK_SIZE, CMD_GET_HWINFO, CMD_SEND_FWC_DATA, CMD_SET_FWC_META, CMD_SET_UPG_CFG,
    CMD_SET_UPG_END,
};
use artinchip_flash::transport::CswPolicy;
use artinchip_flash::{BurnEvent, FirmwareImage, UpgDevice, UpgTransport};

#[path = "../support/firmware.rs"]
mod fixture;

enum Step {
    Write(Vec<u8>, CswPolicy),
    Read(Vec<u8>, CswPolicy),
}

#[derive(Default)]
struct MockTransport {
    script: VecDeque<Step>,
    tag: u32,
}

impl MockTransport {
    fn write(&mut self, bytes: &[u8]) {
        self.script
            .push_back(Step::Write(bytes.to_vec(), CswPolicy::Required));
    }

    fn read(&mut self, bytes: &[u8], policy: CswPolicy) {
        self.script.push_back(Step::Read(bytes.to_vec(), policy));
    }

    fn response(&mut self, command: u8, policy: CswPolicy) {
        let mut response = *b"UPGR\x01\x01\0\0\0\0\0\0\0\0\0\0";
        response[6] = command;
        self.read(&response, policy);
    }

    fn expect_hwinfo(&mut self) {
        self.write(CmdHeader::new(CMD_GET_HWINFO, 0).to_bytes());
        self.response(CMD_GET_HWINFO, CswPolicy::Required);
        let mut info = [0; 104];
        info[..8].copy_from_slice(b"HWINFO\0\0");
        info[40] = 1;
        self.read(&info, CswPolicy::Required);
    }

    fn expect_component(&mut self, meta: &[u8], data: &[u8]) {
        self.write(CmdHeader::new(CMD_SET_FWC_META, 512).to_bytes());
        self.write(meta);
        self.response(CMD_SET_FWC_META, CswPolicy::Required);
        self.write(CmdHeader::new(CMD_GET_BLOCK_SIZE, 0).to_bytes());
        self.response(CMD_GET_BLOCK_SIZE, CswPolicy::Required);
        self.read(&512u32.to_le_bytes(), CswPolicy::Required);
        self.write(CmdHeader::new(CMD_SEND_FWC_DATA, data.len() as u32).to_bytes());
        self.write(data);
        self.response(CMD_SEND_FWC_DATA, CswPolicy::Required);
    }

    fn acknowledge(&mut self, cbw: &AicCbw) -> Result<AicCsw, String> {
        let mut raw = *b"USBS\0\0\0\0\0\0\0\0\0";
        raw[4..8].copy_from_slice(&cbw.to_bytes()[4..8]);
        let csw = AicCsw::from_bytes(&raw).ok_or("Truncated mock CSW")?;
        if !csw.is_ok() || csw.tag_val() != self.tag {
            return Err("Invalid mock CSW".into());
        }
        self.tag += 1;
        Ok(csw)
    }
}

impl UpgTransport for MockTransport {
    fn write_txn(&mut self, payload: &[u8], policy: CswPolicy) -> Result<Option<AicCsw>, String> {
        match self.script.pop_front() {
            Some(Step::Write(expected, required)) if expected == payload && policy == required => {
                self.acknowledge(&AicCbw::new_write(self.tag, payload.len() as u32))
                    .map(Some)
            }
            _ => Err(format!("Unexpected write of {} bytes", payload.len())),
        }
    }

    fn read_txn(&mut self, len: u32, policy: CswPolicy) -> Result<Vec<u8>, String> {
        match self.script.pop_front() {
            Some(Step::Read(bytes, expected))
                if bytes.len() == len as usize && expected == policy =>
            {
                self.acknowledge(&AicCbw::new_read(self.tag, len))?;
                Ok(bytes)
            }
            _ => Err(format!("Unexpected read of {len} bytes")),
        }
    }

    fn reconnect(&mut self, _: Duration) -> Result<(), String> {
        Err("Unexpected reconnect in bootloader mode".into())
    }

    fn transport_name(&self) -> &'static str {
        "offline mock"
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let bytes = fixture::firmware();
    let image = FirmwareImage::from_bytes(bytes.clone())?;
    image.validate()?;
    let mut options = image.default_options();
    options.selected_parts = vec!["os".into()];
    options.partition_upgrade = true;
    options.no_updater = true;
    options.reset_after_burn = false;
    let flash = image.prepare(options)?;
    assert_eq!(flash.plan().total_bytes, 2055);

    let mut transport = MockTransport::default();
    transport.expect_hwinfo();
    transport.expect_hwinfo();
    transport.write(CmdHeader::new(CMD_SET_UPG_CFG, 36).to_bytes());
    transport.write(&32u32.to_le_bytes());
    transport.write(&[
        1, 0xea, 0, 0, 0xbc, 0xf5, 0x44, 4, 0x50, 0xf5, 0x44, 4, 1, 0, 0, 0, 0x18, 0x73, 0xdf, 5,
        0x50, 0xf5, 0x44, 4, 0x40, 0xfe, 0xf1, 0, 0x18, 0x73, 0xdf, 5,
    ]);
    transport.response(CMD_SET_UPG_CFG, CswPolicy::Required);
    transport.expect_component(&bytes[2560..3072], &bytes[..2048]);
    transport.expect_component(&bytes[3584..4096], b"OS DEMO");
    transport.write(CmdHeader::new(CMD_SET_UPG_END, 36).to_bytes());
    let mut end = [0; 36];
    end[0] = 32;
    transport
        .script
        .push_back(Step::Write(end.to_vec(), CswPolicy::AllowMissing));
    transport.response(CMD_SET_UPG_END, CswPolicy::AllowMissing);

    let mut device = UpgDevice::new(transport);
    assert_eq!(device.get_hwinfo()?.boot_stage(), 1);
    let mut completed = false;
    flash.execute(
        &mut device,
        Some(&mut |event| {
            if matches!(event, BurnEvent::Finished) {
                completed = true;
            }
        }),
    )?;
    assert!(completed && device.transport_mut().script.is_empty());
    println!(
        "{}: simulated partition upgrade complete",
        image.summary().product
    );
    Ok(())
}
