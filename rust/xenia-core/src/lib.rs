//! New Rust implementation track for Xenia Canary.
//! This crate starts with small, testable primitives; it does not yet emulate games.

pub mod address_space;
pub mod cpu;
pub mod memory;
pub mod layout;
pub mod pe;
pub mod xex;

pub use address_space::{AddressSpaceError, GuestAddressSpace, MemoryPermissions};
pub use cpu::{PpcError, PpcInterpreter, PpcState};
pub use memory::{GuestAddress, GuestMemory, GuestMemoryAccess, MemoryError};
pub use layout::{ImageLayout, LayoutError, LoadedSection, plan_image_layout};
pub use pe::{PeError, PeSection, XboxPeImage, parse_xbox_pe32};
pub use xex::{XEX_HEADER_ENTRY_POINT, XexError, XexHeader, XexOptionalHeader, parse_xex2_header};
