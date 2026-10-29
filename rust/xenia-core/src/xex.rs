//! Defensive parser for the fixed XEX2 header and optional-header table.
//!
//! This module only reads metadata. It does not decrypt/decompress the image,
//! parse the embedded PE, map sections, or prepare an executable for boot.

use core::fmt;

const XEX2_MAGIC: u32 = u32::from_be_bytes(*b"XEX2");
const FIXED_HEADER_SIZE: usize = 24;
const OPTIONAL_HEADER_ENTRY_SIZE: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XexOptionalHeader {
    pub key: u32,
    pub value: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XexHeader {
    pub module_flags: u32,
    pub header_size: u32,
    pub reserved: u32,
    pub security_info_offset: u32,
    pub optional_headers: Vec<XexOptionalHeader>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XexError {
    TruncatedHeader,
    InvalidMagic,
    OptionalHeaderTableOutOfBounds,
    InvalidHeaderSize,
}

impl fmt::Display for XexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedHeader => f.write_str("XEX2 fixed header is truncated"),
            Self::InvalidMagic => f.write_str("invalid XEX2 magic"),
            Self::OptionalHeaderTableOutOfBounds => {
                f.write_str("XEX2 optional-header table exceeds file bounds")
            }
            Self::InvalidHeaderSize => f.write_str("invalid XEX2 header size"),
        }
    }
}

impl std::error::Error for XexError {}

/// Parses the XEX2 fixed header and optional-header entries from a file image.
pub fn parse_xex2_header(bytes: &[u8]) -> Result<XexHeader, XexError> {
    if bytes.len() < FIXED_HEADER_SIZE {
        return Err(XexError::TruncatedHeader);
    }

    if read_u32_be(bytes, 0)? != XEX2_MAGIC {
        return Err(XexError::InvalidMagic);
    }

    let module_flags = read_u32_be(bytes, 4)?;
    let header_size = read_u32_be(bytes, 8)?;
    let reserved = read_u32_be(bytes, 12)?;
    let security_info_offset = read_u32_be(bytes, 16)?;
    let optional_header_count = read_u32_be(bytes, 20)? as usize;

    let table_size = optional_header_count
        .checked_mul(OPTIONAL_HEADER_ENTRY_SIZE)
        .ok_or(XexError::OptionalHeaderTableOutOfBounds)?;
    let table_end = FIXED_HEADER_SIZE
        .checked_add(table_size)
        .ok_or(XexError::OptionalHeaderTableOutOfBounds)?;
    if table_end > bytes.len() {
        return Err(XexError::OptionalHeaderTableOutOfBounds);
    }
    if (header_size as usize) < table_end || (header_size as usize) > bytes.len() {
        return Err(XexError::InvalidHeaderSize);
    }

    let mut optional_headers = Vec::with_capacity(optional_header_count);
    for index in 0..optional_header_count {
        let entry_offset = FIXED_HEADER_SIZE + index * OPTIONAL_HEADER_ENTRY_SIZE;
        optional_headers.push(XexOptionalHeader {
            key: read_u32_be(bytes, entry_offset)?,
            value: read_u32_be(bytes, entry_offset + 4)?,
        });
    }

    Ok(XexHeader {
        module_flags,
        header_size,
        reserved,
        security_info_offset,
        optional_headers,
    })
}

fn read_u32_be(bytes: &[u8], offset: usize) -> Result<u32, XexError> {
    let end = offset.checked_add(4).ok_or(XexError::TruncatedHeader)?;
    let slice = bytes.get(offset..end).ok_or(XexError::TruncatedHeader)?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

#[cfg(test)]
mod tests {
    use super::{XexError, parse_xex2_header};

    fn header_with_count(count: u32) -> Vec<u8> {
        let mut bytes = Vec::from(*b"XEX2");
        for value in [0x1234, 0x40, 0, 0x30, count] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.resize(0x40, 0);
        bytes
    }

    #[test]
    fn parses_fixed_header_and_optional_headers() {
        let mut bytes = header_with_count(1);
        bytes[24..28].copy_from_slice(&0x0001_0002_u32.to_be_bytes());
        bytes[28..32].copy_from_slice(&0x0000_0040_u32.to_be_bytes());

        let parsed = parse_xex2_header(&bytes).unwrap();

        assert_eq!(parsed.module_flags, 0x1234);
        assert_eq!(parsed.header_size, 0x40);
        assert_eq!(parsed.reserved, 0);
        assert_eq!(parsed.security_info_offset, 0x30);
        assert_eq!(parsed.optional_headers.len(), 1);
        assert_eq!(parsed.optional_headers[0].key, 0x0001_0002);
        assert_eq!(parsed.optional_headers[0].value, 0x40);
    }

    #[test]
    fn rejects_truncated_fixed_header() {
        assert_eq!(parse_xex2_header(b"XEX2"), Err(XexError::TruncatedHeader));
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut bytes = header_with_count(0);
        bytes[0..4].copy_from_slice(b"NOPE");
        assert_eq!(parse_xex2_header(&bytes), Err(XexError::InvalidMagic));
    }

    #[test]
    fn rejects_header_size_outside_file_bounds() {
        let mut bytes = header_with_count(0);
        bytes[8..12].copy_from_slice(&0x100_u32.to_be_bytes());
        assert_eq!(parse_xex2_header(&bytes), Err(XexError::InvalidHeaderSize));
    }

    #[test]
    fn rejects_optional_header_table_past_end() {
        let bytes = header_with_count(6);
        assert_eq!(
            parse_xex2_header(&bytes),
            Err(XexError::OptionalHeaderTableOutOfBounds)
        );
    }
}
