//! Bounds-checked guest-memory primitives. This is not yet a full Xbox 360 MMU.

use core::fmt;

/// A 32-bit guest virtual address used by the initial memory model.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GuestAddress(pub u32);

/// Errors returned by checked guest-memory operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryError {
    OutOfBounds,
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds => f.write_str("guest memory access out of bounds"),
        }
    }
}

impl std::error::Error for MemoryError {}

/// A contiguous guest-memory region for early tests; MMIO and mapping are future work.
#[derive(Clone, Debug)]
pub struct GuestMemory {
    base: GuestAddress,
    bytes: Vec<u8>,
}

impl GuestMemory {
    pub fn new(base: GuestAddress, size: usize) -> Self {
        Self {
            base,
            bytes: vec![0; size],
        }
    }

    pub const fn base(&self) -> GuestAddress {
        self.base
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn read_u8(&self, address: GuestAddress) -> Result<u8, MemoryError> {
        let offset = self.offset_of(address)?;
        self.bytes
            .get(offset)
            .copied()
            .ok_or(MemoryError::OutOfBounds)
    }

    pub fn write_u8(&mut self, address: GuestAddress, value: u8) -> Result<(), MemoryError> {
        let offset = self.offset_of(address)?;
        let byte = self.bytes.get_mut(offset).ok_or(MemoryError::OutOfBounds)?;
        *byte = value;
        Ok(())
    }

    /// Reads a 32-bit value in the Xbox 360's big-endian byte order.
    pub fn read_u32_be(&self, address: GuestAddress) -> Result<u32, MemoryError> {
        let offset = self.offset_of(address)?;
        let end = offset.checked_add(4).ok_or(MemoryError::OutOfBounds)?;
        let bytes = self
            .bytes
            .get(offset..end)
            .ok_or(MemoryError::OutOfBounds)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Writes a 32-bit value in the Xbox 360's big-endian byte order.
    pub fn write_u32_be(&mut self, address: GuestAddress, value: u32) -> Result<(), MemoryError> {
        let offset = self.offset_of(address)?;
        let end = offset.checked_add(4).ok_or(MemoryError::OutOfBounds)?;
        let bytes = self
            .bytes
            .get_mut(offset..end)
            .ok_or(MemoryError::OutOfBounds)?;
        bytes.copy_from_slice(&value.to_be_bytes());
        Ok(())
    }

    fn offset_of(&self, address: GuestAddress) -> Result<usize, MemoryError> {
        let offset = address
            .0
            .checked_sub(self.base.0)
            .ok_or(MemoryError::OutOfBounds)? as usize;
        if offset >= self.bytes.len() {
            return Err(MemoryError::OutOfBounds);
        }
        Ok(offset)
    }
}

/// Minimal memory interface used by the interpreter. Implementations can route
/// accesses through mapped regions or MMIO handlers in later milestones.
pub trait GuestMemoryAccess {
    type Error;

    fn read_u32_be(&self, address: GuestAddress) -> Result<u32, Self::Error>;

    fn fetch_u32_be(&self, address: GuestAddress) -> Result<u32, Self::Error> {
        self.read_u32_be(address)
    }

    fn write_u32_be(&mut self, address: GuestAddress, value: u32) -> Result<(), Self::Error>;
}

impl GuestMemoryAccess for GuestMemory {
    type Error = MemoryError;

    fn read_u32_be(&self, address: GuestAddress) -> Result<u32, Self::Error> {
        GuestMemory::read_u32_be(self, address)
    }

    fn write_u32_be(&mut self, address: GuestAddress, value: u32) -> Result<(), Self::Error> {
        GuestMemory::write_u32_be(self, address, value)
    }
}

#[cfg(test)]
mod tests {
    use super::{GuestAddress, GuestMemory, MemoryError};

    #[test]
    fn reads_and_writes_inside_region() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        memory.write_u8(GuestAddress(0x1003), 0xAB).unwrap();
        assert_eq!(memory.read_u8(GuestAddress(0x1003)), Ok(0xAB));
    }

    #[test]
    fn rejects_addresses_outside_region() {
        let memory = GuestMemory::new(GuestAddress(0x1000), 16);
        assert_eq!(
            memory.read_u8(GuestAddress(0x0FFF)),
            Err(MemoryError::OutOfBounds)
        );
        assert_eq!(
            memory.read_u8(GuestAddress(0x1010)),
            Err(MemoryError::OutOfBounds)
        );
    }

    #[test]
    fn empty_region_has_no_valid_addresses() {
        let memory = GuestMemory::new(GuestAddress(0x1000), 0);
        assert!(memory.is_empty());
        assert_eq!(
            memory.read_u8(GuestAddress(0x1000)),
            Err(MemoryError::OutOfBounds)
        );
    }

    #[test]
    fn region_near_address_space_end_does_not_wrap() {
        let mut memory = GuestMemory::new(GuestAddress(u32::MAX - 1), 4);
        memory.write_u8(GuestAddress(u32::MAX), 0xCD).unwrap();
        assert_eq!(memory.read_u8(GuestAddress(u32::MAX)), Ok(0xCD));
        assert_eq!(
            memory.read_u8(GuestAddress(0)),
            Err(MemoryError::OutOfBounds)
        );
    }

    #[test]
    fn reads_and_writes_big_endian_words() {
        let mut memory = GuestMemory::new(GuestAddress(0x3000), 8);
        memory
            .write_u32_be(GuestAddress(0x3002), 0x1234_ABCD)
            .unwrap();
        assert_eq!(memory.read_u32_be(GuestAddress(0x3002)), Ok(0x1234_ABCD));
        assert_eq!(memory.read_u8(GuestAddress(0x3002)), Ok(0x12));
        assert_eq!(memory.read_u8(GuestAddress(0x3005)), Ok(0xCD));
    }

    #[test]
    fn word_access_rejects_partial_ranges_without_mutating_memory() {
        let mut memory = GuestMemory::new(GuestAddress(0x4000), 3);
        assert_eq!(
            memory.write_u32_be(GuestAddress(0x4000), 0xDEAD_BEEF),
            Err(MemoryError::OutOfBounds)
        );
        assert_eq!(memory.read_u8(GuestAddress(0x4000)), Ok(0));
        assert_eq!(
            memory.read_u32_be(GuestAddress(0x4000)),
            Err(MemoryError::OutOfBounds)
        );
    }

    #[test]
    fn reports_region_length_and_base() {
        let memory = GuestMemory::new(GuestAddress(0x2000), 7);
        assert_eq!(memory.base(), GuestAddress(0x2000));
        assert_eq!(memory.len(), 7);
    }
}
