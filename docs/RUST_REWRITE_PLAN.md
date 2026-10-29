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

See [Technical Audit — Phase 1](TECHNICAL_AUDIT_PHASE_1.md) for source-based findings.
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
- [ ] Run formatting, tests, and Clippy in a real Rust toolchain environment.
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
and SUBSYSTEM_INVENTORY.md. GitHub Actions successfully passed format, tests and
Clippy for the initial PPC/memory implementation; each subsequent code change is
being checked again.

### M2 — Memory and executable foundations
- [ ] Add mapped address-space abstraction, endianness-safe access, permissions, and MMIO.
- [ ] Parse executable/container metadata with malformed-input tests.
- [ ] Establish logging, errors, and diagnostics conventions.

### M3 — CPU interpreter
- [ ] Define Xenon/PowerPC architectural state and instruction decoding.
- [ ] Implement instruction groups incrementally with reference tests.
- [ ] Test branches, flags, exceptions, loads/stores, and floating-point behavior.
- [ ] Never silently treat unsupported instructions as successful.

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
with big-endian 32-bit reads/writes; a non-overlapping, permissioned region-based
address space with protect/unmap operations; a memory-access interface shared by
CPU and memory models, including execute-permission checks for instruction fetch; a
deterministic PPC subset (addi, addis, ori, oris, b, cmpwi/cmplwi, CR-only
beq/bne forms, lwz and stw); and a defensive XEX2 fixed-header/optional-header
parser. The CLI can inspect XEX2 metadata. Unsupported CPU opcodes fail explicitly.
This is not a complete Xbox 360 memory model or executable loader: MMIO, aliases,
page-level permissions, decryption/decompression, PE section mapping, kernel
services, concurrency, most PPC instructions and exception semantics remain
future work. The CLI cannot run Xbox 360 games.

## Local validation commands

Run from the repository root:
- cargo fmt --all -- --check
- cargo test --workspace
- cargo clippy --workspace --all-targets -- -D warnings
- cargo run -p xenia-rust

These commands are documented but have not been executed by this change.
