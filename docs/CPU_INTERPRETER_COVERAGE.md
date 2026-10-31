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
| Integer arithmetic | add, addc, adde, addme, addze, subf, subfc, subfe, subfme, subfze, neg, mullw, mulhw/mulhwu/mulhd/mulhdu, divw/divwu/divd/divdu | remaining 64-bit arithmetic families, mullwo variants, exhaustive OE/Rc behavior, and other divide edge cases are incomplete. |
| Integer logical | and/andc/nor/eqv/xor/or/orc/nand; immediate forms | exhaustive operand-field, Rc and 64-bit semantics tests absent. |
| Shift/count/sign extension | slw, srw, sraw, srawi, cntlzw/cntlzd, extsb/extsh/extsw | 64-bit shifts and complete CA edge cases require validation; current shift code needs review against ISA vectors. |
| Branch/control | b, bc, bclr, bcctr; CR bit logical subset; mfspr/mtspr for XER/LR/CTR | traps, syscall/system instructions, remaining SPR/TBR/MSR, synchronization, complete BO/BI combinations and exception semantics absent. |
| Integer memory | lbz/lhz/lha/stb/sth/lwz/stw; D-form update loads/stores; indexed byte/halfword/word forms and updates; lmw/stmw | string operations, byte-reverse, reservation/conditional, DS-form, integer 64-bit loads/stores, alignment exceptions, and complete store-range atomicity are absent. |
| Scalar FP memory | lfs/lfd/stfs/stfd and indexed forms | FPSCR effects, NaN payloads, denormal/rounding behavior, precise FP exceptions and atomic 64-bit memory access need work. |
| Scalar FP execution | fadd/fadds, fsub/fsubs, fdiv/fdivs, fmul/fmuls, fsel, fsqrt/fsqrts, fres, frsqrte, fmadd/fmsub/fnmadd/fnmsub and single variants; fabs/fmr/fnabs/fneg/frsp; fctiw/fctiwz/fctid/fctidz/fcfid; fcmpu/fcmpo; mcrfs/mffs/mtfsb0/mtfsb1/mtfsf/mtfsfi | FPSCR core status, FPRF, field moves, sticky summaries, invalid subcauses, conversion rounding, and single-result directed rounding are implemented. Exact binary64 directed rounding, full FR behavior, precise exception delivery, NaN payload fidelity, and hardware reciprocal estimates remain incomplete. |
| VMX/AltiVec + Xenon VMX128 | The current dispatch has substantial classic VMX coverage: wrapping and saturating lane arithmetic, carry/borrow, averages, min/max, multiply/widen, multiply-sum, compares, shifts/rotates, splats, merges, packs/unpacks, vector FP arithmetic/conversions/estimates, permute/select, VSCR transfers, and a subset of vector memory operations. VMX128 paths exist under primary opcodes 4, 5, and 6, including 7-bit register reconstruction, byte permute, selected FP/integer arithmetic, pack/unpack and D3D conversions, plus indexed memory forms. | This is not yet a complete VMX/VMX128 implementation. Many reference-table entries remain absent or not proven connected to the correct decoder selector. Classic partial element loads/stores (lvebx/lvehx/lvlx/lvrx and store-left/right families), additional multiply/merge/sum variants, and multiple VMX128 operations (including word/byte shift variants, comparisons, floating estimates/rounding and other special forms) need exact per-mnemonic auditing. Some element-memory paths still perform multiple byte accesses. Full VMX/VMX128 vector stores now use the contiguous-write interface; the concrete GuestMemory implementation preflights the complete range before modifying RAM. Other GuestMemoryAccess implementations must override write_bytes to provide the same all-or-nothing guarantee (the trait fallback is sequential). VSCR behavior, NaN/rounding edge cases, record forms, and all register/immediate field encodings still require differential validation. |
| Exceptions | memory errors and unsupported-opcode errors surface to caller | no architectural program/data/alignment/FPU exceptions, interrupt state, exception vectors, or precise fault/retirement model. |

## VMX/AltiVec and Xenon VMX128 focused audit (2026-10-31)

