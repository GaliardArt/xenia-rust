//! Minimal, deterministic PowerPC interpreter scaffolding.
//!
//! Only a deliberately small instruction subset is implemented. This is not
//! sufficient to boot an Xbox 360 title; unsupported instructions fail loudly.

use core::cmp::Ordering;

use crate::address_space::AddressSpaceError;
use crate::memory::{GuestAddress, GuestMemoryAccess, MemoryError};

/// Decoded register operands for the Xenon VMX128 three-register format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vmx128RegisterFields {
    pub vd: usize,
    pub va: usize,
    pub vb: usize,
}

/// Extracts VMX128 register indices using the Xenon-specific split high bits.
///
/// VD and VB have 2 extension bits, while VA has a distinct 6th and 7th bit.
pub fn decode_vmx128_register_fields(instruction: u32) -> Vmx128RegisterFields {
    let vd_low = ((instruction >> 21) & 0x1F) as usize;
    let va_low = ((instruction >> 16) & 0x1F) as usize;
    let vb_low = ((instruction >> 11) & 0x1F) as usize;
    let vd_high = ((instruction >> 2) & 0x3) as usize;
    let va_high = ((instruction >> 5) & 0x1) as usize;
    let va_top = ((instruction >> 10) & 0x1) as usize;
    let vb_high = (instruction & 0x3) as usize;

    Vmx128RegisterFields {
        vd: vd_low | (vd_high << 5),
        va: va_low | (va_high << 5) | (va_top << 6),
        vb: vb_low | (vb_high << 5),
    }
}

/// PowerPC MSR floating-point exception mode bits in Xenon's 64-bit MSR.
pub const PPC_MSR_FE0: u64 = 1 << 11;
pub const PPC_MSR_FE1: u64 = 1 << 8;
/// MSR[FP], which enables execution of scalar floating-point instructions.
pub const PPC_MSR_FP: u64 = 1 << 50;
/// Effective address of the Xenon Program Exception vector.
pub const PPC_PROGRAM_EXCEPTION_VECTOR: u32 = 0x700;
/// Effective address of the Xenon Floating-Point Unavailable vector.
pub const PPC_FP_UNAVAILABLE_VECTOR: u32 = 0x800;
/// SRR1 bit 43: the program exception was a floating-point enabled exception.
pub const PPC_SRR1_FP_ENABLED_EXCEPTION: u64 = 1 << 20;
/// SRR1 bit 47: SRR0 identifies a subsequent instruction, not the faulting one.
pub const PPC_SRR1_IMPRECISE: u64 = 1 << 16;

#[derive(Clone, Debug, PartialEq)]
pub struct PpcState {
    /// General-purpose registers. The Xenon PPC context uses 64-bit GPR storage.
    pub gpr: [u64; 32],
    pub fpr: [f64; 32],
    /// VMX128 register file. Each vector is stored in guest byte order.
    pub vr: [[u8; 16]; 128],
    pub pc: u32,
    /// Xenon's 64-bit Machine State Register (FE0/FE1 occupy bits 11 and 8).
    pub msr: u64,
    /// Machine Status Save/Restore Register 0.
    pub srr0: u64,
    /// Machine Status Save/Restore Register 1.
    pub srr1: u64,
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
            msr: 0,
            srr0: 0,
            srr1: 0,
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
    UnsupportedInstruction {
        pc: u32,
        opcode: u32,
    },
    UnsupportedBranchOption {
        pc: u32,
        bo: u8,
    },
    /// The caller must dispatch or explicitly consume this exception before stepping again.
    PendingException(PpcException),
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
#[derive(Clone, Copy, Debug, Default)]
struct FpExceptionFlags {
    invalid: bool,
    div_zero: bool,
    overflow: bool,
    underflow: bool,
    inexact: bool,
    raise_inexact: bool,
    fraction_rounded: bool,
    /// Whether this instruction updates FPSCR[FR:FI]. Comparisons and
    /// non-rounding estimate operations preserve the previous status.
    update_rounding: bool,
    invalid_cause: u32,
}

/// Interpretation of MSR[FE0:FE1] for floating-point enabled exceptions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PpcFpExceptionMode {
    Disabled,
    ImpreciseNonRecoverable,
    ImpreciseRecoverable,
    Precise,
}

/// A guest-visible floating-point exception awaiting CPU-layer handling.
///
/// `instruction_pc` identifies the instruction that first raised the event;
/// in imprecise modes it is not necessarily the architecturally saved SRR0.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PpcException {
    /// An FP instruction was attempted while MSR[FP] was clear.
    FloatingPointUnavailable { instruction_pc: u32 },
    FloatingPoint {
        instruction_pc: u32,
        causes: u32,
        mode: PpcFpExceptionMode,
    },
    /// Enabling FE0/FE1 while FPSCR.FEX is already set raises a
    /// program exception associated with the next instruction address.
    FloatingPointEnableTransition {
        next_pc: u32,
        causes: u32,
        mode: PpcFpExceptionMode,
    },
}

#[derive(Clone, Debug, Default)]
pub struct PpcInterpreter {
    pub state: PpcState,
    pending_exception: Option<PpcException>,
}

impl PpcInterpreter {
    /// Decodes the four architecturally distinct MSR[FE0:FE1] modes.
    pub fn fp_exception_mode(&self) -> PpcFpExceptionMode {
        match (
            self.state.msr & PPC_MSR_FE0 != 0,
            self.state.msr & PPC_MSR_FE1 != 0,
        ) {
            (false, false) => PpcFpExceptionMode::Disabled,
            (false, true) => PpcFpExceptionMode::ImpreciseNonRecoverable,
            (true, false) => PpcFpExceptionMode::ImpreciseRecoverable,
            (true, true) => PpcFpExceptionMode::Precise,
        }
    }

    /// Removes the pending guest exception notification for CPU-layer handling.
    /// Ordinary floating-point events in imprecise modes are deferred until a
    /// synchronization point; FE-enable transitions are delivered immediately.
    pub fn take_pending_exception(&mut self) -> Option<PpcException> {
        self.pending_exception.take()
    }

    /// Inspects the unconsumed guest exception event.
    /// Ordinary imprecise events are held until synchronization; precise
    /// events and FE-enable transition events stop execution immediately.
    pub fn pending_exception(&self) -> Option<PpcException> {
        self.pending_exception
    }

    /// Enters the Xenon Program Exception handler for the pending FP event.
    ///
    /// The saved MSR fields follow the Xenon PPU exception-entry mask. SRR1
    /// uses PowerPC's MSB-first bit numbering: bit 43 identifies an FP-enabled
    /// Program exception and bit 47 marks a subsequent saved instruction.
    /// For imprecise arithmetic events we retain the first faulting PC tracked
    /// by the interpreter; an FE-enable transition instead saves its next PC.
    pub fn enter_pending_exception(&mut self) -> Option<PpcException> {
        const SRR1_MSR_MASK: u64 = 0xFFFF_FFFF_87C0_FFFF;
        const PROGRAM_ENTRY_MSR_MASK: u64 = 0xFFFF_FFFF_FFFF_10C8;
        const PROGRAM_ENTRY_MSR_BITS: u64 = 0x9000_0000_0000_0000;

        let exception = self.pending_exception.take()?;
        let (saved_pc, subsequent_instruction, fp_enabled_exception, vector) = match exception {
            PpcException::FloatingPointUnavailable { instruction_pc } => (
                instruction_pc as u64,
                false,
                false,
                PPC_FP_UNAVAILABLE_VECTOR,
            ),
            PpcException::FloatingPoint { instruction_pc, .. } => (
                instruction_pc as u64,
                false,
                true,
                PPC_PROGRAM_EXCEPTION_VECTOR,
            ),
            PpcException::FloatingPointEnableTransition { next_pc, .. } => {
                (next_pc as u64, true, true, PPC_PROGRAM_EXCEPTION_VECTOR)
            }
        };

        self.state.srr0 = saved_pc;
        let exception_status = if subsequent_instruction {
            PPC_SRR1_IMPRECISE
        } else {
            0
        };
        let fp_enabled_status = if fp_enabled_exception {
            PPC_SRR1_FP_ENABLED_EXCEPTION
        } else {
            0
        };
        self.state.srr1 = (self.state.msr & SRR1_MSR_MASK) | fp_enabled_status | exception_status;
        self.state.msr = (self.state.msr & PROGRAM_ENTRY_MSR_MASK) | PROGRAM_ENTRY_MSR_BITS;
        self.state.pc = vector;

        Some(exception)
    }

    /// Executes one instruction and automatically enters the Program Exception
    /// vector when an FP exception reaches its architectural delivery boundary.
    /// Returns the delivered exception so the host can log or inspect it.
    pub fn step_and_dispatch<M>(&mut self, memory: &mut M) -> Result<Option<PpcException>, PpcError>
    where
        M: GuestMemoryAccess,
        M::Error: Into<PpcError>,
    {
        match self.step(memory) {
            Ok(()) => Ok(None),
            Err(PpcError::PendingException(_)) => Ok(self.enter_pending_exception()),
            Err(error) => Err(error),
        }
    }

    /// Applies a full MSR write and detects the FE=00 -> FE!=00 transition
    /// that delivers an already-sticky FPSCR.FEX condition at the next PC.
    fn apply_msr_write(&mut self, value: u64, next_pc: u32) {
        let previous_mode = self.fp_exception_mode();
        self.state.msr = value;
        let mode = self.fp_exception_mode();
        self.state.pc = next_pc;
        if previous_mode == PpcFpExceptionMode::Disabled
            && mode != PpcFpExceptionMode::Disabled
            && self.state.fpscr & (1 << 30) != 0
            && self.pending_exception.is_none()
        {
            let causes = self.enabled_fpscr_exception_causes();
            if causes != 0 {
                self.pending_exception = Some(PpcException::FloatingPointEnableTransition {
                    next_pc,
                    causes,
                    mode,
                });
            }
        }
    }

    /// Returns an ordinary imprecise FP event that must be delivered before
    /// an FPSCR instruction begins. FPSCR instructions serialize prior
    /// floating-point work, including its enabled-exception handler.
    fn pending_imprecise_fp_exception(&self) -> Option<PpcException> {
        match self.pending_exception {
            Some(
                exception @ PpcException::FloatingPoint {
                    mode:
                        PpcFpExceptionMode::ImpreciseNonRecoverable
                        | PpcFpExceptionMode::ImpreciseRecoverable,
                    ..
                },
            ) => Some(exception),
            _ => None,
        }
    }

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

        const VX_CAUSES: u32 = 0x01F8_0700;

        if self.state.fpscr & VX_CAUSES != 0 {
            self.state.fpscr |= VX;
        } else {
            self.state.fpscr &= !VX;
        }

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

    /// Checks sticky FPSCR state after an explicit FPSCR write. Unlike an
    /// arithmetic instruction, mtfs* can enable a previously recorded cause
    /// or set a cause and its enable bit in the same instruction.
    fn queue_fpscr_write_exception(&mut self, instruction_pc: u32, previous_fpscr: u32) {
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
        const VX_CAUSES: u32 = 0x01F8_0700;

        if self.state.fpscr & FEX == 0 || self.pending_exception.is_some() {
            return;
        }

        let newly_set = self.state.fpscr & !previous_fpscr;
        let mut causes = 0;

        let invalid_causes = self.state.fpscr & VX_CAUSES;
        if self.state.fpscr & VE != 0
            && (previous_fpscr & VE == 0 || newly_set & VX_CAUSES != 0)
            && invalid_causes != 0
        {
            causes |= VX | invalid_causes;
        }
        for (status, enable, cause) in [(OX, OE, OX), (UX, UE, UX), (ZX, ZE, ZX), (XX, XE, XX)] {
            if self.state.fpscr & status != 0
                && self.state.fpscr & enable != 0
                && (newly_set & status != 0 || previous_fpscr & enable == 0)
            {
                causes |= cause;
            }
        }

        let mode = self.fp_exception_mode();
        if causes != 0 && mode != PpcFpExceptionMode::Disabled {
            self.pending_exception = Some(PpcException::FloatingPoint {
                instruction_pc,
                causes,
                mode,
            });
        }
    }

    fn enabled_fpscr_exception_causes(&self) -> u32 {
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
        const VX_CAUSES: u32 = 0x01F8_0700;

        let fpscr = self.state.fpscr;
        let mut causes = 0;
        if fpscr & VX != 0 && fpscr & VE != 0 {
            causes |= VX | (fpscr & VX_CAUSES);
        }
        for (status, enable, cause) in [(OX, OE, OX), (UX, UE, UX), (ZX, ZE, ZX), (XX, XE, XX)] {
            if fpscr & status != 0 && fpscr & enable != 0 {
                causes |= cause;
            }
        }
        causes
    }

    fn update_fpscr_exceptions(&mut self, exceptions: FpExceptionFlags, record: bool) {
        const FX: u32 = 1 << 31;
        const VX: u32 = 1 << 29;
        const OX: u32 = 1 << 28;
        const UX: u32 = 1 << 27;
        const ZX: u32 = 1 << 26;
        const XX: u32 = 1 << 25;
        const FR: u32 = 1 << 18;
        const FI: u32 = 1 << 17;

        if exceptions.update_rounding {
            self.state.fpscr &= !(FR | FI);
            if exceptions.fraction_rounded {
                self.state.fpscr |= FR;
            }
            if exceptions.inexact {
                self.state.fpscr |= FI;
            }
        }
        let mut raised = 0;
        if exceptions.invalid {
            raised |= VX | exceptions.invalid_cause;
        }
        if exceptions.div_zero {
            raised |= ZX;
        }
        if exceptions.overflow {
            raised |= OX;
        }
        if exceptions.underflow {
            raised |= UX;
        }
        if exceptions.raise_inexact {
            raised |= XX;
        }
        if raised & !self.state.fpscr != 0 {
            self.state.fpscr |= FX;
        }
        self.state.fpscr |= raised;

        // FPSCR enable bits request a guest exception for this instruction.
        // Do not infer this solely from sticky FEX: the same cause may have
        // been recorded by an earlier instruction.
        let mut enabled_causes = 0;
        if exceptions.invalid && self.state.fpscr & (1 << 7) != 0 {
            enabled_causes |= VX | exceptions.invalid_cause;
        }
        if exceptions.div_zero && self.state.fpscr & (1 << 4) != 0 {
            enabled_causes |= ZX;
        }
        if exceptions.overflow && self.state.fpscr & (1 << 6) != 0 {
            enabled_causes |= OX;
        }
        if exceptions.underflow && self.state.fpscr & (1 << 5) != 0 {
            enabled_causes |= UX;
        }
        if exceptions.raise_inexact && self.state.fpscr & (1 << 3) != 0 {
            enabled_causes |= XX;
        }
        let fp_exception_mode = self.fp_exception_mode();
        if enabled_causes != 0
            && fp_exception_mode != PpcFpExceptionMode::Disabled
            && self.pending_exception.is_none()
        {
            self.pending_exception = Some(PpcException::FloatingPoint {
                instruction_pc: self.state.pc,
                causes: enabled_causes,
                mode: fp_exception_mode,
            });
        }

        self.refresh_fpscr_fex();
        if record {
            self.copy_fpscr_to_cr1();
        }
    }

    fn set_fprf_for_value(&mut self, value: f64) {
        self.set_fprf_for_value_precision(value, false);
    }

    fn set_fprf_for_value_precision(&mut self, value: f64, single_precision: bool) {
        const FPRF_MASK: u32 = 0x0001_F000;
        let denormal = if single_precision {
            value.abs() < f32::MIN_POSITIVE as f64
        } else {
            value.is_subnormal()
        };
        let class = if value.is_nan() {
            if Self::is_signaling_nan(value) {
                0x00
            } else {
                0x11
            }
        } else if value.is_infinite() {
            if value.is_sign_negative() { 0x09 } else { 0x05 }
        } else if value == 0.0 {
            if value.is_sign_negative() { 0x12 } else { 0x02 }
        } else if denormal {
            if value.is_sign_negative() { 0x18 } else { 0x14 }
        } else if value.is_sign_negative() {
            0x08
        } else {
            0x04
        };
        self.state.fpscr = (self.state.fpscr & !FPRF_MASK) | ((class as u32) << 12);
    }

    fn set_fprf_condition(&mut self, condition: u8) {
        // Compare instructions update the four-bit FPCC field, not the
        // separate FPRF class bit C.
        const FPCC_MASK: u32 = 0x0000_F000;
        self.state.fpscr = (self.state.fpscr & !FPCC_MASK) | (((condition as u32) & 0x0F) << 12);
    }

    fn is_signaling_nan(value: f64) -> bool {
        let bits = value.to_bits();
        (bits & 0x7FF0_0000_0000_0000) == 0x7FF0_0000_0000_0000
            && (bits & 0x000F_FFFF_FFFF_FFFF) != 0
            && (bits & 0x0008_0000_0000_0000) == 0
    }

    #[allow(clippy::too_many_arguments)]
    fn update_fpscr_for_result(
        &mut self,
        value: f64,
        operands: &[f64],
        source_signaling_nan: bool,
        operation: u32,
        inexact: bool,
        fraction_rounded: bool,
        overflow: bool,
        underflow: bool,
        single_precision: bool,
        record: bool,
    ) -> bool {
        let has_nan_operand = operands.iter().any(|operand| operand.is_nan());
        let signaling_nan = source_signaling_nan
            || operands
                .iter()
                .any(|operand| Self::is_signaling_nan(*operand));
        let mut invalid_cause = if signaling_nan { 1 << 24 } else { 0 }; // VXSNAN

        // Determine invalid-operation causes from the operands themselves.
        // This matters for fused operations where a QNaN addend must not hide
        // an independent invalid infinity-times-zero product.
        match operation {
            18 if operands.len() >= 2 && operands[0] == 0.0 && operands[1] == 0.0 => {
                invalid_cause |= 1 << 21; // VXZDZ
            }
            18 if operands.len() >= 2 && operands[0].is_infinite() && operands[1].is_infinite() => {
                invalid_cause |= 1 << 22; // VXIDI
            }
            20 if operands.len() >= 2
                && operands[0].is_infinite()
                && operands[1].is_infinite()
                && operands[0].is_sign_negative() == operands[1].is_sign_negative() =>
            {
                invalid_cause |= 1 << 23; // VXISI: infinity minus same-sign infinity
            }
            21 if operands.len() >= 2
                && operands[0].is_infinite()
                && operands[1].is_infinite()
                && operands[0].is_sign_negative() != operands[1].is_sign_negative() =>
            {
                invalid_cause |= 1 << 23; // VXISI: opposite-sign infinities added
            }
            25 if operands.len() >= 2
                && ((operands[0].is_infinite() && operands[1] == 0.0)
                    || (operands[1].is_infinite() && operands[0] == 0.0)) =>
            {
                invalid_cause |= 1 << 20; // VXIMZ
            }
            28..=31
                if operands.len() >= 3
                    && ((operands[0].is_infinite() && operands[2] == 0.0)
                        || (operands[2].is_infinite() && operands[0] == 0.0)) =>
            {
                invalid_cause |= 1 << 20; // VXIMZ
            }
            28..=31
                if operands.len() >= 3
                    && (operands[0].is_infinite() || operands[2].is_infinite())
                    && operands[1].is_infinite()
                    && ((operands[0].is_sign_negative() ^ operands[2].is_sign_negative())
                        != (operands[1].is_sign_negative() ^ matches!(operation, 28 | 30))) =>
            {
                invalid_cause |= 1 << 23; // VXISI
            }
            22 | 26 if operands.last().is_some_and(|operand| *operand < 0.0) => {
                invalid_cause |= 1 << 9; // VXSQRT
            }
            _ => {}
        }

        let invalid = signaling_nan || invalid_cause != 0 || (value.is_nan() && !has_nan_operand);
        // ZX is specific to division/reciprocal operations. A zero
        // second operand in add, subtract, multiply, or fused operations is
        // ordinary arithmetic and must not raise divide-by-zero.
        let div_zero = match operation {
            18 => {
                operands.len() >= 2
                    && operands[1] == 0.0
                    && operands[0] != 0.0
                    && operands[0].is_finite()
            }
            24 | 26 => operands.last().is_some_and(|operand| *operand == 0.0),
            _ => false,
        };
        let overflow = !div_zero
            && operands.iter().all(|operand| operand.is_finite())
            && (overflow || value.is_infinite());
        let underflow = underflow && !div_zero;
        let invalid_enabled = invalid && self.state.fpscr & (1 << 7) != 0;
        let div_zero_enabled = div_zero && self.state.fpscr & (1 << 4) != 0;
        let overflow_enabled = overflow && self.state.fpscr & (1 << 6) != 0;
        if !invalid_enabled && !div_zero_enabled {
            self.set_fprf_for_value_precision(value, single_precision);
        }
        let update_rounding = operation != 26 || invalid || div_zero || overflow || underflow;
        self.update_fpscr_exceptions(
            FpExceptionFlags {
                invalid,
                div_zero,
                overflow,
                underflow,
                inexact: if overflow && !overflow_enabled {
                    true
                } else {
                    inexact
                },
                raise_inexact: if overflow && !overflow_enabled {
                    true
                } else {
                    inexact
                },
                fraction_rounded: fraction_rounded && !(overflow && !overflow_enabled),
                update_rounding,
                invalid_cause,
            },
            record,
        );
        invalid_enabled || div_zero_enabled
    }

