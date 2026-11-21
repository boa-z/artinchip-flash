use super::options::{mode_name, upg_mode, BurnOptions};
use crate::image::parser::FwHeader;
use crate::protocol::commands::{FwcMeta, FWC_META_SIZE, UPG_MODE_PARTITION_UPGRADE};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ComponentKind {
    Updater,
    ImageInfo,
    Target,
    Other,
}

pub(super) struct FirmwareComponent<'a> {
    pub(super) meta: &'a FwcMeta,
    pub(super) kind: ComponentKind,
    pub(super) selected: bool,
}

pub(super) fn classify_components<'a>(
    metas: &'a [FwcMeta],
    selected_parts: &[String],
) -> Vec<FirmwareComponent<'a>> {
    metas
        .iter()
        .map(|meta| {
            let name = meta.name_str();
            let kind = if name.starts_with("image.updater.") {
                ComponentKind::Updater
            } else if name == "image.info" {
                ComponentKind::ImageInfo
            } else if name.starts_with("image.target.") {
                ComponentKind::Target
            } else {
                ComponentKind::Other
            };
            let selected =
                kind != ComponentKind::Target || target_part_selected(meta, selected_parts);
            FirmwareComponent {
                meta,
                kind,
                selected,
            }
        })
        .collect()
}

fn target_part_selected(meta: &FwcMeta, selected_parts: &[String]) -> bool {
    selected_parts
        .iter()
        .any(|part| target_part_matches(meta, part))
}

fn target_part_matches(meta: &FwcMeta, part: &str) -> bool {
    let part = part.trim();
    let partition = meta.partition_str();
    let target_name = meta
        .name_str()
        .strip_prefix("image.target.")
        .unwrap_or_else(|| meta.name_str());
    partition == part || target_name == part || meta.name_str() == part
}

/// Match the default application selection for RT-Thread and Luban Linux images.
pub fn default_burn_target(name: &str, media: &str) -> bool {
    name.strip_prefix("image.target.")
        .is_some_and(|part| media.is_empty() || ["spl", "env", "os"].contains(&part))
}

/// Return fully qualified META names for the default target selection.
pub fn default_burn_parts(metas: &[FwcMeta]) -> Vec<String> {
    metas
        .iter()
        .filter(|meta| default_burn_target(meta.name_str(), meta.partition_media()))
        .map(|meta| meta.name_str().to_string())
        .collect()
}

/// One component in a validated host-side burn plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BurnPlanComponent {
    pub kind: &'static str,
    pub name: String,
    pub partition: String,
    pub media: String,
    pub size: u32,
    pub crc: u32,
    pub selected: bool,
}

/// Validated host-side burn plan.
///
/// The `mode` byte is informational to the D213 bootloader (it only stores
/// and logs `SET_UPG_CFG`); the selected target components are the real
/// write-scope filter.  See `nand_fwc.c`/`fwc_cmd.c` in Luban-Lite:
/// `set_fwc_meta_cmd_end` resolves only the FWC's own partition and
/// `nand_fwc_start` parses only that partition string.
#[derive(Clone, Debug)]
pub struct BurnPlan {
    pub components: Vec<BurnPlanComponent>,
    pub mode: u8,
    pub total_bytes: usize,
    pub(super) uses_header_media: bool,
}

impl BurnPlan {
    /// Return the descriptive name of the selected upgrade mode.
    pub fn mode_name(&self) -> &'static str {
        mode_name(self.mode)
    }

    /// Render the stable offline plan schema used by the applications.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        let components = self.components.iter().map(|component| json!({
            "kind": component.kind, "name": component.name, "partition": component.partition,
            "media": component.media, "size": component.size, "crc": format!("{:08x}", component.crc),
            "selected": component.selected,
        })).collect::<Vec<_>>();
        json!({ "mode": self.mode, "mode_name": self.mode_name(), "total_bytes": self.total_bytes, "components": components })
    }
}

