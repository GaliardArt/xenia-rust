//! Bounds-checked guest-memory primitives. This is not yet a full Xbox 360 MMU.

use core::fmt;

/// A 32-bit guest virtual address used by the initial memory model.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GuestAddress(pub u32);

/// Errors returned by checked guest-memory operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryError {
    OutOfBounds,
    AllocationFailed,
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds => f.write_str("guest memory access out of bounds"),
            Self::AllocationFailed => f.write_str("unable to allocate guest memory"),
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
        Self::try_new(base, size).expect("guest memory allocation failed")
    }

    pub fn try_new(base: GuestAddress, size: usize) -> Result<Self, MemoryError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| MemoryError::AllocationFailed)?;
        bytes.resize(size, 0);
        Ok(Self { base, bytes })
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

    pub fn write_bytes(&mut self, address: GuestAddress, data: &[u8]) -> Result<(), MemoryError> {
        let offset = self.offset_of(address)?;
        let end = offset
            .checked_add(data.len())
            .ok_or(MemoryError::OutOfBounds)?;
        let destination = self
            .bytes
            .get_mut(offset..end)
            .ok_or(MemoryError::OutOfBounds)?;
        destination.copy_from_slice(data);
        Ok(())
    }

    pub fn read_bytes(&self, address: GuestAddress, size: usize) -> Result<&[u8], MemoryError> {
        let offset = self.offset_of(address)?;
        let end = offset.checked_add(size).ok_or(MemoryError::OutOfBounds)?;
        self.bytes.get(offset..end).ok_or(MemoryError::OutOfBounds)
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

    /// Reads a 16-bit value in the Xbox 360's big-endian byte order.
    pub fn read_u16_be(&self, address: GuestAddress) -> Result<u16, MemoryError> {
        let bytes = self.read_bytes(address, 2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Writes a 16-bit value in the Xbox 360's big-endian byte order atomically.
    pub fn write_u16_be(&mut self, address: GuestAddress, value: u16) -> Result<(), MemoryError> {
        self.write_bytes(address, &value.to_be_bytes())
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

    /// Reads a 64-bit value in the Xbox 360's big-endian byte order.
    pub fn read_u64_be(&self, address: GuestAddress) -> Result<u64, MemoryError> {
        let offset = self.offset_of(address)?;
        let end = offset.checked_add(8).ok_or(MemoryError::OutOfBounds)?;
        let bytes = self
            .bytes
            .get(offset..end)
            .ok_or(MemoryError::OutOfBounds)?;
        Ok(u64::from_be_bytes(
            bytes.try_into().expect("slice length is checked"),
        ))
    }

    /// Writes a 64-bit value in the Xbox 360's big-endian byte order.
    pub fn write_u64_be(&mut self, address: GuestAddress, value: u64) -> Result<(), MemoryError> {
        let offset = self.offset_of(address)?;
        let end = offset.checked_add(8).ok_or(MemoryError::OutOfBounds)?;
        let bytes = self
            .bytes
            .get_mut(offset..end)
            .ok_or(MemoryError::OutOfBounds)?;
        bytes.copy_from_slice(&value.to_be_bytes());
        Ok(())
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

    fn read_u8(&self, address: GuestAddress) -> Result<u8, Self::Error>;

    fn write_u8(&mut self, address: GuestAddress, value: u8) -> Result<(), Self::Error>;

    /// Reads a contiguous byte range into a caller-owned buffer.
    fn read_bytes(&self, address: GuestAddress, data: &mut [u8]) -> Result<(), Self::Error> {
        for (offset, byte) in data.iter_mut().enumerate() {
            *byte = self.read_u8(GuestAddress(address.0.wrapping_add(offset as u32)))?;
        }
        Ok(())
    }

    /// Writes a contiguous byte range. Implementations backed by ordinary RAM
    /// should override this with an all-or-nothing bounds check and copy.
    fn write_bytes(&mut self, address: GuestAddress, data: &[u8]) -> Result<(), Self::Error> {
        for (offset, byte) in data.iter().copied().enumerate() {
            self.write_u8(GuestAddress(address.0.wrapping_add(offset as u32)), byte)?;
        }
        Ok(())
    }

    fn read_u16_be(&self, address: GuestAddress) -> Result<u16, Self::Error> {
        let high = self.read_u8(address)?;
        let low = self.read_u8(GuestAddress(address.0.wrapping_add(1)))?;
        Ok(u16::from_be_bytes([high, low]))
    }

    fn write_u16_be(&mut self, address: GuestAddress, value: u16) -> Result<(), Self::Error> {
        let [high, low] = value.to_be_bytes();
        self.write_u8(address, high)?;
        self.write_u8(GuestAddress(address.0.wrapping_add(1)), low)
    }

    fn read_u32_be(&self, address: GuestAddress) -> Result<u32, Self::Error>;

    fn read_u64_be(&self, address: GuestAddress) -> Result<u64, Self::Error>;

    fn write_u64_be(&mut self, address: GuestAddress, value: u64) -> Result<(), Self::Error>;

    fn fetch_u32_be(&self, address: GuestAddress) -> Result<u32, Self::Error> {
        self.read_u32_be(address)
    }

    fn write_u32_be(&mut self, address: GuestAddress, value: u32) -> Result<(), Self::Error>;
}

impl GuestMemoryAccess for GuestMemory {
    type Error = MemoryError;

    fn read_u8(&self, address: GuestAddress) -> Result<u8, Self::Error> {
        GuestMemory::read_u8(self, address)
    }

    fn write_u8(&mut self, address: GuestAddress, value: u8) -> Result<(), Self::Error> {
        GuestMemory::write_u8(self, address, value)
    }

    fn write_bytes(&mut self, address: GuestAddress, data: &[u8]) -> Result<(), Self::Error> {
        GuestMemory::write_bytes(self, address, data)
    }

    fn read_bytes(&self, address: GuestAddress, data: &mut [u8]) -> Result<(), Self::Error> {
        data.copy_from_slice(GuestMemory::read_bytes(self, address, data.len())?);
        Ok(())
    }

    fn read_u16_be(&self, address: GuestAddress) -> Result<u16, Self::Error> {
        GuestMemory::read_u16_be(self, address)
    }

    fn write_u16_be(&mut self, address: GuestAddress, value: u16) -> Result<(), Self::Error> {
        GuestMemory::write_u16_be(self, address, value)
    }

    fn read_u32_be(&self, address: GuestAddress) -> Result<u32, Self::Error> {
        GuestMemory::read_u32_be(self, address)
    }

    fn read_u64_be(&self, address: GuestAddress) -> Result<u64, Self::Error> {
        GuestMemory::read_u64_be(self, address)
    }

    fn write_u64_be(&mut self, address: GuestAddress, value: u64) -> Result<(), Self::Error> {
        GuestMemory::write_u64_be(self, address, value)
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
    fn reads_and_writes_big_endian_doublewords_atomically() {
        let mut memory = GuestMemory::new(GuestAddress(0x5000), 8);
        memory
            .write_u64_be(GuestAddress(0x5000), 0x0123_4567_89AB_CDEF)
            .unwrap();
        assert_eq!(
            memory.read_u64_be(GuestAddress(0x5000)),
            Ok(0x0123_4567_89AB_CDEF)
        );
        assert_eq!(memory.read_u8(GuestAddress(0x5000)), Ok(0x01));

        let mut short = GuestMemory::new(GuestAddress(0x6000), 7);
        assert_eq!(
            short.write_u64_be(GuestAddress(0x6000), u64::MAX),
            Err(MemoryError::OutOfBounds)
        );
        assert_eq!(short.read_u8(GuestAddress(0x6000)), Ok(0));
        assert_eq!(
            short.read_u64_be(GuestAddress(0x6000)),
            Err(MemoryError::OutOfBounds)
        );
    }

    #[test]
    fn contiguous_writes_are_atomic_when_the_range_is_out_of_bounds() {
        let mut memory = GuestMemory::new(GuestAddress(0x7000), 4);
        memory
            .write_bytes(GuestAddress(0x7000), &[1, 2, 3, 4])
            .unwrap();

        assert_eq!(
            memory.write_bytes(GuestAddress(0x7002), &[9, 8, 7]),
            Err(MemoryError::OutOfBounds)
        );
        assert_eq!(
            memory.read_bytes(GuestAddress(0x7000), 4),
            Ok(&[1, 2, 3, 4][..])
        );
    }

    #[test]
    fn reports_region_length_and_base() {
        let memory = GuestMemory::new(GuestAddress(0x2000), 7);
        assert_eq!(memory.base(), GuestAddress(0x2000));
        assert_eq!(memory.len(), 7);
    }
}
