# Technical Audit — Phase 1

Date: 2026-10-29  
Audited baseline: `canary_experimental`  
Working branch: `rust-rewrite`

## Scope and confidence

This is a source/build audit with a complete tracked-tree comparison against upstream at the audited ref. It is not yet a full semantic review of every source file or a game-compatibility report. Local toolchain execution is unavailable in this environment; validation is being performed through GitHub Actions.

## 1. Fork delta against upstream Canary

A recursive Git tree comparison was performed between `GaliardArt/xenia-rust` and `xenia-canary/xenia-canary`, both at `canary_experimental`. Both trees resolve to commit/tree SHA `48b318a2ad7c36f8f3901a94e290ba70ae0ffec5`, with 2,097 entries each. There were zero paths unique to either repository, zero blob/gitlink SHA differences, and zero mode/type differences. All 33 third-party submodule gitlinks match exactly. The repositories point to the same upstream commit at this ref.

The following representative files were also fetched individually from both repositories; their blob SHAs match exactly:

- `CMakeLists.txt`
- `src/xenia/cpu/CMakeLists.txt`
- `src/xenia/gpu/command_processor.cc`
- `src/xenia/kernel/CMakeLists.txt`
- `src/xenia/apu/CMakeLists.txt`
- `.gitmodules`
- `src/xenia/memory.h`

Conclusion: at `canary_experimental`, the tracked source tree and submodule pointers are identical to upstream. The fork's prior work is therefore represented by its upstream history at this branch, not a tracked-source divergence. This comparison does not compare untracked local files or private configuration; the GitHub tree endpoint reported no truncation.

## 2. Subsystem findings

### Guest memory — highest foundational risk

Source inspected: `src/xenia/memory.h`.

The existing model is substantially more than a byte buffer. It includes:
- Distinct guest virtual and physical address spaces/heaps and multiple host mappings/views.
- Page-based allocation state, reservation/commit, protection flags, and range-access queries.
- MMIO integration and access-violation handling.
- Physical-memory invalidation/read callbacks, relevant to CPU/JIT and GPU-visible data.
- A system heap, heap lookup/statistics, and save/restore support.

Rust implication: keep the current contiguous `GuestMemory` as a tiny testable primitive only. Do not use it as the final address-space design. The next memory design should separate address translation, region/page metadata, access permissions, MMIO dispatch, and invalidation events. Overflow and cross-region accesses need explicit tests. Memory callbacks and invalidation semantics must be designed before a JIT or GPU cache is introduced.

### CPU / PowerPC

Source inspected: `src/xenia/cpu/CMakeLists.txt` and `src/xenia/cpu/ppc/ppc_context.h`.

The CPU module is split across backend, compiler, HIR, and PPC layers. The PPC context defines architectural register slots for 32 GPRs, 32 FPRs, LR, CTR, XER, FPSCR, VSCR, condition-register fields, and vector registers. This points to a large architectural surface beyond opcode decoding: state layout, flags, exceptions, vector/floating-point semantics, and execution integration all matter.

Rust implication: start with a documented architectural state and a deterministic interpreter. Use table-driven instruction tests and compare state changes against known/reference behavior. JIT work should wait until interpreter semantics and memory invalidation are reliable.

### GPU

Sources inspected: `src/xenia/gpu/command_processor.cc` and `src/xenia/gpu/CMakeLists.txt`.

The command processor is not just a command parser: it owns a worker thread, register restoration and write callbacks, tracing, shader-storage hooks, and synchronization with kernel/threading facilities. The build links shader translation and graphics-related dependencies; Vulkan is conditionally built on non-Apple platforms, while platform UI/rendering modules are configured elsewhere.

Rust implication: define a backend-independent command/state model first. Keep host graphics API objects behind a backend boundary, and preserve command ordering, synchronization, resource lifetime, and guest-visible register behavior in tests.

### Kernel/runtime

Source inspected: `src/xenia/kernel/CMakeLists.txt` and `src/xenia/CMakeLists.txt`.

The kernel links CPU, APU, base, HID, VFS, compression, XML, and crypto dependencies. It is therefore a central integration layer, not a small syscall shim. Threading, waits, events, handles, virtual objects, module loading, and guest API dispatch need explicit contracts and deterministic tests.

Rust implication: implement kernel primitives in layers, beginning with errors/handles and synchronization abstractions, before broad API coverage. Avoid coupling guest semantics directly to Windows/Linux host APIs.

### Audio

Source inspected: `src/xenia/apu/CMakeLists.txt`.

The APU links FFmpeg libraries and Xenia base. Platform-specific audio backends are configured separately (including SDL and platform modules in the top-level source CMake). Guest audio behavior, codec/decode paths, buffering, timing, and host output need to be treated as separate concerns.

Rust implication: establish a testable guest audio pipeline and backend trait; do not make the first audio milestone depend on a particular host output device.

### Build, platform, and third-party dependencies

Sources inspected: root `CMakeLists.txt`, `src/xenia/CMakeLists.txt`, `third_party/CMakeLists.txt`, and `.gitmodules`.

The existing C++ project uses CMake/C++20, Python-based shader tooling, platform-specific branches, and many third-party git submodules (including LLVM-related CPU support, FFmpeg, Vulkan/shader tooling, SDL, and utility libraries). The root build has explicit x86-64/AArch64 decisions and platform-specific system dependencies.

Rust implication:
- Keep Cargo independent from CMake while the Rust track is experimental.
- Record every dependency's purpose and license before introducing Rust equivalents or bindings.
- Prefer replacing a dependency only when its behavior and compatibility role are understood.
- Keep platform backends behind traits; don't assume all targets have Vulkan or the same host APIs.

## 3. Proposed implementation order

1. **M0 validation:** a Rust GitHub Actions workflow now runs `cargo fmt`, `cargo test`, and Clippy; fix any failures until the latest run is green.
2. **M1 repository inventory:** tracked-tree comparison and all 33 submodule gitlinks are complete for the audited ref. A detailed semantic inventory of all subsystems remains in progress.
3. **M2 memory contracts:** define guest address spaces, checked translation, region permissions, endian-safe reads/writes, MMIO routing, and invalidation events; add property and boundary tests.
4. **M3 executable foundations:** identify supported XEX/container formats and document parsing/loading responsibilities before implementing a loader.
5. **M4 CPU interpreter:** architectural state, instruction decode, integer/control-flow/load-store first; then floating-point/vector behavior and exceptions.
6. **M5 kernel/runtime:** handles, guest objects, synchronization, threads, module and API dispatch.
7. **M6 GPU/APU/HID:** command stream and resource contracts, shader/render backend boundary, audio pipeline, and input.
8. **Later:** JIT, performance work, game compatibility matrix, and UI integration.

## 4. Risks and open questions

- The tracked Git tree and all 33 submodule pointers match upstream exactly at the audited ref; a semantic source-by-source behavior audit remains incomplete.
- Local clone/build was blocked by the execution environment's lack of network DNS. GitHub Actions is the validation path; the latest CI result must be checked after each code change.
- The Rust workspace is currently a scaffold; it cannot load or execute Xbox 360 games.
- Memory/JIT/GPU coherence and thread scheduling are high-risk cross-subsystem contracts.
- Third-party reuse must respect each upstream license and preserve notices.
- Compatibility must be measured with reproducible tests; no game compatibility claim is made by this audit.

## References

- Upstream Canary: https://github.com/xenia-canary/xenia-canary/tree/canary_experimental
- Fork baseline: https://github.com/GaliardArt/xenia-rust/tree/canary_experimental
- Rust work branch: https://github.com/GaliardArt/xenia-rust/tree/rust-rewrite