    fn next_up_f32(value: f32) -> f32 {
        if value.is_nan() || value == f32::INFINITY {
            return value;
        }
        if value == 0.0 {
            return f32::from_bits(1);
        }
        let bits = value.to_bits();
        f32::from_bits(if value.is_sign_positive() {
            bits + 1
        } else {
            bits - 1
        })
    }

    fn next_down_f32(value: f32) -> f32 {
        if value.is_nan() || value == f32::NEG_INFINITY {
            return value;
        }
        if value == 0.0 {
            return f32::from_bits(0x8000_0001);
        }
        let bits = value.to_bits();
        f32::from_bits(if value.is_sign_positive() {
            bits - 1
        } else {
            bits + 1
        })
    }

    fn scale_by_power_of_two(mut value: f64, mut exponent: i32) -> f64 {
        const TWO_POW_1023: f64 = f64::from_bits(0x7FE0_0000_0000_0000);
        const TWO_POW_NEG_1022: f64 = f64::from_bits(0x0010_0000_0000_0000);
        while exponent > 1023 {
            value *= TWO_POW_1023;
            exponent -= 1023;
        }
        while exponent < -1022 {
            value *= TWO_POW_NEG_1022;
            exponent += 1022;
        }
        value * 2.0_f64.powi(exponent)
    }

    fn round_to_single_significand(&self, value: f64) -> f64 {
        if value == 0.0 || !value.is_finite() {
            return value;
        }
        let exponent = value.abs().log2().floor() as i32;
        let normalized = Self::scale_by_power_of_two(value, -exponent);
        let rounded = self.round_to_single(normalized);
        Self::scale_by_power_of_two(rounded, exponent)
    }

    fn round_to_single(&self, value: f64) -> f64 {
        let nearest = value as f32;
        let nearest_as_double = nearest as f64;
        if value.is_nan() || value.is_infinite() || nearest_as_double == value {
            return nearest_as_double;
        }
        let adjusted = match self.fpscr_rounding_mode() {
            0 => nearest,
            1 if nearest_as_double > value => Self::next_down_f32(nearest),
            1 if nearest_as_double < value => Self::next_up_f32(nearest),
            2 if nearest_as_double < value => Self::next_up_f32(nearest),
            3 if nearest_as_double > value => Self::next_down_f32(nearest),
            _ => nearest,
        };
        adjusted as f64
    }

    fn extend_single_preserving_nan(value: f32) -> f64 {
        let bits = value.to_bits();
        let exponent = bits & 0x7F80_0000;
        let fraction = bits & 0x007F_FFFF;
        if exponent == 0x7F80_0000 && fraction != 0 {
            let sign = ((bits as u64) & 0x8000_0000) << 32;
            f64::from_bits(sign | 0x7FF0_0000_0000_0000 | ((fraction as u64) << 29))
        } else {
            value as f64
        }
    }

    fn narrow_single_for_store(&self, value: f64) -> f32 {
        if value.is_nan() {
            Self::narrow_single_preserving_nan(value)
        } else {
            self.round_to_single(value) as f32
        }
    }

    fn narrow_single_preserving_nan(value: f64) -> f32 {
        let bits = value.to_bits();
        let exponent = bits & 0x7FF0_0000_0000_0000;
        let fraction = bits & 0x000F_FFFF_FFFF_FFFF;
        if exponent == 0x7FF0_0000_0000_0000 && fraction != 0 {
            let sign = ((bits >> 32) as u32) & 0x8000_0000;
            let mut payload = ((fraction >> 29) as u32) & 0x007F_FFFF;
            if payload == 0 {
                payload = 1;
            }
            f32::from_bits(sign | 0x7F80_0000 | payload)
        } else {
            value as f32
        }
    }

    fn next_up_f64(value: f64) -> f64 {
        if value.is_nan() || value == f64::INFINITY {
            return value;
        }
        if value == 0.0 {
            return f64::from_bits(1);
        }
        let bits = value.to_bits();
        f64::from_bits(if value.is_sign_positive() {
            bits + 1
        } else {
            bits - 1
        })
    }

    fn next_down_f64(value: f64) -> f64 {
        if value.is_nan() || value == f64::NEG_INFINITY {
            return value;
        }
        if value == 0.0 {
            return f64::from_bits(0x8000_0000_0000_0001);
        }
        let bits = value.to_bits();
        f64::from_bits(if value.is_sign_positive() {
            bits - 1
        } else {
            bits + 1
        })
    }

    /// Error-free TwoSum residual for finite, non-overflowing additions.
    fn two_sum_error(a: f64, b: f64, sum: f64) -> f64 {
        let b_virtual = sum - a;
        let a_virtual = sum - b_virtual;
        let b_roundoff = b - b_virtual;
        let a_roundoff = a - a_virtual;
        a_roundoff + b_roundoff
    }

    /// Applies FPSCR-directed binary64 rounding and reports whether the
    /// operation was inexact and whether the stored fraction rounded away
    /// from zero. The residual is exact for add/sub/mul/div/sqrt in the
    /// ordinary finite range; fused operations use a compensated residual.
    fn round_binary64_result(
        &self,
        operation: u32,
        a: f64,
        b: f64,
        c: f64,
        result: f64,
    ) -> (f64, bool, bool, bool) {
        if result.is_nan() {
            return (result, false, false, false);
        }
        if result.is_infinite() {
            let operands_finite = match operation {
                21 | 20 | 18 => a.is_finite() && b.is_finite(),
                25 => a.is_finite() && c.is_finite(),
                22 | 24 | 26 => b.is_finite(),
                28..=31 => a.is_finite() && b.is_finite() && c.is_finite(),
                _ => false,
            };
            let divide_by_zero =
                matches!(operation, 18 | 24 | 26) && b == 0.0 && a != 0.0 && a.is_finite();
            if !operands_finite || divide_by_zero {
                return (result, false, false, false);
            }
            let negative = result.is_sign_negative();
            let adjusted = match self.fpscr_rounding_mode() {
                0 => result,
                1 => {
                    if negative {
                        -f64::MAX
                    } else {
                        f64::MAX
                    }
                }
                2 if negative => -f64::MAX,
                3 if !negative => f64::MAX,
                _ => result,
            };
            return (adjusted, true, adjusted.is_infinite(), true);
        }

        let underflowed_to_zero = result == 0.0
            && match operation {
                18 => a != 0.0 && b != 0.0 && a.is_finite() && b.is_finite(),
                25 => a != 0.0 && c != 0.0 && a.is_finite() && c.is_finite(),
                _ => false,
            };
        if underflowed_to_zero {
            let negative = match operation {
                18 => a.is_sign_negative() ^ b.is_sign_negative(),
                25 => a.is_sign_negative() ^ c.is_sign_negative(),
                _ => false,
            };
            let min_subnormal = f64::from_bits(1);
            let adjusted = match self.fpscr_rounding_mode() {
                2 if !negative => min_subnormal,
                3 if negative => -min_subnormal,
                _ => result,
            };
            return (adjusted, true, adjusted != 0.0, false);
        }

        let residual = match operation {
            21 => Self::two_sum_error(a, b, result),
            20 => Self::two_sum_error(a, -b, result),
            18 if b != 0.0 && a.is_finite() && b.is_finite() => (-result).mul_add(b, a) / b,
            25 => a.mul_add(c, -result),
            22 if b >= 0.0 => (-result).mul_add(result, b),
            28..=31 => {
                let subtract_b = matches!(operation, 28 | 30);
                let negate = matches!(operation, 30 | 31);
                let addend = if subtract_b { -b } else { b };
                let product = a * c;
                let product_error = a.mul_add(c, -product);
                let sum = product + addend;
                let sum_error = Self::two_sum_error(product, addend, sum);
                let compensated =
                    (sum - if negate { -result } else { result }) + product_error + sum_error;
                if negate { -compensated } else { compensated }
            }
            _ => 0.0,
        };

        if !residual.is_finite() {
            return (result, false, false, false);
        }
        let inexact = residual != 0.0;
        if !inexact {
            return (result, false, false, false);
        }

        let adjusted = match self.fpscr_rounding_mode() {
            0 => result,
            1 if (result.is_sign_positive() && residual < 0.0)
                || (result.is_sign_negative() && residual > 0.0) =>
            {
                if result.is_sign_positive() {
                    Self::next_down_f64(result)
                } else {
                    Self::next_up_f64(result)
                }
            }
            2 if residual > 0.0 => Self::next_up_f64(result),
            3 if residual < 0.0 => Self::next_down_f64(result),
            _ => result,
        };
        let rounded_minus_exact = (adjusted - result) - residual;
        let fraction_rounded = if adjusted.is_sign_positive() {
            adjusted > 0.0 && rounded_minus_exact > 0.0
        } else {
            adjusted < 0.0 && rounded_minus_exact < 0.0
        };
        (adjusted, true, fraction_rounded, false)
    }

    fn round_double_underflow_enabled(
        &self,
        operation: u32,
        a: f64,
        b: f64,
        c: f64,
        result: f64,
    ) -> (f64, bool, bool) {
        match operation {
            18 => {
                let numerator_exponent = a.abs().log2().floor() as i32;
                let (scaled_a, scaled_b) = if numerator_exponent <= -513 {
                    (Self::scale_by_power_of_two(a, 1536), b)
                } else {
                    (a, Self::scale_by_power_of_two(b, -1536))
                };
                let intermediate = scaled_a / scaled_b;
                let rounded = self.round_binary64_result(18, scaled_a, scaled_b, 0.0, intermediate);
                (rounded.0, rounded.1, rounded.2)
            }
            25 => {
                let (scaled_a, scaled_c) = if a.abs() <= c.abs() {
                    (Self::scale_by_power_of_two(a, 1536), c)
                } else {
                    (a, Self::scale_by_power_of_two(c, 1536))
                };
                let intermediate = scaled_a * scaled_c;
                let rounded = self.round_binary64_result(25, scaled_a, 0.0, scaled_c, intermediate);
                (rounded.0, rounded.1, rounded.2)
            }
            28..=31 if b == 0.0 => {
                let (mut scaled_a, scaled_c) = if a.abs() <= c.abs() {
                    (Self::scale_by_power_of_two(a, 1536), c)
                } else {
                    (a, Self::scale_by_power_of_two(c, 1536))
                };
                if matches!(operation, 30 | 31) {
                    scaled_a = -scaled_a;
                }
                let intermediate = scaled_a * scaled_c;
                let rounded = self.round_binary64_result(25, scaled_a, 0.0, scaled_c, intermediate);
                (rounded.0, rounded.1, rounded.2)
            }
            _ => (Self::scale_by_power_of_two(result, 1536), false, false),
        }
    }

    fn round_double_overflow_enabled(
        &self,
        operation: u32,
        a: f64,
        b: f64,
        c: f64,
    ) -> (f64, bool, bool) {
        let (scaled_a, scaled_b, scaled_c, intermediate) = match operation {
            21 => {
                let sa = Self::scale_by_power_of_two(a, -1536);
                let sb = Self::scale_by_power_of_two(b, -1536);
                (sa, sb, c, sa + sb)
            }
            20 => {
                let sa = Self::scale_by_power_of_two(a, -1536);
                let sb = Self::scale_by_power_of_two(b, -1536);
                (sa, sb, c, sa - sb)
            }
            18 => {
                let exponent = b.abs().log2().floor() as i32;
                if exponent <= -513 {
                    let sb = Self::scale_by_power_of_two(b, 1536);
                    (a, sb, c, a / sb)
                } else {
                    let sa = Self::scale_by_power_of_two(a, -1536);
                    (sa, b, c, sa / b)
                }
            }
            25 => {
                let sa = Self::scale_by_power_of_two(a, -768);
                let sc = Self::scale_by_power_of_two(c, -768);
                (sa, b, sc, sa * sc)
            }
            28..=31 => {
                let sa = Self::scale_by_power_of_two(a, -768);
                let sb = Self::scale_by_power_of_two(b, -1536);
                let sc = Self::scale_by_power_of_two(c, -768);
                let result = match operation {
                    29 => sa.mul_add(sc, sb),
                    28 => sa.mul_add(sc, -sb),
                    31 => -sa.mul_add(sc, sb),
                    30 => -sa.mul_add(sc, -sb),
                    _ => unreachable!(),
                };
                (sa, sb, sc, result)
            }
            _ => return (f64::NAN, false, false),
        };
        let rounded =
            self.round_binary64_result(operation, scaled_a, scaled_b, scaled_c, intermediate);
        (rounded.0, rounded.1, rounded.2)
    }

    fn fpscr_rounding_mode(&self) -> u32 {
        self.state.fpscr & 0b11
    }

    fn round_i64_to_f64(&self, integer: i64) -> (f64, bool, bool) {
        let nearest = integer as f64;
        let represented = nearest as i128;
        let exact = integer as i128;
        if represented == exact {
            return (nearest, false, false);
        }

        let adjusted = match self.fpscr_rounding_mode() {
            0 => nearest,
            1 if integer > 0 && represented > exact => Self::next_down_f64(nearest),
            1 if integer < 0 && represented < exact => Self::next_up_f64(nearest),
            2 if represented < exact => Self::next_up_f64(nearest),
            3 if represented > exact => Self::next_down_f64(nearest),
            _ => nearest,
        };
        let adjusted_integer = adjusted as i128;
        let fraction_rounded = if integer > 0 {
            adjusted_integer > exact
        } else {
            integer < 0 && adjusted_integer < exact
        };
        (adjusted, true, fraction_rounded)
    }

    fn round_fp_to_integer(&self, value: f64) -> f64 {
        match self.fpscr_rounding_mode() {
            0 => value.round_ties_even(),
            1 => value.trunc(),
            2 => value.ceil(),
            _ => value.floor(),
        }
    }

    fn set_fx_for_new_exception_bits(&mut self, previous_fpscr: u32) {
        const EXCEPTION_BITS: u32 = 0x1FF8_0700;
        let newly_set = self.state.fpscr & EXCEPTION_BITS & !previous_fpscr;
        if newly_set != 0 {
            self.state.fpscr |= 1 << 31;
        }
    }

