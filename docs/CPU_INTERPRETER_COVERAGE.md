# CPU Interpreter Coverage Audit

Branch: `rust-rewrite`  
Reference implementation: `canary_experimental` (C++ Xenia PPC backend)  
Audit basis: current `rust/xenia-core/src/cpu.rs` and the reference PPC emitter/metadata sources.

## Executive status

The Rust CPU is no longer limited to the original integer skeleton: it currently contains
integer immediate and X-form operations, conditional/unconditional branches, several
D-form and indexed memory operations, scalar floating-point subsets, and broad VMX/
VMX128 dispatch coverage. It is still **far from a complete Xenon interpreter**.
There is no reliable whole-ISA coverage percentage yet; the VMX-specific inventory
is tracked separately from the remaining PowerPC/Xenon instruction families.

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
| VMX/AltiVec + Xenon VMX128 | All 239 entries in the focused Xenia reference inventory now map to Rust dispatch paths: 144 classic VMX arithmetic/logical/compare entries, 20 classic X-form vector-memory entries, and 75 Xenon VMX128 entries. Coverage includes primary opcodes 4/5/6, 128 vector registers, split register fields, all listed vector-memory selectors, arithmetic, permutation, conversion, pack/unpack, and VSCR paths. | This is **dispatch coverage, not semantic conformance**. VSCR.NJ denormal handling, exact hardware estimate results, floating NaN/signed-zero/rounding behavior, D3D pack/unpack lane placement, and full per-instruction differential validation remain open. The manifest is docs/ppc/vmx_opcode_audit.csv. GuestMemory implements checked contiguous reads/writes; generic GuestMemoryAccess fallbacks are sequential unless a backend overrides them. |
| Exceptions | memory errors and unsupported-opcode errors surface to caller | no architectural program/data/alignment/FPU exceptions, interrupt state, exception vectors, or precise fault/retirement model. |

## VMX/AltiVec and Xenon VMX128 focused audit (2026-10-31)

The focused inventory is derived from `src/xenia/cpu/ppc/ppc_opcode_table_gen.cc` and cross-checked
against `src/xenia/cpu/ppc/ppc_opcode_lookup_gen.cc`. It contains **239 unique VMX-related
mnemonics**:

| Group | Entries | Reference scope |
|---|---:|---|
| Classic VMX arithmetic, logic, compare, rearrangement and state | 144 | Primary opcode 4, non-VMX128 formats |
| Classic VMX X-form vector memory | 20 | Primary opcode 31: element, aligned, left/right, and VSCR-related memory forms |
| Xenon VMX128 | 75 | Primary opcodes 4 (17), 5 (25), and 6 (33), including 16 VMX128 memory forms |
| **Total** | **239** | Focused VMX/VMX128 reference inventory |

The complete machine-readable list is `docs/ppc/vmx_opcode_audit.csv`. Each row records
the mnemonic, opcode, encoding format, selector/decoder key, dispatch status, and
explicitly leaves per-mnemonic test and differential status unclaimed.

### Dispatch audit result

**All 239 inventory entries have a corresponding Rust dispatch path** in
`rust/xenia-core/src/cpu.rs`. This is 100% dispatch mapping against this focused
reference inventory; it is not 100% architectural implementation or conformance.
The hand-written interpreter has grouped match arms, so the audit checked selector
formulas and opcode-table lookup keys rather than merely searching for mnemonic text.

The audit verified these decoder families:

- Primary opcode 4 classic VX/VC/VA operations, including VC record fields.
- Primary opcode 4 VMX128 memory selectors, reconstructed from instruction bits
  21..27 and 30..31 exactly as the Xenia lookup does.
- Primary opcode 4 `vsldoi128` special selector.
- Primary opcode 5 VMX128 operation selectors and `vperm128` special form.
- Primary opcode 6 VMX128 unary, compare, shift, permutation-immediate, D3D pack,
  and rotate-insert forms.
- Primary opcode 31 classic vector X-form memory selectors.
- VMX128 7-bit register reconstruction for VD/VA/VB fields, with exhaustive
  0..127 register-index tests.

### Confirmed corrections from this audit

1. **VMX128 memory selector extraction:** corrected the shift used to construct the
   VX128_1 selector so it matches the Xenia lookup key. The prior implementation
   and its test encoder shared the same wrong shift and could make tests pass with
   noncanonical instruction encodings.
2. **`stvewx128`:** corrected selector 387 to store the address-selected 32-bit
   vector lane to memory. Selector 131 (`lvewx128`) remains a full aligned vector
   load, matching the Xenia emitter.
3. **`vrlimi128`:** corrected rotation to rotate whole 32-bit words across the
   vector, not bytes within each word. Regression expectations now use the
   reference word rotation.
4. **`vpkd3d128` word placement:** corrected the packed low-word placement and the mode/shift byte selection to mirror Xenia's `MakePermuteMask` controls, including the special shift-3 cases for 32-bit and 64-bit packed formats. Regression tests cover these selector combinations.
5. **Classic `vpkpx`:** corrected the one-bit alpha extraction to test the source alpha byte for nonzero, rather than only its most significant bit. Added the Xenia reference fixture for all eight packed pixels, plus lane-order checks for `vupkhpx`/`vupklpx`.
6. **`vcmpbfp` and `vcmpbfp128`:** NaN lanes now set both high result bits, and
   CR6 record summaries are based on the out-of-bounds predicate. Added tests for
   NaN masks and all-in/all-out/mixed summaries.
