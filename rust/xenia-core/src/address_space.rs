//! Region-based guest address space with explicit permissions.
//!
//! MMIO routing, page-level protection changes, aliases, physical mappings, and
//! invalidation callbacks are intentionally not implemented yet.

use crate::memory::{GuestAddress, GuestMemory, MemoryError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryPermissions {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl MemoryPermissions {
    pub const READ_ONLY: Self = Self {
        read: true,
        write: false,
        execute: false,
    };
    pub const READ_WRITE: Self = Self {
        read: true,
        write: true,
        execute: false,
    };
    pub const READ_EXECUTE: Self = Self {
        read: true,
        write: false,
        execute: true,
    };
    pub const READ_WRITE_EXECUTE: Self = Self {
        read: true,
        write: true,
        execute: true,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressSpaceError {
    EmptyRegion,
    AddressRangeOverflow,
    OverlappingRegion,
    RegionNotFound,
    UnmappedAddress,
    PermissionDenied,
    Memory(MemoryError),
}

impl From<MemoryError> for AddressSpaceError {
    fn from(value: MemoryError) -> Self {
        Self::Memory(value)
    }
}

#[derive(Clone, Debug)]
struct Region {
    memory: GuestMemory,
    permissions: MemoryPermissions,
}

impl Region {
    fn start(&self) -> u64 {
        self.memory.base().0 as u64
    }

    fn end(&self) -> u64 {
        self.start() + self.memory.len() as u64
    }

    fn contains(&self, address: GuestAddress) -> bool {
        let address = address.0 as u64;
        address >= self.start() && address < self.end()
    }
}

/// A non-overlapping collection of guest memory regions.
#[derive(Clone, Debug, Default)]
pub struct GuestAddressSpace {
    regions: Vec<Region>,
}

impl GuestAddressSpace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Maps a zero-initialized region. Region end may equal 2^32, but not exceed it.
    pub fn map(
        &mut self,
        base: GuestAddress,
        size: usize,
        permissions: MemoryPermissions,
    ) -> Result<(), AddressSpaceError> {
        if size == 0 {
            return Err(AddressSpaceError::EmptyRegion);
        }

        let start = base.0 as u64;
        let end = start
            .checked_add(size as u64)
            .ok_or(AddressSpaceError::AddressRangeOverflow)?;
        if end > (u32::MAX as u64) + 1 {
            return Err(AddressSpaceError::AddressRangeOverflow);
        }

        if self
            .regions
            .iter()
            .any(|region| start < region.end() && region.start() < end)
        {
            return Err(AddressSpaceError::OverlappingRegion);
        }

        self.regions.push(Region {
            memory: GuestMemory::new(base, size),
            permissions,
        });
        self.regions.sort_by_key(Region::start);
        Ok(())
    }

    /// Changes permissions for the region whose base address matches exactly.
    pub fn protect(
        &mut self,
        base: GuestAddress,
        permissions: MemoryPermissions,
    ) -> Result<(), AddressSpaceError> {
        let region = self
            .regions
            .iter_mut()
            .find(|region| region.memory.base() == base)
            .ok_or(AddressSpaceError::RegionNotFound)?;
        region.permissions = permissions;
        Ok(())
    }

    /// Removes the region whose base address matches exactly.
    pub fn unmap(&mut self, base: GuestAddress) -> Result<(), AddressSpaceError> {
        let index = self
            .regions
            .iter()
            .position(|region| region.memory.base() == base)
            .ok_or(AddressSpaceError::RegionNotFound)?;
        self.regions.remove(index);
        Ok(())
    }

    pub fn read_u8(&self, address: GuestAddress) -> Result<u8, AddressSpaceError> {
        let region = self.find_region(address)?;
        if !region.permissions.read {
            return Err(AddressSpaceError::PermissionDenied);
        }
        Ok(region.memory.read_u8(address)?)
    }

    pub fn write_u8(&mut self, address: GuestAddress, value: u8) -> Result<(), AddressSpaceError> {
        let region = self.find_region_mut(address)?;
        if !region.permissions.write {
            return Err(AddressSpaceError::PermissionDenied);
        }
        region.memory.write_u8(address, value)?;
        Ok(())
    }

    pub fn read_u32_be(&self, address: GuestAddress) -> Result<u32, AddressSpaceError> {
        let region = self.find_region(address)?;
        if !region.permissions.read {
            return Err(AddressSpaceError::PermissionDenied);
        }
        Ok(region.memory.read_u32_be(address)?)
    }

    pub fn write_u32_be(
        &mut self,
        address: GuestAddress,
        value: u32,
    ) -> Result<(), AddressSpaceError> {
        let region = self.find_region_mut(address)?;
        if !region.permissions.write {
            return Err(AddressSpaceError::PermissionDenied);
        }
        region.memory.write_u32_be(address, value)?;
        Ok(())
    }

    fn find_region(&self, address: GuestAddress) -> Result<&Region, AddressSpaceError> {
        self.regions
            .iter()
            .find(|region| region.contains(address))
            .ok_or(AddressSpaceError::UnmappedAddress)
    }

    fn find_region_mut(&mut self, address: GuestAddress) -> Result<&mut Region, AddressSpaceError> {
        self.regions
            .iter_mut()
            .find(|region| region.contains(address))
            .ok_or(AddressSpaceError::UnmappedAddress)
    }
}

impl crate::memory::GuestMemoryAccess for GuestAddressSpace {
    type Error = AddressSpaceError;

    fn read_u32_be(&self, address: GuestAddress) -> Result<u32, Self::Error> {
        GuestAddressSpace::read_u32_be(self, address)
    }

    fn fetch_u32_be(&self, address: GuestAddress) -> Result<u32, Self::Error> {
        let region = self.find_region(address)?;
        if !region.permissions.execute {
            return Err(AddressSpaceError::PermissionDenied);
        }
        Ok(region.memory.read_u32_be(address)?)
    }

    fn write_u32_be(&mut self, address: GuestAddress, value: u32) -> Result<(), Self::Error> {
        GuestAddressSpace::write_u32_be(self, address, value)
    }
}

#[cfg(test)]
mod tests {
    use super::{AddressSpaceError, GuestAddressSpace, MemoryPermissions};
    use crate::memory::GuestAddress;

    #[test]
    fn maps_regions_and_reads_big_endian_words() {
        let mut space = GuestAddressSpace::new();
        space
            .map(GuestAddress(0x1000), 16, MemoryPermissions::READ_WRITE)
            .unwrap();
        space
            .write_u32_be(GuestAddress(0x1004), 0x1234_ABCD)
            .unwrap();
        assert_eq!(space.read_u32_be(GuestAddress(0x1004)), Ok(0x1234_ABCD));
    }

    #[test]
    fn rejects_overlapping_regions() {
        let mut space = GuestAddressSpace::new();
        space
            .map(GuestAddress(0x1000), 0x100, MemoryPermissions::READ_WRITE)
            .unwrap();
        assert_eq!(
            space.map(GuestAddress(0x1080), 0x100, MemoryPermissions::READ_WRITE),
            Err(AddressSpaceError::OverlappingRegion)
        );
    }

    #[test]
    fn execute_permission_is_separate_from_read_permission() {
        let mut space = GuestAddressSpace::new();
        space
            .map(GuestAddress(0x1800), 4, MemoryPermissions::READ_ONLY)
            .unwrap();
        assert_eq!(
            crate::memory::GuestMemoryAccess::fetch_u32_be(&space, GuestAddress(0x1800)),
            Err(AddressSpaceError::PermissionDenied)
        );
        assert_eq!(space.read_u32_be(GuestAddress(0x1800)), Ok(0));
    }

    #[test]
    fn enforces_write_permissions() {
        let mut space = GuestAddressSpace::new();
        space
            .map(GuestAddress(0x2000), 16, MemoryPermissions::READ_ONLY)
            .unwrap();
        assert_eq!(
            space.write_u8(GuestAddress(0x2000), 1),
            Err(AddressSpaceError::PermissionDenied)
        );
    }

    #[test]
    fn rejects_empty_and_wrapping_regions() {
        let mut space = GuestAddressSpace::new();
        assert_eq!(
            space.map(GuestAddress(0x1000), 0, MemoryPermissions::READ_WRITE),
            Err(AddressSpaceError::EmptyRegion)
        );
        assert_eq!(
            space.map(GuestAddress(u32::MAX), 2, MemoryPermissions::READ_WRITE),
            Err(AddressSpaceError::AddressRangeOverflow)
        );
    }

    #[test]
    fn adjacent_regions_are_allowed_but_cross_region_word_access_is_rejected() {
        let mut space = GuestAddressSpace::new();
        space
            .map(GuestAddress(0x3000), 2, MemoryPermissions::READ_WRITE)
            .unwrap();
        space
            .map(GuestAddress(0x3002), 2, MemoryPermissions::READ_WRITE)
            .unwrap();
        assert_eq!(
            space.read_u32_be(GuestAddress(0x3000)),
            Err(AddressSpaceError::Memory(
                crate::memory::MemoryError::OutOfBounds
            ))
        );
    }
}
