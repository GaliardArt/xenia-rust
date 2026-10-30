# Xenia Canary Rust Rewrite Plan

## Goal

Build a Rust implementation informed by this repository's Xenia Canary codebase.
The existing C++ implementation remains the behavioral reference while Rust
subsystems are reimplemented and tested. This is a staged rewrite, not an
automatic source-to-source translation.

## Repository strategy

- Keep the C++ source, CMake build, and history intact as reference.
- Develop the Rust implementation under rust/ on the rust-rewrite branch.
- Keep Cargo independent from the existing CMake build.
- Do not delete or rename C++ files during early milestones.
- Map responsibilities, dependencies, invariants, and test strategy before
  implementing each subsystem.
- Check source and third-party licenses before reusing code; retain required notices.

## Initial source map

The current baseline is the canary_experimental branch of this fork. The tree is
still the C++/CMake Xenia Canary implementation; this Rust workspace is a parallel
implementation track.

Areas requiring a detailed audit:
- src/xenia/cpu: Xenon/PowerPC execution, decoding, and JIT.
- src/xenia/memory.h and src/xenia/base: guest address space, mappings, permissions, MMIO.
- src/xenia/kernel: threads, synchronization, objects, modules, and guest APIs.
- src/xenia/gpu: command processing, shaders, render targets, graphics backends.
- src/xenia/apu: audio processing and guest audio interfaces.
- src/xenia/hid, src/xenia/vfs, executable loading, UI, configuration, and platform integration.
- third_party and Git submodules: dependency and license inventory.

See [Technical Audit — Phase 1](TECHNICAL_AUDIT_PHASE_1.md) for source-based findings and [XEX Loader Contract](XEX_LOADER_CONTRACT.md) for the verified container layout and safe loading stages.
The audit is preliminary: only a sample of files has been compared against upstream,
and the full repository delta remains open.

## Architecture principles

1. Separate guest-visible behavior from host OS integration.
2. Prefer safe Rust; document and isolate any future unsafe code.
3. Establish reference tests before optimization.
4. Implement a deterministic interpreter before a JIT.
5. Treat guest memory, endianness, exceptions, synchronization, and timing as explicit contracts.
6. Keep graphics translation behind backend interfaces.
7. Make compatibility claims only when backed by reproducible tests.

## Milestones

### M0 — Baseline and workspace
- [x] Preserve the C++ baseline and create a separate Rust branch.
- [x] Add Cargo workspace, core/CLI crates, and checked memory primitive.
- [x] Run formatting, tests, and Clippy in a real Rust toolchain environment.
- [ ] Record exact parent/base commit and inventory all fork-specific changes.

Acceptance: Rust builds independently, tests pass, and the C++ tree remains untouched.

### M1 — Source and behavior audit
- [ ] Inventory all modules, generated code, submodules, and third-party libraries.
- [ ] Compare all tracked files with upstream and inspect fork history/submodule gitlinks.
- [ ] Document CPU, memory, loader, kernel, GPU, audio, HID, and VFS interfaces.
- [ ] Define compatibility tests and legally sourced fixtures.

Progress: the complete recursive tracked tree matches upstream at baseline commit
48b318a2ad7c36f8f3901a94e290ba70ae0ffec5: 2,097 tree entries, zero path/SHA/mode
differences, and 33 identical submodule gitlinks. See TECHNICAL_AUDIT_PHASE_1.md
and SUBSYSTEM_INVENTORY.md. GitHub Actions passed format, workspace tests, Clippy with warnings denied,
and the CLI help smoke test on commit `d99be0c7509808ef818981e264538a72fa8a2e46`.
The current loader extension is being revalidated after its final allocation guard.

### M2 — Memory and executable foundations
- [x] Add mapped address-space abstraction, endianness-safe access, and region permissions; MMIO remains open.
- [x] Parse XEX2 fixed/security metadata and Xbox PE32 headers with malformed-input tests.
- [x] Extract unencrypted XEX2 images (none/basic compression) and map page-descriptor ranges transactionally with code/data/read-only permissions.
- [ ] Establish logging, errors, and diagnostics conventions.

### M3 — CPU interpreter
- [x] Establish the initial Xenon/PowerPC state model and deterministic step/fetch loop.
- [x] Implement a broad but incomplete subset of integer immediate/X-form, branch, indexed/update memory, scalar FP, and VMX operations.
- [x] Keep unsupported opcodes explicit and add focused instruction tests.
- [ ] Generate an opcode-by-opcode coverage manifest from the C++ reference metadata.
- [ ] Resolve known semantic risks (including 64-bit divide decoding, atomic 64-bit memory access, FP/FPSCR fidelity, and VMX128 encoding).
- [ ] Complete branch/CR/XER edge semantics and architectural exception behavior.
- [ ] Expand integer instruction coverage (64-bit arithmetic, remaining update/indexed forms, rotate/shift, multiply/divide, traps and control instructions).
- [ ] Implement full scalar floating-point and Xenon VMX/VMX128 behavior with reference vectors.
- [ ] Differentially validate instruction behavior against the C++ reference.

See [CPU Interpreter Coverage Audit](CPU_INTERPRETER_COVERAGE.md) for the inspected opcode-family inventory, correctness risks, source map, and completion gates.

### M4 — Kernel and runtime
- [ ] Model threads, scheduling, synchronization, virtual objects, and guest API dispatch.
- [ ] Add module loading and required system-call behavior.
- [ ] Track API coverage and known gaps.

### M5 — Graphics, audio, and input
- [ ] Define graphics command/state interfaces and backend boundary.
- [ ] Implement shader translation and rendering incrementally.
- [ ] Add testable audio and controller abstractions.

### M6 — Performance and compatibility
- [ ] Add JIT only after interpreter behavior is well-tested.
- [ ] Add profiling, cache invalidation, and repeatable benchmarks.
- [ ] Build a game-by-game compatibility matrix.

## Current implementation status

The xenia-core crate now contains a contiguous, bounds-checked memory region
with big-endian byte, halfword and word access; a non-overlapping, permissioned
region-based address space with protect/unmap operations; a memory-access
interface shared by CPU and memory models, including execute-permission checks
for instruction fetch; and a deterministic but still partial PPC interpreter.
Its current instruction groups include integer immediate and X-form arithmetic/logical/shift operations, conditional and unconditional branches, CR bit logic, indexed and update-form memory operations, a partial scalar floating-point subset, and a small standard VMX subset. This broader subset remains incomplete and has known semantic risks; see [CPU Interpreter Coverage Audit](CPU_INTERPRETER_COVERAGE.md).
 Both parsers have malformed-input tests; the CLI can inspect XEX2 and
PE metadata. Unsupported CPU opcodes fail explicitly.
This is not a complete Xbox 360 memory model or executable loader: MMIO, aliases,
page-granular MMU semantics, AES decryption, normal/LZX compression, XEX patching,
import resolution, kernel services, concurrency, most PPC instructions and
exception semantics remain future work. The current page-descriptor mapper is a
first safe loading stage, not title boot support. The CLI cannot run Xbox 360 games. Current CI runs `cargo fmt`, workspace tests,
Clippy with warnings denied, and a CLI help smoke test.

## Local validation commands

Run from the repository root:
- cargo fmt --all -- --check
- cargo test --workspace
- cargo clippy --workspace --all-targets -- -D warnings
- cargo run -p xenia-rust

These commands are documented but have not been executed by this change.
