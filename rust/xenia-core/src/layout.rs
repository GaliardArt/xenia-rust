//! Pure planning of PE section mappings before guest memory is mutated.

use core::fmt;
use crate::pe::XboxPeImage;

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
            Self::EntryPointNotExecutable => f.write_str("entry point is not in executable section"),
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
        let end = address as u64 + section.virtual_size as u64;
        if end > ADDRESS_SPACE_END {
            return Err(LayoutError::AddressOverflow);
        }

        let loaded = LoadedSection {
            name: section.name,
            address,
            size: section.virtual_size,
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

    Ok(ImageLayout { load_base, entry_point, image_end_exclusive, sections })
}
