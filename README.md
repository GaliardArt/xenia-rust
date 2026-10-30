# Xenia Rust

<p align="center">
  <img height="160" src="https://raw.githubusercontent.com/xenia-canary/xenia/master/assets/icon/256.png" alt="Xenia emulator icon" />
</p>

<h1 align="center">Xenia Rust — A Rust Rewrite of Xenia Canary</h1>

<p align="center">
  An experimental, long-term effort to bring the <a href="https://github.com/xenia-canary/xenia-canary">Xenia Canary</a> Xbox 360 emulator to Rust.
</p>

> **Project status: early development.** This repository is being used to progressively reimplement Xenia's emulator architecture in Rust, using the Xenia Canary codebase as the primary reference. It is not yet a playable Xbox 360 emulator.

## Project goal

The goal of **Xenia Rust** is to create a Rust-based implementation of the Xenia Xbox 360 emulator, guided by the behavior, architecture, and compatibility work in the Xenia Canary fork.

This is a substantial reimplementation, not a claim that the existing C++ code has already been converted automatically or that a complete emulator is available today. The original C++ code is retained as reference material while Rust components are developed and validated separately.

The long-term objective is to build toward Xbox 360 game execution while preserving correctness, maintainability, clear module boundaries, and a practical testing strategy.

## Guiding principles

- **Use Xenia Canary as the reference:** study its implementation, subsystem boundaries, behavior, and compatibility fixes before reimplementing them.
- **Rust-first implementation:** new emulator components in the rewrite are implemented in Rust and kept isolated from the original C++ implementation.
- **Incremental, verifiable progress:** establish a buildable Rust workspace, document dependencies and interfaces, and validate each subsystem as it is introduced.
- **Compatibility over shortcuts:** use the original implementation and legitimate test cases to guide behavioral parity rather than creating superficial placeholders that imply functionality is complete.
- **Preserve upstream attribution:** Xenia and Xenia Canary remain the upstream projects and retain their respective licenses and contributions.

## Current status

The rewrite has completed its initial repository audit and is implementing the executable-loading foundation. The Rust workspace now includes checked guest memory, a small PPC interpreter subset, XEX2 and Xbox PE metadata parsing, and a restricted unencrypted XEX2 image path for no-compression and basic-compression profiles. This remains an early experimental foundation, not a playable emulator.

| Area | Current direction |
|---|---|
| Repository and architecture audit | Inventory the existing code, subsystems, dependencies, and interfaces |
| Rust workspace and build foundation | Establish an isolated, testable structure for the rewrite |
| Memory and guest address handling | Build foundational guest-memory abstractions |
| PowerPC CPU | Start with an interpreter and incrementally implement instruction behavior |
| Xbox 360 kernel and system services | Reimplement required operating-system behavior as dependencies become clear |
| Graphics and GPU | Analyze the graphics pipeline and plan a compatible Rust implementation |
| Audio, storage, and input | Implement and validate subsystem support progressively |
| Executable loading and game execution | Initial unencrypted/uncompressed XEX2 image path is implemented; encrypted/compressed formats, mapping, imports, kernel boot, and execution remain open |
| Compatibility and regression testing | Compare behavior against reference implementations and maintain targeted tests |

**Not implemented yet:** a complete PowerPC CPU, functional Xbox 360 GPU, full kernel/system services, complete XEX decryption/decompression, guest-memory mapping, imports, end-to-end boot, or playable game support. The implemented XEX path only validates and prepares an unencrypted/uncompressed image plan; it does not start execution.

## Development approach

Work is expected to proceed in stages:

1. Audit the Xenia Canary source tree and map its major subsystems, dependencies, and platform-specific code.
2. Define the Rust workspace, module boundaries, error handling, logging, and test conventions.
3. Implement foundational types and memory handling, with focused unit tests.
4. Develop a PowerPC interpreter and validate instruction behavior.
5. Add the Xbox 360 kernel and system services needed by the execution path.
6. Develop graphics, audio, input, and storage support.
7. Integrate executable loading and the runtime, then expand compatibility tests.
8. Track unsupported behavior explicitly and avoid reporting a game as working until it has been tested.

The order may change when the source audit reveals dependency constraints, but each milestone should be measurable and independently verifiable.

## Repository structure

The original Xenia Canary C++ source is preserved as reference material. The Rust rewrite is developed as a separate implementation so that new Rust code, tests, and build configuration can evolve without implying that the legacy code has been ported.

As the project grows, subsystem-specific documentation should record:
- the corresponding upstream subsystem or source files;
- the Rust module responsible for the behavior;
- known gaps and compatibility differences;
- tests and evidence used to validate the implementation.

## Building and testing

Build instructions will be added and kept aligned with the actual Rust workspace as it becomes buildable. At this stage, do not assume that the repository can build or run a complete Rust emulator.

Once the workspace is established, the expected baseline checks are:

- format Rust code with `cargo fmt --check`;
- run unit and integration tests with `cargo test --workspace`;
- run static checks with `cargo clippy --workspace --all-targets`;
- document any required platform-specific tools and dependencies.

Only claim a check passes after it has actually been run successfully.

## Upstream projects and attribution

This project is based on study and reimplementation of the open-source **Xenia Canary** emulator:

- [Xenia Canary repository](https://github.com/xenia-canary/xenia-canary)
- [Xenia Canary wiki](https://github.com/xenia-canary/xenia-canary/wiki)
- [Original Xenia project](https://github.com/xenia-project/xenia)

Xenia Rust is an independent experimental rewrite effort; it is not an official release of Xenia or Xenia Canary and is not affiliated with their maintainers. Existing source files and contributions remain subject to the licenses and notices that apply to them. Consult the repository's license files before redistributing or reusing code.

## Disclaimer

This project is intended for research, education, and experimentation in emulation. Use only games and system materials that you are legally entitled to use, and comply with applicable laws and the licenses of all relevant projects.