The reference Xenia opcode table at `src/xenia/cpu/ppc/ppc_opcode_table_gen.cc`
contains **233 VMX/VMX128-related instruction entries** when classic vector
memory, VMX arithmetic, and VMX128 forms are counted together. This is the
reference inventory size, **not** a Rust coverage percentage: aliases, encoding
formats, record variants, and the distinction between classic VMX and Xenon
VMX128 must be resolved per entry before a denominator is meaningful.

### Confirmed Rust dispatch families from source inspection

- **Classic VMX integer:** byte/halfword/word wrapping add/subtract; saturating
  add/subtract; carry/borrow; rounded averages; signed/unsigned min/max;
  widening even/odd multiply; six multiply-sum forms; lane comparisons;
  shifts/rotates; immediate and indexed splats; merges; pack/unpack; and
  integer/FP conversion families.
- **Classic VMX floating point:** add/subtract, reciprocal and reciprocal-square-
  root estimates, exponent/log estimates, rounding operations, min/max, and
  selected fused multiply-add/subtract forms.
- **Classic VMX rearrangement/state:** permute, select, shift-left-double-octet,
  VSCR read/write, and SAT updates in selected saturating paths.
- **Classic vector memory:** aligned indexed vector load/store and some element
  memory forms. This does not establish full coverage of all classic partial
  left/right and element load/store instructions.
- **VMX128 register/encoding support:** a 128-entry vector register file and
  split high-bit reconstruction; dedicated primary-opcode paths for VMX128
  memory, arithmetic, conversions, byte permutation, word permutation immediate,
  and selected pack/unpack operations.
- **VMX128 floating point/integer:** selected binary32 arithmetic, fused
  operations, dot/multiply-sum forms, logical operations, select, byte shifts,
  saturating/modulo pack operations, conversion/rounding/estimate unary forms,
  and D3D packed vertex conversions.

### Confirmed gaps and risks (do not count as implemented)

1. The reference table has many classic instructions that are not represented by
   an obvious current dispatch arm, including several element/partial vector
   memory forms and selected multiply, merge, unpack, and sum operations.
2. VMX128 is split across distinct VX128 formats (VX128_1 through VX128_5 and
   predicate/permutation forms). A generic 7-bit register decoder does not by
   itself prove that each instruction's selector and immediate fields are
   decoded correctly.
3. Some vector memory paths issue multiple byte reads/writes and update state
   incrementally. A fault late in the operation can therefore expose partial
   architectural effects; transactional vector memory semantics are not yet
   established.
4. Floating-point host operations and estimates are not a differential oracle.
   NaN payloads, signed zero, subnormals, rounding, fused behavior, and exception
   flags need explicit reference vectors.
5. Existing unit tests cover representative paths and exhaustive VMX128 register
   indices, but do not yet constitute one test per reference-table instruction,
   all encoding variants, or a differential suite against Xenia/Xenon behavior.

### Evidence levels used for this family

- **Present in dispatch:** an opcode/selector path exists in `cpu.rs`.
- **Unit-tested in source:** a Rust test exists; this does not mean it has run.
- **Executed locally/CI:** test execution is confirmed by an actual run record.
- **Differentially validated:** outputs and side effects match a trusted reference
  for deterministic vectors.

As of this audit, the repository HEAD is
`97484a7e8a645cdea356765a3576f104078c1a2a`. GitHub reports no status checks
and no associated workflow runs for that commit. Therefore no claim is made
here that the current VMX tests passed in CI or were executed locally.

## Correctness risks found during inspection

These are audit findings to resolve before treating the corresponding family as complete. Resolved findings are retained here as regression history:

