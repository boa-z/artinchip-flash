use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use crate::protocol::commands::{
    UPG_MODE_BURN_IMG_FORCE, UPG_MODE_FULL_DISK_UPGRADE, UPG_MODE_PARTITION_UPGRADE,
};

#[derive(Clone, Debug)]
/// Partition selection, upgrade mode and execution options.
pub struct BurnOptions {
    /// Target META names or their unambiguous partition aliases.
    pub selected_parts: Vec<String>,
    /// Send the reset command after ending the upgrade, when the mode permits it.
    pub reset_after_burn: bool,
    /// Time allowed for the bootloader reconnection after the updater.
    pub burn_timeout: Duration,
    /// Cooperative cancellation. When the flag is set, the burn loop aborts
    /// at the next chunk boundary and returns a "cancelled" error, leaving
    /// the device in upgrade mode so the user can retry.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Experimental force upgrade (AiBurn "强制升级").
    ///
    /// Uses `UPG_MODE_BURN_IMG_FORCE` instead of `UPG_MODE_FULL_DISK_UPGRADE`
    /// and skips the post-burn reset (the two are mutually exclusive).
    /// Requires the device-side force-upgrade switch (Luban/Luban-Lite
    /// config, official manual §2.1.4); not verified on hardware.
    pub force_upgrade: bool,
    /// Send `UPG_MODE_PARTITION_UPGRADE` instead of the full-disk mode.
    ///
    /// The D213 bootloader only stores/logs this byte; the actual write
    /// scope is the host-side `selected_parts` list.  This flag records the
    /// user's intent and is asserted by the wire tests.
    pub partition_upgrade: bool,
    /// Skip the updater components.  Use when the device is already in the
    /// vendor bootloader upgrade mode (`aicupg gotobl`); this avoids the
    /// BROM updater re-enumeration step.
    pub no_updater: bool,
}

impl Default for BurnOptions {
    fn default() -> Self {
        Self {
            selected_parts: ["spl", "env", "os"]
                .iter()
                .map(|part| (*part).to_string())
                .collect(),
            reset_after_burn: true,
            burn_timeout: Duration::from_secs(60),
            cancel: None,
            force_upgrade: false,
            partition_upgrade: false,
            no_updater: false,
        }
    }
}

/// Upgrade-mode byte for `SET_UPG_CFG` derived from burn options.
///
/// Pure helper so frontends and tests can assert the mode without a device.
pub fn upg_mode(options: &BurnOptions) -> u8 {
    if options.force_upgrade {
        UPG_MODE_BURN_IMG_FORCE
    } else if options.partition_upgrade {
        UPG_MODE_PARTITION_UPGRADE
    } else {
        UPG_MODE_FULL_DISK_UPGRADE
    }
}

/// Human-readable upgrade-mode name for stage logs.
pub fn upg_mode_name(options: &BurnOptions) -> &'static str {
    mode_name(upg_mode(options))
}

pub(super) fn mode_name(mode: u8) -> &'static str {
    match mode {
        UPG_MODE_BURN_IMG_FORCE => "force upgrade mode (BURN_IMG_FORCE, experimental)",
        UPG_MODE_PARTITION_UPGRADE => "partition upgrade mode",
        _ => "full-disk upgrade mode",
    }
}

/// Report the current state of the optional cooperative cancellation flag.
pub fn is_cancelled(options: &BurnOptions) -> bool {
    options
        .cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
}
