use std::{fs, path::Path};

use super::parser::{parse_image, FwHeader, ImageSummary};
use crate::device::{
    default_burn_parts, is_cancelled, plan_burn, BurnCallback, BurnOptions, BurnPlan, UpgDevice,
};
use crate::error::FlashError;
use crate::protocol::commands::FwcMeta;
use crate::transport::UpgTransport;

/// An owned AIC.FW image with parsed metadata and checked container bounds.
pub struct FirmwareImage {
    data: Vec<u8>,
    header: FwHeader,
    metas: Vec<FwcMeta>,
    summary: ImageSummary,
}

impl FirmwareImage {
    /// Read and parse an image without contacting a device.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, FlashError> {
        let path = path.as_ref();
        let data = fs::read(path).map_err(|source| FlashError::ImageIo {
            path: path.to_path_buf(),
            source,
        })?;
        let mut image = Self::from_bytes(data)?;
        image.summary.path = Some(path.to_path_buf());
        Ok(image)
    }

    /// Parse an in-memory image. Use [`Self::validate`] to verify component integrity.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self, FlashError> {
        let (header, metas, _) = parse_image(&data).map_err(FlashError::InvalidImage)?;
        super::parser::validate_component_layout(&header, &metas, data.len())
            .map_err(FlashError::InvalidImage)?;
        let summary = ImageSummary::from_parts(None, data.len(), &header, &metas);
        Ok(Self {
            data,
            header,
            metas,
            summary,
        })
    }

    /// Return the image header without copying it.
    pub fn header(&self) -> &FwHeader {
        &self.header
    }

    /// Return the descriptive header and component metadata.
    pub fn summary(&self) -> &ImageSummary {
        &self.summary
    }

    /// Return burn options with the default partition selection for this image family.
    pub fn default_options(&self) -> BurnOptions {
        BurnOptions {
            selected_parts: default_burn_parts(&self.metas),
            ..Default::default()
        }
    }

    /// Validate all components, including their stored CRC32 and partition metadata, offline.
    pub fn validate(&self) -> Result<(), FlashError> {
        let options = BurnOptions {
            selected_parts: self
                .metas
                .iter()
                .filter(|meta| meta.name_str().starts_with("image.target."))
                .map(|meta| meta.name_str().to_owned())
                .collect(),
            ..Default::default()
        };
        self.prepare(options).map(|_| ())
    }

    /// Verify component integrity and build a plan for the requested selection without device I/O.
    pub fn prepare(&self, options: BurnOptions) -> Result<PreparedFlash<'_>, FlashError> {
        if options.partition_upgrade && options.force_upgrade {
            return Err(FlashError::InvalidPlan(
                "Partition upgrade cannot be combined with force upgrade".into(),
            ));
        }
        let media = self.header.media_type_str();
        if !matches!(media, "" | "spi-nand" | "spi-nor" | "mmc") {
            return Err(FlashError::InvalidPlan(format!(
                "Unsupported image storage media '{media}'"
            )));
        }
        let plan = plan_burn(&self.data, &self.metas, &options).map_err(FlashError::InvalidPlan)?;
        Ok(PreparedFlash {
            image: self,
            options,
            plan,
        })
    }
}

/// A checked write plan bound to immutable image bytes and the options that produced it.
pub struct PreparedFlash<'image> {
    image: &'image FirmwareImage,
    options: BurnOptions,
    plan: BurnPlan,
}

impl PreparedFlash<'_> {
    /// Return the header of the image that this plan will write.
    pub fn header(&self) -> &FwHeader {
        self.image.header()
    }

    /// Inspect mode, selected components and byte count before opening a device.
    pub fn plan(&self) -> &BurnPlan {
        &self.plan
    }

    /// Read the validated upgrade options.
    pub fn options(&self) -> &BurnOptions {
        &self.options
    }

    /// Return a cancellation error if the caller has set the shared cancellation flag.
    pub fn check_cancelled(&self) -> Result<(), FlashError> {
        if is_cancelled(&self.options) {
            Err(FlashError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Verify the expected boot stage, then run synchronously and emit progress events.
    /// Use [`BurnOptions::cancel`] to request cancellation at a transfer boundary.
    pub fn execute<T: UpgTransport>(
        &self,
        device: &mut UpgDevice<T>,
        callback: Option<&mut BurnCallback<'_>>,
    ) -> Result<(), FlashError> {
        device.burn_planned(
            &self.image.data,
            &self.image.metas,
            &self.options,
            &self.plan,
            true,
            callback,
        )
    }
}
