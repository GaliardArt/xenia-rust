//! Pure planning of PE section mappings before guest memory is mutated.

use crate::pe::XboxPeImage;
use core::fmt;

const EXECUTE: u32 = 0x2000_0000;
const READ: u32 = 0x4000_0000;
const WRITE: u32 = 0x8000_0000;
const ADDRESS_SPACE_END: u64 = (u32::MAX as u64) + 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedSection {
    pub name: [u8; 8],
    pub address: u32,
    pub size: u32,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

impl LoadedSection {
    fn end(&self) -> u64 {
        self.address as u64 + self.size as u64
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageLayout {
    pub load_base: u32,
    pub entry_point: u32,
    pub image_end_exclusive: u64,
    pub sections: Vec<LoadedSection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutError {
    AddressOverflow,
    OverlappingSections,
    EntryPointNotExecutable,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AddressOverflow => f.write_str("image section exceeds guest address space"),
            Self::OverlappingSections => f.write_str("image sections overlap"),
            Self::EntryPointNotExecutable => {
                f.write_str("entry point is not in executable section")
            }
        }
    }
}

impl std::error::Error for LayoutError {}

/// Computes guest addresses and permissions without mutating guest memory.
pub fn plan_image_layout(image: &XboxPeImage, load_base: u32) -> Result<ImageLayout, LayoutError> {
    let entry_point = load_base
        .checked_add(image.entry_point_rva)
        .ok_or(LayoutError::AddressOverflow)?;
    let mut sections: Vec<LoadedSection> = Vec::with_capacity(image.sections.len());
    let mut image_end_exclusive = load_base as u64;
    let mut entry_is_executable = false;

    for section in &image.sections {
        let address = load_base
            .checked_add(section.virtual_address)
            .ok_or(LayoutError::AddressOverflow)?;
        let mapped_size = section.virtual_size.max(section.raw_size);
        let end = address as u64 + mapped_size as u64;
        if end > ADDRESS_SPACE_END {
            return Err(LayoutError::AddressOverflow);
        }

        let loaded = LoadedSection {
            name: section.name,
            address,
            size: mapped_size,
            readable: section.characteristics & READ != 0,
            writable: section.characteristics & WRITE != 0,
            executable: section.characteristics & EXECUTE != 0,
        };

        if loaded.size != 0 {
            if sections.iter().any(|previous| {
                previous.size != 0
                    && (loaded.address as u64) < previous.end()
                    && (previous.address as u64) < loaded.end()
            }) {
                return Err(LayoutError::OverlappingSections);
            }
            if loaded.executable
                && (entry_point as u64) >= loaded.address as u64
                && (entry_point as u64) < loaded.end()
            {
                entry_is_executable = true;
            }
        }

        image_end_exclusive = image_end_exclusive.max(end);
        sections.push(loaded);
    }

    if !entry_is_executable {
        return Err(LayoutError::EntryPointNotExecutable);
    }

    Ok(ImageLayout {
        load_base,
        entry_point,
        image_end_exclusive,
        sections,
    })
}

#[cfg(test)]
mod tests {
    use super::{LayoutError, plan_image_layout};
    use crate::pe::{PeSection, XboxPeImage};

    fn section(name: &[u8], rva: u32, size: u32, flags: u32) -> PeSection {
        let mut section_name = [0; 8];
        section_name[..name.len()].copy_from_slice(name);
        PeSection {
            name: section_name,
            virtual_size: size,
            virtual_address: rva,
            raw_size: size,
            raw_pointer: 0,
            characteristics: flags,
        }
    }

    fn image(sections: Vec<PeSection>) -> XboxPeImage {
        XboxPeImage {
            entry_point_rva: 0x1000,
            image_base: 0x8200_0000,
            subsystem: 14,
            sections,
        }
    }

    #[test]
    fn computes_addresses_and_permissions_without_mapping() {
        let parsed = image(vec![
            section(b".text", 0x1000, 0x1000, 0x6000_0020),
            section(b".data", 0x3000, 0x800, 0xC000_0040),
        ]);
        let layout = plan_image_layout(&parsed, 0x8200_0000).unwrap();
        assert_eq!(layout.entry_point, 0x8200_1000);
        assert_eq!(layout.sections[0].address, 0x8200_1000);
        assert!(layout.sections[0].readable);
        assert!(layout.sections[0].executable);
        assert!(!layout.sections[0].writable);
        assert!(layout.sections[1].writable);
        assert_eq!(layout.image_end_exclusive, 0x8200_3800);
    }

    #[test]
    fn rejects_overlapping_sections() {
        let parsed = image(vec![
            section(b".text", 0x1000, 0x1000, 0x6000_0020),
            section(b".data", 0x1800, 0x100, 0xC000_0040),
        ]);
        assert_eq!(
            plan_image_layout(&parsed, 0x8200_0000),
            Err(LayoutError::OverlappingSections)
        );
    }

    #[test]
    fn rejects_address_overflow_and_non_executable_entry() {
        let parsed = image(vec![section(b".text", 0x1000, 0x100, 0x6000_0020)]);
        assert_eq!(
            plan_image_layout(&parsed, 0xFFFF_F000),
            Err(LayoutError::AddressOverflow)
        );
        let parsed = image(vec![section(b".data", 0x1000, 0x1000, 0xC000_0040)]);
        assert_eq!(
            plan_image_layout(&parsed, 0x8200_0000),
            Err(LayoutError::EntryPointNotExecutable)
        );
    }
}
