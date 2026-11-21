use crate::protocol::commands::{StorageGeometry, StorageMedia};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EraseScope {
    Region { offset: u64, length: u64 },
    Partition { offset: u64, length: u64 },
    WholeMedia,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErasePreview {
    pub media: StorageMedia,
    pub scope: EraseScope,
    pub offset_bytes: u64,
    pub length_bytes: u64,
    pub capacity_bytes: u64,
    pub alignment_bytes: u64,
}

pub fn preview_erase(
    media: StorageMedia,
    geometry: &StorageGeometry,
    mmc_sector_bytes: Option<u32>,
    scope: EraseScope,
) -> Result<ErasePreview, String> {
    let unit_bytes = match media.media_type.as_str() {
        "spi-nor" | "spi-nand" | "raw-nand" => 1,
        "mmc" => match mmc_sector_bytes {
            Some(sector_bytes @ (512 | 4096)) => u64::from(sector_bytes),
            _ => {
                return Err(
                    "MMC requires a confirmed logical sector size (512 or 4096 bytes)".into(),
                )
            }
        },
        other => return Err(format!("Cannot calculate erase range for media {other:?}")),
    };
    let capacity_bytes = geometry
        .total_size
        .checked_mul(unit_bytes)
        .ok_or("Capacity overflows bytes")?;
    let alignment_bytes = u64::from(geometry.erase_size) * unit_bytes;
    if alignment_bytes == 0
        || capacity_bytes == 0
        || !capacity_bytes.is_multiple_of(alignment_bytes)
    {
        return Err("Invalid capacity or erase granularity".into());
    }
    let (offset_bytes, length_bytes) = match scope {
        EraseScope::Region { offset, length } | EraseScope::Partition { offset, length } => {
            (offset, length)
        }
        EraseScope::WholeMedia => (0, capacity_bytes),
    };
    if length_bytes == 0
        || offset_bytes
            .checked_add(length_bytes)
            .is_none_or(|end| end > capacity_bytes)
    {
        return Err("Erase range is empty or outside the media capacity".into());
    }
    if !offset_bytes.is_multiple_of(alignment_bytes)
        || !length_bytes.is_multiple_of(alignment_bytes)
    {
        return Err(format!("Erase range must align to {alignment_bytes} bytes"));
    }
    Ok(ErasePreview {
        media,
        scope,
        offset_bytes,
        length_bytes,
        capacity_bytes,
        alignment_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nand_partition_and_whole_media_use_bytes() {
        let media = StorageMedia::new("spi-nand", 0);
        let geometry = StorageGeometry {
            total_size: 128 << 20,
            erase_size: 128 << 10,
            write_size: 2048,
            oob_size: 64,
        };
        let scope = EraseScope::Partition {
            offset: 2 << 20,
            length: 32 << 20,
        };
        let preview = preview_erase(media.clone(), &geometry, None, scope).unwrap();
        assert_eq!(
            (preview.offset_bytes, preview.length_bytes),
            (2 << 20, 32 << 20)
        );
        assert_eq!(
            preview_erase(media.clone(), &geometry, None, EraseScope::WholeMedia)
                .unwrap()
                .length_bytes,
            128 << 20
        );
        for (offset, length) in [
            (1, 128 << 10),
            (0, 0),
            (128 << 20, 128 << 10),
            (u64::MAX, 1),
        ] {
            assert!(preview_erase(
                media.clone(),
                &geometry,
                None,
                EraseScope::Region { offset, length }
            )
            .is_err());
        }
    }

    #[test]
    fn mmc_requires_confirmed_logical_sector_and_valid_capacity() {
        let media = StorageMedia::new("mmc", 0);
        let mut geometry = StorageGeometry {
            total_size: 4096,
            erase_size: 8,
            write_size: 512,
            oob_size: 0,
        };
        assert!(preview_erase(media.clone(), &geometry, None, EraseScope::WholeMedia).is_err());
        assert_eq!(
            preview_erase(media.clone(), &geometry, Some(512), EraseScope::WholeMedia)
                .unwrap()
                .alignment_bytes,
            4096
        );
        geometry.total_size = u64::MAX;
        assert!(preview_erase(media, &geometry, Some(512), EraseScope::WholeMedia).is_err());
    }
}