    fn write_fpscr_field(&mut self, field: usize, value: u8) {
        let shift = 28 - (field * 4);
        let mut mask = 0xF_u32 << shift;
        if field == 0 {
            // FEX and VX are computed summaries, not writable field bits.
            mask &= !((1 << 30) | (1 << 29));
        }
        let previous_fpscr = self.state.fpscr;
        self.state.fpscr = (self.state.fpscr & !mask) | (((value as u32) << shift) & mask);
        // mtfsfi writes FX explicitly; unlike arithmetic and mtfsb1, it must
        // not set FX as a side effect of setting a sticky exception cause.
        self.refresh_fpscr_fex();
        self.queue_fpscr_write_exception(self.state.pc, previous_fpscr);
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

    fn is_fpu_instruction(opcode: u32, instruction: u32) -> bool {
        match opcode {
            48..=55 | 59 | 63 => true,
            31 => matches!((instruction >> 1) & 0x3FF, 855 | 887 | 983),
            _ => false,
        }
    }

    /// Fetches and executes one big-endian 32-bit instruction.
    pub fn step<M>(&mut self, memory: &mut M) -> Result<(), PpcError>
    where
        M: GuestMemoryAccess,
        M::Error: Into<PpcError>,
    {
        // A precise exception must be dispatched before another instruction executes.
        // Imprecise-mode events remain pending while the guest advances to a
        // synchronization point; the outer CPU loop owns that scheduling.
        if let Some(
            exception @ PpcException::FloatingPoint {
                mode: PpcFpExceptionMode::Precise,
                ..
            },
        ) = self.pending_exception
        {
            return Err(PpcError::PendingException(exception));
        }
        if let Some(
            exception @ (PpcException::FloatingPointUnavailable { .. }
            | PpcException::FloatingPointEnableTransition { .. }),
        ) = self.pending_exception
        {
            return Err(PpcError::PendingException(exception));
        }
        let pc = self.state.pc;
        let instruction = memory.fetch_u32_be(GuestAddress(pc)).map_err(Into::into)?;
        let opcode = instruction >> 26;
        if Self::is_fpu_instruction(opcode, instruction) && self.state.msr & PPC_MSR_FP == 0 {
            let exception = PpcException::FloatingPointUnavailable { instruction_pc: pc };
            self.pending_exception = Some(exception);
            return Err(PpcError::PendingException(exception));
        }
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
                if extended_opcode == 18 {
                    // rfid restores the architecturally defined MSR fields
                    // from SRR1. Exception syndrome bits 43:47 are not MSR
                    // state and must not leak back into the running context.
                    const RESTORE_MASK: u64 = 0xEFFF_FFFF_87C0_6FCF;
                    const HYPERVISOR_RESTORE_MASK: u64 = (1 << 60) | (1 << 12);
                    let saved = self.state.srr1;
                    let mut restored = saved & RESTORE_MASK;
                    if self.state.msr & (1 << 60) != 0 {
                        restored |= saved & HYPERVISOR_RESTORE_MASK;
                    }
                    if saved & ((1 << 63) | (1 << 62)) != 0 {
                        restored |= 1 << 63; // MSR[0] <- SRR1[0] | SRR1[1].
                    }
                    if saved & ((1 << 15) | (1 << 14)) != 0 {
                        restored |= 1 << 15; // MSR[48] <- SRR1[48] | SRR1[49].
                    }
                    if saved & ((1 << 5) | (1 << 14)) != 0 {
                        restored |= 1 << 5; // MSR[58] <- SRR1[58] | SRR1[49].
                    }
                    if saved & ((1 << 4) | (1 << 14)) != 0 {
                        restored |= 1 << 4; // MSR[59] <- SRR1[59] | SRR1[49].
                    }
                    self.state.msr = restored;
                    self.state.pc = (self.state.srr0 as u32) & !3;
                    return Ok(());
                }
                if extended_opcode == 150 {
                    // isync is context-synchronizing. All prior guest
                    // instructions have completed before an imprecise FP
                    // event is made visible to the outer CPU loop.
                    self.state.pc = next_pc;
                    if let Some(
                        exception @ PpcException::FloatingPoint {
                            mode:
                                PpcFpExceptionMode::ImpreciseNonRecoverable
                                | PpcFpExceptionMode::ImpreciseRecoverable,
                            ..
                        },
                    ) = self.pending_exception
                    {
                        return Err(PpcError::PendingException(exception));
                    }
                    return Ok(());
                }
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
                    83 => {
                        // mfmsr reads the complete Xenon 64-bit MSR.
                        if record || instruction & 0x001F_0000 != 0 {
                            return Err(PpcError::UnsupportedInstruction { pc, opcode });
                        }
                        self.state.gpr[rt_rs] = self.state.msr;
                    }
                    339 | 467 => {
                        // mfspr / mtspr. The SPR field is split across the
                        // instruction and its two five-bit halves are swapped.
                        if record {
                            return Err(PpcError::UnsupportedInstruction { pc, opcode });
                        }
                        let spr =
                            (((instruction >> 11) & 0x1F) << 5) | ((instruction >> 16) & 0x1F);
                        match (extended_opcode, spr) {
                            (339, 1) => self.state.gpr[rt_rs] = self.state.xer as u32 as u64,
                            (339, 8) => self.state.gpr[rt_rs] = self.state.lr,
                            (339, 9) => self.state.gpr[rt_rs] = self.state.ctr,
                            (339, 26) => self.state.gpr[rt_rs] = self.state.srr0,
                            (339, 27) => self.state.gpr[rt_rs] = self.state.srr1,
                            (467, 1) => self.state.xer = self.state.gpr[rt_rs] as u32 as u64,
                            (467, 8) => self.state.lr = self.state.gpr[rt_rs],
                            (467, 9) => self.state.ctr = self.state.gpr[rt_rs],
                            (467, 26) => self.state.srr0 = self.state.gpr[rt_rs],
                            (467, 27) => self.state.srr1 = self.state.gpr[rt_rs],
                            _ => return Err(PpcError::UnsupportedInstruction { pc, opcode }),
                        }
                        self.state.pc = next_pc;
                    }
                    146 | 178 => {
                        // mtmsr and mtmsrd are context-synchronizing: delayed
                        // imprecise FP events must be surfaced before the MSR
                        // can be changed.
                        if let Some(
                            exception @ PpcException::FloatingPoint {
                                mode:
                                    PpcFpExceptionMode::ImpreciseNonRecoverable
                                    | PpcFpExceptionMode::ImpreciseRecoverable,
                                ..
                            },
                        ) = self.pending_exception
                        {
                            return Err(PpcError::PendingException(exception));
                        }

                        let source = self.state.gpr[rt_rs];
                        let value = if extended_opcode == 146 {
                            // mtmsr's L=1 form is reserved in this encoding.
                            if instruction & 0x001F_0000 != 0 {
                                return Err(PpcError::UnsupportedInstruction { pc, opcode });
                            }
                            source
                        } else {
                            // mtmsrd permits L=0 (full write with architectural
                            // aliases) and L=1 (update only EE and RI).
                            if instruction & 0x001E_0000 != 0 {
                                return Err(PpcError::UnsupportedInstruction { pc, opcode });
                            }
                            if instruction & (1 << 16) != 0 {
                                let mut value = self.state.msr;
                                const EE: u64 = 1 << 15; // MSR[48]
                                const RI: u64 = 1 << 1; // MSR[62]
                                value = (value & !EE) | (source & EE);
                                value = (value & !RI) | (source & RI);
                                value
                            } else {
                                let mut value = source;
                                if source & ((1 << 63) | (1 << 62)) != 0 {
                                    value |= 1 << 63; // MSR[0] <- RS[0] | RS[1].
                                }
                                if source & ((1 << 15) | (1 << 14)) != 0 {
                                    value |= 1 << 15; // MSR[48] <- RS[48] | RS[49].
                                }
                                if source & ((1 << 5) | (1 << 14)) != 0 {
                                    value |= 1 << 5; // MSR[58] <- RS[58] | RS[49].
                                }
                                if source & ((1 << 4) | (1 << 14)) != 0 {
                                    value |= 1 << 4; // MSR[59] <- RS[59] | RS[49].
                                }
                                value
                            }
                        };
                        self.apply_msr_write(value, next_pc);
                        return Ok(());
                    }
                    598 => {
                        // sync is execution-synchronizing. Surface delayed
                        // imprecise FP exceptions only after advancing past it.
                        self.state.pc = next_pc;
                        if let Some(
                            exception @ PpcException::FloatingPoint {
                                mode:
                                    PpcFpExceptionMode::ImpreciseNonRecoverable
                                    | PpcFpExceptionMode::ImpreciseRecoverable,
                                ..
                            },
                        ) = self.pending_exception
                        {
                            return Err(PpcError::PendingException(exception));
                        }
                        return Ok(());
                    }
                    855 | 887 | 983 => {
                        // Integer-word transfers use the low-order 32 bits of
                        // the FPR. lfiwax sign-fills the upper word; lfiwzx
                        // zero-fills it; stfiwx stores without conversion.
                        let address = (if ra == 0 { 0 } else { left }).wrapping_add(right);
                        match extended_opcode {
                            855 => {
                                let word = memory
                                    .read_u32_be(GuestAddress(address as u32))
                                    .map_err(Into::into)?;
                                let upper = if word & 0x8000_0000 != 0 {
                                    0xFFFF_FFFF_0000_0000
                                } else {
                                    0
                                };
                                self.state.fpr[rt_rs] = f64::from_bits(upper | word as u64);
                            }
                            887 => {
                                let word = memory
                                    .read_u32_be(GuestAddress(address as u32))
                                    .map_err(Into::into)?;
                                self.state.fpr[rt_rs] = f64::from_bits(word as u64);
                            }
                            983 => {
                                memory
                                    .write_u32_be(
                                        GuestAddress(address as u32),
                                        self.state.fpr[rt_rs].to_bits() as u32,
                                    )
                                    .map_err(Into::into)?;
                            }
                            _ => unreachable!(),
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
                            Self::extend_single_preserving_nan(f32::from_bits(
                                memory
                                    .read_u32_be(GuestAddress(address))
                                    .map_err(Into::into)?,
                            ))
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
                                    self.narrow_single_for_store(self.state.fpr[rt_rs])
                                        .to_bits(),
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
            // Xenon VMX128 operations using the architectural VX128/VX128_2 layouts.
            5 => {
                let registers = decode_vmx128_register_fields(instruction);
                let operation = (((instruction >> 6) & 0xF) << 2) | ((instruction >> 4) & 1);
                let a = self.state.vr[registers.va];
                let b = self.state.vr[registers.vb];
                let mut result = [0_u8; 16];

                // VX128_2: byte permute has an independent 3-bit control operand.
                if instruction & ((1 << 9) | (1 << 4)) == 0 {
                    let vd = registers.vd;
                    let va = registers.va;
                    let vb = registers.vb;
                    let vc = ((instruction >> 6) & 0x7) as usize;
                    let control = self.state.vr[vc];
                    let left = self.state.vr[va];
                    let right = self.state.vr[vb];
                    for lane in 0..16 {
                        let selector = (control[lane] & 0x1F) as usize;
                        result[lane] = if selector < 16 {
                            left[selector]
                        } else {
                            right[selector - 16]
                        };
                    }
                    self.state.vr[vd] = result;
                    self.state.pc = next_pc;
                    return Ok(());
                }

                match operation {
                    0b000001 | 0b000101 | 0b001001 | 0b001101 | 0b010001 | 0b010101 => {
                        // Four IEEE-754 binary32 lanes. Fused multiply-add variants
                        // use the destination as the third source, as specified by VX128.
                        let destination = self.state.vr[registers.vd];
                        for lane in 0..4 {
                            let offset = lane * 4;
                            let av = f32::from_bits(u32::from_be_bytes(
                                a[offset..offset + 4].try_into().unwrap(),
                            ));
                            let bv = f32::from_bits(u32::from_be_bytes(
                                b[offset..offset + 4].try_into().unwrap(),
                            ));
                            let dv = f32::from_bits(u32::from_be_bytes(
                                destination[offset..offset + 4].try_into().unwrap(),
                            ));
                            let value = match operation {
                                0b000001 => av + bv,
                                0b000101 => av - bv,
                                0b001001 => av * bv,
                                0b001101 | 0b010001 => av.mul_add(dv, bv),
                                0b010101 => (-av).mul_add(dv, bv),
                                _ => unreachable!(),
                            };
                            result[offset..offset + 4]
                                .copy_from_slice(&value.to_bits().to_be_bytes());
                        }
                    }
                    0b011001 | 0b011101 => {
                        let lane_count = if operation == 0b011001 { 3 } else { 4 };
                        let mut sum = 0.0_f32;
                        for lane in 0..lane_count {
                            let offset = lane * 4;
                            let av = f32::from_bits(u32::from_be_bytes(
                                a[offset..offset + 4].try_into().unwrap(),
                            ));
                            let bv = f32::from_bits(u32::from_be_bytes(
                                b[offset..offset + 4].try_into().unwrap(),
                            ));
                            sum = av.mul_add(bv, sum);
                        }
                        for lane in 0..4 {
                            let offset = lane * 4;
                            result[offset..offset + 4]
                                .copy_from_slice(&sum.to_bits().to_be_bytes());
                        }
                    }
                    0b100001 | 0b100101 | 0b101001 | 0b101101 | 0b110001 => {
                        for lane in 0..16 {
                            result[lane] = match operation {
                                0b100001 => a[lane] & b[lane],
                                0b100101 => a[lane] & !b[lane],
                                0b101001 => !(a[lane] | b[lane]),
                                0b101101 => a[lane] | b[lane],
                                0b110001 => a[lane] ^ b[lane],
                                _ => unreachable!(),
                            };
                        }
                    }
                    0b110101 => {
                        // vsel128 uses the old destination as the per-bit selector.
                        let mask = self.state.vr[registers.vd];
                        for (lane, output) in result.iter_mut().enumerate() {
                            *output = (a[lane] & !mask[lane]) | (b[lane] & mask[lane]);
                        }
                    }
                    0b111001 | 0b111101 => {
                        let shift = (b[15] & 0x78) >> 3;
                        for (lane, output) in result.iter_mut().enumerate() {
                            let source_lane = if operation == 0b111001 {
                                lane + shift as usize
                            } else {
                                lane.checked_sub(shift as usize).unwrap_or(usize::MAX)
                            };
                            *output = if source_lane < 16 { a[source_lane] } else { 0 };
                        }
                    }
                    // Saturating and modulo pack operations.
                    0b100000 | 0b100100 | 0b101000 | 0b101100 | 0b110000 | 0b110100 | 0b111000
                    | 0b111100 => {
                        let mut saturated = false;
                        let mut out = 0;
                        let signed_input =
                            matches!(operation, 0b100000 | 0b100100 | 0b101000 | 0b101100);
                        let word_input =
                            matches!(operation, 0b101000 | 0b101100 | 0b111000 | 0b111100);
                        for source in [a, b] {
                            if word_input {
                                for lane in 0..4 {
                                    let offset = lane * 4;
                                    let raw = u32::from_be_bytes(
                                        source[offset..offset + 4].try_into().unwrap(),
                                    );
                                    let value = if signed_input {
                                        raw as i32 as i64
                                    } else {
                                        raw as i64
                                    };
                                    let (packed, did_saturate) = match operation {
                                        0b101000 => (
                                            value.clamp(i16::MIN as i64, i16::MAX as i64) as i16
                                                as u16,
                                            value < i16::MIN as i64 || value > i16::MAX as i64,
                                        ),
                                        0b101100 => (
                                            value.clamp(0, u16::MAX as i64) as u16,
                                            value < 0 || value > u16::MAX as i64,
                                        ),
                                        0b111000 => (raw as u16, false),
                                        0b111100 => {
                                            (raw.min(u16::MAX as u32) as u16, raw > u16::MAX as u32)
                                        }
                                        _ => unreachable!(),
                                    };
                                    result[out..out + 2].copy_from_slice(&packed.to_be_bytes());
                                    out += 2;
                                    saturated |= did_saturate;
                                }
                            } else {
                                for lane in 0..8 {
                                    let offset = lane * 2;
                                    let raw = u16::from_be_bytes(
                                        source[offset..offset + 2].try_into().unwrap(),
                                    );
                                    let value = if signed_input {
                                        raw as i16 as i64
                                    } else {
                                        raw as i64
                                    };
                                    let (packed, did_saturate) = match operation {
                                        0b100000 => (
                                            value.clamp(i8::MIN as i64, i8::MAX as i64) as i8 as u8,
                                            value < i8::MIN as i64 || value > i8::MAX as i64,
                                        ),
                                        0b100100 => (
                                            value.clamp(0, u8::MAX as i64) as u8,
                                            value < 0 || value > u8::MAX as i64,
                                        ),
                                        0b110000 => (raw as u8, false),
                                        0b110100 => {
                                            (raw.min(u8::MAX as u16) as u8, raw > u8::MAX as u16)
                                        }
                                        _ => unreachable!(),
                                    };
                                    result[out] = packed;
                                    out += 1;
                                    saturated |= did_saturate;
                                }
                            }
                        }
                        if saturated {
                            // VSCR.SAT is architecturally sticky (bit 0 in PPC numbering).
                            self.state.vscr |= 1 << 31;
                        }
                    }
                    _ => return Err(PpcError::UnsupportedInstruction { pc, opcode }),
                }
                self.state.vr[registers.vd] = result;
                self.state.pc = next_pc;
            }
            // VMX128 extended shifts, comparisons, floating min/max, and permute-immediate.
            6 => {
                let selector_p = (((instruction >> 9) & 0x3) << 5) | ((instruction >> 4) & 0x3);
                if selector_p == 0b0100001 {
                    let vd =
                        (((instruction >> 21) & 0x1F) | (((instruction >> 2) & 0x3) << 5)) as usize;
                    let vb = (((instruction >> 11) & 0x1F) | ((instruction & 0x3) << 5)) as usize;
                    let permutation =
                        ((instruction >> 16) & 0x1F) | (((instruction >> 6) & 0x7) << 5);
                    let source = self.state.vr[vb];
                    let mut result = [0_u8; 16];
                    for output_word in 0..4 {
                        let selector_shift = 6 - (output_word * 2);
                        let source_word = ((permutation >> selector_shift) & 0x3) as usize;
                        let source_start = source_word * 4;
                        let output_start = output_word * 4;
                        result[output_start..output_start + 4]
                            .copy_from_slice(&source[source_start..source_start + 4]);
                    }
                    self.state.vr[vd] = result;
                    self.state.pc = next_pc;
                    return Ok(());
                }

                let registers = decode_vmx128_register_fields(instruction);
                let a = self.state.vr[registers.va];
                let b = self.state.vr[registers.vb];
                let shift_operation = (((instruction >> 6) & 0xF) << 2) | ((instruction >> 4) & 1);
                let compare_operation =
                    (((instruction >> 7) & 0x7) << 3) | ((instruction >> 4) & 1);
                let mut result = [0_u8; 16];

                // VX128_3 unary and conversion instructions. These encodings
                // use VD/VB plus a five-bit immediate in the VA field.
                let unary_operation = (instruction >> 4) & 0x7F;
                let immediate = (instruction >> 16) & 0x1F;
                let unary_handled = match unary_operation {
                    0b0100011 | 0b0100111 | 0b0101011 | 0b0101111 | 0b0110011 | 0b0110111
                    | 0b0111011 | 0b0111111 | 0b1100011 | 0b1100111 | 0b1101011 | 0b1101111
                    | 0b1110011 | 0b1110111 => {
                        for lane in 0..4 {
                            let offset = lane * 4;
                            let raw = u32::from_be_bytes(b[offset..offset + 4].try_into().unwrap());
                            let float = f32::from_bits(raw);
                            let value = match unary_operation {
                                0b0100011 => {
                                    let scaled = float * 2.0_f32.powi(immediate as i32);
                                    let clamped = scaled.clamp(i32::MIN as f32, i32::MAX as f32);
                                    let integer = if scaled.is_nan() { 0 } else { clamped as i32 };
                                    let saturated = !scaled.is_finite()
                                        || scaled < i32::MIN as f32
                                        || scaled >= 2_147_483_648.0;
                                    if saturated {
                                        self.state.vscr |= 1 << 31;
                                    }
                                    integer as u32
                                }
                                0b0100111 => {
                                    let scaled = float * 2.0_f32.powi(immediate as i32);
                                    let clamped = scaled.clamp(0.0, u32::MAX as f32);
                                    let integer = if scaled.is_nan() || scaled < 0.0 {
                                        0
                                    } else {
                                        clamped as u32
                                    };
                                    if !scaled.is_finite()
                                        || scaled < 0.0
                                        || scaled >= 4_294_967_296.0
                                    {
                                        self.state.vscr |= 1 << 31;
                                    }
                                    integer
                                }
                                0b0101011 => (((raw as i32) as f32)
                                    / 2.0_f32.powi(immediate as i32))
                                .to_bits(),
                                0b0101111 => {
                                    ((raw as f32) / 2.0_f32.powi(immediate as i32)).to_bits()
                                }
                                0b0110011 => float.floor().to_bits(),
                                0b0110111 => float.round_ties_even().to_bits(),
                                0b0111011 => float.ceil().to_bits(),
                                0b0111111 => float.trunc().to_bits(),
                                0b1100011 => (1.0_f32 / float).to_bits(),
                                0b1100111 => (1.0_f32 / float.sqrt()).to_bits(),
                                0b1101011 => 2.0_f32.powf(float).to_bits(),
                                0b1101111 => float.log2().to_bits(),
                                0b1110011 => {
                                    let selected = ((immediate & 3) as usize) * 4;
                                    u32::from_be_bytes(
                                        b[selected..selected + 4].try_into().unwrap(),
                                    )
                                }
                                0b1110111 => {
                                    let signed = if immediate & 0x10 != 0 {
                                        (immediate | 0xFFFF_FFE0) as i32
                                    } else {
                                        immediate as i32
                                    };
                                    signed as u32
                                }
                                _ => unreachable!(),
                            };
                            result[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
                        }
                        true
                    }
                    _ => false,
                };
                if unary_handled {
                    self.state.vr[registers.vd] = result;
                    self.state.pc = next_pc;
                    return Ok(());
                }

                // VX128_4 immediate word rotate/insert.
                let special_selector =
                    (((instruction >> 6) & 0x7) << 2) | ((instruction >> 4) & 0x3);
                if special_selector == 0b1110001 {
                    let rotate = ((instruction >> 6) & 0x3) as usize;
                    let mask = (instruction >> 11) & 0x1F;
                    let source = self.state.vr[registers.vb];
                    let old = self.state.vr[registers.vd];
                    for lane in 0..4 {
                        let source_lane = (lane + rotate) & 3;
                        let use_source = mask & (1 << (4 - lane)) != 0;
                        let start = lane * 4;
                        let source_start = source_lane * 4;
                        let selected = if use_source {
                            &source[source_start..source_start + 4]
                        } else {
                            &old[start..start + 4]
                        };
                        result[start..start + 4].copy_from_slice(selected);
                    }
                    self.state.vr[registers.vd] = result;
                    self.state.pc = next_pc;
                    return Ok(());
                }

                match shift_operation {
                    0b000101 | 0b001101 | 0b010101 | 0b011101 => {
                        for lane in 0..4 {
                            let offset = lane * 4;
                            let value =
                                u32::from_be_bytes(a[offset..offset + 4].try_into().unwrap());
                            let amount =
                                u32::from_be_bytes(b[offset..offset + 4].try_into().unwrap())
                                    & 0x1F;
                            let shifted = match shift_operation {
                                0b000101 => value.rotate_left(amount),
                                0b001101 => value.wrapping_shl(amount),
                                0b010101 => ((value as i32) >> amount) as u32,
                                0b011101 => value.wrapping_shr(amount),
                                _ => unreachable!(),
                            };
                            result[offset..offset + 4].copy_from_slice(&shifted.to_be_bytes());
                        }
                    }
                    0b101000 | 0b101100 => {
                        for lane in 0..4 {
                            let offset = lane * 4;
                            let av = f32::from_bits(u32::from_be_bytes(
                                a[offset..offset + 4].try_into().unwrap(),
                            ));
                            let bv = f32::from_bits(u32::from_be_bytes(
                                b[offset..offset + 4].try_into().unwrap(),
                            ));
                            let value = if shift_operation == 0b101000 {
                                av.max(bv)
                            } else {
                                av.min(bv)
                            };
                            result[offset..offset + 4]
                                .copy_from_slice(&value.to_bits().to_be_bytes());
                        }
                    }
                    0b110000 | 0b110100 => {
                        for lane in 0..2 {
                            let source_lane = if shift_operation == 0b110000 {
                                lane
                            } else {
                                lane + 2
                            };
                            let output_a = lane * 2;
                            let output_b = output_a + 1;
                            result[output_a * 4..output_a * 4 + 4]
                                .copy_from_slice(&a[source_lane * 4..source_lane * 4 + 4]);
                            result[output_b * 4..output_b * 4 + 4]
                                .copy_from_slice(&b[source_lane * 4..source_lane * 4 + 4]);
                        }
                    }
                    0b111000 | 0b111100 => {
                        if registers.va == 0x60 {
                            for lane in 0..4 {
                                let source_lane = if shift_operation == 0b111000 {
                                    lane
                                } else {
                                    lane + 4
                                };
                                let offset = source_lane * 2;
                                let half = i16::from_be_bytes([b[offset], b[offset + 1]]);
                                result[lane * 4..lane * 4 + 4]
                                    .copy_from_slice(&(half as i32).to_be_bytes());
                            }
                        } else {
                            for lane in 0..8 {
                                let source_lane = if shift_operation == 0b111000 {
                                    lane
                                } else {
                                    lane + 8
                                };
                                let byte = b[source_lane];
                                let value = (byte as i8 as i16).to_be_bytes();
                                result[lane * 2..lane * 2 + 2].copy_from_slice(&value);
                            }
                        }
                    }
                    _ => match compare_operation {
                        0b000000 | 0b001000 | 0b010000 | 0b011000 | 0b100000 => {
                            let mut all_true = true;
                            let mut all_false = true;
                            for lane in 0..4 {
                                let offset = lane * 4;
                                let av =
                                    u32::from_be_bytes(a[offset..offset + 4].try_into().unwrap());
                                let bv =
                                    u32::from_be_bytes(b[offset..offset + 4].try_into().unwrap());
                                let af = f32::from_bits(av);
                                let bf = f32::from_bits(bv);
                                let (mask, condition) = if compare_operation == 0b011000 {
                                    let above_upper = af > bf;
                                    let below_lower = af < -bf || af.is_nan() || bf.is_nan();
                                    let mask = (if above_upper { 0x8000_0000 } else { 0 })
                                        | (if below_lower { 0x4000_0000 } else { 0 });
                                    (mask, mask == 0)
                                } else {
                                    let condition = match compare_operation {
                                        0b000000 => af == bf,
                                        0b001000 => af >= bf,
                                        0b010000 => af > bf,
                                        0b100000 => av == bv,
                                        _ => unreachable!(),
                                    };
                                    (if condition { u32::MAX } else { 0 }, condition)
                                };
                                result[offset..offset + 4].copy_from_slice(&mask.to_be_bytes());
                                all_true &= condition;
                                all_false &= !condition;
                            }
                            if instruction & (1 << 6) != 0 {
                                self.state.cr[6] = if all_true {
                                    0b1000
                                } else if all_false {
                                    0b0010
                                } else {
                                    0b0100
                                };
                            }
                        }
                        _ => return Err(PpcError::UnsupportedInstruction { pc, opcode }),
                    },
                }
                self.state.vr[registers.vd] = result;
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
                    48 | 49 => Self::extend_single_preserving_nan(f32::from_bits(
                        memory
                            .read_u32_be(GuestAddress(address))
                            .map_err(Into::into)?,
                    )),
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
                            self.narrow_single_for_store(self.state.fpr[rt_rs])
                                .to_bits(),
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

                // All FPSCR access instructions serialize prior floating-point
                // operations. Deliver a delayed exception before the current
                // instruction can read, clear, or replace the FPSCR state.
                if opcode == 63 && matches!(xo10, 64 | 38 | 70 | 134 | 711 | 583) {
                    if let Some(exception) = self.pending_imprecise_fp_exception() {
                        return Err(PpcError::PendingException(exception));
                    }
                }
                if (single && !matches!(xo5, 18 | 20 | 21 | 22 | 24 | 25 | 28..=31))
                    || (!single && xo10 == 24)
                {
                    return Err(PpcError::UnsupportedInstruction { pc, opcode });
                }
                let record = instruction & 1 != 0;
                let source_a = self.state.fpr[fra];
                let source_b = self.state.fpr[frb];
                let source_c = self.state.fpr[frc];
                let mut a = source_a;
                let mut b = source_b;
                let mut c = source_c;
                if single && xo10 != 24 {
                    // Single-precision arithmetic consumes binary32 source
                    // operands. FRES is the exception: its source is binary64.
                    a = (a as f32) as f64;
                    b = (b as f32) as f64;
                    c = (c as f32) as f64;
                }

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
                        let clear_mask = match crfs {
                            0 => (1 << 31) | (1 << 28), // FX and OX; FEX/VX are summaries.
                            1 => 0x0F00_0000,           // UX, ZX, XX, VXSNAN.
                            2 => 0x00F0_0000,           // VXISI, VXIDI, VXZDZ, VXIMZ.
                            3 => (1 << 19) | (1 << 18) | (1 << 17), // VXVC, FR, FI.
                            5 => 0x0000_0700,           // VXSOFT, VXSQRT, VXCVI.
                            _ => 0,                     // Non-exception status/control fields.
                        };
                        self.state.fpscr &= !clear_mask;
                        self.refresh_fpscr_fex();
                        self.state.pc = next_pc;
                        return Ok(());
                    }
                    38 | 70 => {
                        // mtfsb1 / mtfsb0
                        let previous_fpscr = self.state.fpscr;
                        let bit = (instruction >> 21) & 0x1F;
                        // FEX and VX are derived summaries and cannot be
                        // explicitly changed by mtfsb0/mtfsb1.
                        if bit != 1 && bit != 2 {
                            let mask = 1_u32 << (31 - bit);
                            if xo10 == 38 {
                                self.state.fpscr |= mask;
                                self.set_fx_for_new_exception_bits(previous_fpscr);
                            } else {
                                self.state.fpscr &= !mask;
                            }
                        }
                        self.refresh_fpscr_fex();
                        self.queue_fpscr_write_exception(pc, previous_fpscr);
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
                        let previous_fpscr = self.state.fpscr;
                        let field_mask = ((instruction >> 17) & 0xFF) as u8;
                        let value = self.state.fpr[frb].to_bits() as u32;
                        if instruction & (1 << 16) != 0 {
                            let summary_mask = (1 << 30) | (1 << 29);
                            self.state.fpscr =
                                (value & !summary_mask) | (self.state.fpscr & summary_mask);
                        } else {
                            let mut mask = 0_u32;
                            for field in 0..8 {
                                if field_mask & (1 << (7 - field)) != 0 {
                                    mask |= 0xF << (28 - field * 4);
                                }
                            }
                            mask &= !((1 << 30) | (1 << 29));
                            self.state.fpscr = (self.state.fpscr & !mask) | (value & mask);
                        }
                        // mtfsf can explicitly write FX, so setting a sticky
                        // cause in the same transfer must not implicitly set it.
                        self.refresh_fpscr_fex();
                        self.queue_fpscr_write_exception(pc, previous_fpscr);
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
                    // Xenia's Xenon execution model zero-extends the
                    // 32-bit FPSCR payload into the 64-bit FPR container.
                    self.state.fpr[frt] = f64::from_bits(self.state.fpscr as u64);
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
                    let (cr_condition, fprf_condition) = if unordered {
                        (0b0001, 0b1_0001)
                    } else if a < b {
                        (0b1000, 0b0_1000)
                    } else if a > b {
                        (0b0100, 0b0_0100)
                    } else {
                        (0b0010, 0b0_0010)
                    };
                    self.state.cr[field] = cr_condition;
                    let signaling_nan = [a, b].iter().any(|value| {
                        let bits = value.to_bits();
                        (bits & 0x7FF0_0000_0000_0000) == 0x7FF0_0000_0000_0000
                            && (bits & 0x000F_FFFF_FFFF_FFFF) != 0
                            && (bits & 0x0008_0000_0000_0000) == 0
                    });
                    let invalid = unordered && (xo10 == 32 || signaling_nan);
                    // Compare instructions update FPCC only; the FPRF
                    // class bit C is preserved for all comparison results.
                    self.set_fprf_condition(fprf_condition);
                    let mut invalid_cause = 0;
                    if signaling_nan {
                        invalid_cause |= 1 << 24; // VXSNAN
                    }
                    if unordered
                        && xo10 == 32
                        && (!signaling_nan || self.state.fpscr & (1 << 7) == 0)
                    {
                        invalid_cause |= 1 << 19; // VXVC
                    }
                    self.update_fpscr_exceptions(
                        FpExceptionFlags {
                            invalid,
                            invalid_cause,
                            ..FpExceptionFlags::default()
                        },
                        false,
                    );
                    self.state.pc = next_pc;
                    return Ok(());
                }

                let source_signaling_nan = match xo10 {
                    846 => false, // fcfid interprets the source register as an integer bit pattern.
                    12 | 14 | 15 | 814 | 815 => Self::is_signaling_nan(source_b),
                    _ => match xo5 {
                        18 => Self::is_signaling_nan(source_a) || Self::is_signaling_nan(source_b),
                        22 | 26 | 24 => Self::is_signaling_nan(source_b),
                        25 => Self::is_signaling_nan(source_a) || Self::is_signaling_nan(source_c),
                        29..=31 => {
                            Self::is_signaling_nan(source_a)
                                || Self::is_signaling_nan(source_b)
                                || Self::is_signaling_nan(source_c)
                        }
                        _ => Self::is_signaling_nan(source_a) || Self::is_signaling_nan(source_b),
                    },
                };

                let mut arithmetic = false;
                let mut update_status = false;
                let mut conversion_invalid = false;
                let mut conversion_inexact = false;
                let mut conversion_fraction_rounded = false;
                let mut frsp_overflow = false;
                let mut frsp_underflow = false;
                let mut frsp_inexact = false;
                let mut frsp_fraction_rounded = false;
                let mut value = match xo10 {
                    264 => b.abs(),  // fabs
                    72 => b,         // fmr
                    136 => -b.abs(), // fnabs
                    40 => -b,        // fneg
                    12 => {
                        update_status = true;
                        frsp_overflow = b.is_finite() && b.abs() > f32::MAX as f64;
                        let tiny = b != 0.0 && b.abs() < f32::MIN_POSITIVE as f64;
                        let overflow_enabled = frsp_overflow && self.state.fpscr & (1 << 6) != 0;
                        let underflow_enabled = tiny && self.state.fpscr & (1 << 5) != 0;
                        let adjusted_value = if overflow_enabled {
                            Self::scale_by_power_of_two(b, -192)
                        } else if underflow_enabled {
                            Self::scale_by_power_of_two(b, 192)
                        } else {
                            b
                        };
                        let adjusted = overflow_enabled || underflow_enabled;
                        let mut rounded = if adjusted {
                            self.round_to_single_significand(adjusted_value)
                        } else {
                            self.round_to_single(b)
                        };
                        let ni_flush = !underflow_enabled
                            && self.state.fpscr & (1 << 2) != 0
                            && rounded != 0.0
                            && rounded.abs() < f32::MIN_POSITIVE as f64;
                        if ni_flush {
                            rounded = if rounded.is_sign_negative() {
                                -0.0
                            } else {
                                0.0
                            };
                        }
                        frsp_inexact = ni_flush
                            || if adjusted {
                                rounded != adjusted_value
                            } else {
                                (b.is_finite() && rounded.is_finite() && rounded != b)
                                    || (frsp_overflow && !overflow_enabled)
                            };
                        frsp_underflow = tiny && (underflow_enabled || frsp_inexact);
                        frsp_fraction_rounded = !frsp_overflow
                            && if adjusted {
                                rounded.abs() > adjusted_value.abs()
                            } else {
                                frsp_inexact && b.is_finite() && rounded.abs() > b.abs()
                            };
                        rounded
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
                        if !rounded.is_finite() || rounded < min || rounded >= max_exclusive {
                            conversion_invalid = true;
                            self.state.fpscr |= 1 << 8; // VXCVI
                            let negative_or_nan = rounded.is_nan() || rounded < min;
                            if xo10 == 14 || xo10 == 15 {
                                let integer = if negative_or_nan { i32::MIN } else { i32::MAX };
                                f64::from_bits(
                                    (self.state.fpr[frt].to_bits() & 0xFFFF_FFFF_0000_0000)
                                        | (integer as u32 as u64),
                                )
                            } else {
                                let integer = if negative_or_nan { i64::MIN } else { i64::MAX };
                                f64::from_bits(integer as u64)
                            }
                        } else {
                            conversion_inexact = rounded != b;
                            conversion_fraction_rounded =
                                conversion_inexact && rounded.abs() > b.abs();
                            if xo10 == 14 || xo10 == 15 {
                                f64::from_bits(
                                    (self.state.fpr[frt].to_bits() & 0xFFFF_FFFF_0000_0000)
                                        | (rounded as i32 as u32 as u64),
                                )
                            } else {
                                f64::from_bits((rounded as i64) as u64)
                            }
                        }
                    }
                    846 => {
                        update_status = true;
                        let converted = self.round_i64_to_f64(b.to_bits() as i64);
                        conversion_inexact = converted.1;
                        conversion_fraction_rounded = converted.2;
                        converted.0
                    } // fcfid
                    _ => {
                        arithmetic = true;
                        update_status = xo5 != 23;
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
                            24 => 1.0 / b, // fres: binary64 source, binary32 estimate result
                            26 => 1.0 / b.sqrt(), // frsqrte
                            29 => a.mul_add(c, b), // fmadd
                            28 => a.mul_add(c, -b), // fmsub
                            31 => -a.mul_add(c, b), // fnmadd
                            30 => -a.mul_add(c, -b), // fnmsub
                            _ => return Err(PpcError::UnsupportedInstruction { pc, opcode }),
                        }
                    }
                };

                let mut double_inexact = false;
                let mut double_fraction_rounded = false;
                let mut double_overflow = false;
                let mut double_underflow = false;
                if arithmetic && !single && update_status && !matches!(xo5, 23 | 24 | 26) {
                    let rounded = self.round_binary64_result(xo5, a, b, c, value);
                    value = rounded.0;
                    double_inexact = rounded.1;
                    double_fraction_rounded = rounded.2;
                    double_overflow = rounded.3;
                    if double_overflow && self.state.fpscr & (1 << 6) != 0 {
                        let adjusted = self.round_double_overflow_enabled(xo5, a, b, c);
                        value = adjusted.0;
                        double_inexact = adjusted.1;
                        double_fraction_rounded = adjusted.2;
                    }
                    let double_tiny = value.is_subnormal()
                        || (value == 0.0 && double_inexact)
                        || (value.abs() == f64::MIN_POSITIVE && double_fraction_rounded);
                    let underflow_enabled = self.state.fpscr & (1 << 5) != 0;
                    if !underflow_enabled
                        && self.state.fpscr & (1 << 2) != 0
                        && value.is_subnormal()
                    {
                        value = if value.is_sign_negative() { -0.0 } else { 0.0 };
                        double_inexact = true;
                        double_fraction_rounded = false;
                    }
                    double_underflow = double_tiny && (underflow_enabled || double_inexact);
                    if double_underflow && underflow_enabled {
                        let adjusted = self.round_double_underflow_enabled(xo5, a, b, c, value);
                        value = adjusted.0;
                        if matches!(xo5, 18 | 25) || (matches!(xo5, 28..=31) && b == 0.0) {
                            double_inexact = adjusted.1;
                            double_fraction_rounded = adjusted.2;
                        }
                    }
                }

                if xo5 == 23 && !update_status {
                    // fsel updates FPRF regardless of the record bit.
                    self.set_fprf_for_value(value);
                } else if !arithmetic && !update_status && record {
                    // Unary move/sign instructions update FPRF only in their
                    // record form; they never raise arithmetic exceptions.
                    self.set_fprf_for_value(value);
                }

                let operand_storage = match xo5 {
                    18 | 20 | 21 => [a, b, 0.0],
                    25 => [a, c, 0.0],
                    22 | 24 | 26 => [b, 0.0, 0.0],
                    28..=31 => [a, b, c],
                    _ => [a, b, c],
                };
                let operand_count = match xo5 {
                    22 | 24 | 26 => 1,
                    28..=31 => 3,
                    _ => 2,
                };
                let operands = &operand_storage[..operand_count];

                if arithmetic && single {
                    // Single-precision arithmetic rounds the stored result,
                    // not the source operands, to IEEE-754 binary32.
                    let divide_by_zero = match xo5 {
                        18 => {
                            operands.len() >= 2
                                && operands[1] == 0.0
                                && operands[0] != 0.0
                                && operands[0].is_finite()
                        }
                        24 | 26 => operands.first().is_some_and(|operand| *operand == 0.0),
                        _ => false,
                    };
                    let overflow = !divide_by_zero
                        && operands.iter().all(|operand| operand.is_finite())
                        && (value.is_infinite()
                            || (value.is_finite() && value.abs() > f32::MAX as f64));
                    let tiny = value != 0.0 && value.abs() < f32::MIN_POSITIVE as f64;
                    let overflow_enabled = overflow && self.state.fpscr & (1 << 6) != 0;
                    let underflow_enabled = tiny && self.state.fpscr & (1 << 5) != 0;
                    let adjusted_value = if overflow_enabled {
                        if xo5 == 24 && value.is_infinite() && b.is_finite() && b != 0.0 {
                            1.0 / Self::scale_by_power_of_two(b, 192)
                        } else {
                            Self::scale_by_power_of_two(value, -192)
                        }
                    } else if underflow_enabled {
                        Self::scale_by_power_of_two(value, 192)
                    } else {
                        value
                    };
                    let adjusted_exponent = overflow_enabled || underflow_enabled;
                    let mut rounded = if adjusted_exponent {
                        self.round_to_single_significand(adjusted_value)
                    } else {
                        self.round_to_single(value)
                    };
                    let ni_flush = !underflow_enabled
                        && self.state.fpscr & (1 << 2) != 0
                        && rounded != 0.0
                        && rounded.abs() < f32::MIN_POSITIVE as f64;
                    if ni_flush {
                        rounded = if rounded.is_sign_negative() {
                            -0.0
                        } else {
                            0.0
                        };
                    }
                    let previous_fpr = self.state.fpr[frt];
                    self.state.fpr[frt] = rounded;
                    if update_status {
                        let inexact = ni_flush
                            || if adjusted_exponent {
                                rounded != adjusted_value
                            } else {
                                (value.is_finite() && rounded.is_finite() && rounded != value)
                                    || (overflow && !overflow_enabled)
                            };
                        let fraction_rounded = if overflow {
                            false
                        } else if adjusted_exponent {
                            rounded.abs() > adjusted_value.abs()
                        } else {
                            (value.is_finite() && rounded.abs() > value.abs())
                                || (value.is_infinite()
                                    && rounded.is_infinite()
                                    && !divide_by_zero
                                    && operands.iter().all(|operand| operand.is_finite()))
                        };
                        let underflow = tiny && (underflow_enabled || inexact);
                        let invalid_enabled = self.update_fpscr_for_result(
                            rounded,
                            operands,
                            source_signaling_nan,
                            xo5,
                            inexact,
                            fraction_rounded,
                            overflow,
                            underflow,
                            true,
                            record,
                        );
                        if invalid_enabled {
                            self.state.fpr[frt] = previous_fpr;
                        }
                    } else if record {
                        self.copy_fpscr_to_cr1();
                    }
                } else {
                    let previous_fpr = self.state.fpr[frt];
                    self.state.fpr[frt] = value;
                    if update_status {
                        let operands: &[f64] = match xo10 {
                            12 | 14 | 15 | 814 | 815 | 846 => &[b],
                            _ => match xo5 {
                                18 => &[a, b],
                                22 => &[b],
                                24 | 26 => &[1.0, b],
                                25 => &[a, c],
                                29..=31 => &[a, b, c],
                                _ => &[a, b],
                            },
                        };
                        let inexact = if arithmetic {
                            double_inexact
                        } else if matches!(xo10, 14 | 15 | 814 | 815 | 846) {
                            conversion_inexact
                        } else {
                            frsp_inexact
                        };
                        if matches!(xo10, 14 | 15 | 814 | 815) {
                            self.update_fpscr_exceptions(
                                FpExceptionFlags {
                                    invalid: conversion_invalid,
                                    inexact,
                                    raise_inexact: inexact,
                                    fraction_rounded: conversion_fraction_rounded,
                                    update_rounding: true,
                                    invalid_cause: (if conversion_invalid { 1 << 8 } else { 0 })
                                        | (if source_signaling_nan { 1 << 24 } else { 0 }),
                                    ..FpExceptionFlags::default()
                                },
                                record,
                            );
                            if conversion_invalid && self.state.fpscr & (1 << 7) != 0 {
                                self.state.fpr[frt] = previous_fpr;
                            }
                        } else {
                            let invalid_enabled = self.update_fpscr_for_result(
                                value,
                                operands,
                                source_signaling_nan,
                                xo5,
                                inexact,
                                if arithmetic {
                                    double_fraction_rounded
                                } else if xo10 == 846 {
                                    conversion_fraction_rounded
                                } else {
                                    frsp_fraction_rounded
                                },
                                double_overflow || frsp_overflow,
                                double_underflow || frsp_underflow,
                                xo10 == 12,
                                record,
                            );
                            let reciprocal_zero_divide_enabled = matches!(xo5, 24 | 26)
                                && b == 0.0
                                && self.state.fpscr & (1 << 4) != 0;
                            if invalid_enabled || reciprocal_zero_divide_enabled {
                                self.state.fpr[frt] = previous_fpr;
                            }
                        }
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
    use super::{
        PPC_FP_UNAVAILABLE_VECTOR, PPC_MSR_FE0, PPC_MSR_FE1, PPC_MSR_FP,
        PPC_PROGRAM_EXCEPTION_VECTOR, PPC_SRR1_FP_ENABLED_EXCEPTION, PPC_SRR1_IMPRECISE, PpcError,
        PpcException, PpcFpExceptionMode, PpcInterpreter,
    };

    fn fp_enabled_cpu() -> PpcInterpreter {
        let mut cpu = PpcInterpreter::default();
        cpu.state.msr |= PPC_MSR_FP;
        cpu
    }

    use crate::address_space::{GuestAddressSpace, MemoryPermissions};
    use crate::memory::{GuestAddress, GuestMemory, MemoryError};

    fn put_instruction(memory: &mut GuestMemory, pc: u32, instruction: u32) {
        memory.write_u32_be(GuestAddress(pc), instruction).unwrap();
    }

    #[test]
    fn vmx128_register_decode_reconstructs_split_extension_bits() {
        // Encode VD=0x7D, VA=0x72, VB=0x43 using the upstream VX128 layout.
        let instruction = (0x1D_u32 << 21)
            | (0x12_u32 << 16)
            | (0x03_u32 << 11)
            | (0x3_u32 << 2)
            | (0x1_u32 << 5)
            | (0x1_u32 << 10)
            | 0x2_u32;
        let decoded = super::decode_vmx128_register_fields(instruction);

        assert_eq!(decoded.vd, 0x7D);
        assert_eq!(decoded.va, 0x72);
        assert_eq!(decoded.vb, 0x43);
    }

    #[test]
    fn vmx128_register_decode_reaches_all_128_register_indices() {
        let instruction = (0x1F_u32 << 21)
            | (0x1F_u32 << 16)
            | (0x1F_u32 << 11)
            | (0x3_u32 << 2)
            | (0x1_u32 << 5)
            | (0x1_u32 << 10)
            | 0x3_u32;
        let decoded = super::decode_vmx128_register_fields(instruction);

        assert_eq!(decoded.vd, 0x7F);
        assert_eq!(decoded.va, 0x7F);
        assert_eq!(decoded.vb, 0x7F);
    }

    #[test]
    fn vmx_wrapping_add_and_subtract_cover_byte_halfword_and_word_lanes() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 24);
        let encode_vmx = |vd: u32, va: u32, vb: u32, xo: u32| {
            (4 << 26) | (vd << 21) | (va << 16) | (vb << 11) | xo
        };
        // vaddubm v3,v1,v2; vadduhm v4,v1,v2; vadduwm v5,v1,v2
        // vsububm v6,v1,v2; vsubuhm v7,v1,v2; vsubuwm v8,v1,v2
        for (index, (vd, xo)) in [
            (3, 0x000),
            (4, 0x040),
            (5, 0x080),
            (6, 0x400),
            (7, 0x440),
            (8, 0x480),
        ]
        .into_iter()
        .enumerate()
        {
            put_instruction(
                &mut memory,
                0x1000 + (index as u32 * 4),
                encode_vmx(vd, 1, 2, xo),
            );
        }

        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.vr[1] = [
            0xFF, 0x00, 0x7F, 0x80, 0x01, 0x02, 0xFE, 0xFF, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60,
            0x70, 0x80,
        ];
        cpu.state.vr[2] = [
            0x01, 0xFF, 0x01, 0x80, 0xFF, 0xFE, 0x02, 0x01, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
            0x07, 0x08,
        ];

        for _ in 0..6 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(
            cpu.state.vr[3],
            [
                0x00, 0xFF, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
                0x77, 0x88,
            ]
        );
        assert_eq!(
            cpu.state.vr[4],
            [
                0x00, 0xFF, 0x81, 0x00, 0x01, 0x00, 0x01, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
                0x77, 0x88,
            ]
        );
        assert_eq!(
            cpu.state.vr[5],
            [
                0x00, 0xFF, 0x81, 0x00, 0x01, 0x01, 0x01, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
                0x77, 0x88,
            ]
        );
        assert_eq!(
            cpu.state.vr[6],
            [
                0xFE, 0x01, 0x7E, 0x00, 0x02, 0x04, 0xFC, 0xFE, 0x0F, 0x1E, 0x2D, 0x3C, 0x4B, 0x5A,
                0x69, 0x78,
            ]
        );
        assert_eq!(
            cpu.state.vr[7],
            [
                0xFD, 0x01, 0x7E, 0x00, 0x01, 0x04, 0xFC, 0xFE, 0x0F, 0x1E, 0x2D, 0x3C, 0x4B, 0x5A,
                0x69, 0x78,
            ]
        );
        assert_eq!(
            cpu.state.vr[8],
            [
                0xFD, 0x01, 0x7E, 0x00, 0x01, 0x04, 0xFC, 0xFE, 0x0F, 0x1E, 0x2D, 0x3C, 0x4B, 0x5A,
                0x69, 0x78,
            ]
        );
        assert_eq!(cpu.state.pc, 0x1018);
    }

    #[test]
    fn vmx_logical_operations_preserve_guest_byte_order() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 20);
        let encode_vmx = |vd: u32, va: u32, vb: u32, xo: u32| {
            (4 << 26) | (vd << 21) | (va << 16) | (vb << 11) | xo
        };
        // vand, vandc, vor, vxor and vnor
        for (index, (vd, xo)) in [(3, 0x404), (4, 0x444), (5, 0x484), (6, 0x4C4), (7, 0x504)]
            .into_iter()
            .enumerate()
        {
            put_instruction(
                &mut memory,
                0x1000 + (index as u32 * 4),
                encode_vmx(vd, 1, 2, xo),
            );
        }

        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.vr[1] = [
            0xF0, 0xCC, 0xAA, 0x55, 0x0F, 0x33, 0xFF, 0x00, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC,
            0xDE, 0xF0,
        ];
        cpu.state.vr[2] = [
            0x0F, 0x0F, 0x55, 0xAA, 0xF0, 0x33, 0x0F, 0xFF, 0x21, 0x43, 0x65, 0x87, 0xA9, 0xCB,
            0xED, 0x0F,
        ];

        for _ in 0..5 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(
            cpu.state.vr[3],
            [
                0x00, 0x0C, 0x00, 0x00, 0x00, 0x33, 0x0F, 0x00, 0x00, 0x00, 0x44, 0x00, 0x88, 0x88,
                0xCC, 0x00
            ]
        );
        assert_eq!(
            cpu.state.vr[4],
            [
                0xF0, 0xC0, 0xAA, 0x55, 0x0F, 0x00, 0xF0, 0x00, 0x12, 0x34, 0x12, 0x78, 0x12, 0x34,
                0x12, 0xF0
            ]
        );
        assert_eq!(
            cpu.state.vr[5],
            [
                0xFF, 0xCF, 0xFF, 0xFF, 0xFF, 0x33, 0xFF, 0xFF, 0x33, 0x77, 0x77, 0xFF, 0xBB, 0xFF,
                0xFF, 0xFF
            ]
        );
        assert_eq!(
            cpu.state.vr[6],
            [
                0xFF, 0xC3, 0xFF, 0xFF, 0xFF, 0x00, 0xF0, 0xFF, 0x33, 0x77, 0x33, 0xFF, 0x33, 0x77,
                0x33, 0xFF
            ]
        );
        assert_eq!(
            cpu.state.vr[7],
            [
                0x00, 0x30, 0x00, 0x00, 0x00, 0xCC, 0x00, 0x00, 0xCC, 0x88, 0x88, 0x00, 0x44, 0x00,
                0x00, 0x00
            ]
        );
        assert_eq!(cpu.state.pc, 0x1014);
    }

    #[test]
    fn addi_uses_zero_when_ra_is_zero_and_sign_extends_immediate() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        // addi r3, r0, -1
        put_instruction(&mut memory, 0x1000, (14 << 26) | (3 << 21) | 0xFFFF);
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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

        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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

        let mut cpu = fp_enabled_cpu();
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

        let mut cpu = fp_enabled_cpu();
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

        let mut cpu = fp_enabled_cpu();
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

        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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

        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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

        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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

        let mut cpu = fp_enabled_cpu();
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
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000); // FPRF: greater
    }

