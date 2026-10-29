//! Defensive parser for the Xbox 360 PE32 header profile used inside XEX images.
//!
//! The parser only reads metadata. It does not map sections or assume that PE
//! raw-file offsets describe the already-decompressed XEX image layout.

use core::fmt;

const DOS_HEADER_MIN_SIZE: usize = 0x40;
const NT_FILE_HEADER_SIZE: usize = 20;
const PE32_OPTIONAL_HEADER_SIZE: usize = 224;
const SECTION_HEADER_SIZE: usize = 40;
const IMAGE_FILE_MACHINE_POWERPCBE: u16 = 0x01F2;
const IMAGE_FILE_32BIT_MACHINE: u16 = 0x0100;
const IMAGE_NT_OPTIONAL_HDR32_MAGIC: u16 = 0x010B;
const IMAGE_SUBSYSTEM_XBOX: u16 = 14;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeSection {
    pub name: [u8; 8],
    pub virtual_size: u32,
    pub virtual_address: u32,
    pub raw_size: u32,
    pub raw_pointer: u32,
    pub characteristics: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XboxPeImage {
    pub entry_point_rva: u32,
    pub image_base: u32,
    pub subsystem: u16,
    pub sections: Vec<PeSection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeError {
    TruncatedDosHeader,
    InvalidDosMagic,
    InvalidNtOffset,
    TruncatedNtHeaders,
    InvalidNtSignature,
    UnsupportedMachine,
    Not32BitImage,
    InvalidOptionalHeaderSize,
    InvalidOptionalHeaderMagic,
    UnsupportedSubsystem,
    SectionTableOutOfBounds,
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedDosHeader => f.write_str("PE DOS header is truncated"),
            Self::InvalidDosMagic => f.write_str("invalid PE DOS signature"),
            Self::InvalidNtOffset => f.write_str("PE NT header offset is invalid"),
            Self::TruncatedNtHeaders => f.write_str("PE NT headers are truncated"),
            Self::InvalidNtSignature => f.write_str("invalid PE NT signature"),
            Self::UnsupportedMachine => f.write_str("PE machine is not Xbox 360 PowerPCBE"),
            Self::Not32BitImage => f.write_str("PE image is not marked as 32-bit"),
            Self::InvalidOptionalHeaderSize => f.write_str("invalid PE32 optional-header size"),
            Self::InvalidOptionalHeaderMagic => f.write_str("invalid PE32 optional-header magic"),
            Self::UnsupportedSubsystem => f.write_str("PE subsystem is not Xbox"),
            Self::SectionTableOutOfBounds => f.write_str("PE section table exceeds image bounds"),
        }
    }
}

impl std::error::Error for PeError {}

/// Parses and validates the PE32 header profile expected for an Xbox 360 image.
pub fn parse_xbox_pe32(bytes: &[u8]) -> Result<XboxPeImage, PeError> {
    if bytes.len() < DOS_HEADER_MIN_SIZE {
        return Err(PeError::TruncatedDosHeader);
    }
    if bytes.get(0..2) != Some(b"MZ") {
        return Err(PeError::InvalidDosMagic);
    }

    let nt_offset = read_u32_le(bytes, 0x3C)? as usize;
    if nt_offset < DOS_HEADER_MIN_SIZE {
        return Err(PeError::InvalidNtOffset);
    }

    let nt_signature = get_range(bytes, nt_offset, 4).map_err(|_| PeError::TruncatedNtHeaders)?;
    if nt_signature != b"PE\0\0" {
        return Err(PeError::InvalidNtSignature);
    }

    let file_header_offset = nt_offset + 4;
    let file_header = get_range(bytes, file_header_offset, NT_FILE_HEADER_SIZE)
        .map_err(|_| PeError::TruncatedNtHeaders)?;
    let machine = read_u16_le(file_header, 0).map_err(|_| PeError::TruncatedNtHeaders)?;
    let section_count = read_u16_le(file_header, 2).map_err(|_| PeError::TruncatedNtHeaders)? as usize;
    let optional_header_size =
        read_u16_le(file_header, 16).map_err(|_| PeError::TruncatedNtHeaders)? as usize;
    let characteristics =
        read_u16_le(file_header, 18).map_err(|_| PeError::TruncatedNtHeaders)?;

    if machine != IMAGE_FILE_MACHINE_POWERPCBE {
        return Err(PeError::UnsupportedMachine);
    }
    if characteristics & IMAGE_FILE_32BIT_MACHINE == 0 {
        return Err(PeError::Not32BitImage);
    }
    if optional_header_size != PE32_OPTIONAL_HEADER_SIZE {
        return Err(PeError::InvalidOptionalHeaderSize);
    }

    let optional_offset = file_header_offset + NT_FILE_HEADER_SIZE;
    let optional_header = get_range(bytes, optional_offset, optional_header_size)
        .map_err(|_| PeError::TruncatedNtHeaders)?;
    if read_u16_le(optional_header, 0).map_err(|_| PeError::TruncatedNtHeaders)?
        != IMAGE_NT_OPTIONAL_HDR32_MAGIC
    {
        return Err(PeError::InvalidOptionalHeaderMagic);
    }

    let entry_point_rva =
        read_u32_le(optional_header, 16).map_err(|_| PeError::TruncatedNtHeaders)?;
    let image_base = read_u32_le(optional_header, 28).map_err(|_| PeError::TruncatedNtHeaders)?;
    let subsystem = read_u16_le(optional_header, 68).map_err(|_| PeError::TruncatedNtHeaders)?;
    if subsystem != IMAGE_SUBSYSTEM_XBOX {
        return Err(PeError::UnsupportedSubsystem);
    }

    let section_table_offset = optional_offset + optional_header_size;
    let section_table_size = section_count
        .checked_mul(SECTION_HEADER_SIZE)
        .ok_or(PeError::SectionTableOutOfBounds)?;
    let section_table = get_range(bytes, section_table_offset, section_table_size)
        .map_err(|_| PeError::SectionTableOutOfBounds)?;

    let mut sections = Vec::with_capacity(section_count);
    for index in 0..section_count {
        let offset = index * SECTION_HEADER_SIZE;
        let header = &section_table[offset..offset + SECTION_HEADER_SIZE];
        let mut name = [0; 8];
        name.copy_from_slice(&header[0..8]);
        sections.push(PeSection {
            name,
            virtual_size: read_u32_le(header, 8).map_err(|_| PeError::SectionTableOutOfBounds)?,
            virtual_address: read_u32_le(header, 12).map_err(|_| PeError::SectionTableOutOfBounds)?,
            raw_size: read_u32_le(header, 16).map_err(|_| PeError::SectionTableOutOfBounds)?,
            raw_pointer: read_u32_le(header, 20).map_err(|_| PeError::SectionTableOutOfBounds)?,
            characteristics: read_u32_le(header, 36)
                .map_err(|_| PeError::SectionTableOutOfBounds)?,
        });
    }

    Ok(XboxPeImage {
        entry_point_rva,
        image_base,
        subsystem,
        sections,
    })
}

