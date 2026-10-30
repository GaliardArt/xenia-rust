//! Safe first-stage XEX2 image loading for the unencrypted, uncompressed profile.
//!
//! This module prepares owned image bytes and metadata. It deliberately does not
//! mutate guest memory, resolve imports, apply patches, or start execution.

use core::fmt;

use crate::address_space::{AddressSpaceError, GuestAddressSpace, MemoryPermissions};
use crate::layout::{ImageLayout, LayoutError, plan_image_layout};
use crate::memory::GuestAddress;
use crate::pe::{PeError, XboxPeImage, parse_xbox_pe32};
use crate::security::{SecurityInfoError, XexSecurityInfo, parse_xex2_security_info};
use crate::xex::{XexError, XexHeader, parse_xex2_header};

pub const XEX_HEADER_FILE_FORMAT_INFO: u32 = 0x0000_03FF;
pub const XEX_HEADER_IMAGE_BASE_ADDRESS: u32 = 0x0001_0201;
const FILE_FORMAT_FIXED_SIZE: usize = 8;
const XEX_ENCRYPTION_NONE: u16 = 0;
const XEX_COMPRESSION_NONE: u16 = 0;
const XEX_COMPRESSION_BASIC: u16 = 1;
const BASIC_BLOCK_ENTRY_SIZE: usize = 8;
const MAX_IMAGE_SIZE: usize = 256 * 1024 * 1024;

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
    AddressSpace(AddressSpaceError),
    ImageSizeDoesNotMatchPageTable,
    SectionOutsideImage,
    MappedImageTooLarge,
    MissingFileFormatHeader,
    FileFormatHeaderOutOfBounds,
    InvalidFileFormatHeaderSize,
    UnsupportedEncryption(u16),
    UnsupportedCompression(u16),
    ImagePayloadOutOfBounds,
    ImageSmallerThanDeclared,
    InvalidPageLayout,
    ImageTooLarge,
    AllocationFailed,
}

impl fmt::Display for XexLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header(error) => write!(f, "XEX header: {error}"),
            Self::SecurityInfo(error) => write!(f, "XEX security info: {error}"),
            Self::Pe(error) => write!(f, "embedded PE image: {error}"),
            Self::Layout(error) => write!(f, "PE image layout: {error}"),
            Self::AddressSpace(error) => write!(f, "guest memory mapping: {error}"),
            Self::SectionOutsideImage => {
                f.write_str("PE section extends beyond the loaded XEX image")
            }
            Self::ImageSizeDoesNotMatchPageTable => {
                f.write_str("XEX image size does not match its page-descriptor table")
            }
            Self::MappedImageTooLarge => f.write_str("mapped PE image exceeds the safety limit"),
            Self::MissingFileFormatHeader => {
                f.write_str("XEX file-format optional header is missing")
            }
            Self::FileFormatHeaderOutOfBounds => {
                f.write_str("XEX file-format header exceeds file bounds")
            }
            Self::InvalidFileFormatHeaderSize => {
                f.write_str("XEX file-format header size is invalid")
            }
            Self::UnsupportedEncryption(value) => {
                write!(f, "unsupported XEX encryption type {value}")
            }
            Self::UnsupportedCompression(value) => {
                write!(f, "unsupported XEX compression type {value}")
            }
            Self::ImagePayloadOutOfBounds => {
                f.write_str("XEX image payload starts beyond end of file")
            }
            Self::ImageSmallerThanDeclared => {
                f.write_str("XEX image payload is smaller than declared image size")
            }
            Self::InvalidPageLayout => f.write_str("XEX basic-compression page layout is invalid"),
            Self::ImageTooLarge => f.write_str("XEX image exceeds the configured safety limit"),
            Self::AllocationFailed => f.write_str("unable to allocate XEX image buffer"),
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

