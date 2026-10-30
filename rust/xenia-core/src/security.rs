//! Bounds-checked XEX2 security metadata parsing.

use core::fmt;

use crate::xex::XexHeader;

const SECURITY_FIXED_SIZE: usize = 0x184;
const PAGE_DESCRIPTOR_SIZE: usize = 0x18;
const PAGE_DESCRIPTOR_COUNT_OFFSET: usize = 0x180;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XexPageDescriptor {
    /// XEX section type encoded in the high four bits (1=code, 2=data, 3=read-only data).
    pub section_type: u8,
    pub page_count: u32,
    pub digest: [u8; 20],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XexSecurityInfo {
    pub header_size: u32,
    pub image_size: u32,
    pub info_size: u32,
    pub image_flags: u32,
    pub load_address: u32,
    pub import_table_count: u32,
    pub aes_key: [u8; 16],
    pub export_table: u32,
    pub region: u32,
    pub allowed_media_types: u32,
    pub page_descriptors: Vec<XexPageDescriptor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecurityInfoError {
    OffsetOutOfBounds,
    TruncatedFixedHeader,
    DescriptorTableOverflow,
    DescriptorTableOutOfBounds,
    InvalidSectionType,
    InvalidImageSize,
}

impl fmt::Display for SecurityInfoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OffsetOutOfBounds => f.write_str("XEX security-info offset is outside the file"),
            Self::TruncatedFixedHeader => {
                f.write_str("XEX2 security-info fixed header is truncated")
            }
            Self::DescriptorTableOverflow => {
                f.write_str("XEX page-descriptor table size overflows")
            }
            Self::DescriptorTableOutOfBounds => {
                f.write_str("XEX page-descriptor table exceeds file bounds")
            }
            Self::InvalidSectionType => {
                f.write_str("XEX page descriptor has an unknown section type")
            }
            Self::InvalidImageSize => f.write_str("XEX image size is zero"),
        }
    }
}

impl std::error::Error for SecurityInfoError {}

/// Parses the XEX2 security-info structure at the offset declared by its XEX header.
/// The caller must have already validated the XEX2 container header.
pub fn parse_xex2_security_info(
    bytes: &[u8],
    header: &XexHeader,
) -> Result<XexSecurityInfo, SecurityInfoError> {
    let start = header.security_info_offset as usize;
    let fixed_end = start
        .checked_add(SECURITY_FIXED_SIZE)
        .ok_or(SecurityInfoError::OffsetOutOfBounds)?;
    let fixed = bytes
        .get(start..fixed_end)
        .ok_or(SecurityInfoError::TruncatedFixedHeader)?;

    let header_size = read_u32_be(fixed, 0);
    let image_size = read_u32_be(fixed, 4);
    let info_size = read_u32_be(fixed, 0x108);
    let image_flags = read_u32_be(fixed, 0x10C);
    let load_address = read_u32_be(fixed, 0x110);
    let import_table_count = read_u32_be(fixed, 0x128);
    let mut aes_key = [0; 16];
    aes_key.copy_from_slice(&fixed[0x150..0x160]);
    let export_table = read_u32_be(fixed, 0x160);
    let region = read_u32_be(fixed, 0x178);
    let allowed_media_types = read_u32_be(fixed, 0x17C);
    let descriptor_count = read_u32_be(fixed, PAGE_DESCRIPTOR_COUNT_OFFSET) as usize;

    if image_size == 0 {
        return Err(SecurityInfoError::InvalidImageSize);
    }

    let descriptor_bytes = descriptor_count
        .checked_mul(PAGE_DESCRIPTOR_SIZE)
        .ok_or(SecurityInfoError::DescriptorTableOverflow)?;
    let descriptor_end = fixed_end
        .checked_add(descriptor_bytes)
        .ok_or(SecurityInfoError::DescriptorTableOverflow)?;
    let descriptor_table = bytes
        .get(fixed_end..descriptor_end)
        .ok_or(SecurityInfoError::DescriptorTableOutOfBounds)?;

    let mut page_descriptors = Vec::with_capacity(descriptor_count);
    for descriptor in descriptor_table.chunks_exact(PAGE_DESCRIPTOR_SIZE) {
        let encoded = read_u32_be(descriptor, 0);
        let section_type = (encoded >> 28) as u8;
        if !(1..=3).contains(&section_type) {
            return Err(SecurityInfoError::InvalidSectionType);
        }
        let mut digest = [0; 20];
        digest.copy_from_slice(&descriptor[4..24]);
        page_descriptors.push(XexPageDescriptor {
            section_type,
            page_count: encoded & 0x0FFF_FFFF,
            digest,
        });
    }

    Ok(XexSecurityInfo {
        header_size,
        image_size,
        info_size,
        image_flags,
        load_address,
        import_table_count,
        aes_key,
        export_table,
        region,
        allowed_media_types,
        page_descriptors,
    })
}