fn get_range(bytes: &[u8], offset: usize, size: usize) -> Result<&[u8], PeError> {
    let end = offset.checked_add(size).ok_or(PeError::TruncatedNtHeaders)?;
    bytes.get(offset..end).ok_or(PeError::TruncatedNtHeaders)
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16, PeError> {
    let slice = get_range(bytes, offset, 2)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, PeError> {
    let slice = get_range(bytes, offset, 4)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

#[cfg(test)]
mod tests {
    use super::{PeError, parse_xbox_pe32};

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn valid_image() -> Vec<u8> {
        let nt = 0x80;
        let file_header = nt + 4;
        let optional = file_header + 20;
        let section = optional + 224;
        let mut bytes = vec![0; section + 40];
        bytes[0..2].copy_from_slice(b"MZ");
        put_u32(&mut bytes, 0x3C, nt as u32);
        bytes[nt..nt + 4].copy_from_slice(b"PE\0\0");
        put_u16(&mut bytes, file_header, 0x01F2);
        put_u16(&mut bytes, file_header + 2, 1);
        put_u16(&mut bytes, file_header + 16, 224);
        put_u16(&mut bytes, file_header + 18, 0x0100);
        put_u16(&mut bytes, optional, 0x010B);
        put_u32(&mut bytes, optional + 16, 0x1000);
        put_u32(&mut bytes, optional + 28, 0x8200_0000);
        put_u16(&mut bytes, optional + 68, 14);
        bytes[section..section + 5].copy_from_slice(b".text");
        put_u32(&mut bytes, section + 8, 0x100);
        put_u32(&mut bytes, section + 12, 0x1000);
        put_u32(&mut bytes, section + 16, 0x100);
        put_u32(&mut bytes, section + 20, 0x200);
        put_u32(&mut bytes, section + 36, 0x6000_0020);
        bytes
    }

    #[test]
    fn parses_xbox_powerpc_pe32_and_sections() {
        let parsed = parse_xbox_pe32(&valid_image()).unwrap();
        assert_eq!(parsed.entry_point_rva, 0x1000);
        assert_eq!(parsed.image_base, 0x8200_0000);
        assert_eq!(parsed.subsystem, 14);
        assert_eq!(parsed.sections.len(), 1);
        assert_eq!(&parsed.sections[0].name[..5], b".text");
        assert_eq!(parsed.sections[0].virtual_address, 0x1000);
        assert_eq!(parsed.sections[0].characteristics, 0x6000_0020);
    }

    #[test]
    fn rejects_truncated_and_invalid_dos_headers() {
        assert_eq!(parse_xbox_pe32(b"MZ"), Err(PeError::TruncatedDosHeader));
        let mut bytes = valid_image();
        bytes[0] = b'N';
        assert_eq!(parse_xbox_pe32(&bytes), Err(PeError::InvalidDosMagic));
    }

    #[test]
    fn rejects_wrong_machine_and_subsystem() {
        let mut bytes = valid_image();
        put_u16(&mut bytes, 0x84, 0x014C);
        assert_eq!(parse_xbox_pe32(&bytes), Err(PeError::UnsupportedMachine));

        let mut bytes = valid_image();
        put_u16(&mut bytes, 0x80 + 4 + 20 + 68, 3);
        assert_eq!(parse_xbox_pe32(&bytes), Err(PeError::UnsupportedSubsystem));
    }

    #[test]
    fn rejects_section_table_outside_image() {
        let mut bytes = valid_image();
        put_u16(&mut bytes, 0x80 + 4 + 2, 0xFFFF);
        assert_eq!(
            parse_xbox_pe32(&bytes),
            Err(PeError::SectionTableOutOfBounds)
        );
    }
}