1. **64-bit divide decode/semantics — corrected:** the initial implementation confused OE-enabled XO encodings with separate operations and truncated 64-bit operands. The Rust decoder now distinguishes `divdu` (XO 457), `divwu` (459), `divd` (489), and `divw` (491), including their OE-enabled encodings. Regression tests cover high-bit operands, word truncation, divide-by-zero, and signed minimum divided by -1. CI passed on commit `5daf1e6b2346252193a7738d89041ebd88477408`.
2. **Floating-point fidelity — substantially expanded, still incomplete:** the interpreter now models the principal FPSCR status/control paths and scalar FP instruction subset, but host `f64` arithmetic still rounds binary64 operations using the host's default mode. Exact binary64 directed rounding, full FR/FI behavior, precise FP exception delivery, NaN payload preservation, and Xenon-specific reciprocal estimates still need work. Do not use this subset as evidence of game compatibility.
3. **64-bit FP memory operations — corrected:** the original pair of 32-bit accesses could partially complete if the second access faulted. `GuestMemoryAccess` now exposes checked big-endian 64-bit reads/writes; contiguous memory and mapped address space implement them as single range-checked operations. The interpreter's `lfd/stfd` and indexed equivalents now use these operations. Tests cover endianness, short ranges, permissions, and cross-region rejection.
4. **VMX register model is incomplete:** the state allocates 128 vector slots, but the current primary-opcode-4 decoder uses ordinary 5-bit register fields and a small set of standard VMX XO values. This does not implement the Xenon VMX128 instruction encoding space.
5. **Instruction coverage is not machine-counted:** dispatch is hand-written and currently groups many operations under primary opcodes. A generated manifest derived from the C++ opcode metadata/lookup tables is required for defensible coverage reporting.

6. **Multiply-high arithmetic — corrected/expanded:** `mulhwu` now uses a full-width unsigned intermediate (avoiding `i64` overflow for large `u32` operands); `mulhw`, `mulhdu`, and `mulhd` are implemented with correctly sized signed/unsigned intermediates. Regression vectors cover maximum unsigned operands and signed negative products.
7. **Multiple-register memory — added:** `lmw` and `stmw` transfer GPR ranges in big-endian word order. `lmw` stages all reads before updating the register file, so a later read fault does not partially mutate GPR state. `stmw` still performs sequential stores and does not yet provide an all-range transaction.
8. **Floating-point CR1 — corrected:** record-form FP operations now copy FPSCR[0:3] into CR1 instead of synthesizing CR1 from the result's sign. Full FPSCR updates and exception behavior remain unimplemented.

9. **Special-purpose register transfers — partial:** `mfspr`/`mtspr` now handle the XER, LR, and CTR registers represented in `PpcState`. Other SPRs, time-base registers, privilege checks, and system-register semantics remain unsupported.

10. **Scalar FPU/FPSCR — core execution and control paths implemented:** scalar arithmetic, fused operations, compare, select, square root/reciprocal operations, single rounding, integer conversions, and the FPSCR control family (mcrfs, mffs, mtfsb0, mtfsb1, mtfsf, mtfsfi) are implemented. FPSCR now tracks FX/FEX/VX/OX/UX/ZX/XX, the supported invalid-operation subcauses, FI, and the FPRF result/compare classification. FEX is recomputed from sticky exception flags and their enables; Rc copies FPSCR[0:3] into CR1. FPSCR rounding modes are applied to integer conversions and binary32 result rounding. Regression coverage includes directed frsp rounding, FPRF classes, NaN compare behavior, invalid-operation subcauses, and enabled divide-by-zero. The C++ Xenia HIR builder itself leaves much of UpdateFPSCR stubbed, so its output is not a complete oracle for FPSCR flags. Remaining gaps: exact binary64 directed rounding, complete FR semantics, precise exception/trap delivery, NaN payload preservation, and Xenon-specific reciprocal estimates. The workspace CI passed with 80 tests on the implementation commit.

## Reference source map

The original C++ implementation remains unchanged and is the behavioral reference:

- `src/xenia/cpu/ppc/ppc_opcode_info.h`: opcode formats, groups, and operand fields.
- `src/xenia/cpu/ppc/ppc_opcode_info.cc`, `ppc_opcode_lookup_gen.cc`, `ppc_opcode_table_gen.cc`: opcode metadata and generated lookup/table definitions.
- `src/xenia/cpu/ppc/ppc_emit_alu.cc`: integer arithmetic/logical/shift emitters.
- `src/xenia/cpu/ppc/ppc_emit_control.cc`: branch, control, trap, and system behavior.
- `src/xenia/cpu/ppc/ppc_emit_memory.cc`: load/store and memory semantics.
- `src/xenia/cpu/ppc/ppc_emit_fpu.cc`: scalar floating-point instruction behavior.
- `src/xenia/cpu/ppc/ppc_hir_builder.cc`: reference FPSCR helpers; UpdateFPSCR is intentionally stubbed in this C++ path and must not be treated as a complete exception-status oracle.
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