/// Validate the image and selection, and build the exact component plan.
///
/// This never touches a device: callers can print the plan before burning.
pub fn plan_burn(
    img_data: &[u8],
    metas: &[FwcMeta],
    options: &BurnOptions,
) -> Result<BurnPlan, String> {
    if options.selected_parts.is_empty() {
        return Err("No target partitions selected".to_string());
    }
    let mut seen = std::collections::BTreeSet::new();
    for part in &options.selected_parts {
        let part = part.trim();
        if part.is_empty() {
            return Err("Empty partition name in selection".to_string());
        }
        if !seen.insert(part.to_string()) {
            return Err(format!("Duplicate partition '{}' in selection", part));
        }
    }

    if metas.iter().any(|meta| meta.bytes.len() != FWC_META_SIZE) {
        return Err("Invalid META entry length".into());
    }
    let classified = classify_components(metas, &options.selected_parts);
    let header = FwHeader::from_bytes(img_data);
    let image_media = header
        .as_ref()
        .filter(|header| header.magic_str() == "AIC.FW")
        .map_or("", FwHeader::media_type_str);
    let mut uses_header_media = false;
    let mut info_is_header = false;
    let mut names = std::collections::BTreeSet::new();
    let mut target_parts = Vec::new();
    let mut partition_keys = std::collections::BTreeSet::new();
    let mut info_count = 0usize;
    let mut selected_count = 0usize;
    let mut total: usize = 0;
    let mut components = Vec::with_capacity(classified.len());

    for component in &classified {
        let meta = component.meta;
        let name = meta.name_str();
        if name.is_empty() {
            return Err("META entry with an empty name".to_string());
        }
        if !names.insert(name.to_string()) {
            return Err(format!("Duplicate META component '{}'", name));
        }
        if meta.magic_str() != "META" {
            return Err(format!(
                "{}: invalid META magic '{}'",
                name,
                meta.magic_str()
            ));
        }
        let size = meta.size_val();
        let offset = meta.offset_val() as usize;
        let end = offset
            .checked_add(size as usize)
            .ok_or_else(|| format!("{}: offset/size overflow", name))?;
        if size == 0 || end > img_data.len() {
            return Err(format!(
                "{}: image range out of bounds (offset={:#x}, size={}, image_len={})",
                name,
                offset,
                size,
                img_data.len()
            ));
        }
        let actual = crc32fast::hash(&img_data[offset..end]);
        if actual != meta.crc_val() {
            return Err(format!(
                "{}: CRC mismatch (expected 0x{:08x}, actual 0x{:08x})",
                name,
                meta.crc_val(),
                actual
            ));
        }

        let mut media = meta.partition_media();
        let kind = match component.kind {
            ComponentKind::Updater => {
                if !media.is_empty() && media != "ram" {
                    return Err(format!("{}: updater must be a RAM component", name));
                }
                "updater"
            }
            ComponentKind::ImageInfo => {
                info_count += 1;
                info_is_header =
                    offset == 0 && size as usize == crate::image::parser::FW_HEADER_SIZE;
                if !media.is_empty() && media != "ram" {
                    return Err(format!("{}: image.info must be a RAM component", name));
                }
                "image.info"
            }
            ComponentKind::Target => {
                if meta.partition_str().is_empty() {
                    return Err(format!("{}: target has no partition name", name));
                }
                if meta.uses_image_media() {
                    if !matches!(image_media, "spi-nand" | "spi-nor" | "mmc") {
                        return Err(format!(
                            "{}: unsupported image storage media '{}'",
                            name, image_media
                        ));
                    }
                    media = image_media;
                    uses_header_media = true;
                } else if media.is_empty() || media == "ram" {
                    return Err(format!("{}: target has no storage media", name));
                }
                if !matches!(media, "spi-nand" | "spi-nor" | "mmc") {
                    return Err(format!(
                        "{name}: unsupported component storage media '{media}'"
                    ));
                }
                if !partition_keys.insert((media, meta.partition_str())) {
                    return Err(format!(
                        "Duplicate partition '{}' on {media}",
                        meta.partition_str()
                    ));
                }
                target_parts.push(meta);
                if component.selected {
                    selected_count += 1;
                }
                "target"
            }
            ComponentKind::Other => {
                return Err(format!("{}: unsupported META component", name));
            }
        };
        let selected = component.selected && !(options.no_updater && kind == "updater");
        if selected {
            total = total
                .checked_add(size as usize)
                .ok_or("Selected component sizes overflow")?;
        }
        components.push(BurnPlanComponent {
            kind,
            name: name.to_string(),
            partition: meta.partition_str().to_string(),
            media: media.to_string(),
            size,
            crc: meta.crc_val(),
            selected,
        });
    }

    if info_count != 1 {
        return Err(format!(
            "image.info must appear exactly once (found {})",
            info_count
        ));
    }
    if uses_header_media && !info_is_header {
        return Err(
            "image.info must reference the image header for header-media targets".to_string(),
        );
    }
    for part in &options.selected_parts {
        let part = part.trim();
        let matches = target_parts
            .iter()
            .filter(|target| target_part_matches(target, part))
            .count();
        if matches == 0 {
            return Err(format!(
                "Unknown target partition '{}'; available: {}",
                part,
                target_parts
                    .iter()
                    .map(|meta| meta.name_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if matches > 1 {
            return Err(format!(
                "Ambiguous target partition '{}' ({} matching components)",
                part, matches
            ));
        }
    }
    if selected_count == 0 {
        return Err("No target components selected".to_string());
    }

    let mode = if uses_header_media && selected_count < target_parts.len() {
        if options.force_upgrade {
            return Err(
                "Linux partition selection cannot be combined with force upgrade".to_string(),
            );
        }
        UPG_MODE_PARTITION_UPGRADE
    } else {
        upg_mode(options)
    };
    Ok(BurnPlan {
        components,
        mode,
        total_bytes: total,
        uses_header_media,
    })
}

/// Render a plan using the CLI text layout.
pub fn format_plan_lines(plan: &BurnPlan) -> Vec<String> {
    let mut lines = vec!["Burn plan:".to_string()];
    for component in &plan.components {
        lines.push(format!(
            "  {}: {} partition={} media={} size={} crc=0x{:08x} selected={}",
            component.kind,
            component.name,
            component.partition,
            component.media,
            component.size,
            component.crc,
            component.selected
        ));
    }
    lines.push(format!("  upgrade mode: 0x{:02x}", plan.mode));
    lines
}

/// Require all target components to be selected before whole-chip erase.
pub fn validate_erase_plan(plan: &BurnPlan, erase_all: bool) -> Result<(), String> {
    if erase_all
        && (plan.mode == UPG_MODE_PARTITION_UPGRADE
            || plan
                .components
                .iter()
                .any(|component| component.kind == "target" && !component.selected))
    {
        return Err("Partition upgrade cannot be combined with full-chip erase".to_string());
    }
    Ok(())
}
