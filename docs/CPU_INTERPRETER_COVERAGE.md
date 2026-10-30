# CPU Interpreter Coverage Audit

Branch: `rust-rewrite`  
Reference implementation: `canary_experimental` (C++ Xenia PPC backend)  
Audit basis: current `rust/xenia-core/src/cpu.rs` and the reference PPC emitter/metadata sources.

## Executive status

The Rust CPU is no longer limited to the original integer skeleton: it currently contains
integer immediate and X-form operations, conditional/unconditional branches, several
D-form and indexed memory operations, scalar floating-point subsets, and a small VMX
subset. It is still **far from a complete Xenon interpreter**. There is no reliable
whole-ISA opcode coverage percentage yet: the implementation has no generated opcode
manifest or exhaustive decode table against which to calculate one.

The interpreter must continue to reject unknown instructions. A recognized primary
opcode is not proof that all extended opcodes, operand forms, record/overflow variants,
or Xenon-specific encodings are implemented correctly.

## Current implementation inventory

The following inventory describes the Rust dispatch as inspected, not a claim of full
architectural conformance.

| Family | Present subset | Important gaps |
|---|---|---|
| Integer immediate | addi, addis, mulli, subfic, addic/addic., cmpwi, cmplwi; ori/oris, xori/xoris, andi./andis. | addic carry edge semantics need vectors; other immediate arithmetic/logical forms and traps are absent. |
| Rotate/mask | rlwimi, rlwinm, rlwnm | 64-bit rotate/mask forms (rld*) and exhaustive mask/record tests absent. |
| Integer compare | cmpw/cmplw and immediate forms | cmpd/cmpld field/L-bit handling and complete CR semantics need validation. |
| Integer arithmetic | add, addc, adde, subf, subfc, subfe, neg, mullw, mulhw/mulhwu, divw/divwu subsets | 64-bit arithmetic/divide, addme/addze/subfme/subfze, mullwo variants, exact OE/Rc behavior and divide corner cases are incomplete. |
| Integer logical | and/andc/nor/eqv/xor/or/orc/nand; immediate forms | exhaustive operand-field, Rc and 64-bit semantics tests absent. |
| Shift/count/sign extension | slw, srw, sraw, srawi, cntlzw/cntlzd, extsb/extsh/extsw | 64-bit shifts and complete CA edge cases require validation; current shift code needs review against ISA vectors. |
| Branch/control | b, bc, bclr, bcctr; CR bit logical subset | traps, syscall/system instructions, SPR/TBR/MSR, synchronization, complete BO/BI combinations and exception semantics absent. |
| Integer memory | lbz/lhz/lha/stb/sth/lwz/stw; D-form update loads/stores; indexed byte/halfword/word forms and updates | lmw/stmw, string operations, byte-reverse, reservation/conditional, DS-form, 64-bit loads/stores, alignment exceptions, and complete atomicity are absent. |
| Scalar FP memory | lfs/lfd/stfs/stfd and indexed forms | FPSCR effects, NaN payloads, denormal/rounding behavior, precise FP exceptions and atomic 64-bit memory access need work. |
| Scalar FP execution | small arithmetic/compare/conversion/select subset | current operations are approximations: FPSCR is not modeled architecturally, conversion/rounding is incomplete, single-precision instruction distinctions and exception flags need validation. |
| VMX/AltiVec | selected byte/halfword/word add/sub and basic logical operations; aligned vector load/store | most VMX instructions absent: compares, saturation, permute, pack/unpack, shifts, floating-point vector ops, VSCR semantics, and Xenon VMX128 encodings/register operands. |
| Exceptions | memory errors and unsupported-opcode errors surface to caller | no architectural program/data/alignment/FPU exceptions, interrupt state, exception vectors, or precise fault/retirement model. |

## Correctness risks found during inspection

These are audit findings to resolve before treating the corresponding family as complete. Resolved findings are retained here as regression history:

1. **64-bit divide decode/semantics — corrected:** the initial implementation confused OE-enabled XO encodings with separate operations and truncated 64-bit operands. The Rust decoder now distinguishes `divdu` (XO 457), `divwu` (459), `divd` (489), and `divw` (491), including their OE-enabled encodings. Regression tests cover high-bit operands, word truncation, divide-by-zero, and signed minimum divided by -1. CI passed on commit `5daf1e6b2346252193a7738d89041ebd88477408`.
2. **Floating-point is approximate:** FPR values are represented as host `f64`; current conversions and arithmetic do not implement full FPSCR, rounding modes, exception flags, NaN behavior, or Xenon-specific details. Do not use this subset as evidence of game compatibility.
3. **64-bit FP memory operations are composed from two 32-bit accesses:** this can partially mutate/read state if the second access faults. Add a checked atomic guest-memory 64-bit operation before claiming these forms correct.
4. **VMX register model is incomplete:** the state allocates 128 vector slots, but the current primary-opcode-4 decoder uses ordinary 5-bit register fields and a small set of standard VMX XO values. This does not implement the Xenon VMX128 instruction encoding space.
5. **Instruction coverage is not machine-counted:** dispatch is hand-written and currently groups many operations under primary opcodes. A generated manifest derived from the C++ opcode metadata/lookup tables is required for defensible coverage reporting.

## Reference source map

The original C++ implementation remains unchanged and is the behavioral reference:

- `src/xenia/cpu/ppc/ppc_opcode_info.h`: opcode formats, groups, and operand fields.
- `src/xenia/cpu/ppc/ppc_opcode_info.cc`, `ppc_opcode_lookup_gen.cc`, `ppc_opcode_table_gen.cc`: opcode metadata and generated lookup/table definitions.
- `src/xenia/cpu/ppc/ppc_emit_alu.cc`: integer arithmetic/logical/shift emitters.
- `src/xenia/cpu/ppc/ppc_emit_control.cc`: branch, control, trap, and system behavior.
- `src/xenia/cpu/ppc/ppc_emit_memory.cc`: load/store and memory semantics.
- `src/xenia/cpu/ppc/ppc_emit_fpu.cc`: scalar floating-point behavior.
- `src/xenia/cpu/ppc/ppc_emit_altivec.cc`: VMX/AltiVec behavior.
- `src/xenia/cpu/ppc/ppc_context.h`: architectural register/context layout.

## Required completion gates

The CPU milestone is not complete until all of the following are true:

- [ ] Build a machine-readable inventory of the C++ opcode table and map every relevant opcode to implemented, intentionally unsupported, or unverified.
- [ ] Correct the known 64-bit divide issue and add focused regression vectors.
- [ ] Establish atomic 64-bit and vector memory access contracts with permission/bounds tests.
- [ ] Complete integer arithmetic, rotate/shift, load/store, branch, CR/XER and control families used by Xenon titles.
- [ ] Implement scalar FPU semantics including FPSCR and architecturally relevant exceptions.
- [ ] Implement the required Xenon VMX/VMX128 instruction set and VSCR behavior.
- [ ] Add deterministic per-instruction tests and differential vectors against the C++ reference.
- [ ] Model architectural exceptions and precise state updates for faults.
- [ ] Run formatting, workspace tests, Clippy, and CI successfully; document any intentionally unsupported instructions with evidence.
- [ ] Demonstrate high coverage by opcode count and semantic test coverage, not by counting primary-opcode arms.

Only after these gates are met should work move to kernel/runtime integration.
