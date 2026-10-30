//! Minimal, deterministic PowerPC interpreter scaffolding.
//!
//! Only a deliberately small instruction subset is implemented. This is not
//! sufficient to boot an Xbox 360 title; unsupported instructions fail loudly.

use core::cmp::Ordering;

use crate::address_space::AddressSpaceError;
use crate::memory::{GuestAddress, GuestMemoryAccess, MemoryError};

#[derive(Clone, Debug, PartialEq)]
pub struct PpcState {
    /// General-purpose registers. The Xenon PPC context uses 64-bit GPR storage.
    pub gpr: [u64; 32],
    pub fpr: [f64; 32],
    /// VMX128 register file. Each vector is stored in guest byte order.
    pub vr: [[u8; 16]; 128],
    pub pc: u32,
    pub lr: u64,
    pub ctr: u64,
    pub xer: u64,
    pub cr: [u8; 8],
    pub fpscr: u32,
    pub vscr: u32,
}

impl Default for PpcState {
    fn default() -> Self {
        Self {
            gpr: [0; 32],
            fpr: [0.0; 32],
            vr: [[0; 16]; 128],
            pc: 0,
            lr: 0,
            ctr: 0,
            xer: 0,
            cr: [0; 8],
            fpscr: 0,
            vscr: 0,
        }
    }
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

    fn copy_fpscr_to_cr1(&mut self) {
        // PowerPC record-form floating-point instructions copy FPSCR[0:3]
        // (FX, FEX, VX, OX) into CR1; CR1 is not a sign/zero compare.
        self.state.cr[1] = ((self.state.fpscr >> 28) & 0x0F) as u8;
    }

    fn refresh_fpscr_fex(&mut self) {
        const FEX: u32 = 1 << 30;
        const VX: u32 = 1 << 29;
        const OX: u32 = 1 << 28;
        const UX: u32 = 1 << 27;
        const ZX: u32 = 1 << 26;
        const XX: u32 = 1 << 25;
        const VE: u32 = 1 << 7;
        const OE: u32 = 1 << 6;
        const UE: u32 = 1 << 5;
        const ZE: u32 = 1 << 4;
        const XE: u32 = 1 << 3;

        let enabled = (self.state.fpscr & VX != 0 && self.state.fpscr & VE != 0)
            || (self.state.fpscr & OX != 0 && self.state.fpscr & OE != 0)
            || (self.state.fpscr & UX != 0 && self.state.fpscr & UE != 0)
            || (self.state.fpscr & ZX != 0 && self.state.fpscr & ZE != 0)
            || (self.state.fpscr & XX != 0 && self.state.fpscr & XE != 0);
        if enabled {
            self.state.fpscr |= FEX;
        } else {
            self.state.fpscr &= !FEX;
        }
    }

    fn update_fpscr_exceptions(
        &mut self,
        invalid: bool,
        div_zero: bool,
        overflow: bool,
        underflow: bool,
        inexact: bool,
        record: bool,
    ) {
        const FX: u32 = 1 << 31;
        const VX: u32 = 1 << 29;
        const OX: u32 = 1 << 28;
        const UX: u32 = 1 << 27;
        const ZX: u32 = 1 << 26;
        const XX: u32 = 1 << 25;

        let mut raised = 0;
        if invalid {
            raised |= VX;
        }
        if div_zero {
            raised |= ZX;
        }
        if overflow {
            raised |= OX;
        }
        if underflow {
            raised |= UX;
        }
        if inexact {
            raised |= XX;
        }
        if raised != 0 {
            self.state.fpscr |= FX | raised;
        }
        self.refresh_fpscr_fex();
        if record {
            self.copy_fpscr_to_cr1();
        }
    }

    fn is_signaling_nan(value: f64) -> bool {
        let bits = value.to_bits();
        (bits & 0x7FF0_0000_0000_0000) == 0x7FF0_0000_0000_0000
            && (bits & 0x000F_FFFF_FFFF_FFFF) != 0
            && (bits & 0x0008_0000_0000_0000) == 0
    }

    fn update_fpscr_for_result(
        &mut self,
        value: f64,
        operands: &[f64],
        inexact: bool,
        record: bool,
    ) {
        let has_nan_operand = operands.iter().any(|operand| operand.is_nan());
        let invalid = value.is_nan()
            && (!has_nan_operand || operands.iter().any(|operand| Self::is_signaling_nan(*operand)));
        let div_zero = operands.len() >= 2
            && operands[1] == 0.0
            && operands[0] != 0.0
            && operands[0].is_finite();
        let overflow = value.is_infinite()
            && !div_zero
            && operands.iter().all(|operand| operand.is_finite());
        let underflow = value != 0.0 && value.is_subnormal();
        self.update_fpscr_exceptions(invalid, div_zero, overflow, underflow, inexact, record);
    }

    fn fpscr_rounding_mode(&self) -> u32 {
        self.state.fpscr & 0b11
    }

    fn round_fp_to_integer(&self, value: f64) -> f64 {
        match self.fpscr_rounding_mode() {
            0 => value.round_ties_even(),
            1 => value.trunc(),
            2 => value.ceil(),
            _ => value.floor(),
        }
    }

