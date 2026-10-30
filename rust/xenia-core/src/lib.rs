//! New Rust implementation track for Xenia Canary.
//! This crate starts with small, testable primitives; it does not yet emulate games.

pub mod address_space;
pub mod cpu;
pub mod layout;
pub mod loader;
pub mod memory;
pub mod pe;
pub mod security;
pub mod xex;

pub use address_space::{AddressSpaceError, GuestAddressSpace, MemoryPermissions};
pub use cpu::{
    PPC_MSR_FE0, PPC_MSR_FE1, PpcError, PpcException, PpcFpExceptionMode, PpcInterpreter,
    PpcState,
};
pub use layout::{ImageLayout, LayoutError, LoadedSection, plan_image_layout};
pub use loader::{
    XEX_HEADER_FILE_FORMAT_INFO, XEX_HEADER_IMAGE_BASE_ADDRESS, XexLoadError, XexLoadedImage,
    load_xex2_image,
};
pub use memory::{GuestAddress, GuestMemory, GuestMemoryAccess, MemoryError};
pub use pe::{PeError, PeSection, XboxPeImage, parse_xbox_pe32};
pub use security::{
    SecurityInfoError, XexPageDescriptor, XexSecurityInfo, parse_xex2_security_info,
};
pub use xex::{XEX_HEADER_ENTRY_POINT, XexError, XexHeader, XexOptionalHeader, parse_xex2_header};
