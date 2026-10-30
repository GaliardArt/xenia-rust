# Xenia Canary Subsystem Inventory

Baseline: `canary_experimental` at commit `48b318a2ad7c36f8f3901a94e290ba70ae0ffec5`.  
Repository: `GaliardArt/xenia-rust`; the recursive tracked-tree comparison against upstream is identical at this ref.

## Repository footprint

The recursive Git tree contains 2,097 entries (1,972 files/submodule entries and 125 tree entries). The main top-level file counts are:

| Area | Files / gitlinks |
|---|---:|
| `src/` | 1,747 |
| `third_party/` | 105 |
| `android/` | 36 |
| `assets/` | 29 |
| `tools/` | 19 |
| root files | 14 |
| `docs/` | 12 |
| `.github/` | 9 |

Counts are inventory signals, not estimates of implementation effort. Generated files and nested resources can affect counts.

## Main `src/xenia` areas

Counts below are tracked file/blob entries under each top-level area in `src/xenia`; they include headers, source files, tests, and build metadata.

| Area | Entries | Responsibility / Rust direction | Risk |
|---|---:|---|---|
| `cpu/` | 778 | PPC/Xenon architectural state, instruction decoding, HIR/compiler, host backends and JIT. Start with deterministic interpreter and reference tests. | Critical |
| `gpu/` | 299 | Guest command processor, register/state model, shader translation, resources, render backends. Define backend-neutral guest state first. | Critical |
| `kernel/` | 215 | Guest-facing system APIs, modules, objects, threads, synchronization and runtime services. Build incrementally against API coverage tests. | Critical |
| `ui/` | 151 | Host UI, graphics presentation, dialogs and platform integration. Defer until runtime interfaces stabilize. | High |
| `base/` | 125 | Common primitives, platform abstractions, threading, memory helpers, logging, files and synchronization. Inventory each dependency before designing Rust equivalents. | Critical |
| `vfs/` | 54 | Guest file systems, paths, devices and host-backed file access. Use trait-based host providers and path/permission tests. | High |
| `apu/` | 50 | Audio processing, guest audio services, codec integration and host output. Separate emulated audio state from output devices. | High |
| `hid/` | 34 | Controller and input device abstractions. Implement stable guest-facing state independent of host input APIs. | Medium |
| `app/` | 14 | Application entry points and host app lifecycle. Port late, after core boot pipeline exists. | Medium |
| `patcher/` | 7 | Game-specific patch application. Needs a loader/memory model first. | Medium |
| `tools/` | 4 | Development/diagnostic tools. Port only those needed for testability. | Low |
| `debug/` | 3 | Debugging support. Defer broad port until execution state is stable. | Low |
| `helper/` | 3 | Small integration helpers. Assess individually. | Low |

Additional direct `src/xenia` files include the emulator orchestration layer, configuration, guest pointer wrappers, memory implementation, and Xbox-specific definitions. These connect the subsystems and need to be documented as interfaces, not ported independently without dependency context.

## Dependency / integration map

- **Guest memory** is consumed by CPU execution, kernel guest pointers, GPU-visible resources, and patching. Its address translation, permissions, MMIO and invalidation semantics are foundational.
- **CPU** depends on memory, thread state, kernel integration, and host execution backends. The Rust interpreter should be the correctness baseline before JIT work.
- **Kernel** links CPU, APU, base, HID and VFS in the existing CMake graph. This is evidence that kernel/runtime is an integration hub.
- **GPU** links base/UI and shader translation dependencies; Vulkan and platform UI are conditional. Keep guest GPU state separate from host graphics handles.
- **APU** links FFmpeg and base, with host audio backends selected by platform. Codec behavior and audio timing must be separated from device output.
- **VFS/HID/UI** touch host OS resources. Rust traits should keep platform APIs out of guest-visible semantics.

## Implementation order and acceptance gates

1. **Memory contract:** address translation, checked range operations, big-endian guest access, region permissions, MMIO dispatch, invalidation events and boundary tests.
2. **CPU interpreter:** explicit state model; decode/execute a small instruction group at a time; fail loudly for unsupported opcodes; use test vectors and differential checks.
3. **Executable loader:** identify XEX/container structures and malformed-input handling before trying to boot games.
4. **Kernel core:** handles, objects, events, waits, threads, module loading and guest API coverage.
5. **VFS and HID:** guest-visible abstractions with fake host providers for tests.
6. **GPU/APU:** command/state model and audio pipeline; backend boundaries; integration tests.
7. **UI, JIT and optimization:** only after a deterministic boot/execution path exists.

## Current Rust track

The current Rust workspace contains:
- `xenia-core`: checked contiguous memory primitive plus a non-overlapping, permissioned region-based address space with protect/unmap operations; a memory-access trait used by the PPC interpreter; initial PPC subset (addi, addis, ori, oris, b, CR compares, CR-only beq/bne forms, lwz and stw); defensive XEX2 fixed-header/optional-header and security-info/page-descriptor parsers; Xbox-specific PE32 header/section parser; pure PE image-layout planner.
- `xenia-rust`: diagnostic CLI with `inspect-xex <file.xex>` (including security metadata), `inspect-pe <decompressed-image.bin>`, `--help`, and `--version`; it cannot execute games.

These components are learning/test foundations, not a usable emulator. The XEX2 code does not decrypt/decompress, parse the embedded PE, map sections, or boot a title. Missing PPC instruction families, exceptions, full MMU behavior, executable loading, kernel APIs, graphics, audio, input, threading semantics, and game-level compatibility remain substantial work.
