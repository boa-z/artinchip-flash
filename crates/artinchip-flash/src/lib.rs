//! ArtInChip firmware and diagnostic SDK with optional USB and UART support.

pub use aic_upg_core::{device, error, image, log_verbose, protocol, transport, verbosity};
pub use device::{BurnEvent, BurnOptions, BurnPlan, UpgDevice};
pub use error::FlashError;
pub use image::{FirmwareImage, PreparedFlash};
pub use transport::UpgTransport;
#[cfg(feature = "uart")]
pub use uart::{UartDevice, UartOptions};
#[cfg(feature = "usb")]
pub use usb::device::{AicDevice, DeviceSelector};

pub mod paths;
#[cfg(feature = "usb")]
pub mod storage;
#[cfg(feature = "uart")]
pub mod uart;
#[cfg(feature = "usb")]
pub mod usb;