    fn write_fpscr_field(&mut self, field: usize, value: u8) {
        let shift = 28 - (field * 4);
        let mask = 0xF_u32 << shift;
        self.state.fpscr = (self.state.fpscr & !mask) | (((value as u32) & 0xF) << shift);
        self.refresh_fpscr_fex();
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
                    339 | 467 => {
                        // mfspr/mtspr encode the two five-bit SPR halves in
                        // reversed field order. Only architectural SPRs represented
                        // by the current CPU state are accepted.
                        let spr =
                            ((instruction >> 16) & 0x1F) | (((instruction >> 11) & 0x1F) << 5);
                        match (extended_opcode, spr) {
                            (339, 1) => self.state.gpr[rt_rs] = self.state.xer as u32 as u64,
                            (467, 1) => self.state.xer = self.state.gpr[rt_rs] as u32 as u64,
                            (339, 8) => self.state.gpr[rt_rs] = self.state.lr,
                            (467, 8) => self.state.lr = self.state.gpr[rt_rs],
                            (339, 9) => self.state.gpr[rt_rs] = self.state.ctr,
                            (467, 9) => self.state.ctr = self.state.gpr[rt_rs],
                            _ => return Err(PpcError::UnsupportedInstruction { pc, opcode }),
                        }
                    }
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
                    200 | 202 | 232 | 234 | 712 | 714 | 744 | 746 => {
                        // subfze/subfme/addze/addme and their OE variants.
                        let carry_in = (self.state.xer >> 29) & 1;
                        let addend = match xo {
                            200 | 232 => !left,
                            202 | 234 => left,
                            _ => unreachable!(),
                        };
                        let minus_one = matches!(xo, 232 | 234);
                        let second = if minus_one { u64::MAX } else { 0 };
                        let (partial, carry_a) = addend.overflowing_add(second);
                        let (value, carry_b) = partial.overflowing_add(carry_in);
                        let signed_base = addend as i64 as i128;
                        let signed_second = if minus_one { -1_i128 } else { 0_i128 };
                        let signed_result = signed_base + signed_second + carry_in as i128;
                        self.state.gpr[rt_rs] = value;
                        self.set_carry(carry_a || carry_b);
                        if oe {
                            self.update_overflow(
                                signed_result < i64::MIN as i128
                                    || signed_result > i64::MAX as i128,
                            );
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
                    9 | 11 | 73 | 75 => {
                        // Multiply-high operations must use a product wide enough
                        // for the full architectural operand width. In particular,
                        // mulhwu's u32*u32 product does not fit in i64 for all inputs.
                        let value = match xo {
                            9 => {
                                let product = (left as u128) * (right as u128);
                                (product >> 64) as u64
                            }
                            11 => {
                                let product = (left as u32 as u128) * (right as u32 as u128);
                                (product >> 32) as u64
                            }
                            73 => {
                                let product = (left as i64 as i128) * (right as i64 as i128);
                                (product >> 64) as u64
                            }
                            75 => {
                                let a = left as u32 as i32 as i64;
                                let b = right as u32 as i32 as i64;
                                ((a * b) >> 32) as u64
                            }
                            _ => unreachable!(),
                        };
                        self.state.gpr[rt_rs] = value;
                        if record {
                            self.set_cr_field(0, (value as i64).cmp(&0));
                        }
                    }
                    457 | 459 | 489 | 491 | 969 | 971 | 1001 | 1003 => {
                        // divdu/divwu/divd/divw. OE is decoded independently from
                        // the XO field; it must not be mistaken for a 64-bit opcode.
                        let is_64_bit = matches!(xo, 457 | 489);
                        let signed = matches!(xo, 489 | 491);
                        let (value, overflow) = if is_64_bit {
                            if signed {
                                match (left as i64).checked_div(right as i64) {
                                    Some(result) => (result as u64, false),
                                    None => (0, true),
                                }
                            } else {
                                match left.checked_div(right) {
                                    Some(result) => (result, false),
                                    None => (0, true),
                                }
                            }
                        } else {
                            let dividend = left as u32;
                            let divisor = right as u32;
                            if signed {
                                match (dividend as i32).checked_div(divisor as i32) {
                                    Some(result) => (result as u32 as u64, false),
                                    None => (0, true),
                                }
                            } else {
                                match dividend.checked_div(divisor) {
                                    Some(result) => (result as u64, false),
                                    None => (0, true),
                                }
                            }
                        };
                        self.state.gpr[rt_rs] = value;
                        if oe {
                            self.update_overflow(overflow);
                        }
                        if record {
                            self.set_cr_field(0, (value as i64).cmp(&0));
                        }
                    }
                    535 | 567 | 599 | 631 => {
                        let update = matches!(extended_opcode, 567 | 631);
                        if update && (ra == 0 || ra == rt_rs) {
                            return Err(PpcError::UnsupportedInstruction { pc, opcode });
                        }
                        let base = if ra == 0 {
                            0
                        } else {
                            self.state.gpr[ra] as u32
                        };
                        let address = base.wrapping_add(self.state.gpr[rb] as u32);
                        let value = if extended_opcode == 535 || extended_opcode == 567 {
                            f32::from_bits(
                                memory
                                    .read_u32_be(GuestAddress(address))
                                    .map_err(Into::into)?,
                            ) as f64
                        } else {
                            f64::from_bits(
                                memory
                                    .read_u64_be(GuestAddress(address))
                                    .map_err(Into::into)?,
                            )
                        };
                        self.state.fpr[rt_rs] = value;
                        if update {
                            self.state.gpr[ra] = address as u64;
                        }
                    }
                    663 | 695 | 727 | 759 => {
                        let update = matches!(extended_opcode, 695 | 759);
                        if update && ra == 0 {
                            return Err(PpcError::UnsupportedInstruction { pc, opcode });
                        }
                        let base = if ra == 0 {
                            0
                        } else {
                            self.state.gpr[ra] as u32
                        };
                        let address = base.wrapping_add(self.state.gpr[rb] as u32);
                        if extended_opcode == 663 || extended_opcode == 695 {
                            memory
                                .write_u32_be(
                                    GuestAddress(address),
                                    (self.state.fpr[rt_rs] as f32).to_bits(),
                                )
                                .map_err(Into::into)?;
                        } else {
                            memory
                                .write_u64_be(
                                    GuestAddress(address),
                                    self.state.fpr[rt_rs].to_bits(),
                                )
                                .map_err(Into::into)?;
                        }
                        if update {
                            self.state.gpr[ra] = address as u64;
                        }
                    }
                    103 => {
                        let base = if ra == 0 {
                            0
                        } else {
                            self.state.gpr[ra] as u32
                        };
                        let address = base.wrapping_add(self.state.gpr[rb] as u32) & !0xF;
                        let mut vector = [0_u8; 16];
                        for (index, byte) in vector.iter_mut().enumerate() {
                            *byte = memory
                                .read_u8(GuestAddress(address.wrapping_add(index as u32)))
                                .map_err(Into::into)?;
                        }
                        self.state.vr[rt_rs] = vector;
                    }
                    231 => {
                        let base = if ra == 0 {
                            0
                        } else {
                            self.state.gpr[ra] as u32
                        };
                        let address = base.wrapping_add(self.state.gpr[rb] as u32) & !0xF;
                        for (index, byte) in self.state.vr[rt_rs].iter().copied().enumerate() {
                            memory
                                .write_u8(GuestAddress(address.wrapping_add(index as u32)), byte)
                                .map_err(Into::into)?;
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
                        self.state.gpr[rt_rs] = if extended_opcode == 792 || extended_opcode == 824
                        {
                            (value as i32 as i64) as u64
                        } else {
                            value as u64
                        };
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
            // Initial VMX integer/logic subset (standard 5-bit register forms).
            4 => {
                let vd = ((instruction >> 21) & 0x1F) as usize;
                let va = ((instruction >> 16) & 0x1F) as usize;
                let vb = ((instruction >> 11) & 0x1F) as usize;
                let xo = instruction & 0x7FF;
                let a = self.state.vr[va];
                let b = self.state.vr[vb];
                let mut result = [0_u8; 16];
                match xo {
                    0x000 => {
                        for lane in 0..16 {
                            result[lane] = a[lane].wrapping_add(b[lane]);
                        }
                    }
                    0x040 => {
                        for lane in 0..8 {
                            let offset = lane * 2;
                            let av = u16::from_be_bytes([a[offset], a[offset + 1]]);
                            let bv = u16::from_be_bytes([b[offset], b[offset + 1]]);
                            result[offset..offset + 2]
                                .copy_from_slice(&av.wrapping_add(bv).to_be_bytes());
                        }
                    }
                    0x080 => {
                        for lane in 0..4 {
                            let offset = lane * 4;
                            let av = u32::from_be_bytes(a[offset..offset + 4].try_into().unwrap());
                            let bv = u32::from_be_bytes(b[offset..offset + 4].try_into().unwrap());
                            result[offset..offset + 4]
                                .copy_from_slice(&av.wrapping_add(bv).to_be_bytes());
                        }
                    }
                    0x400 => {
                        for lane in 0..16 {
                            result[lane] = a[lane].wrapping_sub(b[lane]);
                        }
                    }
                    0x440 => {
                        for lane in 0..8 {
                            let offset = lane * 2;
                            let av = u16::from_be_bytes([a[offset], a[offset + 1]]);
                            let bv = u16::from_be_bytes([b[offset], b[offset + 1]]);
                            result[offset..offset + 2]
                                .copy_from_slice(&av.wrapping_sub(bv).to_be_bytes());
                        }
                    }
                    0x480 => {
                        for lane in 0..4 {
                            let offset = lane * 4;
                            let av = u32::from_be_bytes(a[offset..offset + 4].try_into().unwrap());
                            let bv = u32::from_be_bytes(b[offset..offset + 4].try_into().unwrap());
                            result[offset..offset + 4]
                                .copy_from_slice(&av.wrapping_sub(bv).to_be_bytes());
                        }
                    }
                    0x404 => {
                        for lane in 0..16 {
                            result[lane] = a[lane] & b[lane];
                        }
                    }
                    0x444 => {
                        for lane in 0..16 {
                            result[lane] = a[lane] & !b[lane];
                        }
                    }
                    0x484 => {
                        for lane in 0..16 {
                            result[lane] = a[lane] | b[lane];
                        }
                    }
                    0x4C4 => {
                        for lane in 0..16 {
                            result[lane] = a[lane] ^ b[lane];
                        }
                    }
                    0x504 => {
                        for lane in 0..16 {
                            result[lane] = !(a[lane] | b[lane]);
                        }
                    }
                    _ => return Err(PpcError::UnsupportedInstruction { pc, opcode }),
                }
                self.state.vr[vd] = result;
                self.state.pc = next_pc;
            }
            // Floating-point displacement loads/stores.
            48..=51 => {
                let update = matches!(opcode, 49 | 51);
                if update && (ra == 0 || ra == rt_rs) {
                    return Err(PpcError::UnsupportedInstruction { pc, opcode });
                }
                let address = (if ra == 0 {
                    0
                } else {
                    self.state.gpr[ra] as u32
                })
                .wrapping_add(immediate as u32);
                let value = match opcode {
                    48 | 49 => f32::from_bits(
                        memory
                            .read_u32_be(GuestAddress(address))
                            .map_err(Into::into)?,
                    ) as f64,
                    50 | 51 => f64::from_bits(
                        memory
                            .read_u64_be(GuestAddress(address))
                            .map_err(Into::into)?,
                    ),
                    _ => unreachable!(),
                };
                self.state.fpr[rt_rs] = value;
                if update {
                    self.state.gpr[ra] = address as u64;
                }
                self.state.pc = next_pc;
            }
            52..=55 => {
                let update = matches!(opcode, 53 | 55);
                if update && ra == 0 {
                    return Err(PpcError::UnsupportedInstruction { pc, opcode });
                }
                let address = (if ra == 0 {
                    0
                } else {
                    self.state.gpr[ra] as u32
                })
                .wrapping_add(immediate as u32);
                let bits = self.state.fpr[rt_rs].to_bits();
                if matches!(opcode, 52 | 53) {
                    memory
                        .write_u32_be(
                            GuestAddress(address),
                            (self.state.fpr[rt_rs] as f32).to_bits(),
                        )
                        .map_err(Into::into)?;
                } else {
                    memory
                        .write_u64_be(GuestAddress(address), bits)
                        .map_err(Into::into)?;
                }
                if update {
                    self.state.gpr[ra] = address as u64;
                }
                self.state.pc = next_pc;
            }
            // lmw/stmw transfer the selected GPR range in ascending register order.
            // Stage lmw values before committing them so a failed memory read cannot
            // leave only part of the architectural register range modified.
            46 => {
                let base = if ra == 0 {
                    0
                } else {
                    self.state.gpr[ra] as u32
                };
                let address = base.wrapping_add(immediate as u32);
                let mut loaded = [0_u64; 32];
                for (register, value) in loaded.iter_mut().enumerate().skip(rt_rs) {
                    let offset = ((register - rt_rs) as u32).wrapping_mul(4);
                    *value = memory
                        .read_u32_be(GuestAddress(address.wrapping_add(offset)))
                        .map_err(Into::into)? as u64;
                }
                self.state.gpr[rt_rs..32].copy_from_slice(&loaded[rt_rs..32]);
                self.state.pc = next_pc;
            }
            47 => {
                let base = if ra == 0 {
                    0
                } else {
                    self.state.gpr[ra] as u32
                };
                let address = base.wrapping_add(immediate as u32);
                for (register, value) in self.state.gpr.iter().enumerate().skip(rt_rs) {
                    let offset = ((register - rt_rs) as u32).wrapping_mul(4);
                    memory
                        .write_u32_be(GuestAddress(address.wrapping_add(offset)), *value as u32)
                        .map_err(Into::into)?;
                }
                self.state.pc = next_pc;
            }
            // Scalar PowerPC floating-point arithmetic and FPSCR control.
            59 | 63 => {
                let frt = rt_rs;
                let fra = ra;
                let frb = ((instruction >> 11) & 0x1F) as usize;
                let frc = ((instruction >> 6) & 0x1F) as usize;
                let xo10 = (instruction >> 1) & 0x3FF;
                let xo5 = xo10 & 0x1F;
                let single = opcode == 59;
                let record = instruction & 1 != 0;
                let a = self.state.fpr[fra];
                let b = self.state.fpr[frb];
                let c = self.state.fpr[frc];

                // FPSCR field moves and bit operations use PowerPC's MSB-first
                // bit numbering. They do not execute an arithmetic operation.
                match xo10 {
                    64 => {
                        // mcrfs CRFD,CRFS
                        let crfd = ((instruction >> 23) & 0x7) as usize;
                        let crfs = ((instruction >> 18) & 0x7) as usize;
                        let shift = 28 - crfs * 4;
                        let field = ((self.state.fpscr >> shift) & 0xF) as u8;
                        self.state.cr[crfd] = field;
                        self.state.fpscr &= !(0xF << shift);
                        self.refresh_fpscr_fex();
                        self.state.pc = next_pc;
                        return Ok(());
                    }
                    38 | 70 => {
                        // mtfsb1 / mtfsb0
                        let bit = ((instruction >> 21) & 0x1F) as u32;
                        let mask = 1_u32 << (31 - bit);
                        if xo10 == 38 {
                            self.state.fpscr |= mask;
                        } else {
                            self.state.fpscr &= !mask;
                        }
                        self.refresh_fpscr_fex();
                        if record {
                            self.copy_fpscr_to_cr1();
                        }
                        self.state.pc = next_pc;
                        return Ok(());
                    }
                    134 => {
                        // mtfsfi BF,IMM
                        let field = ((instruction >> 23) & 0x7) as usize;
                        let immediate = ((instruction >> 12) & 0xF) as u8;
                        self.write_fpscr_field(field, immediate);
                        if record {
                            self.copy_fpscr_to_cr1();
                        }
                        self.state.pc = next_pc;
                        return Ok(());
                    }
                    711 => {
                        // mtfsf FM,FRB (and the Xenia-supported L form)
                        let field_mask = ((instruction >> 17) & 0xFF) as u8;
                        let value = self.state.fpr[frb] as u32;
                        if instruction & (1 << 16) != 0 {
                            self.state.fpscr = value;
                        } else {
                            let mut mask = 0_u32;
                            for field in 0..8 {
                                if field_mask & (1 << (7 - field)) != 0 {
                                    mask |= 0xF << (28 - field * 4);
                                }
                            }
                            self.state.fpscr = (self.state.fpscr & !mask) | (value & mask);
                        }
                        self.refresh_fpscr_fex();
                        if record {
                            self.copy_fpscr_to_cr1();
                        }
                        self.state.pc = next_pc;
                        return Ok(());
                    }
                    _ => {}
                }

                if xo10 == 583 {
                    // mffs
                    self.state.fpr[frt] = self.state.fpscr as f64;
                    if record {
                        self.copy_fpscr_to_cr1();
                    }
                    self.state.pc = next_pc;
                    return Ok(());
                }

                if xo10 == 0 || xo10 == 32 {
                    // fcmpu / fcmpo
                    let field = ((instruction >> 23) & 0x7) as usize;
                    let unordered = a.is_nan() || b.is_nan();
                    self.state.cr[field] = if unordered {
                        0b0001
                    } else if a < b {
                        0b1000
                    } else if a > b {
                        0b0100
                    } else {
                        0b0010
                    };
                    let signaling_nan = [a, b].iter().any(|value| {
                        let bits = value.to_bits();
                        (bits & 0x7FF0_0000_0000_0000) == 0x7FF0_0000_0000_0000
                            && (bits & 0x000F_FFFF_FFFF_FFFF) != 0
                            && (bits & 0x0008_0000_0000_0000) == 0
                    });
                    let invalid = unordered && (xo10 == 32 || signaling_nan);
                    self.update_fpscr_exceptions(invalid, false, false, false, false, false);
                    self.state.pc = next_pc;
                    return Ok(());
                }

                let mut arithmetic = false;
                let mut update_status = false;
                let value = match xo10 {
                    264 => b.abs(),          // fabs
                    72 => b,                 // fmr
                    136 => -b.abs(),         // fnabs
                    40 => -b,                // fneg
                    12 => {
                        update_status = true;
                        (b as f32) as f64
                    } // frsp
                    14 | 15 | 814 | 815 => {
                        // fctiw/z and fctid/z
                        let rounded = if xo10 == 15 || xo10 == 815 {
                            b.trunc()
                        } else {
                            self.round_fp_to_integer(b)
                        };
                        let (min, max_exclusive) = if xo10 == 14 || xo10 == 15 {
                            (i32::MIN as f64, (i32::MAX as f64) + 1.0)
                        } else {
                            (i64::MIN as f64, 9_223_372_036_854_775_808.0)
                        };
                        update_status = true;
                        if !rounded.is_finite()
                            || rounded < min
                            || rounded >= max_exclusive
                        {
                            self.update_fpscr_exceptions(true, false, false, false, false, record);
                            if xo10 == 14 || xo10 == 15 {
                                i32::MIN as f64
                            } else {
                                i64::MIN as f64
                            }
                        } else if xo10 == 14 || xo10 == 15 {
                            (rounded as i32) as f64
                        } else {
                            (rounded as i64) as f64
                        }
                    }
                    846 => {
                        update_status = true;
                        (b as i64) as f64
                    } // fcfid
                    _ => {
                        arithmetic = true;
                        update_status = true;
                        match xo5 {
                            21 => a + b, // fadd
                            20 => a - b, // fsub
                            18 => a / b, // fdiv
                            25 => a * c, // fmul
                            23 => {
                                if a >= 0.0 {
                                    c
                                } else {
                                    b
                                }
                            } // fsel
                            22 => b.sqrt(), // fsqrt
                            24 => (1.0_f32 / (b as f32)) as f64, // fres (single estimate domain)
                            26 => 1.0 / b.sqrt(), // frsqrte
                            29 => a.mul_add(c, b), // fmadd
                            28 => a.mul_add(c, -b), // fmsub
                            31 => -a.mul_add(c, b), // fnmadd
                            30 => -a.mul_add(c, -b), // fnmsub
                            _ => return Err(PpcError::UnsupportedInstruction { pc, opcode }),
                        }
                    }
                };

                if arithmetic && single {
                    // Single-precision arithmetic rounds the stored result,
                    // not the source operands, to IEEE-754 binary32.
                    let rounded = (value as f32) as f64;
                    self.state.fpr[frt] = rounded;
                    if update_status {
                        let operands: &[f64] = match xo10 {
                            12 | 14 | 15 | 814 | 815 | 846 => &[b],
                            _ => match xo5 {
                                18 => &[a, b],
                                22 | 26 => &[b],
                                24 => &[1.0, b],
                                25 => &[a, c],
                                29..=31 => &[a, b, c],
                                _ => &[a, b],
                            },
                        };
                        self.update_fpscr_for_result(rounded, operands, rounded != value, record);
                    }
                } else {
                    self.state.fpr[frt] = value;
                    if update_status {
                        let operands: &[f64] = match xo5 {
                            18 => &[a, b],
                            22 | 26 => &[b],
                            24 => &[1.0, b],
                            25 => &[a, c],
                            29..=31 => &[a, b, c],
                            _ => &[a, b],
                        };
                        let inexact = matches!(xo10, 12 | 14 | 15 | 814 | 815) && value != b;
                        self.update_fpscr_for_result(value, operands, inexact, record);
                    } else if record {
                        self.copy_fpscr_to_cr1();
                    }
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
    fn multiply_high_operations_use_full_width_intermediate_products() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        let encode = |rt: u32, ra: u32, rb: u32, xo: u32| {
            (31 << 26) | (rt << 21) | (ra << 16) | (rb << 11) | (xo << 1)
        };
        put_instruction(&mut memory, 0x1000, encode(5, 3, 4, 11)); // mulhwu
        put_instruction(&mut memory, 0x1004, encode(6, 3, 4, 75)); // mulhw
        put_instruction(&mut memory, 0x1008, encode(7, 3, 4, 9)); // mulhdu
        put_instruction(&mut memory, 0x100C, encode(8, 3, 4, 73)); // mulhd

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = u32::MAX as u64;
        cpu.state.gpr[4] = u32::MAX as u64;
        for _ in 0..2 {
            cpu.step(&mut memory).unwrap();
        }
        assert_eq!(cpu.state.gpr[5], 0xFFFF_FFFE);
        assert_eq!(cpu.state.gpr[6], 0);

        // mulhw writes the signed high-word result sign-extended to 64 bits.
        cpu.state.pc = 0x1004;
        cpu.state.gpr[3] = (-2_i64) as u64;
        cpu.state.gpr[4] = 1;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[6], u64::MAX);

        cpu.state.pc = 0x1008;
        cpu.state.gpr[3] = u64::MAX;
        cpu.state.gpr[4] = u64::MAX;
        for _ in 0..2 {
            cpu.step(&mut memory).unwrap();
        }
        assert_eq!(cpu.state.gpr[7], u64::MAX - 1);
        assert_eq!(cpu.state.gpr[8], 0);
    }

    #[test]
    fn divide_instructions_distinguish_word_doubleword_and_overflow_forms() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 28);
        // divdu r5,r3,r4; divd r6,r3,r4; divwu r7,r3,r4; divw r8,r3,r4
        let encode = |rt: u32, xo: u32| (31 << 26) | (rt << 21) | (3 << 16) | (4 << 11) | (xo << 1);
        put_instruction(&mut memory, 0x1000, encode(5, 457));
        put_instruction(&mut memory, 0x1004, encode(6, 489));
        put_instruction(&mut memory, 0x1008, encode(7, 459));
        put_instruction(&mut memory, 0x100C, encode(8, 491));

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1_0000_0000;
        cpu.state.gpr[4] = 2;
        for _ in 0..4 {
            cpu.step(&mut memory).unwrap();
        }
        assert_eq!(cpu.state.gpr[5], 0x8000_0000);
        assert_eq!(cpu.state.gpr[6], 0x8000_0000);
        assert_eq!(cpu.state.gpr[7], 0);
        assert_eq!(cpu.state.gpr[8], 0);

        // divd overflow: signed minimum divided by -1 with OE set.
        put_instruction(&mut memory, 0x1010, encode(9, 489) | (1 << 10));
        put_instruction(&mut memory, 0x1014, encode(10, 459) | (1 << 10));
        put_instruction(&mut memory, 0x1018, encode(11, 491) | (1 << 10));
        cpu.state.pc = 0x1010;
        cpu.state.gpr[3] = i64::MIN as u64;
        cpu.state.gpr[4] = u64::MAX;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[9], 0);
        assert_ne!(cpu.state.xer & (1 << 30), 0);
        assert_ne!(cpu.state.xer & (1 << 31), 0);

        // OE variants must decode separately; division by zero sets OV/SO.
        cpu.state.pc = 0x1014;
        cpu.state.gpr[3] = 123;
        cpu.state.gpr[4] = 0;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[10], 0);
        assert_ne!(cpu.state.xer & (1 << 30), 0);

        // Signed 32-bit minimum divided by -1 is also an overflow case.
        cpu.state.pc = 0x1018;
        cpu.state.gpr[3] = i32::MIN as i64 as u64;
        cpu.state.gpr[4] = u64::MAX;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[11], 0);
        assert_ne!(cpu.state.xer & (1 << 30), 0);
    }