impl From<AddressSpaceError> for XexLoadError {
    fn from(value: AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

impl From<LayoutError> for XexLoadError {
    fn from(value: LayoutError) -> Self {
        Self::Layout(value)
    }
}

impl XexLoadedImage {
    /// Maps the decompressed image using XEX page descriptors.
    ///
    /// The operation is transactional: on any invalid descriptor or mapping
    /// conflict, the caller's address space remains unchanged.
    pub fn map_into(&self, address_space: &mut GuestAddressSpace) -> Result<(), XexLoadError> {
        let page_size = if self.security.image_flags & 0x1000_0000 != 0 {
            0x1000_usize
        } else {
            0x1_0000_usize
        };
        let mapped_size =
            self.security
                .page_descriptors
                .iter()
                .try_fold(0_usize, |total, descriptor| {
                    let descriptor_size = (descriptor.page_count as usize)
                        .checked_mul(page_size)
                        .ok_or(XexLoadError::MappedImageTooLarge)?;
                    total
                        .checked_add(descriptor_size)
                        .ok_or(XexLoadError::MappedImageTooLarge)
                })?;
        if mapped_size > MAX_IMAGE_SIZE {
            return Err(XexLoadError::MappedImageTooLarge);
        }
        if mapped_size == 0 || mapped_size != self.image.len() {
            return Err(XexLoadError::ImageSizeDoesNotMatchPageTable);
        }

        let mut staged = address_space.clone();
        let mut offset = 0_usize;
        for descriptor in &self.security.page_descriptors {
            let size = (descriptor.page_count as usize)
                .checked_mul(page_size)
                .ok_or(XexLoadError::MappedImageTooLarge)?;
            if size == 0 {
                continue;
            }
            let end = offset
                .checked_add(size)
                .ok_or(XexLoadError::MappedImageTooLarge)?;
            let bytes = self
                .image
                .get(offset..end)
                .ok_or(XexLoadError::ImageSizeDoesNotMatchPageTable)?;
            let permissions = match descriptor.section_type {
                1 => MemoryPermissions::READ_EXECUTE,
                2 => MemoryPermissions::READ_WRITE,
                3 => MemoryPermissions::READ_ONLY,
                _ => return Err(XexLoadError::ImageSizeDoesNotMatchPageTable),
            };
            let address = self
                .layout
                .load_base
                .checked_add(u32::try_from(offset).map_err(|_| XexLoadError::MappedImageTooLarge)?)
                .ok_or(XexLoadError::MappedImageTooLarge)?;
            staged.map_initialized(GuestAddress(address), size, permissions, bytes)?;
            offset = end;
        }

        *address_space = staged;
        Ok(())
    }
}

/// Loads an XEX2 with no encryption and either no compression or basic compression.
///
/// Encrypted, compressed, patched, and non-XEX2 formats are rejected explicitly.
/// The returned image is not mapped into guest memory.
pub fn load_xex2_image(bytes: &[u8]) -> Result<XexLoadedImage, XexLoadError> {
    let header = parse_xex2_header(bytes)?;
    let security = parse_xex2_security_info(bytes, &header)?;

    let file_format_offset = header
        .optional_header_value(XEX_HEADER_FILE_FORMAT_INFO)
        .ok_or(XexLoadError::MissingFileFormatHeader)? as usize;
    let format_end = file_format_offset
        .checked_add(FILE_FORMAT_FIXED_SIZE)
        .ok_or(XexLoadError::FileFormatHeaderOutOfBounds)?;
    let file_format_fixed = bytes
        .get(file_format_offset..format_end)
        .ok_or(XexLoadError::FileFormatHeaderOutOfBounds)?;

    let info_size = read_u32_be(file_format_fixed, 0) as usize;
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
    let file_format = bytes
        .get(file_format_offset..info_end)
        .ok_or(XexLoadError::FileFormatHeaderOutOfBounds)?;

    let encryption_type = read_u16_be(file_format, 4);
    let compression_type = read_u16_be(file_format, 6);
    if encryption_type != XEX_ENCRYPTION_NONE {
        return Err(XexLoadError::UnsupportedEncryption(encryption_type));
    }
    if compression_type != XEX_COMPRESSION_NONE && compression_type != XEX_COMPRESSION_BASIC {
        return Err(XexLoadError::UnsupportedCompression(compression_type));
    }

    let payload_start = header.header_size as usize;
    if payload_start > bytes.len() {
        return Err(XexLoadError::ImagePayloadOutOfBounds);
    }
    let payload = &bytes[payload_start..];
    let image = if compression_type == XEX_COMPRESSION_NONE {
        if payload.len() < security.image_size as usize {
            return Err(XexLoadError::ImageSmallerThanDeclared);
        }
        payload.to_vec()
    } else {
        decode_basic_compression(payload, file_format, &security)?
    };

    let pe = parse_xbox_pe32(&image)?;
    let load_base = header
        .optional_header_value(XEX_HEADER_IMAGE_BASE_ADDRESS)
        .unwrap_or(security.load_address);
    let layout = plan_image_layout(&pe, load_base)?;
    if layout.image_end_exclusive - load_base as u64 > image.len() as u64 {
        return Err(XexLoadError::SectionOutsideImage);
    }

    Ok(XexLoadedImage {
        header,
        security,
        pe,
        layout,
        image,
    })
}

fn decode_basic_compression(
    payload: &[u8],
    file_format: &[u8],
    security: &XexSecurityInfo,
) -> Result<Vec<u8>, XexLoadError> {
    let page_size = if security.image_flags & 0x1000_0000 != 0 {
        0x1000_usize
    } else {
        0x1_0000_usize
    };
    let page_count = security
        .page_descriptors
        .iter()
        .try_fold(0_usize, |total, descriptor| {
            total
                .checked_add(descriptor.page_count as usize)
                .ok_or(XexLoadError::InvalidPageLayout)
        })?;
    let output_size = page_count
        .checked_mul(page_size)
        .ok_or(XexLoadError::InvalidPageLayout)?;
    if output_size == 0 {
        return Err(XexLoadError::InvalidPageLayout);
    }
    if output_size > MAX_IMAGE_SIZE {
        return Err(XexLoadError::ImageTooLarge);
    }

    let info_size = read_u32_be(file_format, 0) as usize;
    let block_table_size = info_size
        .checked_sub(FILE_FORMAT_FIXED_SIZE)
        .ok_or(XexLoadError::InvalidFileFormatHeaderSize)?;
    if block_table_size == 0 || block_table_size % BASIC_BLOCK_ENTRY_SIZE != 0 {
        return Err(XexLoadError::InvalidFileFormatHeaderSize);
    }

    let block_count = block_table_size / BASIC_BLOCK_ENTRY_SIZE;
    let mut output = Vec::new();
    output
        .try_reserve_exact(output_size)
        .map_err(|_| XexLoadError::AllocationFailed)?;
    output.resize(output_size, 0);

    let mut source_offset = 0_usize;
    let mut destination_offset = 0_usize;
    for index in 0..block_count {
        let entry_offset = FILE_FORMAT_FIXED_SIZE + index * BASIC_BLOCK_ENTRY_SIZE;
        let data_size = read_u32_be(file_format, entry_offset) as usize;
        let zero_size = read_u32_be(file_format, entry_offset + 4) as usize;
        let source_end = source_offset
            .checked_add(data_size)
            .ok_or(XexLoadError::InvalidPageLayout)?;
        let destination_data_end = destination_offset
            .checked_add(data_size)
            .ok_or(XexLoadError::InvalidPageLayout)?;
        let destination_end = destination_data_end
            .checked_add(zero_size)
            .ok_or(XexLoadError::InvalidPageLayout)?;
        let source = payload
            .get(source_offset..source_end)
            .ok_or(XexLoadError::ImagePayloadOutOfBounds)?;
        if destination_end > output_size {
            return Err(XexLoadError::InvalidPageLayout);
        }
        output[destination_offset..destination_data_end].copy_from_slice(source);
        output[destination_data_end..destination_end].fill(0);
        source_offset = source_end;
        destination_offset = destination_end;
    }

    if destination_offset != output_size {
        return Err(XexLoadError::InvalidPageLayout);
    }
    if output.len() < security.image_size as usize {
        return Err(XexLoadError::ImageSmallerThanDeclared);
    }
    Ok(output)
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
    use super::{XEX_HEADER_FILE_FORMAT_INFO, XexLoadError, load_xex2_image};
    use crate::address_space::{AddressSpaceError, GuestAddressSpace, MemoryPermissions};
    use crate::memory::GuestAddress;

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
        bytes.resize(0x300, 0);
        bytes[0x200..0x300].fill(0xA5);
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
        let security_size = 0x184 + 0x18;
        let format_offset = security_offset + security_size;
        let format_size = 8;
        let header_size = format_offset + format_size;
        let image_offset = header_size;
        let mut bytes = vec![0; image_offset + 0x2000];

        bytes[0..4].copy_from_slice(b"XEX2");
        put_u32_be(&mut bytes, 4, 0);
        put_u32_be(&mut bytes, 8, header_size as u32);
        put_u32_be(&mut bytes, 12, 0);
        put_u32_be(&mut bytes, 16, security_offset as u32);
        put_u32_be(&mut bytes, 20, 1);
        put_u32_be(&mut bytes, 24, XEX_HEADER_FILE_FORMAT_INFO);
        put_u32_be(&mut bytes, 28, format_offset as u32);

        put_u32_be(&mut bytes, security_offset, security_size as u32);
        put_u32_be(&mut bytes, security_offset + 4, 0x2000);
        put_u32_be(&mut bytes, security_offset + 0x108, security_size as u32);
        put_u32_be(&mut bytes, security_offset + 0x10C, 0x1000_0000);
        put_u32_be(&mut bytes, security_offset + 0x110, 0x8200_0000);
        put_u32_be(&mut bytes, security_offset + 0x180, 1);
        put_u32_be(&mut bytes, security_offset + 0x184, (1 << 28) | 2);

        put_u32_be(&mut bytes, format_offset, format_size as u32);
        put_u16_be(&mut bytes, format_offset + 4, 0);
        put_u16_be(&mut bytes, format_offset + 6, 0);
        bytes[image_offset..image_offset + payload.len()].copy_from_slice(&payload);
        bytes
    }

    #[test]
    fn loads_unencrypted_uncompressed_xex2_and_plans_image() {
        let loaded = load_xex2_image(&valid_xex()).unwrap();
        assert_eq!(loaded.security.load_address, 0x8200_0000);
        assert_eq!(&loaded.image[..valid_pe().len()], valid_pe().as_slice());
        assert!(
            loaded.image[valid_pe().len()..]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert_eq!(loaded.layout.entry_point, 0x8200_1000);
    }

    fn basic_fixture() -> Vec<u8> {
        let payload = valid_pe();
        let security_offset = 0x40;
        let security_size = 0x184 + 0x18;
        let format_offset = security_offset + security_size;
        let format_size = 16;
        let header_size = format_offset + format_size;
        let mut bytes = vec![0; header_size + 0x2000];

        bytes[0..4].copy_from_slice(b"XEX2");
        put_u32_be(&mut bytes, 8, header_size as u32);
        put_u32_be(&mut bytes, 16, security_offset as u32);
        put_u32_be(&mut bytes, 20, 1);
        put_u32_be(&mut bytes, 24, XEX_HEADER_FILE_FORMAT_INFO);
        put_u32_be(&mut bytes, 28, format_offset as u32);

        put_u32_be(&mut bytes, security_offset, security_size as u32);
        put_u32_be(&mut bytes, security_offset + 4, 0x2000);
        put_u32_be(&mut bytes, security_offset + 0x108, security_size as u32);
        put_u32_be(&mut bytes, security_offset + 0x10C, 0x1000_0000);
        put_u32_be(&mut bytes, security_offset + 0x110, 0x8200_0000);
        put_u32_be(&mut bytes, security_offset + 0x180, 1);
        put_u32_be(&mut bytes, security_offset + 0x184, (1 << 28) | 2);

        put_u32_be(&mut bytes, format_offset, format_size as u32);
        put_u16_be(&mut bytes, format_offset + 4, 0);
        put_u16_be(&mut bytes, format_offset + 6, 1);
        put_u32_be(&mut bytes, format_offset + 8, payload.len() as u32);
        put_u32_be(
            &mut bytes,
            format_offset + 12,
            0x2000 - payload.len() as u32,
        );
        bytes[header_size..header_size + payload.len()].copy_from_slice(&payload);
        bytes
    }

    #[test]
    fn loads_unencrypted_basic_compressed_xex2() {
        let loaded = load_xex2_image(&basic_fixture()).unwrap();
        assert_eq!(loaded.image.len(), 0x2000);
        assert_eq!(&loaded.image[..2], b"MZ");
        assert!(
            loaded.image[valid_pe().len()..]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert_eq!(loaded.layout.entry_point, 0x8200_1000);
    }

    #[test]
    fn maps_page_descriptors_and_enforces_executable_permissions() {
        let loaded = load_xex2_image(&valid_xex()).unwrap();
        let mut memory = GuestAddressSpace::new();
        loaded.map_into(&mut memory).unwrap();

        assert_eq!(
            memory.read_bytes(GuestAddress(0x8200_0200), 4).unwrap(),
            &[0xA5; 4]
        );
        assert_eq!(
            memory.read_bytes(GuestAddress(0x8200_02FC), 4).unwrap(),
            &[0xA5; 4]
        );
        assert_eq!(
            memory.read_bytes(GuestAddress(0x8200_1FFF), 2),
            Err(AddressSpaceError::Memory(
                crate::memory::MemoryError::OutOfBounds
            ))
        );
        assert_eq!(
            memory.write_u8(GuestAddress(0x8200_0200), 0),
            Err(AddressSpaceError::PermissionDenied)
        );
        assert_eq!(
            crate::memory::GuestMemoryAccess::fetch_u32_be(&memory, GuestAddress(0x8200_0200)),
            Ok(0xA5A5_A5A5)
        );
    }

    #[test]
    fn applies_code_data_and_read_only_page_permissions() {
        let mut loaded = load_xex2_image(&valid_xex()).unwrap();
        loaded.image.resize(0x3000, 0);
        loaded.security.page_descriptors = vec![
            crate::security::XexPageDescriptor {
                section_type: 1,
                page_count: 1,
                digest: [0; 20],
            },
            crate::security::XexPageDescriptor {
                section_type: 2,
                page_count: 1,
                digest: [0; 20],
            },
            crate::security::XexPageDescriptor {
                section_type: 3,
                page_count: 1,
                digest: [0; 20],
            },
        ];
        let mut memory = GuestAddressSpace::new();
        loaded.map_into(&mut memory).unwrap();

        assert_eq!(
            memory.write_u8(GuestAddress(0x8200_0000), 1),
            Err(AddressSpaceError::PermissionDenied)
        );
        assert_eq!(memory.write_u8(GuestAddress(0x8200_1000), 2), Ok(()));
        assert_eq!(
            memory.write_u8(GuestAddress(0x8200_2000), 3),
            Err(AddressSpaceError::PermissionDenied)
        );
        assert_eq!(
            crate::memory::GuestMemoryAccess::fetch_u32_be(&memory, GuestAddress(0x8200_1000)),
            Err(AddressSpaceError::PermissionDenied)
        );
    }

    #[test]
    fn failed_mapping_does_not_mutate_existing_address_space() {
        let loaded = load_xex2_image(&valid_xex()).unwrap();
        let mut memory = GuestAddressSpace::new();
        memory
            .map(
                GuestAddress(0x8200_0000),
                0x1000,
                MemoryPermissions::READ_WRITE,
            )
            .unwrap();
        memory.write_u8(GuestAddress(0x8200_0000), 0x7B).unwrap();

        assert!(matches!(
            loaded.map_into(&mut memory),
            Err(XexLoadError::AddressSpace(
                AddressSpaceError::OverlappingRegion
            ))
        ));
        assert_eq!(memory.read_u8(GuestAddress(0x8200_0000)), Ok(0x7B));
    }

    #[test]
    fn rejects_page_table_larger_than_decompressed_image() {
        let mut loaded = load_xex2_image(&valid_xex()).unwrap();
        loaded.security.page_descriptors[0].page_count = 3;
        let mut memory = GuestAddressSpace::new();
        assert_eq!(
            loaded.map_into(&mut memory),
            Err(XexLoadError::ImageSizeDoesNotMatchPageTable)
        );
        assert_eq!(
            memory.read_u8(GuestAddress(0x8200_0000)),
            Err(AddressSpaceError::UnmappedAddress)
        );
    }

    #[test]
    fn rejects_encrypted_or_compressed_images() {
        let mut encrypted = valid_xex();
        let format_offset = 0x40 + 0x184 + 0x18;
        put_u16_be(&mut encrypted, format_offset + 4, 1);
        assert_eq!(
            load_xex2_image(&encrypted),
            Err(XexLoadError::UnsupportedEncryption(1))
        );

        let mut compressed = valid_xex();
        put_u16_be(&mut compressed, format_offset + 6, 2);
        assert_eq!(
            load_xex2_image(&compressed),
            Err(XexLoadError::UnsupportedCompression(2))
        );
    }
}
