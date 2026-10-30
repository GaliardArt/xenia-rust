//! Safe first-stage XEX2 image loading for the unencrypted, uncompressed profile.
//!
//! This module prepares owned image bytes and metadata. It deliberately does not
//! mutate guest memory, resolve imports, apply patches, or start execution.

use core::fmt;

use crate::layout::{ImageLayout, LayoutError, plan_image_layout};
use crate::pe::{PeError, XboxPeImage, parse_xbox_pe32};
use crate::security::{SecurityInfoError, XexSecurityInfo, parse_xex2_security_info};
use crate::xex::{XexError, XexHeader, parse_xex2_header};

pub const XEX_HEADER_FILE_FORMAT_INFO: u32 = 0x0000_03FF;
pub const XEX_HEADER_IMAGE_BASE_ADDRESS: u32 = 0x0001_0201;
const FILE_FORMAT_FIXED_SIZE: usize = 8;
const XEX_ENCRYPTION_NONE: u16 = 0;
const XEX_COMPRESSION_NONE: u16 = 0;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XexLoadedImage {
    pub header: XexHeader,
    pub security: XexSecurityInfo,
    pub pe: XboxPeImage,
    pub layout: ImageLayout,
    /// Complete uncompressed image payload, beginning at XEX header_size.
    pub image: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XexLoadError {
    Header(XexError),
    SecurityInfo(SecurityInfoError),
    Pe(PeError),
    Layout(LayoutError),
    MissingFileFormatHeader,
    FileFormatHeaderOutOfBounds,
    InvalidFileFormatHeaderSize,
    UnsupportedEncryption(u16),
    UnsupportedCompression(u16),
    ImagePayloadOutOfBounds,
    ImageSmallerThanDeclared,
}

impl fmt::Display for XexLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header(error) => write!(f, "XEX header: {error}"),
            Self::SecurityInfo(error) => write!(f, "XEX security info: {error}"),
            Self::Pe(error) => write!(f, "embedded PE image: {error}"),
            Self::Layout(error) => write!(f, "PE image layout: {error}"),
            Self::MissingFileFormatHeader => f.write_str("XEX file-format optional header is missing"),
            Self::FileFormatHeaderOutOfBounds => f.write_str("XEX file-format header exceeds file bounds"),
            Self::InvalidFileFormatHeaderSize => f.write_str("XEX file-format header size is invalid"),
            Self::UnsupportedEncryption(value) => write!(f, "unsupported XEX encryption type {value}"),
            Self::UnsupportedCompression(value) => write!(f, "unsupported XEX compression type {value}"),
            Self::ImagePayloadOutOfBounds => f.write_str("XEX image payload starts beyond end of file"),
            Self::ImageSmallerThanDeclared => f.write_str("XEX image payload is smaller than declared image size"),
        }
    }
}

impl std::error::Error for XexLoadError {}

impl From<XexError> for XexLoadError {
    fn from(value: XexError) -> Self {
        Self::Header(value)
    }
}

impl From<SecurityInfoError> for XexLoadError {
    fn from(value: SecurityInfoError) -> Self {
        Self::SecurityInfo(value)
    }
}

impl From<PeError> for XexLoadError {
    fn from(value: PeError) -> Self {
        Self::Pe(value)
    }
}

impl From<LayoutError> for XexLoadError {
    fn from(value: LayoutError) -> Self {
        Self::Layout(value)
    }
}

/// Loads an XEX2 with no encryption and no compression into an owned image plan.
///
/// Encrypted, compressed, patched, and non-XEX2 formats are rejected explicitly.
/// The returned image is not mapped into guest memory.
pub fn load_xex2_uncompressed(bytes: &[u8]) -> Result<XexLoadedImage, XexLoadError> {
    let header = parse_xex2_header(bytes)?;
    let security = parse_xex2_security_info(bytes, &header)?;

    let file_format_offset = header
        .optional_header_value(XEX_HEADER_FILE_FORMAT_INFO)
        .ok_or(XexLoadError::MissingFileFormatHeader)? as usize;
    let format_end = file_format_offset
        .checked_add(FILE_FORMAT_FIXED_SIZE)
        .ok_or(XexLoadError::FileFormatHeaderOutOfBounds)?;
    let file_format = bytes
        .get(file_format_offset..format_end)
        .ok_or(XexLoadError::FileFormatHeaderOutOfBounds)?;

    let info_size = read_u32_be(file_format, 0) as usize;
    if info_size < FILE_FORMAT_FIXED_SIZE {
        return Err(XexLoadError::InvalidFileFormatHeaderSize);
    }
    let info_end = file_format_offset
        .checked_add(info_size)
        .ok_or(XexLoadError::FileFormatHeaderOutOfBounds)?;
    if bytes.get(file_format_offset..info_end).is_none() {
        return Err(XexLoadError::FileFormatHeaderOutOfBounds);
    }
    if info_end > header.header_size as usize {
        return Err(XexLoadError::InvalidFileFormatHeaderSize);
    }

    let encryption_type = read_u16_be(file_format, 4);
    let compression_type = read_u16_be(file_format, 6);
    if encryption_type != XEX_ENCRYPTION_NONE {
        return Err(XexLoadError::UnsupportedEncryption(encryption_type));
    }
    if compression_type != XEX_COMPRESSION_NONE {
        return Err(XexLoadError::UnsupportedCompression(compression_type));
    }

    let payload_start = header.header_size as usize;
    if payload_start > bytes.len() {
        return Err(XexLoadError::ImagePayloadOutOfBounds);
    }
    let image = bytes[payload_start..].to_vec();
    if image.len() < security.image_size as usize {
        return Err(XexLoadError::ImageSmallerThanDeclared);
    }

    let pe = parse_xbox_pe32(&image)?;
    let load_base = header
        .optional_header_value(XEX_HEADER_IMAGE_BASE_ADDRESS)
        .unwrap_or(security.load_address);
    let layout = plan_image_layout(&pe, load_base)?;

    Ok(XexLoadedImage {
        header,
        security,
        pe,
        layout,
        image,
    })
}