fn read_u32_be(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::{SecurityInfoError, parse_xex2_security_info};
    use crate::xex::XexHeader;

    fn put_u32_be(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn header() -> XexHeader {
        XexHeader {
            module_flags: 0,
            header_size: 0x40,
            reserved: 0,
            security_info_offset: 0x40,
            optional_headers: Vec::new(),
        }
    }

    fn fixture(page_count: u32) -> Vec<u8> {
        let mut bytes = vec![0; 0x40 + 0x184 + (page_count as usize * 0x18)];
        let base = 0x40;
        put_u32_be(&mut bytes, base, 0x184 + page_count * 0x18);
        put_u32_be(&mut bytes, base + 4, 0x2000);
        put_u32_be(&mut bytes, base + 0x108, 0x184);
        put_u32_be(&mut bytes, base + 0x10C, 0x2000_0000);
        put_u32_be(&mut bytes, base + 0x110, 0x8200_0000);
        put_u32_be(&mut bytes, base + 0x128, 3);
        bytes[base + 0x150..base + 0x160].copy_from_slice(&[0xA5; 16]);
        put_u32_be(&mut bytes, base + 0x160, 0x1234);
        put_u32_be(&mut bytes, base + 0x178, 0xFFFF_FFFF);
        put_u32_be(&mut bytes, base + 0x17C, 0x10);
        put_u32_be(&mut bytes, base + 0x180, page_count);
        if page_count != 0 {
            put_u32_be(&mut bytes, base + 0x184, (1 << 28) | 2);
            bytes[base + 0x188..base + 0x19C].copy_from_slice(&[0x5A; 20]);
        }
        bytes
    }

    #[test]
    fn parses_security_fields_and_page_descriptors() {
        let parsed = parse_xex2_security_info(&fixture(1), &header()).unwrap();
        assert_eq!(parsed.image_size, 0x2000);
        assert_eq!(parsed.load_address, 0x8200_0000);
        assert_eq!(parsed.aes_key, [0xA5; 16]);
        assert_eq!(parsed.page_descriptors.len(), 1);
        assert_eq!(parsed.page_descriptors[0].section_type, 1);
        assert_eq!(parsed.page_descriptors[0].page_count, 2);
        assert_eq!(parsed.page_descriptors[0].digest, [0x5A; 20]);
    }

    #[test]
    fn rejects_truncated_descriptor_table_and_zero_image() {
        let mut bytes = fixture(1);
        bytes.truncate(bytes.len() - 1);
        assert_eq!(
            parse_xex2_security_info(&bytes, &header()),
            Err(SecurityInfoError::DescriptorTableOutOfBounds)
        );
        let mut bytes = fixture(0);
        put_u32_be(&mut bytes, 0x44, 0);
        assert_eq!(
            parse_xex2_security_info(&bytes, &header()),
            Err(SecurityInfoError::InvalidImageSize)
        );
    }

    #[test]
    fn rejects_unknown_page_section_type() {
        let mut bytes = fixture(1);
        put_u32_be(&mut bytes, 0x40 + 0x184, 0xF000_0001);
        assert_eq!(
            parse_xex2_security_info(&bytes, &header()),
            Err(SecurityInfoError::InvalidSectionType)
        );
    }
}