    #[test]
    fn frsp_classifies_single_precision_denormals_in_fprf() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // frsp f1,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (12 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f32::from_bits(1) as f64;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], f32::from_bits(1) as f64);
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0001_4000); // positive single denormal
    }

    #[test]
    fn stfs_uses_fpscr_rounding_without_changing_fpscr() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // stfs f1,4(r3)
        put_instruction(&mut memory, 0x1000, (52 << 26) | (1 << 21) | (3 << 16) | 4);
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1000;
        cpu.state.fpr[1] = 1.0 + 2.0_f64.powi(-24);
        cpu.state.fpscr = 0b10; // toward +infinity

        cpu.step(&mut memory).unwrap();

        assert_eq!(
            memory.read_u32_be(GuestAddress(0x1004)).unwrap(),
            f32::from_bits(0x3F80_0001).to_bits()
        );
        assert_eq!(cpu.state.fpscr, 0b10); // stores do not update FPSCR
    }

    #[test]
    fn stfsx_uses_fpscr_rounding_for_indexed_stores() {
        let mut memory = GuestMemory::new(GuestAddress(0), 0x2000);
        // stfsx f1,r0,r2
        put_instruction(
            &mut memory,
            0x1000,
            (31 << 26) | (1 << 21) | (2 << 11) | (663 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[2] = 0x1004;
        cpu.state.fpr[1] = 1.0 + 2.0_f64.powi(-24);
        cpu.state.fpscr = 0b10;

        cpu.step(&mut memory).unwrap();

        assert_eq!(
            memory.read_u32_be(GuestAddress(0x1004)).unwrap(),
            f32::from_bits(0x3F80_0001).to_bits()
        );
        assert_eq!(cpu.state.fpscr, 0b10);
    }

    #[test]
    fn single_precision_load_and_store_preserve_signaling_nan_payloads() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        // lfs f1,4(r3); stfs f1,12(r3)
        put_instruction(&mut memory, 0x1000, (48 << 26) | (1 << 21) | (3 << 16) | 4);
        put_instruction(&mut memory, 0x1008, (52 << 26) | (1 << 21) | (3 << 16) | 12);
        memory
            .write_u32_be(GuestAddress(0x1004), 0x7F80_0001)
            .unwrap();
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0x1000;

        cpu.step(&mut memory).unwrap();
        assert!(PpcInterpreter::is_signaling_nan(cpu.state.fpr[1]));
        cpu.state.pc = 0x1008;
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            memory.read_u32_be(GuestAddress(0x100C)).unwrap(),
            0x7F80_0001
        );
    }

    #[test]
    fn single_precision_arithmetic_rounds_operands_before_execution() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadds f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 1.0 + 2.0_f64.powi(-24);
        cpu.state.fpr[3] = 2.0_f64.powi(-24);

        cpu.step(&mut memory).unwrap();

        // FRA rounds to 1.0f before adding FRB; the exact single-precision
        // result is therefore the halfway value, which rounds to 1.0f.
        assert_eq!(cpu.state.fpr[1].to_bits(), 1.0_f64.to_bits());
    }

    #[test]
    fn single_precision_nan_result_does_not_raise_inexact() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadds f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::NAN;
        cpu.state.fpr[3] = 1.0;

        cpu.step(&mut memory).unwrap();

        assert!(cpu.state.fpr[1].is_nan());
        assert_eq!(cpu.state.fpscr & ((1 << 17) | (1 << 25)), 0);

        // A signaling NaN can be quieted by the binary32 operand conversion,
        // but VXSNAN must still be raised from the original FPR bit pattern.
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0;
        cpu.state.fpr[2] = f64::from_bits(0x7FF0_0000_0000_0001);
        cpu.step(&mut memory).unwrap();
        assert_ne!(cpu.state.fpscr & (1 << 24), 0); // VXSNAN
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX summary
        assert_eq!(cpu.state.fpscr & ((1 << 17) | (1 << 25)), 0);
    }

    #[test]
    fn double_precision_arithmetic_updates_inexact_and_obeys_rounding_mode() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadd f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 1.0;
        cpu.state.fpr[3] = 2.0_f64.powi(-53);

        // Nearest-even retains 1.0, but the discarded addend is inexact.
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], 1.0);
        assert_ne!(cpu.state.fpscr & (1 << 17), 0); // FI
        assert_ne!(cpu.state.fpscr & (1 << 25), 0); // XX
        assert_eq!(cpu.state.fpscr & (1 << 18), 0); // FR: rounded down

        // Toward +infinity must advance to the next representable binary64.
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0b10;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1].to_bits(), 1.0_f64.to_bits() + 1);
        assert_ne!(cpu.state.fpscr & (1 << 18), 0); // FR: rounded up
        assert_ne!(cpu.state.fpscr & (1 << 17), 0); // FI
    }

    #[test]
    fn floating_point_move_record_form_updates_fprf_without_exceptions() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fabs. f1,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (264 << 1) | 1,
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = -2.5;
        cpu.state.fpscr = 0xA000_0000;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], 2.5);
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000); // positive normal
        assert_eq!(cpu.state.fpscr & 0xE600_0000, 0xA000_0000); // exception state unchanged
        assert_eq!(cpu.state.cr[1], 0x0A); // CR1 summary copied from FPSCR

        // Signaling NaNs remain signaling through a sign-only move, so their
        // FPRF class is the architectural signaling-NaN class (0x00).
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::from_bits(0x7FF0_0000_0000_0001);
        cpu.state.fpscr = 0x0000_4000;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0);
        assert_eq!(cpu.state.fpscr & 0xE600_0000, 0);
    }

    #[test]
    fn floating_point_select_updates_fprf_without_raising_exceptions() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fsel. f1,f2,f3,f4
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (4 << 6) | (23 << 1) | 1,
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::NAN;
        cpu.state.fpr[3] = -0.0;
        cpu.state.fpr[4] = 3.0;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1].to_bits(), (-0.0_f64).to_bits());
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0001_2000); // negative zero
        assert_eq!(cpu.state.fpscr & 0xE000_0000, 0); // no FX/FEX/VX

        // The single-precision fsel. form also copies FPSCR[0:3] to CR1.
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (4 << 6) | (23 << 1) | 1,
        );
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0xF000_0280; // Seed FX/FEX/VX/OX for the CR1 copy check
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.cr[1], 0x0F); // FX, FEX, VX and OX
        assert_eq!(cpu.state.fpr[1].to_bits(), (-0.0_f64).to_bits());
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
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 1.0;
        cpu.state.fpr[3] = 2.0;
        cpu.state.fpscr = 0xB000_0040; // OX + OE keep FEX asserted without VX

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], 3.0);
        assert_eq!(cpu.state.cr[1], 0x0D);
    }

    #[test]
    fn mffs_and_mtfsf_transfer_the_raw_fpscr_word() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // mffs f1; mtfsf 0xFF,f1
        put_instruction(&mut memory, 0x1000, (63 << 26) | (1 << 21) | (583 << 1));
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (0xFF << 17) | (1 << 11) | (711 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0xA123_4567;

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1].to_bits(), 0x0000_0000_A123_4567);
        cpu.state.fpscr = 0;

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpscr, 0xA123_4567);
    }

    #[test]
    fn fcfid_converts_raw_signed_integer_with_fpscr_rounding() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fcfid f1,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (846 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::from_bits(9_007_199_254_740_993);

        cpu.state.fpscr = 0;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], 9_007_199_254_740_992.0);
        assert_ne!(cpu.state.fpscr & (1 << 17), 0); // FI
        assert_eq!(cpu.state.fpscr & (1 << 18), 0); // rounded toward zero

        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0b10; // toward +infinity
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], 9_007_199_254_740_994.0);
        assert_ne!(cpu.state.fpscr & (1 << 18), 0); // FR
        assert_ne!(cpu.state.fpscr & (1 << 17), 0); // FI
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

        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        for _ in 0..4 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.cr[3], 0x8); // FX and OX; VX summary is derived, not writable
        assert_eq!(cpu.state.cr[4], 0x5);
        assert_eq!(cpu.state.fpscr & 0xF000_0000, 0);
        assert_eq!(cpu.state.fpscr & 0xF, 0x5);
    }

    #[test]
    fn mcrfs_clears_only_exception_bits_and_recomputes_summaries() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 28);
        let encode_mcrfs =
            |crfd: u32, crfs: u32| (63 << 26) | (crfd << 23) | (crfs << 18) | (64 << 1);
        let encode_mtfsb = |bit: u32, xo: u32| (63 << 26) | (bit << 21) | (xo << 1);
        put_instruction(&mut memory, 0x1000, encode_mcrfs(0, 0));
        put_instruction(&mut memory, 0x1004, encode_mtfsb(1, 70)); // cannot clear FEX
        put_instruction(&mut memory, 0x1008, encode_mtfsb(2, 70)); // cannot clear VX
        put_instruction(&mut memory, 0x100C, encode_mcrfs(1, 1));
        put_instruction(&mut memory, 0x1010, encode_mtfsb(1, 38)); // cannot set FEX
        put_instruction(&mut memory, 0x1014, encode_mtfsb(2, 38)); // cannot set VX
        put_instruction(&mut memory, 0x1018, encode_mcrfs(2, 3));

        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = (1 << 31)
            | (1 << 30)
            | (1 << 29)
            | (1 << 28)
            | (1 << 27)
            | (1 << 26)
            | (1 << 25)
            | (1 << 24)
            | (1 << 19)
            | (1 << 18)
            | (1 << 17)
            | 0x0001_4000
            | (1 << 7); // VE enables FEX while VXSNAN and VXVC are pending.

        for _ in 0..7 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.cr[0], 0x0F); // original FX/FEX/VX/OX
        assert_eq!(cpu.state.cr[1], 0x0F); // original UX/ZX/XX/VXSNAN
        assert_eq!(cpu.state.cr[2], 0x0F); // VXVC, FR, FI and C copied
        assert_eq!(cpu.state.fpscr & ((1 << 30) | (1 << 29)), 0); // summaries recomputed
        assert_eq!(cpu.state.fpscr & ((1 << 18) | (1 << 17)), 0); // FR/FI cleared
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0001_4000); // C and FPCC preserved
    }

    #[test]
    fn mcrfs_field_five_clears_all_low_invalid_causes() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mcrfs cr0, FPSCR field 5
        put_instruction(&mut memory, 0x1000, (63 << 26) | (5 << 18) | (64 << 1));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = (1 << 10) | (1 << 9) | (1 << 8); // VXSOFT, VXSQRT, VXCVI

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.cr[0], 0x7);
        assert_eq!(cpu.state.fpscr & 0x0000_0700, 0);
        assert_eq!(cpu.state.fpscr & (1 << 29), 0); // VX summary
    }

    #[test]
    fn mtfsb1_sets_fx_when_it_raises_an_invalid_cause() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtfsb1 VXSQRT (FPSCR bit 22 in PowerPC numbering)
        put_instruction(&mut memory, 0x1000, (63 << 26) | (22 << 21) | (38 << 1));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;

        cpu.step(&mut memory).unwrap();

        assert_ne!(cpu.state.fpscr & (1 << 9), 0); // VXSQRT
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX summary
        assert_ne!(cpu.state.fpscr & (1 << 31), 0); // FX
    }

    #[test]
    fn mtfsfi_sets_exception_bits_without_implicitly_setting_fx() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtfsfi field 5, immediate 2 (VXSQRT)
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (5 << 23) | (2 << 12) | (134 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;

        cpu.step(&mut memory).unwrap();

        assert_ne!(cpu.state.fpscr & (1 << 9), 0); // VXSQRT
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX summary
        assert_eq!(cpu.state.fpscr & (1 << 31), 0); // FX is explicitly controlled
    }

    #[test]
    fn fpscr_vx_summary_excludes_fraction_and_result_flags() {
        let mut cpu = fp_enabled_cpu();
        cpu.state.fpscr = (1 << 18) | (1 << 17) | 0x0001_F000;
        cpu.refresh_fpscr_fex();
        assert_eq!(cpu.state.fpscr & (1 << 29), 0);

        cpu.state.fpscr |= 1 << 9; // VXSQRT
        cpu.refresh_fpscr_fex();
        assert_ne!(cpu.state.fpscr & (1 << 29), 0);
    }

    #[test]
    fn fpscr_bit_and_masked_field_moves_preserve_unselected_bits() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 12);
        let encode_mtfsb = |bit: u32, xo: u32| (63 << 26) | (bit << 21) | (xo << 1);
        let encode_mtfsf = |fm: u32, rb: u32| (63 << 26) | (fm << 17) | (rb << 11) | (711 << 1);
        put_instruction(&mut memory, 0x1000, encode_mtfsb(0, 38)); // set FPSCR[0]
        put_instruction(&mut memory, 0x1004, encode_mtfsb(1, 70)); // clear FPSCR[1]
        put_instruction(&mut memory, 0x1008, encode_mtfsf(0x01, 2)); // replace FPSCR field 7
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0x4000_0000;
        cpu.state.fpr[2] = f64::from_bits(0x5);

        for _ in 0..3 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.fpscr & 0xF000_000F, 0x8000_0005);
    }

    #[test]
    fn floating_point_add_with_zero_does_not_raise_divide_by_zero() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fadd f1,f2,f3; fdiv f4,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (4 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 1.0;
        cpu.state.fpr[3] = 0.0;

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], 1.0);
        assert_eq!(cpu.state.fpscr & (1 << 26), 0); // no ZX for addition

        cpu.step(&mut memory).unwrap();
        assert!(cpu.state.fpr[4].is_infinite());
        assert_ne!(cpu.state.fpscr & (1 << 26), 0); // ZX for division
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
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = 10.0;
        cpu.state.fpr[2] = 1.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 1 << 4; // ZE enable
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP; // precise FP exception mode

        cpu.step(&mut memory).unwrap();

        assert_eq!(
            cpu.take_pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: 1 << 26,
                mode: PpcFpExceptionMode::Precise,
            })
        );
        assert_eq!(cpu.state.fpr[1], 10.0); // enabled exception suppresses destination write
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
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = f64::NAN;
        cpu.state.fpr[2] = 1.0;
        // Comparisons update FPRF and invalid causes, but preserve the
        // rounding status left by the preceding arithmetic instruction.
        cpu.state.fpscr = (1 << 18) | (1 << 17);

        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.fpscr & ((1 << 18) | (1 << 17)),
            (1 << 18) | (1 << 17)
        );
        assert_eq!(cpu.state.cr[2], 0b0001);
        assert_eq!(cpu.state.fpscr & (1 << 29), 0);
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_1000); // unordered FPCC; C remains clear
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.cr[3], 0b0001);
        assert_ne!(cpu.state.fpscr & (1 << 29), 0);
    }

    #[test]
    fn frsp_obeys_all_fpscr_rounding_directions() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // frsp f1,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (12 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 1.0 + 2.0_f64.powi(-24);

        // Round to nearest, ties to even.
        cpu.state.fpscr = 0;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1].to_bits(), 1.0_f64.to_bits());

        // Round toward +infinity.
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0b10;
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.fpr[1].to_bits(),
            (f32::from_bits(0x3F80_0001) as f64).to_bits()
        );

        // Round a negative halfway value toward -infinity.
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = -1.0 - 2.0_f64.powi(-24);
        cpu.state.fpscr = 0b11;
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.fpr[1].to_bits(),
            (f32::from_bits(0xBF80_0001) as f64).to_bits()
        );
        assert_ne!(cpu.state.fpscr & (1 << 18), 0); // FR: rounded away from zero.

        // A finite binary64 value that overflows binary32 must still obey
        // directed rounding instead of unconditionally becoming infinity.
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::MAX;
        cpu.state.fpscr = 0b01; // toward zero
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], f32::MAX as f64);
        assert_ne!(cpu.state.fpscr & (1 << 28), 0); // OX despite finite directed result
        assert_ne!(cpu.state.fpscr & (1 << 25), 0); // XX

        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0b10; // toward +infinity
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], f64::INFINITY);
        assert_eq!(cpu.state.fpscr & (1 << 18), 0); // FR is undefined on disabled overflow; model as zero

        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = -f64::MAX;
        cpu.state.fpscr = 0b10; // toward +infinity, negative magnitude decreases
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], -(f32::MAX as f64));
    }

    #[test]
    fn enabled_zero_divide_preserves_destination_and_fprf() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fdiv. f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1) | 1,
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = 99.0;
        cpu.state.fpr[2] = 1.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = (1 << 4) | (1 << 18) | (1 << 17) | 0x0000_4000; // ZE, FR, FI, FPRF

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], 99.0);
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000); // FPRF unchanged
        assert_eq!(cpu.state.fpscr & ((1 << 18) | (1 << 17)), 0); // FR/FI cleared
        assert_ne!(cpu.state.fpscr & (1 << 26), 0); // ZX
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FEX
        assert_ne!(cpu.state.cr[1], 0); // CR1 summary
    }

    #[test]
    fn reciprocal_estimates_honor_zero_divide_enable() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fres f1,f2; frsqrte f3,f4
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 11) | (24 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (3 << 21) | (4 << 11) | (26 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = 10.0;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 20.0;
        cpu.state.fpr[4] = -0.0;
        cpu.state.fpscr = (1 << 4) | (1 << 18) | (1 << 17) | 0x0000_4000; // ZE + status

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], 10.0);
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000);
        assert_eq!(cpu.state.fpscr & ((1 << 18) | (1 << 17)), 0);
        assert_ne!(cpu.state.fpscr & (1 << 26), 0); // ZX

        cpu.state.pc = 0x1004;
        cpu.state.fpscr = (1 << 4) | (1 << 18) | (1 << 17) | 0x0000_4000;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[3], 20.0);
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000);
        assert_eq!(cpu.state.fpscr & ((1 << 18) | (1 << 17)), 0);
        assert_ne!(cpu.state.fpscr & (1 << 26), 0); // ZX
    }

    #[test]
    fn frsqrte_preserves_rounding_flags_for_nonexceptional_results() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // frsqrte f1,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (26 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 4.0;
        cpu.state.fpscr = (1 << 18) | (1 << 17);

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], 0.5);
        assert_eq!(
            cpu.state.fpscr & ((1 << 18) | (1 << 17)),
            (1 << 18) | (1 << 17)
        );
        assert_eq!(cpu.state.fpscr & (1 << 26), 0); // no ZX
    }

    #[test]
    fn reciprocal_estimate_instructions_are_restricted_to_architectural_opcodes() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 12);
        // fres is opcode 59; frsqrte is opcode 63; fsel has no single form.
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 11) | (26 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (1 << 21) | (2 << 11) | (24 << 1),
        );
        put_instruction(
            &mut memory,
            0x1008,
            (59 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (23 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        assert!(matches!(
            cpu.step(&mut memory),
            Err(PpcError::UnsupportedInstruction { .. })
        ));
        cpu.state.pc = 0x1004;
        assert!(matches!(
            cpu.step(&mut memory),
            Err(PpcError::UnsupportedInstruction { .. })
        ));
        cpu.state.pc = 0x1008;
        assert!(matches!(
            cpu.step(&mut memory),
            Err(PpcError::UnsupportedInstruction { .. })
        ));
    }

    #[test]
    fn fres_uses_binary64_source_and_rounds_estimate_to_single() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fres f1,f2
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 11) | (24 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 1.0 + 2.0_f64.powi(-24);

        cpu.step(&mut memory).unwrap();

        let expected = (1.0 / cpu.state.fpr[2]) as f32 as f64;
        assert_eq!(cpu.state.fpr[1], expected);
        assert_ne!(cpu.state.fpr[1], 1.0);
        assert_ne!(cpu.state.fpscr & (1 << 17), 0); // FI
        assert_ne!(cpu.state.fpscr & (1 << 25), 0); // XX is sticky FI
    }

    #[test]
    fn non_ieee_mode_flushes_single_denormal_results_to_signed_zero() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadds f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = -(f32::from_bits(1) as f64);
        cpu.state.fpr[3] = -0.0;
        cpu.state.fpscr = 1 << 2; // NI

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1].to_bits(), (-0.0_f64).to_bits());
        assert_ne!(cpu.state.fpscr & (1 << 27), 0); // UX
        assert_ne!(cpu.state.fpscr & (1 << 25), 0); // XX
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0001_2000); // negative zero
    }

    #[test]
    fn non_ieee_mode_flushes_double_denormal_results_to_signed_zero() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fmul f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 6) | (25 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::from_bits(1);
        cpu.state.fpr[3] = -1.0;
        cpu.state.fpscr = 1 << 2; // NI

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1].to_bits(), (-0.0_f64).to_bits());
        assert_ne!(cpu.state.fpscr & (1 << 27), 0); // UX
        assert_ne!(cpu.state.fpscr & (1 << 25), 0); // XX
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0001_2000); // negative zero
    }

    #[test]
    fn enabled_single_overflow_adjusts_exponent_and_reports_normal_fprf() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadds f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f32::MAX as f64;
        cpu.state.fpr[3] = f32::MAX as f64;
        cpu.state.fpscr = 1 << 6; // OE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP; // precise FP exception mode

        cpu.step(&mut memory).unwrap();

        assert_eq!(
            cpu.take_pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: 1 << 28,
                mode: PpcFpExceptionMode::Precise,
            })
        );
        assert!(cpu.state.fpr[1].is_finite());
        assert!(cpu.state.fpr[1] > 0.0);
        assert_ne!(cpu.state.fpscr & (1 << 28), 0); // OX
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FEX
        assert_eq!(cpu.state.fpscr & ((1 << 18) | (1 << 17)), 0); // FR/FI cleared
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000); // positive normal
    }

    #[test]
    fn enabled_single_underflow_adjusts_exponent_and_reports_normal_fprf() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadds f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f32::from_bits(1) as f64;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = (1 << 5) | (1 << 2); // UE + NI: enabled underflow keeps adjusted result

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], 2.0_f64.powi(43));
        assert_ne!(cpu.state.fpscr & (1 << 27), 0); // UX
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FEX
        assert_eq!(cpu.state.fpscr & ((1 << 18) | (1 << 17)), 0);
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000); // positive normal
    }

    #[test]
    fn enabled_double_overflow_and_underflow_adjust_exponents() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fadd f1,f2,f3; fmul f4,f5,f6
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (4 << 21) | (5 << 16) | (6 << 6) | (25 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::MAX;
        cpu.state.fpr[3] = f64::MAX;
        cpu.state.fpr[5] = f64::from_bits(1);
        cpu.state.fpr[6] = 1.0;
        cpu.state.fpscr = 1 << 6; // OE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP; // precise FP exception mode

        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.take_pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: 1 << 28,
                mode: PpcFpExceptionMode::Precise,
            })
        );
        assert!(cpu.state.fpr[1].is_finite());
        assert_ne!(cpu.state.fpscr & (1 << 28), 0); // OX
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000);

        cpu.state.pc = 0x1004;
        cpu.state.fpscr = 1 << 5; // UE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP; // precise FP exception mode
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.take_pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1004,
                causes: 1 << 27,
                mode: PpcFpExceptionMode::Precise,
            })
        );
        assert_eq!(cpu.state.fpr[4], 2.0_f64.powi(462));
        assert_ne!(cpu.state.fpscr & (1 << 27), 0); // UX
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000);
    }

    #[test]
    fn double_precision_underflow_updates_fpscr_and_obeys_directed_rounding() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fdiv f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::from_bits(1);
        cpu.state.fpr[3] = 2.0;

        cpu.state.fpscr = 0; // nearest-even: half a minimum subnormal becomes zero
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1].to_bits(), 0);
        assert_ne!(cpu.state.fpscr & (1 << 27), 0); // UX
        assert_ne!(cpu.state.fpscr & (1 << 25), 0); // XX
        assert_ne!(cpu.state.fpscr & (1 << 17), 0); // FI

        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0b10; // toward +infinity
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1].to_bits(), 1);
        assert_ne!(cpu.state.fpscr & (1 << 18), 0); // FR
        assert_ne!(cpu.state.fpscr & (1 << 27), 0); // UX
    }

    #[test]
    fn double_precision_overflow_obeys_fpscr_rounding_modes() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fmul f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 6) | (25 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::MAX;
        cpu.state.fpr[3] = 2.0;

        cpu.state.fpscr = 0b01; // toward zero
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], f64::MAX);
        assert_ne!(cpu.state.fpscr & (1 << 28), 0); // OX
        assert_ne!(cpu.state.fpscr & (1 << 25), 0); // XX
        assert_eq!(cpu.state.fpscr & (1 << 18), 0); // not rounded away from zero

        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0b10; // toward +infinity
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], f64::INFINITY);
        assert_eq!(cpu.state.fpscr & (1 << 18), 0); // FR is undefined on disabled overflow; model as zero

        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = -f64::MAX;
        cpu.state.fpscr = 0b10; // toward +infinity: negative magnitude decreases
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], -f64::MAX);
        assert_eq!(cpu.state.fpscr & (1 << 18), 0);

        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0b11; // toward -infinity
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], f64::NEG_INFINITY);
        assert_eq!(cpu.state.fpscr & (1 << 18), 0); // FR undefined on disabled overflow; modeled as zero
    }

    #[test]
    fn ordered_compare_snan_cause_depends_on_invalid_enable() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fcmpo cr2,f1,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (2 << 23) | (1 << 16) | (2 << 11) | (32 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = f64::from_bits(0x7FF0_0000_0000_0001);
        cpu.state.fpr[2] = 1.0;
        cpu.state.fpscr = 1 << 7; // VE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP; // precise FP exception mode enabled

        cpu.step(&mut memory).unwrap();
        assert_ne!(cpu.state.fpscr & (1 << 24), 0); // VXSNAN
        assert_eq!(cpu.state.fpscr & (1 << 19), 0); // VXVC suppressed for enabled SNaN
        assert!(matches!(
            cpu.take_pending_exception(),
            Some(PpcException::FloatingPoint { .. })
        ));

        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 0; // VE disabled
        cpu.step(&mut memory).unwrap();
        assert_ne!(cpu.state.fpscr & (1 << 24), 0); // VXSNAN
        assert_ne!(cpu.state.fpscr & (1 << 19), 0); // VXVC also raised
    }

    #[test]
    fn enabled_invalid_exception_preserves_previous_fprf() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fdiv f1,f2,f3; fcmpo cr2,f4,f5
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (2 << 23) | (4 << 16) | (5 << 11) | (32 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpr[4] = f64::NAN;
        cpu.state.fpr[5] = 1.0;
        cpu.state.fpscr = (1 << 7) | 0x0001_4000; // VE enabled; prior positive-denormal FPRF (C=1)
        let prior_fprf = cpu.state.fpscr & 0x0001_F000;

        cpu.step(&mut memory).unwrap();
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX
        assert_eq!(cpu.state.fpscr & 0x0001_F000, prior_fprf);

        cpu.state.pc = 0x1004;
        cpu.state.fpscr = (1 << 7) | prior_fprf;
        cpu.step(&mut memory).unwrap();
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX summary
        assert_ne!(cpu.state.fpscr & (1 << 19), 0); // VXVC
        assert_eq!(cpu.state.fpscr & (1 << 16), prior_fprf & (1 << 16)); // C preserved
        assert_eq!(cpu.state.fpscr & 0x0000_F000, 1 << 12); // FPCC = unordered
    }

    #[test]
    fn enabled_invalid_arithmetic_preserves_destination_and_fprf() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fdiv. f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1) | 1,
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = 42.0;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = (1 << 7) | 0x0000_4000; // VE; prior positive-normal FPRF

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], 42.0); // enabled invalid operation suppresses FPR write
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000); // FPRF unchanged
        assert_eq!(cpu.state.fpscr & ((1 << 18) | (1 << 17)), 0); // FR/FI cleared
        assert_ne!(cpu.state.fpscr & (1 << 21), 0); // VXZDZ
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FEX
        assert_ne!(cpu.state.cr[1] & 0x0F, 0); // CR1 summary
    }

    #[test]
    fn enabled_invalid_integer_conversion_preserves_destination() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fctiw f1,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (14 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = 99.0;
        cpu.state.fpr[2] = f64::NAN;
        cpu.state.fpscr = 1 << 7; // VE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP; // precise FP exception mode

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], 99.0);
        assert_ne!(cpu.state.fpscr & (1 << 8), 0); // VXCVI
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FEX
    }

    #[test]
    fn fx_is_set_only_when_an_exception_status_bit_transitions() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 12);
        // fdiv f1,f2,f3; mtfsb0 FPSCR[FX]; fdiv f1,f2,f3
        let fdiv = (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1);
        let clear_fx = (63 << 26) | (70 << 1);
        put_instruction(&mut memory, 0x1000, fdiv);
        put_instruction(&mut memory, 0x1004, clear_fx);
        put_instruction(&mut memory, 0x1008, fdiv);
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;

        cpu.step(&mut memory).unwrap();
        assert_ne!(cpu.state.fpscr & (1 << 31), 0); // first VXZDZ transition sets FX
        assert_ne!(cpu.state.fpscr & (1 << 21), 0); // VXZDZ

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpscr & (1 << 31), 0); // explicitly cleared
        assert_ne!(cpu.state.fpscr & (1 << 21), 0); // cause remains sticky

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpscr & (1 << 31), 0); // no new exception-bit transition
        assert_ne!(cpu.state.fpscr & (1 << 21), 0);
    }

    #[test]
    fn fused_multiply_add_sets_infinity_zero_and_infinity_infinity_causes() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 12);
        // fmadd f1,f2,f3,f4; fmadd f5,f2,f3,f6; fmadd f9,f2,f3,f7
        let fmadd = |frt: u32, fra: u32, frb: u32, frc: u32| {
            (63 << 26) | (frt << 21) | (fra << 16) | (frb << 11) | (frc << 6) | (29 << 1)
        };
        put_instruction(&mut memory, 0x1000, fmadd(1, 2, 4, 3));
        put_instruction(&mut memory, 0x1004, fmadd(5, 2, 6, 3));
        put_instruction(&mut memory, 0x1008, fmadd(9, 2, 7, 3));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::INFINITY;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpr[4] = 1.0;
        cpu.state.fpr[6] = f64::NEG_INFINITY;

        cpu.step(&mut memory).unwrap();
        assert!(cpu.state.fpr[1].is_nan());
        assert_ne!(cpu.state.fpscr & (1 << 20), 0); // VXIMZ

        cpu.state.pc = 0x1004;
        cpu.state.fpr[3] = 1.0;
        cpu.step(&mut memory).unwrap();
        assert!(cpu.state.fpr[5].is_nan());
        assert_ne!(cpu.state.fpscr & (1 << 23), 0); // VXISI
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX summary

        // A QNaN addend must not hide the independent infinity-times-zero
        // invalid product in the fused operation.
        cpu.state.pc = 0x1008;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpr[7] = f64::NAN;
        cpu.state.fpscr = 0;
        cpu.step(&mut memory).unwrap();
        assert!(cpu.state.fpr[9].is_nan());
        assert_ne!(cpu.state.fpscr & (1 << 20), 0); // VXIMZ
    }

    #[test]
    fn negative_square_root_sets_the_vxsqrt_cause_bit() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fsqrt f1,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (22 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = -1.0;

        cpu.step(&mut memory).unwrap();

        assert!(cpu.state.fpr[1].is_nan());
        assert_ne!(cpu.state.fpscr & (1 << 9), 0); // VXSQRT
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX summary
        assert_ne!(cpu.state.fpscr & (1 << 31), 0); // FX
    }

    #[test]
    fn floating_point_invalid_arithmetic_sets_specific_fpscr_causes() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 12);
        // fdiv f1,f2,f3 (0/0); fsqrt f4,f5 (-1); fmul f6,f7,f8 (inf*0)
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (4 << 21) | (5 << 11) | (22 << 1),
        );
        put_instruction(
            &mut memory,
            0x1008,
            (63 << 26) | (6 << 21) | (7 << 16) | (8 << 6) | (25 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpr[5] = -1.0;
        cpu.state.fpr[7] = f64::INFINITY;
        cpu.state.fpr[8] = 0.0;

        for _ in 0..3 {
            cpu.step(&mut memory).unwrap();
        }

        assert_ne!(cpu.state.fpscr & (1 << 21), 0); // VXZDZ
        assert_ne!(cpu.state.fpscr & (1 << 9), 0); // VXSQRT
        assert_ne!(cpu.state.fpscr & (1 << 20), 0); // VXIMZ
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX summary
    }

    #[test]
    fn fctiw_stfiwx_and_integer_word_fpr_loads_preserve_register_bits() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 28);
        // fctiw f1,f2; stfiwx f1,r3,r4; lfiwax f5,r3,r4; lfiwzx f6,r3,r4
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (14 << 1),
        );
        let encode_x =
            |frt: u32, xo: u32| (31 << 26) | (frt << 21) | (3 << 16) | (4 << 11) | (xo << 1);
        put_instruction(&mut memory, 0x1004, encode_x(1, 983));
        put_instruction(&mut memory, 0x1008, encode_x(5, 855));
        put_instruction(&mut memory, 0x100C, encode_x(6, 887));

        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = f64::from_bits(0x1234_5678_9ABC_DEF0);
        cpu.state.fpr[2] = -42.75;
        cpu.state.gpr[3] = 0x1000;
        cpu.state.gpr[4] = 0x18;

        for _ in 0..4 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.fpr[1].to_bits(), 0x1234_5678_FFFF_FFD5);
        assert_eq!(
            memory.read_u32_be(GuestAddress(0x1018)).unwrap(),
            0xFFFF_FFD5
        );
        assert_eq!(cpu.state.fpr[5].to_bits(), 0xFFFF_FFFF_FFFF_FFD5);
        assert_eq!(cpu.state.fpr[6].to_bits(), 0x0000_0000_FFFF_FFD5);
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
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = f64::from_bits(0x1234_5678_9ABC_DEF0);
        cpu.state.fpr[3] = f64::from_bits(0x8765_4321_ABCD_EF01);
        cpu.state.fpr[2] = 2.75;
        cpu.state.fpscr = 0; // nearest, ties to even

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1].to_bits(), 0x1234_5678_0000_0003);
        assert_ne!(cpu.state.fpscr & (1 << 17), 0); // FI
        assert_ne!(cpu.state.fpscr & (1 << 18), 0); // FR: 2.75 rounded to 3

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[3].to_bits(), 0x8765_4321_0000_0002);
        assert_ne!(cpu.state.fpscr & (1 << 17), 0); // FI: fractional bits were discarded
        assert_eq!(cpu.state.fpscr & (1 << 18), 0); // fctiwz truncates toward zero
        assert_ne!(cpu.state.fpscr & (1 << 25), 0); // XX: conversion was inexact
        assert_ne!(cpu.state.fpscr & (1 << 31), 0); // FX: sticky exception summary
    }

    #[test]
    fn floating_point_64_bit_integer_conversions_round_and_truncate() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fctid f1,f2; fctidz f3,f2
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (814 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (3 << 21) | (2 << 11) | (815 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = f64::from_bits(0x1234_5678_9ABC_DEF0);
        cpu.state.fpr[2] = 42.75;
        cpu.state.fpr[3] = f64::from_bits(0x8765_4321_ABCD_EF01);

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1].to_bits(), 43_i64 as u64);
        assert_ne!(cpu.state.fpscr & (1 << 17), 0); // FI
        assert_ne!(cpu.state.fpscr & (1 << 18), 0); // FR

        cpu.state.pc = 0x1004;
        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[3].to_bits(), 42_i64 as u64);
        assert_ne!(cpu.state.fpscr & (1 << 17), 0); // FI
        assert_eq!(cpu.state.fpscr & (1 << 18), 0); // truncation is not away from zero
    }

    #[test]
    fn integer_conversion_overflow_saturates_by_sign() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        // fctiw f1,f2; fctiw f3,f4; fctid f5,f6; fctid f7,f8
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 11) | (14 << 1),
        );
        put_instruction(
            &mut memory,
            0x1004,
            (63 << 26) | (3 << 21) | (4 << 11) | (14 << 1),
        );
        put_instruction(
            &mut memory,
            0x1008,
            (63 << 26) | (5 << 21) | (6 << 11) | (814 << 1),
        );
        put_instruction(
            &mut memory,
            0x100C,
            (63 << 26) | (7 << 21) | (8 << 11) | (814 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::INFINITY;
        cpu.state.fpr[4] = f64::NEG_INFINITY;
        cpu.state.fpr[6] = f64::INFINITY;
        cpu.state.fpr[8] = f64::NEG_INFINITY;

        for _ in 0..4 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.fpr[1].to_bits() as u32, i32::MAX as u32);
        assert_eq!(cpu.state.fpr[3].to_bits() as u32, i32::MIN as u32);
        assert_eq!(cpu.state.fpr[5].to_bits(), i64::MAX as u64);
        assert_eq!(cpu.state.fpr[7].to_bits(), i64::MIN as u64);
        assert_ne!(cpu.state.fpscr & (1 << 8), 0); // VXCVI
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX
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

        let mut cpu = fp_enabled_cpu();
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
    fn vmx128_permute_selects_bytes_from_both_extended_sources() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        let encode = |vd: u32, va: u32, vb: u32, vc: u32| {
            (5 << 26)
                | ((vd & 0x1F) << 21)
                | ((va & 0x1F) << 16)
                | ((vb & 0x1F) << 11)
                | (((vd >> 5) & 3) << 2)
                | (((va >> 5) & 1) << 5)
                | (((va >> 6) & 1) << 10)
                | ((vc & 7) << 6)
                | ((vb >> 5) & 3)
        };
        put_instruction(&mut memory, 0x1000, encode(0x7D, 0x72, 0x43, 5));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.vr[0x72] = std::array::from_fn(|i| i as u8);
        cpu.state.vr[0x43] = std::array::from_fn(|i| (0x80 + i) as u8);
        cpu.state.vr[5] = [0, 15, 16, 31, 1, 14, 17, 30, 2, 13, 18, 29, 3, 12, 19, 28];
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [
                0, 15, 0x80, 0x8F, 1, 14, 0x81, 0x8E, 2, 13, 0x82, 0x8D, 3, 12, 0x83, 0x8C
            ]
        );
    }

    #[test]
    fn vmx128_word_shifts_and_comparisons_execute_on_extended_registers() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        let encode_shift = |vd: u32, va: u32, vb: u32, operation: u32| {
            (6 << 26)
                | ((vd & 0x1F) << 21)
                | ((va & 0x1F) << 16)
                | ((vb & 0x1F) << 11)
                | (((vd >> 5) & 3) << 2)
                | (((va >> 5) & 1) << 5)
                | (((va >> 6) & 1) << 10)
                | ((vb >> 5) & 3)
                | (((operation >> 2) & 0xF) << 6)
                | ((operation & 1) << 4)
        };
        let encode_compare = |vd: u32, va: u32, vb: u32, operation: u32| {
            (6 << 26)
                | ((vd & 0x1F) << 21)
                | ((va & 0x1F) << 16)
                | ((vb & 0x1F) << 11)
                | (((vd >> 5) & 3) << 2)
                | (((va >> 5) & 1) << 5)
                | (((va >> 6) & 1) << 10)
                | ((vb >> 5) & 3)
                | (((operation >> 3) & 7) << 7)
                | ((operation & 1) << 4)
        };
        put_instruction(
            &mut memory,
            0x1000,
            encode_shift(0x7D, 0x72, 0x43, 0b001101),
        );
        put_instruction(
            &mut memory,
            0x1004,
            encode_shift(0x7D, 0x72, 0x43, 0b011101),
        );
        put_instruction(
            &mut memory,
            0x1008,
            encode_compare(0x7D, 0x72, 0x43, 0b100000),
        );
        put_instruction(
            &mut memory,
            0x100C,
            encode_compare(0x7D, 0x72, 0x43, 0b001000) | (1 << 6),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.vr[0x72] = [
            0x80, 0, 0, 1, 0x80, 0, 0, 2, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 1,
        ];
        cpu.state.vr[0x43] = [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1];
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [0, 0, 0, 2, 0, 0, 0, 4, 0xFF, 0xFF, 0xFF, 0xFE, 0, 0, 0, 2]
        );
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [
                0x40, 0, 0, 0, 0x40, 0, 0, 1, 0x7F, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0
            ]
        );
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]
        );

        cpu.state.vr[0x72] = [
            1.0_f32.to_bits().to_be_bytes(),
            2.0_f32.to_bits().to_be_bytes(),
            3.0_f32.to_bits().to_be_bytes(),
            4.0_f32.to_bits().to_be_bytes(),
        ]
        .concat()
        .try_into()
        .unwrap();
        cpu.state.vr[0x43] = [
            1.0_f32.to_bits().to_be_bytes(),
            3.0_f32.to_bits().to_be_bytes(),
            2.0_f32.to_bits().to_be_bytes(),
            4.0_f32.to_bits().to_be_bytes(),
        ]
        .concat()
        .try_into()
        .unwrap();
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [
                0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF
            ]
        );
        assert_eq!(cpu.state.cr[6], 0b0100);
    }

    #[test]
    fn vmx128_float_arithmetic_and_saturating_pack_update_architectural_state() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        let encode = |vd: u32, va: u32, vb: u32, operation: u32| {
            (5 << 26)
                | ((vd & 0x1F) << 21)
                | ((va & 0x1F) << 16)
                | ((vb & 0x1F) << 11)
                | (((vd >> 5) & 3) << 2)
                | (((va >> 5) & 1) << 5)
                | (((va >> 6) & 1) << 10)
                | ((vb >> 5) & 3)
                | (((operation >> 2) & 0xF) << 6)
                | ((operation & 1) << 4)
        };
        put_instruction(&mut memory, 0x1000, encode(0x7D, 0x72, 0x43, 0b000001));
        put_instruction(&mut memory, 0x1004, encode(0x7D, 0x72, 0x43, 0b100000));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        for lane in 0..4 {
            cpu.state.vr[0x72][lane * 4..lane * 4 + 4]
                .copy_from_slice(&(1.5_f32 + lane as f32).to_bits().to_be_bytes());
            cpu.state.vr[0x43][lane * 4..lane * 4 + 4]
                .copy_from_slice(&(2.5_f32 + lane as f32).to_bits().to_be_bytes());
        }
        cpu.step(&mut memory).unwrap();
        for lane in 0..4 {
            let offset = lane * 4;
            let actual = f32::from_bits(u32::from_be_bytes(
                cpu.state.vr[0x7D][offset..offset + 4].try_into().unwrap(),
            ));
            assert_eq!(actual, 4.0 + (lane as f32 * 2.0));
        }

        cpu.state.vr[0x72] = [
            0x00, 0x7F, 0xFF, 0x80, 0x00, 0x01, 0xFF, 0xFF, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04,
            0x00, 0x05,
        ];
        cpu.state.vr[0x43] = [
            0x00, 0x80, 0x00, 0x7F, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x05, 0x00, 0x06,
            0x00, 0x07,
        ];
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [
                0x7F, 0x80, 0x01, 0xFF, 0x02, 0x03, 0x04, 0x05, 0x7F, 0x7F, 0x02, 0x03, 0x04, 0x05,
                0x06, 0x07
            ]
        );
        assert_ne!(cpu.state.vscr & (1 << 31), 0);
    }

    #[test]
    fn vmx128_logical_instructions_execute_with_extended_register_indices() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 20);
        let encode_vmx128 = |vd: u32, va: u32, vb: u32, operation: u32| {
            (5 << 26)
                | ((vd & 0x1F) << 21)
                | ((va & 0x1F) << 16)
                | ((vb & 0x1F) << 11)
                | (((vd >> 5) & 0x3) << 2)
                | (((va >> 5) & 0x1) << 5)
                | (((va >> 6) & 0x1) << 10)
                | ((vb >> 5) & 0x3)
                | (((operation >> 2) & 0xF) << 6)
                | ((operation & 0x1) << 4)
        };
        let operations = [0b100001, 0b100101, 0b101001, 0b101101, 0b110001];
        for (index, operation) in operations.into_iter().enumerate() {
            put_instruction(
                &mut memory,
                0x1000 + (index as u32 * 4),
                encode_vmx128(0x7D, 0x72, 0x43, operation),
            );
        }

        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        let a = [
            0xF0, 0x0F, 0xAA, 0x55, 0xFF, 0x00, 0x81, 0x18, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC,
            0xDE, 0xF0,
        ];
        let b = [
            0x0F, 0xF0, 0x0F, 0xF0, 0x55, 0xAA, 0x18, 0x81, 0xFF, 0x0F, 0xF0, 0xAA, 0x55, 0x33,
            0xCC, 0x0F,
        ];
        cpu.state.vr[0x72] = a;
        cpu.state.vr[0x43] = b;

        for (index, operation) in operations.into_iter().enumerate() {
            cpu.step(&mut memory).unwrap();
            let expected = std::array::from_fn(|lane| match operation {
                0b100001 => a[lane] & b[lane],
                0b100101 => a[lane] & !b[lane],
                0b101001 => !(a[lane] | b[lane]),
                0b101101 => a[lane] | b[lane],
                0b110001 => a[lane] ^ b[lane],
                _ => unreachable!(),
            });
            assert_eq!(cpu.state.vr[0x7D], expected, "operation {operation:#08b}");
            assert_eq!(cpu.state.pc, 0x1004 + index as u32 * 4);
        }
    }

    #[test]
    fn vmx128_byte_shifts_cover_left_right_zero_and_extended_registers() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        let encode_vmx128 = |vd: u32, va: u32, vb: u32, operation: u32| {
            (5 << 26)
                | ((vd & 0x1F) << 21)
                | ((va & 0x1F) << 16)
                | ((vb & 0x1F) << 11)
                | (((vd >> 5) & 0x3) << 2)
                | (((va >> 5) & 0x1) << 5)
                | (((va >> 6) & 0x1) << 10)
                | ((vb >> 5) & 0x3)
                | (((operation >> 2) & 0xF) << 6)
                | ((operation & 0x1) << 4)
        };
        put_instruction(
            &mut memory,
            0x1000,
            encode_vmx128(0x7D, 0x72, 0x43, 0b111001),
        );
        put_instruction(
            &mut memory,
            0x1004,
            encode_vmx128(0x7D, 0x72, 0x43, 0b111101),
        );

        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        let source = std::array::from_fn(|index| index as u8);
        cpu.state.vr[0x72] = source;
        cpu.state.vr[0x43] = [0; 16];
        cpu.state.vr[0x43][15] = 0x10;

        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0, 0]
        );

        cpu.state.vr[0x43][15] = 0x08;
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]
        );
        assert_eq!(cpu.state.pc, 0x1008);
    }

    #[test]
    fn vmx128_word_permutation_immediate_uses_extended_registers_and_guest_word_order() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        let encode_vpermwi128 = |vd: u32, vb: u32, permutation: u32| {
            (6 << 26)
                | (1 << 9)
                | (1 << 4)
                | ((vd & 0x1F) << 21)
                | ((permutation & 0x1F) << 16)
                | ((vb & 0x1F) << 11)
                | (((permutation >> 5) & 0x7) << 6)
                | (((vd >> 5) & 0x3) << 2)
                | ((vb >> 5) & 0x3)
        };
        // Identity permutation, followed by reverse word order.
        put_instruction(&mut memory, 0x1000, encode_vpermwi128(0x7D, 0x43, 0x1B));
        put_instruction(&mut memory, 0x1004, encode_vpermwi128(0x7D, 0x43, 0xE4));

        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.vr[0x43] = [
            0x00, 0x01, 0x02, 0x03, 0x10, 0x11, 0x12, 0x13, 0x20, 0x21, 0x22, 0x23, 0x30, 0x31,
            0x32, 0x33,
        ];

        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [
                0x00, 0x01, 0x02, 0x03, 0x10, 0x11, 0x12, 0x13, 0x20, 0x21, 0x22, 0x23, 0x30, 0x31,
                0x32, 0x33,
            ]
        );

        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [
                0x30, 0x31, 0x32, 0x33, 0x20, 0x21, 0x22, 0x23, 0x10, 0x11, 0x12, 0x13, 0x00, 0x01,
                0x02, 0x03,
            ]
        );
        assert_eq!(cpu.state.pc, 0x1008);
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

        let mut cpu = fp_enabled_cpu();
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

        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.gpr[3], 9);
        assert_eq!(cpu.state.pc, 0x1004);
    }

    #[test]
    fn unsupported_instruction_fails_instead_of_succeeding_silently() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        put_instruction(&mut memory, 0x1000, 0);
        let mut cpu = fp_enabled_cpu();
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
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;

        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::Memory(MemoryError::OutOfBounds))
        );
    }

    #[test]
    fn exact_single_precision_subnormal_raises_underflow_when_ue_is_enabled() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadds f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f32::from_bits(1) as f64;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 1 << 5; // UE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP; // precise FP exception mode enabled

        cpu.step(&mut memory).unwrap();

        assert_eq!(
            cpu.take_pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: 1 << 27,
                mode: PpcFpExceptionMode::Precise,
            })
        );
        assert_eq!(cpu.state.fpr[1], 2.0_f64.powi(43)); // enabled underflow adjusts exponent by +192
        assert_ne!(cpu.state.fpscr & (1 << 27), 0); // enabled underflow: UX
        assert_eq!(cpu.state.fpscr & 0x0001_F000, 0x0000_4000); // adjusted positive normal
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FEX
    }

    #[test]
    fn exact_single_precision_subnormal_does_not_raise_underflow_when_ue_is_disabled() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadds f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f32::from_bits(1) as f64;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 0; // UE disabled

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.pending_exception(), None);
        assert_eq!(
            cpu.state.fpr[1].to_bits(),
            (f32::from_bits(1) as f64).to_bits()
        );
        assert_eq!(cpu.state.fpscr & (1 << 27), 0); // exact subnormal: no UX
        assert_eq!(cpu.state.fpscr & (1 << 30), 0); // no enabled exception summary
    }
    #[test]
    fn enabled_floating_point_invalid_exception_is_reported_to_cpu_layer() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fdiv f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = 123.0;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 1 << 7; // VE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP; // precise FP exception mode

        let expected = PpcException::FloatingPoint {
            instruction_pc: 0x1000,
            causes: (1 << 29) | (1 << 21),
            mode: PpcFpExceptionMode::Precise,
        };
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.pending_exception(), Some(expected));
        assert_eq!(cpu.take_pending_exception(), Some(expected));
        assert_eq!(cpu.state.fpr[1], 123.0); // enabled invalid suppresses destination write
    }

    #[test]
    fn mtfsfi_enabling_existing_fpscr_cause_raises_fp_exception() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtfsfi 6,8: set FPSCR[VE] while VXZDZ is already sticky.
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (6 << 23) | (8 << 12) | (134 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 1 << 21; // VXZDZ, VE initially clear
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP;

        cpu.step(&mut memory).unwrap();

        assert_eq!(
            cpu.pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: (1 << 29) | (1 << 21),
                mode: PpcFpExceptionMode::Precise,
            })
        );
        assert_eq!(cpu.state.fpscr & (1 << 31), 0); // mtfsfi does not implicitly set FX
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FEX derived after VE is enabled
    }

    #[test]
    fn mtfsf_setting_invalid_cause_and_enable_raises_fp_exception() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtfsf 0xFF,f2: write VXZDZ and VE in one FPSCR transfer.
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (0xFF << 17) | (2 << 11) | (711 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::from_bits((1 << 21) | (1 << 7));
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP;

        cpu.step(&mut memory).unwrap();

        assert_eq!(
            cpu.pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: (1 << 29) | (1 << 21),
                mode: PpcFpExceptionMode::Precise,
            })
        );
        assert_eq!(cpu.state.fpscr & (1 << 31), 0); // mtfsf explicitly supplied FX=0
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FEX recomputed from VX + VE
    }

    #[test]
    fn mtfsb1_setting_invalid_cause_raises_fp_exception() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtfsb1 10: set FPSCR[VXZDZ].
        put_instruction(&mut memory, 0x1000, (63 << 26) | (10 << 21) | (38 << 1));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpscr = 1 << 7; // VE already enabled
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP;

        cpu.step(&mut memory).unwrap();

        assert_eq!(
            cpu.pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: (1 << 29) | (1 << 21),
                mode: PpcFpExceptionMode::Precise,
            })
        );
        assert_ne!(cpu.state.fpscr & (1 << 31), 0); // FX set by mtfsb1
    }

    #[test]
    fn msr_fe_bits_decode_all_four_powerpc_exception_modes() {
        let mut cpu = fp_enabled_cpu();
        assert_eq!(cpu.fp_exception_mode(), PpcFpExceptionMode::Disabled);

        cpu.state.msr = PPC_MSR_FE1 | PPC_MSR_FP;
        assert_eq!(
            cpu.fp_exception_mode(),
            PpcFpExceptionMode::ImpreciseNonRecoverable
        );

        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FP;
        assert_eq!(
            cpu.fp_exception_mode(),
            PpcFpExceptionMode::ImpreciseRecoverable
        );

        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP;
        assert_eq!(cpu.fp_exception_mode(), PpcFpExceptionMode::Precise);
    }

    #[test]
    fn imprecise_recoverable_mode_is_preserved_in_fp_exception_event() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fdiv f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 1 << 7; // VE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FP; // imprecise recoverable

        cpu.step(&mut memory).unwrap();

        assert_eq!(
            cpu.pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: (1 << 29) | (1 << 21),
                mode: PpcFpExceptionMode::ImpreciseRecoverable,
            })
        );
    }

    #[test]
    fn mtmsr_l_form_is_not_misinterpreted_as_a_full_msr_write() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtmsr r3,1 (L=1)
        put_instruction(
            &mut memory,
            0x1000,
            (31 << 26) | (3 << 21) | (1 << 16) | (146 << 1),
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = PPC_MSR_FE0 | PPC_MSR_FE1;

        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::UnsupportedInstruction {
                pc: 0x1000,
                opcode: 31
            })
        );
        assert_eq!(cpu.state.msr, 0);
        assert_eq!(cpu.state.pc, 0x1000);
    }

    #[test]
    fn mtmsr_enabling_fp_exceptions_with_sticky_fex_raises_at_next_pc() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // mtmsr r3; addi r4,r4,1
        put_instruction(&mut memory, 0x1000, (31 << 26) | (3 << 21) | (146 << 1));
        put_instruction(&mut memory, 0x1004, (14 << 26) | (4 << 21) | (4 << 16) | 1);
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = PPC_MSR_FE0 | PPC_MSR_FE1;
        cpu.state.fpscr = (1 << 30) | (1 << 29) | (1 << 24) | (1 << 7);

        cpu.step(&mut memory).unwrap();

        let pending = PpcException::FloatingPointEnableTransition {
            next_pc: 0x1004,
            causes: (1 << 29) | (1 << 24),
            mode: PpcFpExceptionMode::Precise,
        };
        assert_eq!(cpu.state.msr, PPC_MSR_FE0 | PPC_MSR_FE1);
        assert_eq!(cpu.state.pc, 0x1004);
        assert_eq!(cpu.pending_exception(), Some(pending));
        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::PendingException(pending))
        );
        assert_eq!(cpu.state.pc, 0x1004);
        assert_eq!(cpu.state.gpr[4], 0);
    }

    #[test]
    fn mtmsr_surfaces_prior_imprecise_fp_event_before_changing_msr() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtmsr r3
        put_instruction(&mut memory, 0x1000, (31 << 26) | (3 << 21) | (146 << 1));
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = PPC_MSR_FE0 | PPC_MSR_FE1;
        cpu.state.msr = PPC_MSR_FE1;
        cpu.pending_exception = Some(PpcException::FloatingPoint {
            instruction_pc: 0x0FFC,
            causes: (1 << 26) | (1 << 25),
            mode: PpcFpExceptionMode::ImpreciseNonRecoverable,
        });
        let pending = cpu.pending_exception();

        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::PendingException(pending.unwrap()))
        );
        assert_eq!(cpu.state.pc, 0x1000);
        assert_eq!(cpu.state.msr, PPC_MSR_FE1);
    }

    #[test]
    fn mtmsr_fe_imprecise_transition_is_still_delivered_at_next_boundary() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // mtmsr r3; addi r4,r4,1
        put_instruction(&mut memory, 0x1000, (31 << 26) | (3 << 21) | (146 << 1));
        put_instruction(&mut memory, 0x1004, (14 << 26) | (4 << 21) | (4 << 16) | 1);
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = PPC_MSR_FE1;
        cpu.state.fpscr = (1 << 30) | (1 << 26) | (1 << 4); // ZX + ZE => FEX

        cpu.step(&mut memory).unwrap();

        let pending = PpcException::FloatingPointEnableTransition {
            next_pc: 0x1004,
            causes: 1 << 26,
            mode: PpcFpExceptionMode::ImpreciseNonRecoverable,
        };
        assert_eq!(cpu.pending_exception(), Some(pending));
        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::PendingException(pending))
        );
        assert_eq!(cpu.state.pc, 0x1004);
        assert_eq!(cpu.state.gpr[4], 0);
    }

    #[test]
    fn mtmsr_does_not_retrigger_when_fe_was_already_enabled() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtmsr r3
        put_instruction(&mut memory, 0x1000, (31 << 26) | (3 << 21) | (146 << 1));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FP;
        cpu.state.gpr[3] = PPC_MSR_FE0;
        cpu.state.fpscr = (1 << 30) | (1 << 29) | (1 << 24) | (1 << 7);

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.pending_exception(), None);
        assert_eq!(cpu.state.pc, 0x1004);
    }

    #[test]
    fn imprecise_fp_exception_is_reported_at_sync_after_prior_work_completes() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 12);
        // fdiv f1,f2,f3; addi r4,r4,1; sync
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        put_instruction(&mut memory, 0x1004, (14 << 26) | (4 << 21) | (4 << 16) | 1);
        put_instruction(&mut memory, 0x1008, (31 << 26) | (598 << 1));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 1 << 7; // VE
        cpu.state.msr = PPC_MSR_FE1 | PPC_MSR_FP; // imprecise nonrecoverable

        cpu.step(&mut memory).unwrap();
        let pending = cpu.pending_exception().expect("imprecise FP event");
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[4], 1);
        assert_eq!(cpu.state.pc, 0x1008);

        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::PendingException(pending))
        );
        assert_eq!(cpu.state.pc, 0x100C);
        assert_eq!(cpu.pending_exception(), Some(pending));
    }

    #[test]
    fn imprecise_fp_exception_is_reported_at_isync() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fdiv f1,f2,f3; isync
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        put_instruction(&mut memory, 0x1004, (19 << 26) | (150 << 1));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 1 << 7; // VE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FP; // imprecise recoverable

        cpu.step(&mut memory).unwrap();
        let pending = cpu.pending_exception().expect("imprecise FP event");

        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::PendingException(pending))
        );
        assert_eq!(cpu.state.pc, 0x1008);
        assert_eq!(cpu.pending_exception(), Some(pending));
    }

    #[test]
    fn imprecise_fp_exception_mode_does_not_block_following_instruction() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fdiv f1,f2,f3; addi r4,r4,1
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        put_instruction(&mut memory, 0x1004, (14 << 26) | (4 << 21) | (4 << 16) | 1);
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 1 << 7; // VE
        cpu.state.msr = PPC_MSR_FE1 | PPC_MSR_FP; // imprecise nonrecoverable

        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: (1 << 29) | (1 << 21),
                mode: PpcFpExceptionMode::ImpreciseNonRecoverable,
            })
        );

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.gpr[4], 1);
        assert_eq!(cpu.state.pc, 0x1008);
    }

    #[test]
    fn fpscr_enabled_exception_is_not_dispatched_when_msr_fe_bits_are_clear() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fdiv f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 1 << 7; // VE enabled, but MSR[FE0:FE1] = 00

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.pending_exception(), None);
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // sticky VX is still recorded
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FPSCR FEX still reflects VE + VX
    }

    #[test]
    fn interpreter_does_not_drop_an_unconsumed_fp_exception_on_next_step() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fdiv f1,f2,f3; nop-like supported addi r0,r0,0
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        put_instruction(&mut memory, 0x1004, 14 << 26);
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;
        cpu.state.fpscr = 1 << 7; // VE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP;

        cpu.step(&mut memory).unwrap();
        let pending = cpu.pending_exception().expect("enabled FP exception");
        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::PendingException(pending))
        );
        assert_eq!(cpu.state.pc, 0x1004);
        assert_eq!(cpu.pending_exception(), Some(pending));
    }

    #[test]
    fn disabled_floating_point_invalid_exception_does_not_notify_cpu_layer() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fdiv f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 0.0;
        cpu.state.fpr[3] = 0.0;

        cpu.step(&mut memory).unwrap();

        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // sticky VX still records
    }

    #[test]
    fn enabled_inexact_single_precision_emits_guest_exception_event() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadds f1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (59 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 1.0;
        cpu.state.fpr[3] = 2.0_f64.powi(-24);
        cpu.state.fpscr = 1 << 3; // XE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP; // precise FP exception mode

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.fpr[1], 1.0); // tie rounds to even
        assert_eq!(
            cpu.take_pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: 1 << 25,
                mode: PpcFpExceptionMode::Precise,
            })
        );
        assert_ne!(cpu.state.fpscr & (1 << 25), 0); // XX
        assert_ne!(cpu.state.fpscr & (1 << 30), 0); // FEX
    }

    #[test]
    fn enabled_ordered_nan_compare_reports_vxvc_cause_to_cpu_layer() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fcmpo cr1,f2,f3
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 23) | (2 << 16) | (3 << 11) | (32 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::NAN;
        cpu.state.fpr[3] = 1.0;
        cpu.state.fpscr = 1 << 7; // VE
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP; // precise FP exception mode

        cpu.step(&mut memory).unwrap();

        assert_eq!(
            cpu.take_pending_exception(),
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                causes: (1 << 29) | (1 << 19),
                mode: PpcFpExceptionMode::Precise,
            })
        );
        assert_ne!(cpu.state.fpscr & (1 << 19), 0); // VXVC
    }

    #[test]
    fn fpscr_instructions_deliver_prior_imprecise_fp_exceptions_before_execution() {
        let fpscr_instructions = [
            (63 << 26) | (1 << 23) | (64 << 1),  // mcrfs cr1, fpscr0
            (63 << 26) | (1 << 21) | (583 << 1), // mffs f1
            (63 << 26) | (7 << 21) | (38 << 1),  // mtfsb1
            (63 << 26) | (7 << 21) | (70 << 1),  // mtfsb0
            (63 << 26) | (7 << 23) | (1 << 12) | (134 << 1), // mtfsfi
            (63 << 26) | (0xFF << 17) | (1 << 11) | (711 << 1), // mtfsf
        ];
        let pending = PpcException::FloatingPoint {
            instruction_pc: 0x0FFC,
            causes: (1 << 29) | (1 << 21),
            mode: PpcFpExceptionMode::ImpreciseRecoverable,
        };

        for instruction in fpscr_instructions {
            let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
            put_instruction(&mut memory, 0x1000, instruction);
            let mut cpu = fp_enabled_cpu();
            cpu.state.pc = 0x1000;
            cpu.state.fpscr = 0xA123_4567;
            cpu.state.fpr[1] = f64::from_bits(0xFFF8_0000_7654_3210);
            cpu.pending_exception = Some(pending);

            let original_fpscr = cpu.state.fpscr;
            let original_fpr_bits = cpu.state.fpr.map(|value| value.to_bits());
            let original_cr = cpu.state.cr;

            assert_eq!(
                cpu.step(&mut memory),
                Err(PpcError::PendingException(pending))
            );
            assert_eq!(cpu.state.pc, 0x1000);
            assert_eq!(cpu.state.fpscr, original_fpscr);
            assert_eq!(
                cpu.state.fpr.map(|value| value.to_bits()),
                original_fpr_bits
            );
            assert_eq!(cpu.state.cr, original_cr);
            assert_eq!(cpu.pending_exception(), Some(pending));
        }
    }

    #[test]
    fn mtfsf_sets_exception_cause_without_implicitly_setting_fx() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtfsf 0xFF,f2: write VXSQRT while explicitly keeping FX clear.
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (0xFF << 17) | (2 << 11) | (711 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = f64::from_bits(1 << 9);

        cpu.step(&mut memory).unwrap();

        assert_ne!(cpu.state.fpscr & (1 << 9), 0); // VXSQRT
        assert_ne!(cpu.state.fpscr & (1 << 29), 0); // VX summary
        assert_eq!(cpu.state.fpscr & (1 << 31), 0); // FX comes only from source bit 31
    }

    #[test]
    fn precise_fp_exception_enters_program_vector_and_saves_xenon_state() {
        let mut cpu = fp_enabled_cpu();
        let original_msr = 0x0000_1234_0000_0030 | PPC_MSR_FE0 | PPC_MSR_FE1;
        let exception = PpcException::FloatingPoint {
            instruction_pc: 0x1234,
            causes: 1 << 29,
            mode: PpcFpExceptionMode::Precise,
        };
        cpu.state.pc = 0x1238;
        cpu.state.msr = original_msr;
        cpu.pending_exception = Some(exception);

        assert_eq!(cpu.enter_pending_exception(), Some(exception));

        assert_eq!(cpu.state.srr0, 0x1234);
        assert_eq!(
            cpu.state.srr1 & PPC_SRR1_FP_ENABLED_EXCEPTION,
            PPC_SRR1_FP_ENABLED_EXCEPTION
        );
        assert_eq!(cpu.state.srr1 & PPC_SRR1_IMPRECISE, 0);
        assert_eq!(
            cpu.state.srr1 & 0xFFFF_FFFF_87C0_FFFF,
            original_msr & 0xFFFF_FFFF_87C0_FFFF
        );
        assert_eq!(cpu.state.pc, PPC_PROGRAM_EXCEPTION_VECTOR);
        assert_eq!(cpu.state.msr & (PPC_MSR_FE0 | PPC_MSR_FE1), 0);
        assert_eq!(cpu.state.msr & 0x30, 0); // Xenon exception entry clears IR and DR.
        assert_eq!(cpu.state.msr & 0x9000_0000_0000_0000, 0x9000_0000_0000_0000);
        assert_eq!(cpu.pending_exception(), None);
    }

    #[test]
    fn imprecise_fp_exception_saves_first_faulting_instruction_without_subsequent_marker() {
        let mut cpu = fp_enabled_cpu();
        let exception = PpcException::FloatingPoint {
            instruction_pc: 0x1008,
            causes: 1 << 25,
            mode: PpcFpExceptionMode::ImpreciseRecoverable,
        };
        cpu.state.pc = 0x1020; // A later synchronization boundary reported it.
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FP;
        cpu.pending_exception = Some(exception);

        assert_eq!(cpu.enter_pending_exception(), Some(exception));

        assert_eq!(cpu.state.srr0, 0x1008);
        assert_eq!(
            cpu.state.srr1 & PPC_SRR1_FP_ENABLED_EXCEPTION,
            PPC_SRR1_FP_ENABLED_EXCEPTION
        );
        assert_eq!(cpu.state.srr1 & PPC_SRR1_IMPRECISE, 0);
        assert_eq!(cpu.state.pc, PPC_PROGRAM_EXCEPTION_VECTOR);
    }

    #[test]
    fn enabling_fe_with_sticky_fex_saves_next_pc_and_subsequent_marker() {
        let mut cpu = fp_enabled_cpu();
        let exception = PpcException::FloatingPointEnableTransition {
            next_pc: 0x2004,
            causes: 1 << 29,
            mode: PpcFpExceptionMode::ImpreciseNonRecoverable,
        };
        cpu.state.pc = 0x2004;
        cpu.state.msr = PPC_MSR_FE1 | PPC_MSR_FP;
        cpu.pending_exception = Some(exception);

        assert_eq!(cpu.enter_pending_exception(), Some(exception));

        assert_eq!(cpu.state.srr0, 0x2004);
        assert_eq!(
            cpu.state.srr1 & PPC_SRR1_FP_ENABLED_EXCEPTION,
            PPC_SRR1_FP_ENABLED_EXCEPTION
        );
        assert_eq!(cpu.state.srr1 & PPC_SRR1_IMPRECISE, PPC_SRR1_IMPRECISE);
        assert_eq!(cpu.state.pc, PPC_PROGRAM_EXCEPTION_VECTOR);
    }

    #[test]
    fn rfid_restores_fp_exception_context_without_replaying_srr1_syndrome_bits() {
        let mut memory = GuestMemory::new(GuestAddress(0x700), 4);
        // rfid (opcode 19, XO 18).
        put_instruction(&mut memory, 0x700, (19 << 26) | (18 << 1));
        let mut cpu = fp_enabled_cpu();
        let original_msr = 0x0000_1234_0000_0030 | PPC_MSR_FE0 | PPC_MSR_FE1;
        cpu.state.msr = original_msr;
        cpu.state.pc = 0x1234;
        cpu.pending_exception = Some(PpcException::FloatingPoint {
            instruction_pc: 0x1234,
            causes: 1 << 29,
            mode: PpcFpExceptionMode::Precise,
        });

        cpu.enter_pending_exception().unwrap();
        assert_eq!(cpu.state.pc, PPC_PROGRAM_EXCEPTION_VECTOR);
        assert_ne!(cpu.state.srr1 & PPC_SRR1_FP_ENABLED_EXCEPTION, 0);

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.pc, 0x1234);
        assert_eq!(
            cpu.state.msr & (PPC_MSR_FE0 | PPC_MSR_FE1),
            PPC_MSR_FE0 | PPC_MSR_FE1
        );
        assert_eq!(cpu.state.msr & 0x30, 0x30); // IR/DR restored from SRR1.
        assert_eq!(cpu.state.msr & (1 << 20), 0); // SRR1 bit 43 is exception syndrome.
        assert_eq!(cpu.state.msr & (1 << 16), 0); // SRR1 bit 47 is not copied into the MSR.
    }

    #[test]
    fn mfspr_and_mtspr_access_srr0_and_srr1_for_exception_handlers() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        let encode_spr = |rt: u32, spr: u32, xo: u32| {
            (31 << 26) | (rt << 21) | ((spr & 0x1F) << 16) | (((spr >> 5) & 0x1F) << 11) | (xo << 1)
        };
        put_instruction(&mut memory, 0x1000, encode_spr(3, 26, 339)); // mfspr r3,SRR0
        put_instruction(&mut memory, 0x1004, encode_spr(4, 27, 339)); // mfspr r4,SRR1
        put_instruction(&mut memory, 0x1008, encode_spr(5, 26, 467)); // mtspr SRR0,r5
        put_instruction(&mut memory, 0x100C, encode_spr(6, 27, 467)); // mtspr SRR1,r6

        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.srr0 = 0x1234_5678;
        cpu.state.srr1 = 0xFEDC_BA98_7654_3210;
        cpu.state.gpr[5] = 0x8765_4321;
        cpu.state.gpr[6] = 0x0123_4567_89AB_CDEF;

        for _ in 0..4 {
            cpu.step(&mut memory).unwrap();
        }

        assert_eq!(cpu.state.gpr[3], 0x1234_5678);
        assert_eq!(cpu.state.gpr[4], 0xFEDC_BA98_7654_3210);
        assert_eq!(cpu.state.srr0, 0x8765_4321);
        assert_eq!(cpu.state.srr1, 0x0123_4567_89AB_CDEF);
        assert_eq!(cpu.state.pc, 0x1010);
    }

    #[test]
    fn mtmsr_preserves_the_full_64_bit_xenon_machine_state_register() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtmsr r3 (opcode 31, XO 146).
        put_instruction(&mut memory, 0x1000, (31 << 26) | (3 << 21) | (146 << 1));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = 0xA123_4567_89AB_CDEF;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.msr, 0xA123_4567_89AB_CDEF);
        assert_eq!(cpu.state.pc, 0x1004);
    }
    #[test]
    fn step_and_dispatch_enters_program_vector_after_precise_fp_fault() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 8);
        // fdiv f1,f2,f3; addi r4,r4,1 (must not execute before exception entry).
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (18 << 1),
        );
        put_instruction(&mut memory, 0x1004, (14 << 26) | (4 << 21) | (4 << 16) | 1);
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | PPC_MSR_FP;
        cpu.state.fpscr = 1 << 4; // ZE
        cpu.state.fpr[2] = 1.0;
        cpu.state.fpr[3] = 0.0;

        assert_eq!(cpu.step_and_dispatch(&mut memory), Ok(None));
        let delivered = cpu.step_and_dispatch(&mut memory).unwrap();

        assert!(matches!(
            delivered,
            Some(PpcException::FloatingPoint {
                instruction_pc: 0x1000,
                mode: PpcFpExceptionMode::Precise,
                ..
            })
        ));
        assert_eq!(cpu.state.pc, PPC_PROGRAM_EXCEPTION_VECTOR);
        assert_eq!(cpu.state.srr0, 0x1000);
        assert_eq!(cpu.state.gpr[4], 0);
    }
    #[test]
    fn mfmsr_reads_the_full_64_bit_machine_state_register() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mfmsr r3 (opcode 31, XO 83).
        put_instruction(&mut memory, 0x1000, (31 << 26) | (3 << 21) | (83 << 1));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.msr = 0xA123_4567_89AB_CDEF;

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.gpr[3], 0xA123_4567_89AB_CDEF);
        assert_eq!(cpu.state.pc, 0x1004);
    }

    #[test]
    fn mtmsrd_l_zero_applies_xenon_alias_bits_and_detects_fp_enable_transition() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtmsrd r3,L=0 (opcode 31, XO 178).
        put_instruction(&mut memory, 0x1000, (31 << 26) | (3 << 21) | (178 << 1));
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.gpr[3] = PPC_MSR_FE0 | PPC_MSR_FE1 | (1 << 62) | (1 << 14);
        cpu.state.fpscr = (1 << 30) | (1 << 26) | (1 << 4); // FEX + ZX + ZE

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.msr, 0xC000_0000_0000_C930); // Xenon mtmsrd L=0 aliases
        assert_eq!(
            cpu.state.msr & (PPC_MSR_FE0 | PPC_MSR_FE1),
            PPC_MSR_FE0 | PPC_MSR_FE1
        );
        assert!(matches!(
            cpu.pending_exception(),
            Some(PpcException::FloatingPointEnableTransition {
                next_pc: 0x1004,
                mode: PpcFpExceptionMode::Precise,
                ..
            })
        ));
    }

    #[test]
    fn mtmsrd_l_one_updates_only_ee_and_ri() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // mtmsrd r3,L=1 (opcode 31, XO 178).
        put_instruction(
            &mut memory,
            0x1000,
            (31 << 26) | (3 << 21) | (1 << 16) | (178 << 1),
        );
        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.msr = PPC_MSR_FE0 | PPC_MSR_FE1 | 0x1234;
        cpu.state.gpr[3] = (1 << 15) | (1 << 1);

        cpu.step(&mut memory).unwrap();

        assert_eq!(cpu.state.msr, PPC_MSR_FE0 | PPC_MSR_FE1 | 0x9236);
        assert_eq!(cpu.state.pc, 0x1004);
    }
    #[test]
    fn fpu_unavailable_uses_vector_800_and_does_not_set_fp_enabled_syndrome() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
        // fadd f1,f2,f3.
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[1] = 99.0;
        cpu.state.fpr[2] = 2.0;
        cpu.state.fpr[3] = 3.0;
        let exception = PpcException::FloatingPointUnavailable {
            instruction_pc: 0x1000,
        };

        assert_eq!(
            cpu.step(&mut memory),
            Err(PpcError::PendingException(exception))
        );
        assert_eq!(cpu.state.fpr[1], 99.0);
        assert_eq!(cpu.enter_pending_exception(), Some(exception));
        assert_eq!(cpu.state.srr0, 0x1000);
        assert_eq!(cpu.state.srr1 & PPC_SRR1_FP_ENABLED_EXCEPTION, 0);
        assert_eq!(cpu.state.pc, PPC_FP_UNAVAILABLE_VECTOR);
        assert_eq!(cpu.pending_exception(), None);
    }

    #[test]
    fn fpu_unavailable_gate_covers_arithmetic_loads_and_integer_transfers() {
        let instructions = [
            (63 << 26) | (21 << 1),  // fadd
            48 << 26,                // lfs
            (31 << 26) | (887 << 1), // lfiwzx
        ];
        for instruction in instructions {
            let mut memory = GuestMemory::new(GuestAddress(0x1000), 4);
            put_instruction(&mut memory, 0x1000, instruction);
            let mut cpu = PpcInterpreter::default();
            cpu.state.pc = 0x1000;
            assert!(matches!(
                cpu.step(&mut memory),
                Err(PpcError::PendingException(
                    PpcException::FloatingPointUnavailable {
                        instruction_pc: 0x1000
                    }
                ))
            ));
        }
    }

    #[test]
    fn rfid_can_restore_msr_fp_and_resume_the_faulting_fpu_instruction() {
        let mut memory = GuestMemory::new(GuestAddress(0x800), 0x804);
        // rfid at the FPU-unavailable vector, then fadd at the saved PC.
        put_instruction(&mut memory, 0x800, (19 << 26) | (18 << 1));
        put_instruction(
            &mut memory,
            0x1000,
            (63 << 26) | (1 << 21) | (2 << 16) | (3 << 11) | (21 << 1),
        );
        let mut cpu = PpcInterpreter::default();
        cpu.state.pc = 0x1000;
        cpu.state.fpr[2] = 2.0;
        cpu.state.fpr[3] = 3.0;

        let delivered = cpu.step_and_dispatch(&mut memory).unwrap().unwrap();
        assert_eq!(
            delivered,
            PpcException::FloatingPointUnavailable {
                instruction_pc: 0x1000
            }
        );
        assert_eq!(cpu.state.pc, PPC_FP_UNAVAILABLE_VECTOR);

        // The guest handler enables FP in its saved context before rfid.
        cpu.state.srr1 |= PPC_MSR_FP;
        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.pc, 0x1000);
        assert_ne!(cpu.state.msr & PPC_MSR_FP, 0);

        cpu.step(&mut memory).unwrap();
        assert_eq!(cpu.state.fpr[1], 5.0);
        assert_eq!(cpu.state.pc, 0x1004);
    }

    #[test]
    fn vmx128_unary_conversions_rounding_and_splats_execute() {
        let mut memory = GuestMemory::new(GuestAddress(0x1000), 16);
        let encode_unary = |vd: u32, vb: u32, immediate: u32, operation: u32| {
            (6 << 26)
                | ((vd & 0x1F) << 21)
                | ((immediate & 0x1F) << 16)
                | ((vb & 0x1F) << 11)
                | (((vd >> 5) & 3) << 2)
                | ((vb >> 5) & 3)
                | (operation << 4)
        };
        put_instruction(&mut memory, 0x1000, encode_unary(0x7D, 0x43, 1, 0b0101011)); // vcsxwfp128
        put_instruction(&mut memory, 0x1004, encode_unary(0x7D, 0x43, 2, 0b1110011)); // vspltw128
        put_instruction(&mut memory, 0x1008, encode_unary(0x7D, 0x43, 0, 0b0110011)); // vrfim128

        let mut cpu = fp_enabled_cpu();
        cpu.state.pc = 0x1000;
        cpu.state.vr[0x43] = [
            0x00, 0x00, 0x00, 0x02, 0xFF, 0xFF, 0xFF, 0xFE, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00,
            0x00, 0x04,
        ];
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [
                0x3F, 0x80, 0x00, 0x00, 0xBF, 0x80, 0x00, 0x00, 0x3F, 0xC0, 0x00, 0x00, 0x40, 0x00,
                0x00, 0x00,
            ]
        );

        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [
                0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00,
                0x00, 0x03
            ]
        );

        cpu.state.vr[0x43] = [
            0x3F, 0xC0, 0x00, 0x00, // 1.5
            0xBF, 0xC0, 0x00, 0x00, // -1.5
            0x40, 0x60, 0x00, 0x00, // 3.5
            0xC0, 0x60, 0x00, 0x00, // -3.5
        ];
        cpu.step(&mut memory).unwrap();
        assert_eq!(
            cpu.state.vr[0x7D],
            [
                0x3F, 0x80, 0x00, 0x00, // floor(1.5) = 1
                0xC0, 0x00, 0x00, 0x00, // floor(-1.5) = -2
                0x40, 0x40, 0x00, 0x00, // floor(3.5) = 3
                0xC0, 0x80, 0x00, 0x00, // floor(-3.5) = -4
            ]
        );
    }

}