    #[test]
    fn addme_addze_subfme_and_subfze_use_xer_carry() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        let encode = |rt: u32, ra: u32, xo: u32| (31 << 26) | (rt << 21) | (ra << 16) | (xo << 1);
        put_instruction(&mut memory, 0x1000, encode(5, 3, 234)); // addme r5,r3
        put_instruction(&mut memory, 0x1004, encode(6, 3, 202)); // addze r6,r3
        put_instruction(&mut memory, 0x1008, encode(7, 3, 232)); // subfme r7,r3
        put_instruction(&mut memory, 0x100C, encode(8, 3, 200)); // subfze r8,r3

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 5;
        cpu.state.xer = 1 << 29; // CA=1

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[5], 5); // 5 + 1 - 1

        cpu.state.xer |= 1 << 29;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[6], 6); // 5 + 1

        cpu.state.xer |= 1 << 29;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[7], !5_u64); // ~5 + 1 - 1

        cpu.state.xer |= 1 << 29;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[8], !5_u64 + 1); // ~5 + 1
    }

    #[test]
    fn move_to_and_from_supported_special_purpose_registers() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 12);
        // mfspr r5,LR; mtspr LR,r6; mfspr r7,XER
        let encode_spr = |rt_rs: u32, spr: u32, xo: u32| {
            let low = spr & 0x1F;
            let high = (spr >> 5) & 0x1F;
            (31 << 26) | (rt_rs << 21) | (low << 16) | (high << 11) | (xo << 1)
        };
        put_instruction(&mut memory, 0x1000, encode_spr(5, 8, 339));
        put_instruction(&mut memory, 0x1004, encode_spr(6, 8, 467));
        put_instruction(&mut memory, 0x1008, encode_spr(7, 1, 339));

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.lr = 0x1234_5678_9ABC_DEF0;
        cpu.state.gpr[6] = 0xFEDC_BA98_7654_3210;
        cpu.state.xer = 0xE000_0000;

        for _ in 0..3 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.gpr[5], 0x1234_5678_9ABC_DEF0);
        assert_eq!(cpu.state.lr, 0xFEDC_BA98_7654_3210);
        assert_eq!(cpu.state.gpr[7], 0xE000_0000);
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
    fn lmw_and_stmw_transfer_register_ranges_in_big_endian_order() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 0x100);
        // lmw r28,0(r3); stmw r30,16(r3)
        put_instruction(&mut memory, 0x1000, (46 << 26) | (28 << 21) | (3 << 16));
        put_instruction(
            &mut memory,
            0x1004,
            (47 << 26) | (30 << 21) | (3 << 16) | 16,
        );
        for register in 28..32 {
            memory
                .write_u32_be(
                    GuestAddress(0x1020 + ((register - 28) * 4)),
                    0xA000_0000 | register,
                )
                .unwrap();
        }
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1020;

        cpu.step(&mut memory).unwrap();
        for (register, value) in cpu.state.gpr.iter().enumerate().skip(28) {
            assert_eq!(*value, 0xA000_0000 | register as u64);
        }
        cpu.state.gpr[30] = 0x1122_3344;
        cpu.state.gpr[31] = 0x5566_7788;
        cpu.step(&mut memory).unwrap();

        assert_eq!(memory.read_u32_be(GuestAddress(0x1030)), Ok(0x1122_3344));
        assert_eq!(memory.read_u32_be(GuestAddress(0x1034)), Ok(0x5566_7788));
    }

    #[test]
    fn lmw_does_not_partially_update_registers_when_a_later_read_faults() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        put_instruction(&mut memory, 0x1000, (46 << 26) | (30 << 21) | (3 << 16));
        memory
            .write_u32_be(GuestAddress(0x1004), 0x1111_1111)
            .unwrap();
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1004;
        cpu.state.gpr[30] = 0xAAAA_AAAA;
        cpu.state.gpr[31] = 0xBBBB_BBBB;

        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::Memory(MemoryError::OutOfBounds))
        );
        assert_eq!(cpu.state.gpr[30], 0xAAAA_AAAA);
        assert_eq!(cpu.state.gpr[31], 0xBBBB_BBBB);
        assert_eq!(cpu.state.pc, 0x1000);
    }

    #[test]
    fn scalar_floating_point_arithmetic_and_compare_execute() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        // fadd f1,f2,f3; fadds f4,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (59 << 26) | (4 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        // fmul f5,f2,f4; fcmpo cr2,f1,f3
        put_instruction(
            &mut memory,
            0x1008,
            (63 << 26) | (5 << 21) | (2 << 16) | (4 << 6) | (25 << 1),
        );
        put_instruction(
            &mut memory,
            0x100C,
            (63 << 26) | (2 << 23) | (1 << 16) | (3 << 11) | (32 << 1),
        );

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 1.5;
        cpu.state.fpr[3] = 2.25;

        for _ in 0..4 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.fpr[1], 3.75);
        assert_eq!(cpu.state.fpr[4], (3.75_f32) as f64);
        assert_eq!(cpu.state.fpr[5], 5.625);
        assert_eq!(cpu.state.cr[2], 0b0100);
    }

    #[test]
    fn floating_point_record_form_copies_fpscr_summary_to_cr1() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadd. f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1) | 1,
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 1.0;
        cpu.state.fpr[3] = 2.0;
        cpu.state.fpscr = 0xB000_0000;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], 3.0);
        assert_eq!(cpu.state.cr[1], 0x0B);
    }

    #[test]
    fn fpscr_field_instructions_read_write_and_clear_fields() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        let encode_mtfsfi = |bf: u32, imm: u32| (63 << 26) | (bf << 23) | (imm << 12) | (134 << 1);
        let encode_mcrfs =
            |crfd: u32, crfs: u32| (63 << 26) | (crfd << 23) | (crfs << 18) | (64 << 1);
        put_instruction(&mut memory, 0x1000, encode_mtfsfi(0, 0xA));
        put_instruction(&mut memory, 0x1004, encode_mtfsfi(7, 0x5));
        put_instruction(&mut memory, 0x1008, encode_mcrfs(3, 0));
        put_instruction(&mut memory, 0x100C, encode_mcrfs(4, 7));

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        for _ in 0..4 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.cr[3], 0xA);
        assert_eq!(cpu.state.cr[4], 0x5);
        assert_eq!(cpu.state.fpscr & 0xF000_0000, 0);
        assert_eq!(cpu.state.fpscr & 0xF, 0);
    }

    #[test]
    fn fpscr_bit_and_masked_field_moves_preserve_unselected_bits() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 12);
        let encode_mtfsb = |bit: u32, xo: u32| (63 << 26) | (bit << 21) | (xo << 1);
        let encode_mtfsf = |fm: u32, rb: u32| (63 << 26) | (fm << 17) | (rb << 11) | (711 << 1);
        put_instruction(&mut memory, 0x1000, encode_mtfsb(0, 38)); // set FPSCR[0]
        put_instruction(&mut memory, 0x1004, encode_mtfsb(1, 70)); // clear FPSCR[1]
        put_instruction(&mut memory, 0x1008, encode_mtfsf(0x01, 2)); // replace FPSCR field 7
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0x4000_0000;
        cpu.state.fpr[2] = 0x5_u32 as f64;

        for _ in 0..3 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.fpscr & 0xF000_000F, 0x8000_0005);
    }

    #[test]
    fn floating_point_divide_updates_sticky_and_enabled_fpscr_exceptions() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fdiv. f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1) | 1,
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 1.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 1 << 4; // ZE enable

        cpu.step(&mut memory).unwrap();

        assert!(cpu.state.fpr[1].is_infinite());
        assert_ne!(cpu.state.fpscr & (1 << 31), 0); // FX
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FEX
        assert_ne!(cpu.state.fpscr & (1 << 26), 0); // ZX
        assert_eq!(cpu.state.cr[1], 0b1100);
    }

    #[test]
    fn floating_point_compare_distinguishes_ordered_and_unordered_nan() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fcmpu cr2,f1,f2; fcmpo cr3,f1,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (2 << 23) | (1 << 16) | (2 << 11),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (3 << 23) | (1 << 16) | (2 << 11) | (32 << 1),
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = f64::NAN;
        cpu.state.fpr[2] = 1.0;

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.cr[2], 0b0001);
        assert_eq!(cpu.state.fpscr & (1 << 29), 0);
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.cr[3], 0b0001);
        assert_ne!(cpu.state.fpscr & (1 << 29), 0);
    }

    #[test]
    fn floating_point_integer_conversion_obeys_fpscr_rounding_and_truncation() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fctiw f1,f2; fctiwz f3,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (14 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (3 << 21) | (2 << 11) | (15 << 1),
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 2.75;
        cpu.state.fpscr = 0; // nearest, ties to even

        cpu.step(&mut memory).unwrap();
        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], 3.0);
        assert_eq!(cpu.state.fpr[3], 2.0);
    }

    #[test]
    fn vmx_byte_halfword_and_logic_operations_execute() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 12);
        let encode_vx = |vd: u32, va: u32, vb: u32, xo: u32| {
            (4 << 26) | (vd << 21) | (va << 16) | (vb << 11) | xo
        };
        put_instruction(&mut memory, 0x1000, encode_vx(3, 1, 2, 0x000)); // vaddubm
        put_instruction(&mut memory, 0x1004, encode_vx(4, 1, 2, 0x040)); // vadduhm
        put_instruction(&mut memory, 0x1008, encode_vx(5, 1, 2, 0x404)); // vand

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        for lane in 0..16 {
            cpu.state.vr[1][lane] = lane as u8;
            cpu.state.vr[2][lane] = 1;
        }

        for _ in 0..3 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.vr[3], std::array::from_fn(|i| (i + 1) as u8));
        assert_eq!(
            cpu.state.vr[4],
            [
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
                0x0F, 0x10
            ]
        );
        assert_eq!(
            cpu.state.vr[5],
            [0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1]
        );
    }

    #[test]
    fn vmx_aligned_load_store_round_trip_vector_bytes() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 64);
        // lvx v1,r3,r4; stvx v1,r5,r6
        put_instruction(
            &mut memory,
            0x1000,
            (31 << 26) | (1 << 21) | (3 << 16) | (4 << 11) | (103 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (31 << 26) | (1 << 21) | (5 << 16) | (6 << 11) | (231 << 1),
        );
        for index in 0..16 {
            memory
                .write_u8(GuestAddress(0x1010 + index), (0xA0 + index) as u8)
                .unwrap();
        }

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1013;
        cpu.state.gpr[4] = 0;
        cpu.state.gpr[5] = 0x1023;
        cpu.state.gpr[6] = 0;

        cpu.step(&mut memory).unwrap();
        cpu.step(&mut memory).unwrap();

        assert_eq!(
            cpu.state.vr[1],
            std::array::from_fn(|index| (0xA0 + index) as u8)
        );
        for index in 0..16 {
            assert_eq!(
                memory.read_u8(GuestAddress(0x1020 + index)),
                Ok((0xA0 + index) as u8)
            );
        }
    }

    #[test]
    fn floating_point_single_and_double_memory_operations_round_trip() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 64);
        // lfs f1,0(r3); stfd f2,8(r3); lfd f3,8(r3); stfs f3,16(r3)
        put_instruction(&mut memory, 0x1000, (48 << 26) | (1 << 21) | (3 << 16));
        put_instruction(&mut memory, 0x1004, (54 << 26) | (2 << 21) | (3 << 16) | 8);
        put_instruction(&mut memory, 0x1008, (50 << 26) | (3 << 21) | (3 << 16) | 8);
        put_instruction(&mut memory, 0x100C, (52 << 26) | (3 << 21) | (3 << 16) | 16);
        memory
            .write_u32_be(GuestAddress(0x1010), 1.5_f32.to_bits())
            .unwrap();

        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1010;
        cpu.state.fpr[2] = 2.25;

        for _ in 0..4 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.fpr[1], 1.5);
        assert_eq!(cpu.state.fpr[3], 2.25);
        assert_eq!(
            memory.read_u32_be(GuestAddress(0x1020)),
            Ok(2.25_f32.to_bits())
        );
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
