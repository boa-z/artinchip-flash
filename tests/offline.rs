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
        cli()
            .arg(command)
            .arg(&self.0)
            .args(flags)
            .output()
            .unwrap()
    }
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_artinchip-flash"))
}

fn json_stdout(output: &Output) -> Value {
    assert!(!output.stdout.contains(&0x1b));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_help_version_and_exit_contract() {
    let help = cli().arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Commands:"));
    let version = cli().arg("--version").output().unwrap();
    let version_text = String::from_utf8_lossy(&version.stdout);
    assert!(version.status.success());
    assert_eq!(
        version_text.lines().next(),
        Some(concat!("artinchip-flash ", env!("CARGO_PKG_VERSION")))
    );
    for field in ["build: ", "commit: ", "channel: ", "target: "] {
        assert!(version_text.lines().any(|line| line.starts_with(field)));
    }

    for args in [
        &["info", "--uart", "auto", "--device", "1:2"][..],
        &["readl", "bad", "--device", "1:2"],
        &["readl", "0", "--device", "bad:address"],
        &["readl", "0", "--json"],
        &["update", "--channel", "invalid"],
    ] {
        let output = cli().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn cli_queries_emit_single_json_objects_and_empty_scan_is_successful() {
    let image = ImageFile::new(&fixture::firmware());
    let info = image.cli("info", &["--json"]);
    assert!(info.status.success());
    let info = json_stdout(&info);
    assert_eq!(info["kind"], "image");
    assert_eq!(info["components"].as_array().unwrap().len(), 4);
    let scan = cli().args(["scan", "--json"]).output().unwrap();
    assert!(scan.status.success());
    let found = json_stdout(&scan);
    if found["devices"].as_array().unwrap().is_empty() {
        assert!(cli().arg("scan").output().unwrap().status.success());
        let required = cli()
            .args(["scan", "--json", "--require-device"])
            .output()
            .unwrap();
        assert_eq!(required.status.code(), Some(3));
        assert_eq!(json_stdout(&required), found);
        let info = cli().args(["info", "--json"]).output().unwrap();
        assert_eq!(info.status.code(), Some(3));
        assert!(info.stdout.is_empty());
    }
}

#[test]
fn burn_validation_emits_a_final_result_and_saves_the_failure_log() {
    let image = ImageFile::new(&[]);
    let log = image.0.with_extension("ndjson");
    let output = cli()
        .arg("burn")
        .arg(&image.0)
        .args(["--json", "--log-file"])
        .arg(&log)
        .output()
        .unwrap();
    let logged = fs::read(&log).unwrap();
    fs::remove_file(log).unwrap();
    assert_eq!(output.status.code(), Some(2));
    let result = json_stdout(&output);
    assert_eq!(result["type"], "result");
    assert_eq!(result["reset"], "not_attempted");
    assert_eq!(result["bytes_sent"], 0);
    assert_eq!(serde_json::from_slice::<Value>(&logged).unwrap(), result);
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
    assert_eq!(ImageFile::new(&[]).cli("info", &[]).status.code(), Some(2));
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
