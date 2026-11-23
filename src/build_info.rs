pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILD: &str = env!("ARTINCHIP_FLASH_BUILD");
pub const COMMIT: &str = env!("ARTINCHIP_FLASH_COMMIT");
pub const CHANNEL: &str = env!("ARTINCHIP_FLASH_BUILD_CHANNEL");
pub const TARGET: &str = env!("ARTINCHIP_FLASH_TARGET");
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

pub const VERSION_WITH_BUILD: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (build ",
    env!("ARTINCHIP_FLASH_BUILD"),
    ", ",
    env!("ARTINCHIP_FLASH_BUILD_CHANNEL"),
    ")"
);

pub const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nbuild: ",
    env!("ARTINCHIP_FLASH_BUILD"),
    "\ncommit: ",
    env!("ARTINCHIP_FLASH_COMMIT"),
    "\nchannel: ",
    env!("ARTINCHIP_FLASH_BUILD_CHANNEL"),
    "\ntarget: ",
    env!("ARTINCHIP_FLASH_TARGET")
);
