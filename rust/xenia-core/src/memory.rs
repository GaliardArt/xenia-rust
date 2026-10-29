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
        Self { base, bytes: vec![0; size] }
    }

    pub const fn base(&self) -> GuestAddress { self.base }
    pub fn len(&self) -> usize { self.bytes.len() }
    pub fn is_empty(&self) -> bool { self.bytes.is_empty() }

    pub fn read_u8(&self, address: GuestAddress) -> Result<u8, MemoryError> {
        let offset = self.offset_of(address)?;
        self.bytes.get(offset).copied().ok_or(MemoryError::OutOfBounds)
    }

    pub fn write_u8(&mut self, address: GuestAddress, value: u8) -> Result<(), MemoryError> {
        let offset = self.offset_of(address)?;
        let byte = self.bytes.get_mut(offset).ok_or(MemoryError::OutOfBounds)?;
        *byte = value;
        Ok(())
    }

    fn offset_of(&self, address: GuestAddress) -> Result<usize, MemoryError> {
        let offset = address.0.checked_sub(self.base.0).ok_or(MemoryError::OutOfBounds)? as usize;
        if offset >= self.bytes.len() { return Err(MemoryError::OutOfBounds); }
        Ok(offset)
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
        assert_eq!(memory.read_u8(GuestAddress(0x0FFF)), Err(MemoryError::OutOfBounds));
        assert_eq!(memory.read_u8(GuestAddress(0x1010)), Err(MemoryError::OutOfBounds));
    }

    #[test]
    fn empty_region_has_no_valid_addresses() {
        let memory = GuestMemory::new(GuestAddress(0x1000), 0);
        assert!(memory.is_empty());
        assert_eq!(memory.read_u8(GuestAddress(0x1000)), Err(MemoryError::OutOfBounds));
    }
}
