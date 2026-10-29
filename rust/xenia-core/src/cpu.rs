//! Minimal, deterministic PowerPC interpreter scaffolding.
//!
//! Only a deliberately small instruction subset is implemented. This is not
//! sufficient to boot an Xbox 360 title; unsupported instructions fail loudly.

use core::cmp::Ordering;

use crate::address_space::AddressSpaceError;
use crate::memory::{GuestAddress, GuestMemoryAccess, MemoryError};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PpcState {
    /// General-purpose registers. The Xenon PPC context uses 64-bit GPR storage.
    pub gpr: [u64; 32],
    pub fpr: [f64; 32],
    pub pc: u32,
    pub lr: u64,
    pub ctr: u64,
    pub xer: u64,
    pub cr: [u8; 8],
    pub fpscr: u32,
    pub vscr: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PpcError {
    Memory(MemoryError),
    AddressSpace(AddressSpaceError),
    UnsupportedInstruction { pc: u32, opcode: u32 },
    UnsupportedBranchOption { pc: u32, bo: u8 },
}

impl From<MemoryError> for PpcError {
    fn from(value: MemoryError) -> Self {
        Self::Memory(value)
    }
}

impl From<AddressSpaceError> for PpcError {
    fn from(value: AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

/// Executes a small subset of PPC instructions against guest memory.
#[derive(Clone, Debug, Default)]
pub struct PpcInterpreter {
    pub state: PpcState,
}

impl PpcInterpreter {
    fn set_cr_field(&mut self, field: usize, ordering: Ordering) {
        let result = match ordering {
            Ordering::Less => 0b1000,
            Ordering::Greater => 0b0100,
            Ordering::Equal => 0b0010,
        };
        let summary_overflow = ((self.state.xer >> 31) & 1) as u8;
        self.state.cr[field] = result | summary_overflow;
    }

    /// Fetches and executes one big-endian 32-bit instruction.
    pub fn step<M>(&mut self, memory: &mut M) -> Result<(), PpcError>
    where
        M: GuestMemoryAccess,
        M::Error: Into<PpcError>,
    {
        let pc = self.state.pc;
        let instruction = memory
            .read_u32_be(GuestAddress(pc))
            .map_err(Into::into)?;
        let opcode = instruction >> 26;
        let rt_rs = ((instruction >> 21) & 0x1F) as usize;
        let ra = ((instruction >> 16) & 0x1F) as usize;
        let immediate = instruction as u16 as i16 as i64;
        let next_pc = pc.wrapping_add(4);

        match opcode {
            // addi RT,RA,SI — RA=0 selects the literal zero, not GPR[0].
            14 => {
                let base = if ra == 0 { 0 } else { self.state.gpr[ra] };
                self.state.gpr[rt_rs] = base.wrapping_add(immediate as u64);
                self.state.pc = next_pc;
            }
            // addis RT,RA,SI — sign-extended immediate shifted left by 16.
            15 => {
                let base = if ra == 0 { 0 } else { self.state.gpr[ra] };
                self.state.gpr[rt_rs] = base.wrapping_add((immediate << 16) as u64);
                self.state.pc = next_pc;
            }
            // cmpwi BF,RA,SI — compare signed word or doubleword.
            11 => {
                let field = ((instruction >> 23) & 0x7) as usize;
                let is_64_bit = instruction & (1 << 21) != 0;
                let left = if is_64_bit {
                    self.state.gpr[ra] as i64
                } else {
                    self.state.gpr[ra] as u32 as i32 as i64
                };
                let right = if is_64_bit {
                    immediate
                } else {
                    immediate as i32 as i64
                };
                self.set_cr_field(field, left.cmp(&right));
                self.state.pc = next_pc;
            }
            // cmplwi BF,RA,UI — compare unsigned word or doubleword.
            10 => {
                let field = ((instruction >> 23) & 0x7) as usize;
                let is_64_bit = instruction & (1 << 21) != 0;
                let left = if is_64_bit {
                    self.state.gpr[ra]
                } else {
                    self.state.gpr[ra] as u32 as u64
                };
                let right = (instruction & 0xFFFF) as u64;
                self.set_cr_field(field, left.cmp(&right));
                self.state.pc = next_pc;
            }
            // bc BO,BI,BD,AA,LK — initial support for CR-only beq/bne forms.
            16 => {
                let bo = ((instruction >> 21) & 0x1F) as u8;
                if bo != 4 && bo != 12 {
                    return Err(PpcError::UnsupportedBranchOption { pc, bo });
                }
                let bi = ((instruction >> 16) & 0x1F) as usize;
                let field = bi / 4;
                let bit = 1 << (3 - (bi % 4));
                let condition = self.state.cr[field] & bit != 0;
                let branch_if_true = bo == 12;
                let displacement = ((((instruction >> 2) & 0x3FFF) << 2) as u16 as i16) as i32;
                let absolute = instruction & 0x2 != 0;
                let link = instruction & 1 != 0;
                if link {
                    self.state.lr = next_pc as u64;
                }
                if condition == branch_if_true {
                    self.state.pc = if absolute {
                        displacement as u32
                    } else {
                        pc.wrapping_add(displacement as u32)
                    };
                } else {
                    self.state.pc = next_pc;
                }
            }
            // lwz RT,D(RA) — load a zero-extended 32-bit word.
            32 => {
                let base = if ra == 0 {
                    0
                } else {
                    self.state.gpr[ra] as u32
                };
                let address = base.wrapping_add(immediate as u32);
                self.state.gpr[rt_rs] = memory
                    .read_u32_be(GuestAddress(address))
                    .map_err(Into::into)? as u64;
                self.state.pc = next_pc;
            }
            // stw RS,D(RA) — store the low 32 bits of RS.
            36 => {
                let base = if ra == 0 {
                    0
                } else {
                    self.state.gpr[ra] as u32
                };
                let address = base.wrapping_add(immediate as u32);
                memory
                    .write_u32_be(GuestAddress(address), self.state.gpr[rt_rs] as u32)
                    .map_err(Into::into)?;
                self.state.pc = next_pc;
            }
            // ori RA,RS,UIMM — destination is RA, source is RS.
            24 => {
                let destination = ra;
                let source = rt_rs;
                let uimm = (instruction & 0xFFFF) as u64;
                self.state.gpr[destination] = self.state.gpr[source] | uimm;
                self.state.pc = next_pc;
            }
            // oris RA,RS,UIMM
            25 => {
                let destination = ra;
                let source = rt_rs;
                let uimm = ((instruction & 0xFFFF) as u64) << 16;
                self.state.gpr[destination] = self.state.gpr[source] | uimm;
                self.state.pc = next_pc;
            }
            // b LI,AA,LK
            18 => {
                let displacement = (((instruction >> 2) & 0x00FF_FFFF) << 2) as i32;
                let displacement = (displacement << 6 >> 6) as i64;
                let absolute = instruction & 0x2 != 0;
                let link = instruction & 1 != 0;
                if link {
                    self.state.lr = next_pc as u64;
                }
                self.state.pc = if absolute {
                    displacement as u32
                } else {
                    pc.wrapping_add(displacement as u32)
                };
            }
            _ => {
                return Err(PpcError::UnsupportedInstruction { pc, opcode });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{PpcError, PpcInterpreter};
    use crate::address_space::{GuestAddressSpace, MemoryPermissions};
    use crate::memory::{GuestAddress, GuestMemory, MemoryError};

    fn put_instruction(memory: &mut GuestMemory, pc: u32, instruction: u32) {
        memory.write_u32_be(GuestAddress(pc), instruction).unwrap();
    }

    #[test]
    fn addi_uses_zero_when_ra_is_zero_and_sign_extends_immediate() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        // addi r3, r0, -1
        put_instruction(&mut memory, 0x1000, (14 << 26) | (3 << 21) | 0xFFFF);
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.gpr[3], u64::MAX);
        assert_eq!(cpu.state.pc, 0x1004);
    }

    #[test]
    fn ori_uses_rs_as_source_and_ra_as_destination() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        put_instruction(
            &mut memory,
            0x1000,
            (24 << 26) | (4 << 21) | (5 << 16) | 0x00FF,
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[4] = 0x1200;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.gpr[5], 0x12FF);
        assert_eq!(cpu.state.pc, 0x1004);
    }

    #[test]
    fn relative_branch_updates_pc_and_link_register() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        // b +8, LK=1
        put_instruction(&mut memory, 0x1000, (18 << 26) | (2 << 2) | 1);
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.pc, 0x1008);
        assert_eq!(cpu.state.lr, 0x1004);
    }

    #[test]
    fn cmpwi_sets_equal_condition_and_beq_branches() {
        let mut memory = GuestMemory::new(GuestAddress(0), 0x2000);
        // cmpwi cr0,r3,7
        put_instruction(&mut memory, 0x1000, (11 << 26) | (3 << 16) | 7);
        // beq cr0,+8: BO=12, BI=2
        put_instruction(
            &mut memory,
            0x1004,
            (16 << 26) | (12 << 21) | (2 << 16) | (2 << 2),
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 7;

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.cr[0] & 0x0E, 0b0010);
        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.pc, 0x100C);
    }

    #[test]
    fn stw_and_lwz_round_trip_big_endian_memory() {
        let mut memory = GuestMemory::new(GuestAddress(0), 0x2000);
        // stw r3, 16(r0)
        put_instruction(&mut memory, 0x1000, (36 << 26) | (3 << 21) | 16);
        // lwz r4, 16(r0)
        put_instruction(&mut memory, 0x1004, (32 << 26) | (4 << 21) | 16);
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1234_ABCD_FEDC_BA98;

        cpu.step(&mut memory).unwrap();
        assert_eq!(memory.read_u32_be(GuestAddress(0x10)), Ok(0xFEDC_BA98));
        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.gpr[4], 0xFEDC_BA98);
        assert_eq!(cpu.state.pc, 0x1008);
    }

    #[test]
    fn interpreter_uses_permissioned_address_space() {
        let mut memory = GuestAddressSpace::new();
        memory
            .map(
                GuestAddress(0x1000),
                16,
                MemoryPermissions::READ_WRITE,
            )
            .unwrap();
        memory
            .write_u32_be(GuestAddress(0x1000), (14 << 26) | (3 << 21) | 9)
            .unwrap();
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.gpr[3], 9);
        assert_eq!(cpu.state.pc, 0x1004);
    }

    #[test]
    fn unsupported_instruction_fails_instead_of_succeeding_silently() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        put_instruction(&mut memory, 0x1000, 0);
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;

        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::UnsupportedInstruction {
                pc: 0x1000,
                opcode: 0
            })
        );
    }

    #[test]
    fn instruction_fetch_reports_memory_errors() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 0);
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;

        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::Memory(MemoryError::OutOfBounds))
        );
    }
}
