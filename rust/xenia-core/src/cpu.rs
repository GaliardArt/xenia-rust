//! Minimal, deterministic PowerPC interpreter scaffolding.
//!
//! Only a deliberately small instruction subset is implemented. This is not
//! sufficient to boot an Xbox 360 title; unsupported instructions fail loudly.

use crate::memory::{GuestAddress, GuestMemory, MemoryError};

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
    UnsupportedInstruction { pc: u32, opcode: u32 },
}

impl From<MemoryError> for PpcError {
    fn from(value: MemoryError) -> Self {
        Self::Memory(value)
    }
}

/// Executes a small subset of PPC instructions against guest memory.
#[derive(Clone, Debug, Default)]
pub struct PpcInterpreter {
    pub state: PpcState,
}

impl PpcInterpreter {
    /// Fetches and executes one big-endian 32-bit instruction.
    pub fn step(&mut self, memory: &mut GuestMemory) -> Result<(), PpcError> {
        let pc = self.state.pc;
        let instruction = memory.read_u32_be(GuestAddress(pc))?;
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
