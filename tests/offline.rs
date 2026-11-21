use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

use artinchip_flash::{FirmwareImage, FlashError};
use serde_json::{json, Value};

#[path = "support/firmware.rs"]
mod fixture;

struct ImageFile(PathBuf);

impl ImageFile {
    fn new(bytes: &[u8]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "aic-test-{}-{}.img",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, bytes).unwrap();
        Self(path)
    }

    fn cli(&self, command: &str, flags: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_artinchip-flash"))
            .arg(command)
            .arg(&self.0)
            .args(flags)
            .output()
            .unwrap()
    }
}

impl Drop for ImageFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn cli_info_and_text_plan_match_the_original_output() {
    let image = ImageFile::new(&fixture::firmware());
    let info = image.cli("info", &[]);
    assert!(info.status.success());
    let text = String::from_utf8(info.stdout).unwrap();
    for line in ["  Product:     Offline", "  Meta count:  4", "  File offset: 0x1000", "  [ 3] image.target.os (partition: os@spi-nand, offset=0x1009, size=7, crc=0xa56762bb, ram=0x0, attr='')"] {
        assert!(text.lines().any(|actual| actual == line), "missing {line:?} in {text:?}");
    }
    let plan = image.cli("burn", &["--plan"]);
    assert!(plan.status.success());
    assert_eq!(String::from_utf8(plan.stdout).unwrap().lines().collect::<Vec<_>>(), [
        "Image: D213 Offline v1.0.0 (4 components, 4112 bytes total)",
        "Burn plan:",
        "  updater: image.updater.ddr partition= media=ram size=4 crc=0x86451ae2 selected=true",
        "  image.info: image.info partition= media=ram size=2048 crc=0xe9bc1b28 selected=true",
        "  target: image.target.spl partition=spl media=spi-nand size=5 crc=0xcbea3cbd selected=true",
        "  target: image.target.os partition=os media=spi-nand size=7 crc=0xa56762bb selected=true",
        "  upgrade mode: 0x00",
    ]);
}

#[test]
fn cli_json_plan_matches_original_schema_and_partition_selection() {
    let image = ImageFile::new(&fixture::firmware());
    let output = image.cli("burn", &["--plan", "--json"]);
    assert!(output.status.success());
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        actual,
        json!({"mode": 0, "mode_name": "full-disk upgrade mode", "total_bytes": 2064, "components": [
            {"crc":"86451ae2","kind":"updater","media":"ram","name":"image.updater.ddr","partition":"","selected":true,"size":4},
            {"crc":"e9bc1b28","kind":"image.info","media":"ram","name":"image.info","partition":"","selected":true,"size":2048},
            {"crc":"cbea3cbd","kind":"target","media":"spi-nand","name":"image.target.spl","partition":"spl","selected":true,"size":5},
            {"crc":"a56762bb","kind":"target","media":"spi-nand","name":"image.target.os","partition":"os","selected":true,"size":7}
        ]})
    );
    let output = image.cli(
        "burn",
        &["--plan", "--json", "--no-updater", "--partition", "os"],
    );
    assert!(output.status.success());
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual["mode"], 1);
    assert_eq!(actual["total_bytes"], 2055);
    assert_eq!(actual["components"][0]["selected"], false);
    assert_eq!(actual["components"][2]["selected"], false);
}

#[test]
fn cli_checks_invalid_burn_before_using_device_or_vendor_program() {
    let image = ImageFile::new(&fixture::firmware());
    assert_eq!(
        image.cli("burn", &["--unknown-flag"]).status.code(),
        Some(2)
    );
    assert_eq!(
        image.cli("burn", &["--partition", "missing"]).status.code(),
        Some(2)
    );
    assert_eq!(
        image
            .cli("burn", &["--partition", "os", "--erase-all"])
            .status
            .code(),
        Some(2)
    );
    let mut corrupt = fixture::firmware();
    corrupt[4105] ^= 1;
    let output = ImageFile::new(&corrupt).cli("burn", &["--erase-all"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("image.target.os: CRC mismatch"));
    assert_eq!(ImageFile::new(&[]).cli("info", &[]).status.code(), Some(1));
}

#[test]
fn rejects_corrupt_image_layouts() {
    for bytes in [vec![], vec![0x13; 2047], vec![0xab; 4112]] {
        assert!(matches!(
            FirmwareImage::from_bytes(bytes),
            Err(FlashError::InvalidImage(_))
        ));
    }
    for (at, value) in [
        (332, 1),
        (332, u32::MAX),
        (336, 513),
        (336, u32::MAX - 511),
        (340, 4080),
        (344, u32::MAX),
        (2048, 0),
        (3584 + 136, 4100),
        (3584 + 136, 4094),
        (3584 + 136, u32::MAX),
        (3584 + 140, 0),
        (3584 + 140, u32::MAX),
    ] {
        let mut corrupt = fixture::firmware();
        corrupt[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert!(
            matches!(
                FirmwareImage::from_bytes(corrupt),
                Err(FlashError::InvalidImage(_))
            ),
            "at {at} value {value}"
        );
    }
}

#[test]
fn validates_all_component_crcs_partition_keys_and_storage_media() {
    let valid = ImageFile::new(&fixture::firmware());
    let image = FirmwareImage::open(&valid.0).unwrap();
    image.validate().unwrap();
    for (at, replacement, message) in [
        (3584 + 72, b"spl".as_slice(), "Duplicate partition"),
        (
            3584 + 112,
            b"unknown!".as_slice(),
            "unsupported component storage media",
        ),
        (4100, b"?".as_slice(), "CRC mismatch"),
    ] {
        let mut corrupt = fixture::firmware();
        corrupt[at..at + replacement.len()].copy_from_slice(replacement);
        let image = FirmwareImage::from_bytes(corrupt).unwrap();
        let mut options = image.default_options();
        options.selected_parts = vec!["os".into()];
        assert!(
            matches!(image.prepare(options), Err(FlashError::InvalidPlan(detail)) if detail.contains(message))
        );
    }
}