7. **Classic partial vector memory:** implemented `lvlx/lvlxl`, `lvrx/lvrxl`,
   `stvlx/stvlxl`, and `stvrx/stvrxl`. Partial loads read the full aligned block;
   partial stores use full-block read/modify/write, matching the reference emitter's
   memory shape. Full vector stores use contiguous writes.
8. **Contiguous memory interface:** `GuestMemoryAccess` now has caller-buffer
   `read_bytes` and `write_bytes` operations. The concrete `GuestMemory` checks the
   complete range before copying. Default trait fallbacks remain sequential, so
   future mapped/MMIO backends must override them where all-range behavior is
   required.

### Remaining semantic risks (not dispatch gaps)

1. **VSCR.NJ:** vector non-Java/flush-to-zero behavior is not yet applied
   consistently to vector floating-point inputs and results. SAT sticky updates
   exist in selected saturating arithmetic, pack, and conversion paths, but the
   complete VSCR state/side-effect model still needs differential vectors.
2. **Hardware estimates:** `vrefp`, `vrsqrtefp`, `vexptefp`, and `vlogefp` paths use
   host floating-point math. They do not reproduce Xenon estimate tables,
   approximation error, or every special-value result bit-for-bit.
3. **Floating-point fidelity:** NaN payload propagation, signed zero, subnormals,
   rounding, fused multiply-add/subtract behavior, and estimate/conversion edge
   cases are not fully differentially validated.
4. **D3D pack/unpack:** independent Xenia reference vectors now cover D3D color
   and FLOAT16_4 packing, plus D3D color, SHORT_2, FLOAT16_2, and FLOAT16_4
   unpacking. The remaining packed formats (including 2_10_10_10 and 4_20_20_20)
   and the complete mask/shift cross-product still need independent fixtures. A
   round trip between two Rust paths is not proof of architectural correctness.
5. **Memory backends:** the concrete `GuestMemory` implementation preflights
   contiguous ranges. The trait's default `read_bytes`/`write_bytes` implementations
   are sequential; another backend must override them to guarantee the same
   transaction/fault behavior.
6. **Per-instruction tests:** 192 workspace tests pass, but there is not yet one
   independent deterministic test for every mnemonic, operand form, immediate
   value, record variant, and boundary condition. The CSV intentionally marks
   per-mnemonic and differential validation as unverified.

### Evidence levels

- **Dispatch present:** the reference opcode/selector has a Rust dispatch path.
- **Unit-tested:** a Rust regression test exercises the relevant path; tests must
  still be executed before claiming a pass.
- **CI executed:** a GitHub Actions run completed successfully.
- **Differentially validated:** results and side effects match an independent,
  trusted Xenon/reference oracle for deterministic vectors.

Validation update (2026-10-31): commit
`712b51077f635eb4236444cf665da05df8f49bf8` passed the Rust rewrite GitHub
Actions workflow. Formatting, `cargo test --workspace --locked` (192 tests),
Clippy with warnings denied, and CLI help smoke test all passed. This validates
the workspace suite, not per-opcode differential conformance or hardware accuracy.
## Correctness risks found during inspection

These are audit findings to resolve before treating the corresponding family as complete. Resolved findings are retained here as regression history:

1. **64-bit divide decode/semantics — corrected:** the initial implementation confused OE-enabled XO encodings with separate operations and truncated 64-bit operands. The Rust decoder now distinguishes `divdu` (XO 457), `divwu` (459), `divd` (489), and `divw` (491), including their OE-enabled encodings. Regression tests cover high-bit operands, word truncation, divide-by-zero, and signed minimum divided by -1. CI passed on commit `5daf1e6b2346252193a7738d89041ebd88477408`.
2. **Floating-point fidelity — substantially expanded, still incomplete:** the interpreter now models the principal FPSCR status/control paths and scalar FP instruction subset, but host `f64` arithmetic still rounds binary64 operations using the host's default mode. Exact binary64 directed rounding, full FR/FI behavior, precise FP exception delivery, NaN payload preservation, and Xenon-specific reciprocal estimates still need work. Do not use this subset as evidence of game compatibility.
3. **64-bit FP memory operations — corrected:** the original pair of 32-bit accesses could partially complete if the second access faulted. `GuestMemoryAccess` now exposes checked big-endian 64-bit reads/writes; contiguous memory and mapped address space implement them as single range-checked operations. The interpreter's `lfd/stfd` and indexed equivalents now use these operations. Tests cover endianness, short ranges, permissions, and cross-region rejection.
4. **VMX semantic conformance remains incomplete:** the focused 239-entry VMX/VMX128 inventory now has dispatch mappings and 128-register reconstruction, but estimate precision, VSCR.NJ, D3D pack/unpack details, and per-opcode differential behavior remain unverified.
5. **Whole-CPU instruction coverage is not machine-counted:** the VMX-specific CSV inventory is checked in, but it is not yet generated or validated automatically in CI. The remaining PowerPC/Xenon families still need their own opcode manifests.

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
