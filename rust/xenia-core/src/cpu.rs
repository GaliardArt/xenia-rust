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
    fn branch_condition(&mut self, bo: u8, bi: usize) -> bool {
        let ignore_cr = bo & 0x10 != 0;
        let expected_cr = bo & 0x08 != 0;
        let ignore_ctr = bo & 0x04 != 0;
        let branch_if_ctr_zero = bo & 0x02 != 0;

        let condition_ok = if ignore_cr {
            true
        } else {
            let field = bi / 4;
            let bit = 1 << (3 - (bi % 4));
            (self.state.cr[field] & bit != 0) == expected_cr
        };

        let count_ok = if ignore_ctr {
            true
        } else {
            self.state.ctr = self.state.ctr.wrapping_sub(1);
            (self.state.ctr != 0) != branch_if_ctr_zero
        };

        condition_ok && count_ok
    }

    fn set_cr_field(&mut self, field: usize, ordering: Ordering) {
        let result = match ordering {
            Ordering::Less => 0b1000,
            Ordering::Greater => 0b0100,
            Ordering::Equal => 0b0010,
        };
        let summary_overflow = ((self.state.xer >> 31) & 1) as u8;
        self.state.cr[field] = result | summary_overflow;
    }

    fn update_overflow(&mut self, overflow: bool) {
        const SO: u64 = 1 << 31;
        const OV: u64 = 1 << 30;
        self.state.xer &= !OV;
        if overflow {
            self.state.xer |= OV | SO;
        }
    }

    fn set_carry(&mut self, carry: bool) {
        const CA: u64 = 1 << 29;
        self.state.xer = (self.state.xer & !CA) | ((carry as u64) << 29);
    }

    fn rotate_mask32(begin: u32, end: u32) -> u32 {
        let mut mask = 0_u32;
        let mut bit = begin;
        loop {
            mask |= 1_u32 << (31 - bit);
            if bit == end {
                break;
            }
            bit = (bit + 1) & 0x1F;
        }
        mask
    }

    fn set_cr_bit(&mut self, bit_index: usize, value: bool) {
        let field = bit_index / 4;
        let mask = 1 << (3 - (bit_index % 4));
        if value {
            self.state.cr[field] |= mask;
        } else {
            self.state.cr[field] &= !mask;
        }
    }

    /// Fetches and executes one big-endian 32-bit instruction.
    pub fn step<M>(&mut self, memory: &mut M) -> Result<(), PpcError>
    where
        M: GuestMemoryAccess,
        M::Error: Into<PpcError>,
    {
        let pc = self.state.pc;
        let instruction = memory.fetch_u32_be(GuestAddress(pc)).map_err(Into::into)?;
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
            // mulli RT,RA,SI — multiply by a sign-extended immediate.
            7 => {
                let base = self.state.gpr[ra];
                self.state.gpr[rt_rs] = (base as i64).wrapping_mul(immediate) as u64;
                self.state.pc = next_pc;
            }
            // subfic RT,RA,SI — subtract RA from the sign-extended immediate.
            8 => {
                let base = self.state.gpr[ra];
                let minuend = immediate as u64;
                let (value, borrow) = minuend.overflowing_sub(base);
                self.state.gpr[rt_rs] = value;
                self.set_carry(!borrow);
                self.state.pc = next_pc;
            }
            // addic/addic. RT,RA,SI — add immediate and set carry.
            12 | 13 => {
                let base = self.state.gpr[ra];
                let (value, carry) = base.overflowing_add(immediate as u64);
                self.state.gpr[rt_rs] = value;
                self.set_carry(carry);
                if opcode == 13 {
                    self.set_cr_field(0, (value as i64).cmp(&0));
                }
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
            // rlwimi/rlwinm/rlwnm — rotate left word, optionally merge, then mask.
            20 | 21 | 23 => {
                let shift = if opcode == 23 {
                    (self.state.gpr[((instruction >> 11) & 0x1F) as usize] as u32) & 0x1F
                } else {
                    (instruction >> 11) & 0x1F
                };
                let mask =
                    Self::rotate_mask32((instruction >> 6) & 0x1F, (instruction >> 1) & 0x1F);
                let rotated = (self.state.gpr[rt_rs] as u32).rotate_left(shift);
                let value = if opcode == 20 {
                    ((self.state.gpr[ra] as u32) & !mask) | (rotated & mask)
                } else {
                    rotated & mask
                };
                self.state.gpr[ra] = value as u64;
                if instruction & 1 != 0 {
                    self.set_cr_field(0, (value as i32).cmp(&0));
                }
                self.state.pc = next_pc;
            }
            // xori/xoris, andi./andis. — immediate logical operations.
            26..=29 => {
                let source = self.state.gpr[rt_rs];
                let immediate = (instruction & 0xFFFF) as u64;
                let (value, record) = match opcode {
                    26 => (source ^ immediate, false),
                    27 => (source ^ (immediate << 16), false),
                    28 => (source & immediate, true),
                    29 => (source & (immediate << 16), true),
                    _ => unreachable!(),
                };
                self.state.gpr[ra] = value;
                if record {
                    self.set_cr_field(0, (value as i64).cmp(&0));
                }
                self.state.pc = next_pc;
            }
            // bc BO,BI,BD,AA,LK — CR and CTR condition handling.
            16 => {
                let bo = ((instruction >> 21) & 0x1F) as u8;
                let bi = ((instruction >> 16) & 0x1F) as usize;
                let displacement = ((((instruction >> 2) & 0x3FFF) << 2) as u16 as i16) as i32;
                let absolute = instruction & 0x2 != 0;
                let link = instruction & 1 != 0;
                let taken = self.branch_condition(bo, bi);
                if link {
                    self.state.lr = next_pc as u64;
                }
                self.state.pc = if taken {
                    if absolute {
                        displacement as u32
                    } else {
                        pc.wrapping_add(displacement as u32)
                    }
                } else {
                    next_pc
                };
            }
            // bclr/bcctr — branch to the link or count register.
            19 => {
                let bo = ((instruction >> 21) & 0x1F) as u8;
                let bi = ((instruction >> 16) & 0x1F) as usize;
                let extended_opcode = (instruction >> 1) & 0x3FF;
                if matches!(
                    extended_opcode,
                    257 | 129 | 289 | 225 | 33 | 449 | 417 | 193
                ) {
                    let bt = bo as usize;
                    let ba = bi;
                    let bb = ((instruction >> 11) & 0x1F) as usize;
                    let a = self.state.cr[ba / 4] & (1 << (3 - (ba % 4))) != 0;
                    let b = self.state.cr[bb / 4] & (1 << (3 - (bb % 4))) != 0;
                    match extended_opcode {
                        257 => self.set_cr_bit(bt, a & b),
                        129 => self.set_cr_bit(bt, a & !b),
                        289 => self.set_cr_bit(bt, a == b),
                        225 => self.set_cr_bit(bt, !(a & b)),
                        33 => self.set_cr_bit(bt, !(a | b)),
                        449 => self.set_cr_bit(bt, a | b),
                        417 => self.set_cr_bit(bt, a | !b),
                        193 => self.set_cr_bit(bt, a ^ b),
                        _ => unreachable!(),
                    }
                    self.state.pc = next_pc;
                    return Ok(());
                }
                let target = match extended_opcode {
                    16 => self.state.lr as u32 & !3,
                    528 => {
                        if bo & 0x04 == 0 {
                            return Err(PpcError::UnsupportedBranchOption { pc, bo });
                        }
                        self.state.ctr as u32 & !3
                    }
                    _ => return Err(PpcError::UnsupportedInstruction { pc, opcode }),
                };
                let taken = self.branch_condition(bo, bi);
                let link = instruction & 1 != 0;
                if link {
                    self.state.lr = next_pc as u64;
                }
                self.state.pc = if taken { target } else { next_pc };
            }
            // cmpw/cmplw, add, subf, and, or, xor — common X-form integer operations.
            31 => {
                let rb = ((instruction >> 11) & 0x1F) as usize;
                let extended_opcode = (instruction >> 1) & 0x3FF;
                let xo = extended_opcode & 0x1FF;
                let record = instruction & 1 != 0;
                let oe = instruction & (1 << 10) != 0;
                let left = self.state.gpr[ra];
                let right = self.state.gpr[rb];
                match extended_opcode {
                    0 | 32 => {
                        let field = ((instruction >> 23) & 0x7) as usize;
                        let is_64_bit = instruction & (1 << 21) != 0;
                        let ordering = if extended_opcode == 0 {
                            if is_64_bit {
                                (left as i64).cmp(&(right as i64))
                            } else {
                                (left as u32 as i32).cmp(&(right as u32 as i32))
                            }
                        } else if is_64_bit {
                            left.cmp(&right)
                        } else {
                            (left as u32).cmp(&(right as u32))
                        };
                        self.set_cr_field(field, ordering);
                    }
                    266 | 778 | 40 | 552 | 10 | 522 | 138 | 650 | 8 | 520 | 136 | 648 => {
                        let (value, carry, overflow) = match xo {
                            266 | 10 | 138 => {
                                let carry_in = if xo == 138 && self.state.xer & (1 << 29) != 0 {
                                    1_u64
                                } else {
                                    0
                                };
                                let (partial, c1) = left.overflowing_add(right);
                                let (value, c2) = partial.overflowing_add(carry_in);
                                let overflow = ((!(left ^ right) & (left ^ value)) >> 63) != 0;
                                (value, c1 || c2, overflow)
                            }
                            40 | 8 | 136 => {
                                let borrow_in = if xo == 136 && self.state.xer & (1 << 29) == 0 {
                                    1_u64
                                } else {
                                    0_u64
                                };
                                let (partial, b1) = right.overflowing_sub(left);
                                let (value, b2) = partial.overflowing_sub(borrow_in);
                                let overflow = (((right ^ left) & (right ^ value)) >> 63) != 0;
                                (value, !(b1 || b2), overflow)
                            }
                            _ => unreachable!(),
                        };
                        self.state.gpr[rt_rs] = value;
                        if oe {
                            self.update_overflow(overflow);
                        }
                        if xo == 10 || xo == 8 || xo == 138 || xo == 136 {
                            self.set_carry(carry);
                        }
                        if record {
                            self.set_cr_field(0, (value as i64).cmp(&0));
                        }
                    }
                    104 | 616 => {
                        let value = self.state.gpr[ra].wrapping_neg();
                        let overflow = self.state.gpr[ra] == (1_u64 << 63);
                        self.state.gpr[rt_rs] = value;
                        if oe {
                            self.update_overflow(overflow);
                        }
                        if record {
                            self.set_cr_field(0, (value as i64).cmp(&0));
                        }
                    }
                    235 | 747 => {
                        let a = self.state.gpr[ra] as u32 as i32 as i64;
                        let b = self.state.gpr[rb] as u32 as i32 as i64;
                        let product = a.wrapping_mul(b);
                        let value = product as u32 as i32 as i64 as u64;
                        let overflow = product != (product as i32 as i64);
                        self.state.gpr[rt_rs] = value;
                        if oe {
                            self.update_overflow(overflow);
                        }
                        if record {
                            self.set_cr_field(0, (value as i64).cmp(&0));
                        }
                    }
                    11 | 75 => {
                        let a = if xo == 75 {
                            self.state.gpr[ra] as u32 as i32 as i64
                        } else {
                            self.state.gpr[ra] as u32 as u64 as i64
                        };
                        let b = if xo == 75 {
                            self.state.gpr[rb] as u32 as i32 as i64
                        } else {
                            self.state.gpr[rb] as u32 as u64 as i64
                        };
                        let value = ((a * b) >> 32) as u64;
                        self.state.gpr[rt_rs] = value;
                        if record {
                            self.set_cr_field(0, (value as i64).cmp(&0));
                        }
                    }
                    459 | 491 | 971 | 1003 => {
                        let dividend = self.state.gpr[ra] as u32;
                        let divisor = self.state.gpr[rb] as u32;
                        let signed = xo == 491;
                        let (value, overflow) = if divisor == 0 {
                            (0, true)
                        } else if signed {
                            let a = dividend as i32;
                            let b = divisor as i32;
                            if a == i32::MIN && b == -1 {
                                (0, true)
                            } else {
                                ((a / b) as u32 as u64, false)
                            }
                        } else {
                            ((dividend / divisor) as u64, false)
                        };
                        self.state.gpr[rt_rs] = value;
                        if oe {
                            self.update_overflow(overflow);
                        }
                        if record {
                            self.set_cr_field(0, (value as i64).cmp(&0));
                        }
                    }
                    87 | 119 | 279 | 311 | 343 | 375 | 23 | 55 => {
                        let update = matches!(xo, 119 | 311 | 375 | 55);
                        if update && (ra == 0 || ra == rt_rs) {
                            return Err(PpcError::UnsupportedInstruction { pc, opcode });
                        }
                        let base = if ra == 0 {
                            0
                        } else {
                            self.state.gpr[ra] as u32
                        };
                        let address = base.wrapping_add(self.state.gpr[rb] as u32);
                        let value = match xo {
                            87 | 119 => {
                                memory.read_u8(GuestAddress(address)).map_err(Into::into)? as u64
                            }
                            279 | 311 => memory
                                .read_u16_be(GuestAddress(address))
                                .map_err(Into::into)?
                                as u64,
                            343 | 375 => {
                                (memory
                                    .read_u16_be(GuestAddress(address))
                                    .map_err(Into::into)? as i16
                                    as i64) as u64
                            }
                            23 | 55 => memory
                                .read_u32_be(GuestAddress(address))
                                .map_err(Into::into)? as u64,
                            _ => unreachable!(),
                        };
                        self.state.gpr[rt_rs] = value;
                        if update {
                            self.state.gpr[ra] = address as u64;
                        }
                    }
                    215 | 247 | 407 | 439 | 151 | 183 => {
                        let update = matches!(xo, 247 | 439 | 183);
                        if update && ra == 0 {
                            return Err(PpcError::UnsupportedInstruction { pc, opcode });
                        }
                        let base = if ra == 0 {
                            0
                        } else {
                            self.state.gpr[ra] as u32
                        };
                        let address = base.wrapping_add(self.state.gpr[rb] as u32);
                        match xo {
                            215 | 247 => memory
                                .write_u8(GuestAddress(address), self.state.gpr[rt_rs] as u8)
                                .map_err(Into::into)?,
                            407 | 439 => memory
                                .write_u16_be(GuestAddress(address), self.state.gpr[rt_rs] as u16)
                                .map_err(Into::into)?,
                            151 | 183 => memory
                                .write_u32_be(GuestAddress(address), self.state.gpr[rt_rs] as u32)
                                .map_err(Into::into)?,
                            _ => unreachable!(),
                        }
                        if update {
                            self.state.gpr[ra] = address as u64;
                        }
                    }
                    24 | 536 | 792 | 824 => {
                        let source = left as u32 as i32;
                        let shift = if extended_opcode == 824 {
                            rb as u32
                        } else {
                            (right as u32) & 0x3F
                        };
                        let value = match extended_opcode {
                            24 => {
                                if shift > 31 {
                                    0
                                } else {
                                    (left as u32).wrapping_shl(shift)
                                }
                            }
                            536 => {
                                if shift > 31 {
                                    0
                                } else {
                                    (left as u32) >> shift
                                }
                            }
                            792 => {
                                if shift > 31 {
                                    if source < 0 { u32::MAX } else { 0 }
                                } else {
                                    (source >> shift) as u32
                                }
                            }
                            824 => {
                                if shift == 0 {
                                    left as u32
                                } else {
                                    (source >> shift) as u32
                                }
                            }
                            _ => unreachable!(),
                        };
                        if extended_opcode == 792 || extended_opcode == 824 {
                            let shifted_out = match shift {
                                0 => false,
                                1..=31 => ((left as u32) & ((1_u32 << shift) - 1)) != 0,
                                _ => (left as u32) != 0,
                            };
                            self.set_carry(source < 0 && shifted_out);
                        }
                        self.state.gpr[rt_rs] = value as u64;
                        if record {
                            self.set_cr_field(0, (value as i32).cmp(&0));
                        }
                    }
                    26 | 58 | 954 | 922 | 986 => {
                        let value = match extended_opcode {
                            26 => (left as u32).leading_zeros() as u64,
                            58 => left.leading_zeros() as u64,
                            954 => (left as u8 as i8 as i64) as u64,
                            922 => (left as u16 as i16 as i64) as u64,
                            986 => (left as u32 as i32 as i64) as u64,
                            _ => unreachable!(),
                        };
                        self.state.gpr[rt_rs] = value;
                        if record {
                            self.set_cr_field(0, (value as i64).cmp(&0));
                        }
                    }
                    28 | 60 | 124 | 284 | 316 | 412 | 444 | 476 => {
                        let value = match extended_opcode {
                            28 => left & right,
                            60 => left & !right,
                            124 => !(left | right),
                            284 => !(left ^ right),
                            316 => left ^ right,
                            412 => left | !right,
                            444 => left | right,
                            476 => !(left & right),
                            _ => unreachable!(),
                        };
                        self.state.gpr[ra] = value;
                        if record {
                            self.set_cr_field(0, (value as i64).cmp(&0));
                        }
                    }
                    _ => return Err(PpcError::UnsupportedInstruction { pc, opcode }),
                }
                self.state.pc = next_pc;
            }
            // lbz RT,D(RA) — load a zero-extended byte.
            34 => {
                let base = if ra == 0 {
                    0
                } else {
                    self.state.gpr[ra] as u32
                };
                let address = base.wrapping_add(immediate as u32);
                self.state.gpr[rt_rs] =
                    memory.read_u8(GuestAddress(address)).map_err(Into::into)? as u64;
                self.state.pc = next_pc;
            }
            // lhz/lha RT,D(RA) — load an unsigned/signed big-endian halfword.
            40 | 42 => {
                let base = if ra == 0 {
                    0
                } else {
                    self.state.gpr[ra] as u32
                };
                let address = base.wrapping_add(immediate as u32);
                let value = memory
                    .read_u16_be(GuestAddress(address))
                    .map_err(Into::into)?;
                self.state.gpr[rt_rs] = if opcode == 42 {
                    (value as i16 as i64) as u64
                } else {
                    value as u64
                };
                self.state.pc = next_pc;
            }
            // stb/sth RS,D(RA) — store the low byte/halfword in guest big-endian order.
            38 | 44 => {
                let base = if ra == 0 {
                    0
                } else {
                    self.state.gpr[ra] as u32
                };
                let address = base.wrapping_add(immediate as u32);
                if opcode == 38 {
                    memory
                        .write_u8(GuestAddress(address), self.state.gpr[rt_rs] as u8)
                        .map_err(Into::into)?;
                } else {
                    memory
                        .write_u16_be(GuestAddress(address), self.state.gpr[rt_rs] as u16)
                        .map_err(Into::into)?;
                }
                self.state.pc = next_pc;
            }
            // Update-form D loads: effective address is written back to RA.
            33 | 35 | 41 | 43 => {
                if ra == 0 || ra == rt_rs {
                    return Err(PpcError::UnsupportedInstruction { pc, opcode });
                }
                let address = (self.state.gpr[ra] as u32).wrapping_add(immediate as u32);
                let value = match opcode {
                    33 => memory
                        .read_u32_be(GuestAddress(address))
                        .map_err(Into::into)? as u64,
                    35 => memory.read_u8(GuestAddress(address)).map_err(Into::into)? as u64,
                    41 => memory
                        .read_u16_be(GuestAddress(address))
                        .map_err(Into::into)? as u64,
                    43 => {
                        (memory
                            .read_u16_be(GuestAddress(address))
                            .map_err(Into::into)? as i16 as i64) as u64
                    }
                    _ => unreachable!(),
                };
                self.state.gpr[rt_rs] = value;
                self.state.gpr[ra] = address as u64;
                self.state.pc = next_pc;
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
            // Update-form D stores.
            37 | 39 | 45 => {
                if ra == 0 {
                    return Err(PpcError::UnsupportedInstruction { pc, opcode });
                }
                let address = (self.state.gpr[ra] as u32).wrapping_add(immediate as u32);
                match opcode {
                    37 => memory
                        .write_u32_be(GuestAddress(address), self.state.gpr[rt_rs] as u32)
                        .map_err(Into::into)?,
                    39 => memory
                        .write_u8(GuestAddress(address), self.state.gpr[rt_rs] as u8)
                        .map_err(Into::into)?,
                    45 => memory
                        .write_u16_be(GuestAddress(address), self.state.gpr[rt_rs] as u16)
                        .map_err(Into::into)?,
                    _ => unreachable!(),
                }
                self.state.gpr[ra] = address as u64;
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
    fn byte_and_halfword_load_store_preserve_big_endian_semantics() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 32);
        // lbz r3,0(r4); lhz r5,2(r4); lha r6,4(r4); stb r3,6(r4); sth r5,8(r4)
        put_instruction(&mut memory, 0x1000, (34 << 26) | (3 << 21) | (4 << 16));
        put_instruction(&mut memory, 0x1004, (40 << 26) | (5 << 21) | (4 << 16) | 2);
        put_instruction(&mut memory, 0x1008, (42 << 26) | (6 << 21) | (4 << 16) | 2);
        put_instruction(&mut memory, 0x100C, (38 << 26) | (3 << 21) | (4 << 16) | 6);
        put_instruction(&mut memory, 0x1010, (44 << 26) | (5 << 21) | (4 << 16) | 8);
        memory.write_u8(GuestAddress(0x1014), 0xAB).unwrap();
        memory.write_u8(GuestAddress(0x1016), 0xFF).unwrap();
        memory.write_u8(GuestAddress(0x1017), 0x80).unwrap();

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[4] = 0x1014;

        for _ in 0..5 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.gpr[3], 0xAB);
        assert_eq!(cpu.state.gpr[5], 0xFF80);
        assert_eq!(cpu.state.gpr[6], (-128_i64) as u64);
        assert_eq!(memory.read_u8(GuestAddress(0x101A)), Ok(0xAB));
        assert_eq!(memory.read_u8(GuestAddress(0x101C)), Ok(0xFF));
        assert_eq!(memory.read_u8(GuestAddress(0x101D)), Ok(0x80));
        assert_eq!(cpu.state.pc, 0x1014);
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
    fn bdnz_decrements_ctr_and_branches_until_zero() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        // bdnz +8 (BO=16, BI ignored)
        put_instruction(&mut memory, 0x1000, (16 << 26) | (16 << 21) | (2 << 2));
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.ctr = 2;

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.pc, 0x1008);
        assert_eq!(cpu.state.ctr, 1);

        cpu.state.pc = 0x1000;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.pc, 0x1004);
        assert_eq!(cpu.state.ctr, 0);
    }

    #[test]
    fn bclr_branches_to_aligned_lr_and_sets_link_after_target_capture() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        // bclrl with BO=20 (ignore CR and CTR), XO=16.
        put_instruction(&mut memory, 0x1000, (19 << 26) | (20 << 21) | (16 << 1) | 1);
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.lr = 0x1237;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.pc, 0x1234);
        assert_eq!(cpu.state.lr, 0x1004);
    }

    #[test]
    fn add_and_subf_execute_x_form_arithmetic() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        // add r5,r3,r4
        put_instruction(
            &mut memory,
            0x1000,
            (31 << 26) | (5 << 21) | (3 << 16) | (4 << 11) | (266 << 1),
        );
        // subf r6,r3,r4 => r4-r3
        put_instruction(
            &mut memory,
            0x1004,
            (31 << 26) | (6 << 21) | (3 << 16) | (4 << 11) | (40 << 1),
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 7;
        cpu.state.gpr[4] = 12;

        cpu.step(&mut memory).unwrap();
        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.gpr[5], 19);
        assert_eq!(cpu.state.gpr[6], 5);
    }

    #[test]
    fn addc_and_subfc_update_carry_and_record_result() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // addc r5,r3,r4
        put_instruction(
            &mut memory,
            0x1000,
            (31 << 26) | (5 << 21) | (3 << 16) | (4 << 11) | (10 << 1),
        );
        // subfc r6,r3,r4 => r4-r3
        put_instruction(
            &mut memory,
            0x1004,
            (31 << 26) | (6 << 21) | (3 << 16) | (4 << 11) | (8 << 1),
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = u64::MAX;
        cpu.state.gpr[4] = 1;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[5], 0);
        assert_ne!(cpu.state.xer & (1 << 29), 0);
        cpu.state.gpr[3] = 1;
        cpu.state.gpr[4] = 5;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[6], 4);
        assert_ne!(cpu.state.xer & (1 << 29), 0);
    }

    #[test]
    fn condition_register_logic_updates_individual_bits() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // crand bt=0, ba=0, bb=0
        put_instruction(&mut memory, 0x1000, (19 << 26) | (257 << 1));
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.cr[0] = 0b1000;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.cr[0] & 0b1000, 0b1000);
    }

    #[test]
    fn indexed_loads_and_stores_support_update_forms() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 64);
        let encode_x = |rt: u32, ra: u32, rb: u32, xo: u32| {
            (31 << 26) | (rt << 21) | (ra << 16) | (rb << 11) | (xo << 1)
        };
        put_instruction(&mut memory, 0x1000, encode_x(5, 3, 4, 87)); // lbzx
        put_instruction(&mut memory, 0x1004, encode_x(6, 3, 7, 311)); // lhzux
        put_instruction(&mut memory, 0x1008, encode_x(8, 3, 4, 151)); // stwx
        put_instruction(&mut memory, 0x100C, encode_x(9, 3, 7, 183)); // stwux
        memory.write_u8(GuestAddress(0x1014), 0xAB).unwrap();
        memory.write_u16_be(GuestAddress(0x1018), 0x1234).unwrap();

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1010;
        cpu.state.gpr[4] = 4;
        cpu.state.gpr[7] = 8;
        cpu.state.gpr[8] = 0xCAFE_BABE;
        cpu.state.gpr[9] = 0xDEAD_BEEF;

        for _ in 0..4 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.gpr[5], 0xAB);
        assert_eq!(cpu.state.gpr[6], 0x1234);
        assert_eq!(memory.read_u32_be(GuestAddress(0x101C)), Ok(0xCAFE_BABE));
        assert_eq!(memory.read_u32_be(GuestAddress(0x1020)), Ok(0xDEAD_BEEF));
        assert_eq!(cpu.state.gpr[3], 0x1020);
    }

    #[test]
    fn indexed_update_load_rejects_invalid_base_destination_alias() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // lhzux r3,r3,r4 (RA=RT is invalid for update loads).
        put_instruction(
            &mut memory,
            0x1000,
            (31 << 26) | (3 << 21) | (3 << 16) | (4 << 11) | (311 << 1),
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::UnsupportedInstruction {
                pc: 0x1000,
                opcode: 31
            })
        );
    }

    #[test]
    fn displacement_update_load_and_store_write_back_effective_address() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 64);
        // lwzu r5,4(r3); stwu r6,8(r3)
        put_instruction(&mut memory, 0x1000, (33 << 26) | (5 << 21) | (3 << 16) | 4);
        put_instruction(&mut memory, 0x1004, (37 << 26) | (6 << 21) | (3 << 16) | 8);
        memory
            .write_u32_be(GuestAddress(0x1014), 0x1234_5678)
            .unwrap();
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1010;
        cpu.state.gpr[6] = 0xCAFE_BABE;

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[5], 0x1234_5678);
        assert_eq!(cpu.state.gpr[3], 0x1014);
        cpu.step(&mut memory).unwrap();

        assert_eq!(memory.read_u32_be(GuestAddress(0x101C)), Ok(0xCAFE_BABE));
        assert_eq!(cpu.state.gpr[3], 0x101C);
    }

    #[test]
    fn rotate_shift_and_sign_extension_instructions_execute() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 32);
        // rlwimi r4,r3,8,8,15
        put_instruction(
            &mut memory,
            0x1000,
            (20 << 26) | (3 << 21) | (4 << 16) | (8 << 11) | (8 << 6) | (15 << 1),
        );
        // slw r5,r3,r6
        put_instruction(
            &mut memory,
            0x1004,
            (31 << 26) | (5 << 21) | (3 << 16) | (6 << 11) | (24 << 1),
        );
        // srawi r7,r8,4
        put_instruction(
            &mut memory,
            0x1008,
            (31 << 26) | (7 << 21) | (8 << 16) | (4 << 11) | (824 << 1),
        );
        // extsh r9,r10; cntlzw r11,r12
        put_instruction(
            &mut memory,
            0x100C,
            (31 << 26) | (9 << 21) | (10 << 16) | (922 << 1),
        );
        put_instruction(
            &mut memory,
            0x1010,
            (31 << 26) | (11 << 21) | (12 << 16) | (26 << 1),
        );

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1234_5678;
        cpu.state.gpr[4] = 0xA5A5_A5A5;
        cpu.state.gpr[6] = 4;
        cpu.state.gpr[8] = (-17_i64) as u64;
        cpu.state.gpr[10] = 0x8001;
        cpu.state.gpr[12] = 1;

        for _ in 0..5 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.gpr[4], 0xA556_A5A5);
        assert_eq!(cpu.state.gpr[5], 0x2345_6780);
        assert_eq!(cpu.state.gpr[7], (-2_i64) as u64);
        assert_ne!(cpu.state.xer & (1 << 29), 0);
        assert_eq!(cpu.state.gpr[9], 0xFFFF_FFFF_FFFF_8001);
        assert_eq!(cpu.state.gpr[11], 31);
    }

    #[test]
    fn addic_sets_carry_and_andi_updates_cr0() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // addic r4,r3,1
        put_instruction(&mut memory, 0x1000, (12 << 26) | (4 << 21) | (3 << 16) | 1);
        // andi. r5,r4,0xFFFF
        put_instruction(
            &mut memory,
            0x1004,
            (28 << 26) | (5 << 21) | (4 << 16) | 0xFFFF,
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = u64::MAX;

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[4], 0);
        assert_ne!(cpu.state.xer & (1 << 29), 0);
        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.gpr[5], 0);
        assert_eq!(cpu.state.cr[0] & 0x0E, 0b0010);
    }

    #[test]
    fn mulli_reads_gpr_zero_as_a_register() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mulli r4,r0,2
        put_instruction(&mut memory, 0x1000, (7 << 26) | (4 << 21) | 2);
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[0] = 3;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.gpr[4], 6);
    }

    #[test]
    fn rlwinm_rotates_and_masks_word_results() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // rlwinm r4,r3,8,8,23
        put_instruction(
            &mut memory,
            0x1000,
            (21 << 26) | (3 << 21) | (4 << 16) | (8 << 11) | (8 << 6) | (23 << 1),
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1234_5678;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.gpr[4], 0x0056_7800);
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
                MemoryPermissions::READ_WRITE_EXECUTE,
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
