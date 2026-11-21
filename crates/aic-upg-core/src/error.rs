//! Errors from image preparation and upgrade execution.

use std::{fmt, io, path::PathBuf};

/// Failure from the high-level firmware API.
#[derive(Debug)]
#[non_exhaustive]
pub enum FlashError {
    /// The image could not be read.
    ImageIo { path: PathBuf, source: io::Error },
    /// The image header, META table or payload layout is invalid.
    InvalidImage(String),
    /// The selected components or their integrity checks failed.
    InvalidPlan(String),
    /// A caller requested cancellation before the next safe operation.
    Cancelled,
    /// A device command or transport operation failed.
    Device {
        operation: &'static str,
        message: String,
    },
    /// The device checksum differs from the META checksum for a component.
    DeviceCrc {
        component: String,
        expected: u32,
        actual: u32,
    },
}

impl FlashError {
    pub(crate) fn device(operation: &'static str, message: String) -> Self {
        Self::Device { operation, message }
    }
}

impl fmt::Display for FlashError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ImageIo { path, source } => {
                write!(formatter, "Error reading '{}': {source}", path.display())
            }
            Self::InvalidImage(message) | Self::InvalidPlan(message) | Self::Device { message, .. } => {
                formatter.write_str(message)
            }
            Self::Cancelled => formatter.write_str("Burn cancelled by user"),
            Self::DeviceCrc { component, expected, actual } => write!(
                formatter, "{component}: device CRC mismatch (expected 0x{expected:08x}, actual 0x{actual:08x})"
            ),
        }
    }
}

impl std::error::Error for FlashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ImageIo { source, .. } => Some(source),
            _ => None,
        }
    }
}
