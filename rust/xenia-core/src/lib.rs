//! New Rust implementation track for Xenia Canary.
//! This crate starts with small, testable primitives; it does not yet emulate games.

pub mod cpu;
pub mod memory;

pub use cpu::{PpcError, PpcInterpreter, PpcState};
pub use memory::{GuestAddress, GuestMemory, MemoryError};