fn read_u16_be(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
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
    use super::{XEX_HEADER_FILE_FORMAT_INFO, XexLoadError, load_xex2_uncompressed};

    fn put_u16_le(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u16_be(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
    }

    fn put_u32_be(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn put_u32_le(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn valid_pe() -> Vec<u8> {
        let nt = 0x80;
        let file_header = nt + 4;
        let optional = file_header + 20;
        let section = optional + 224;
        let mut bytes = vec![0; section + 40];
        bytes[0..2].copy_from_slice(b"MZ");
        put_u32_le(&mut bytes, 0x3C, nt as u32);
        bytes[nt..nt + 4].copy_from_slice(b"PE\0\0");
        put_u16_le(&mut bytes, file_header, 0x01F2);
        put_u16_le(&mut bytes, file_header + 2, 1);
        put_u16_le(&mut bytes, file_header + 16, 224);
        put_u16_le(&mut bytes, file_header + 18, 0x0100);
        put_u16_le(&mut bytes, optional, 0x010B);
        put_u32_le(&mut bytes, optional + 16, 0x1000);
        put_u32_le(&mut bytes, optional + 28, 0x8200_0000);
        put_u16_le(&mut bytes, optional + 68, 14);
        bytes[section..section + 5].copy_from_slice(b".text");
        put_u32_le(&mut bytes, section + 8, 0x100);
        put_u32_le(&mut bytes, section + 12, 0x1000);
        put_u32_le(&mut bytes, section + 16, 0x100);
        put_u32_le(&mut bytes, section + 20, 0x200);
        put_u32_le(&mut bytes, section + 36, 0x6000_0020);
        bytes
    }

    fn valid_xex() -> Vec<u8> {
        let payload = valid_pe();
        let security_offset = 0x40;
        let security_size = 0x184;
        let format_offset = security_offset + security_size;
        let format_size = 8;
        let header_size = format_offset + format_size;
        let image_offset = header_size;
        let mut bytes = vec![0; image_offset + payload.len()];

        bytes[0..4].copy_from_slice(b"XEX2");
        put_u32_be(&mut bytes, 4, 0);
        put_u32_be(&mut bytes, 8, header_size as u32);
        put_u32_be(&mut bytes, 12, 0);
        put_u32_be(&mut bytes, 16, security_offset as u32);
        put_u32_be(&mut bytes, 20, 1);
        put_u32_be(&mut bytes, 24, XEX_HEADER_FILE_FORMAT_INFO);
        put_u32_be(&mut bytes, 28, format_offset as u32);

        put_u32_be(&mut bytes, security_offset, security_size as u32);
        put_u32_be(&mut bytes, security_offset + 4, payload.len() as u32);
        put_u32_be(&mut bytes, security_offset + 0x108, security_size as u32);
        put_u32_be(&mut bytes, security_offset + 0x110, 0x8200_0000);
        put_u32_be(&mut bytes, security_offset + 0x180, 0);

        put_u32_be(&mut bytes, format_offset, format_size as u32);
        put_u16_be(&mut bytes, format_offset + 4, 0);
        put_u16_be(&mut bytes, format_offset + 6, 0);
        bytes[image_offset..].copy_from_slice(&payload);
        bytes
    }

    #[test]
    fn loads_unencrypted_uncompressed_xex2_and_plans_image() {
        let loaded = load_xex2_uncompressed(&valid_xex()).unwrap();
        assert_eq!(loaded.security.load_address, 0x8200_0000);
        assert_eq!(loaded.image, valid_pe());
        assert_eq!(loaded.layout.entry_point, 0x8200_1000);
    }

    #[test]
    fn rejects_encrypted_or_compressed_images() {
        let mut encrypted = valid_xex();
        let format_offset = 0x40 + 0x184;
        put_u16_be(&mut encrypted, format_offset + 4, 1);
        assert_eq!(
            load_xex2_uncompressed(&encrypted),
            Err(XexLoadError::UnsupportedEncryption(1))
        );

        let mut compressed = valid_xex();
        put_u16_be(&mut compressed, format_offset + 6, 2);
        assert_eq!(
            load_xex2_uncompressed(&compressed),
            Err(XexLoadError::UnsupportedCompression(2))
        );
    }
}
