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
- Memory and base/platform code: guest address space, mappings, permissions, MMIO.
- src/xenia/kernel: threads, synchronization, objects, modules, and guest APIs.
- src/xenia/gpu: command processing, shaders, render targets, graphics backends.
- src/xenia/apu: audio processing and guest audio interfaces.
- HID/input, VFS, executable loader, UI, configuration, and platform integration.
- third_party and Git submodules: dependency and license inventory.

This is a high-level map, not a completed audit. Fork-specific changes still need
to be compared with the parent repository.

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
- [ ] Record exact parent/base commit and inventory fork-specific changes.

Acceptance: Rust builds independently, tests pass, and the C++ tree remains untouched.

### M1 — Source and behavior audit
- [ ] Inventory modules, generated code, submodules, and third-party libraries.
- [ ] Compare this fork with its parent to identify Canary-specific changes.
- [ ] Document CPU, memory, loader, kernel, GPU, and audio interfaces.
- [ ] Define compatibility tests and legally sourced fixtures.

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

The xenia-core crate currently contains only a contiguous, bounds-checked byte
region with unit tests. It is not a complete Xbox 360 memory model: virtual
mapping, permissions, MMIO, multi-region access, and concurrency remain future work.
The xenia-rust executable is a diagnostic shell and cannot run Xbox 360 games.

## Local validation commands

Run from the repository root:
- cargo fmt --all -- --check
- cargo test --workspace
- cargo clippy --workspace --all-targets -- -D warnings
- cargo run -p xenia-rust

These commands are documented but have not been executed by this change.
