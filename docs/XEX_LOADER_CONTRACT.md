# XEX Loader Contract and Porting Notes

## Scope

The Rust `parse_xex2_header` currently validates and reads only the XEX2 fixed header and its optional-header table. It does not load executable bytes or interpret optional-header payloads.

## Fixed-header layout

The audited Xenia Canary definition in `src/xenia/kernel/util/xex2_info.h` defines the 24-byte fixed header as big-endian fields:

| Offset | Field | Rust representation |
|---:|---|---|
| `0x00` | magic (`XEX2`) | validated signature |
| `0x04` | module flags | `u32` |
| `0x08` | header size | `u32` |
| `0x0C` | reserved | `u32` |
| `0x10` | security-info offset | `u32` |
| `0x14` | optional-header count | `u32` |
| `0x18` | optional-header entries | key/value pairs, 8 bytes each |

Important correction made during source inspection: offset `0x08` is **header size**, not a PE-data offset. XEX's embedded image begins after the XEX header area; the PE headers are found after image decryption/decompression and mapping.

## Existing C++ load pipeline (behavioral reference)

Source inspected:
- `src/xenia/cpu/xex_module.cc`
- `src/xenia/cpu/xex_module.h`
- `src/xenia/kernel/util/xex2_info.h`

The pipeline is staged and must not be collapsed into a single unchecked parse:

1. **Identify container variant.** The reference recognizes several XEX variants, but not all are supported. Rust should initially accept XEX2 explicitly and return a clear unsupported-format error for others.
2. **Validate and retain headers.** Validate file length, declared header size, optional-header count/table bounds, and all offsets before slicing.
3. **Read security metadata.** Security information includes image size, load address, image flags, AES session-key material, export metadata, and page descriptors. Its variable-sized tail depends on descriptor count.
4. **Resolve image base.** The reference starts from the security-info load address and may override it with the image-base optional header.
5. **Decode image format.** The file-format optional header selects encryption and compression modes. The reference supports uncompressed, basic-compressed, and normal-compressed paths, with AES decryption where required and LZX handling for compressed images.
6. **Validate embedded PE.** The reference checks DOS `MZ`, NT `PE\0\0`, PowerPC big-endian machine type, 32-bit PE optional header, and Xbox subsystem before accepting the image.
7. **Map sections and imports.** Parse section headers, map guest virtual addresses, resolve imported kernel/user modules, and establish executable/data page ranges.
8. **Apply memory protections.** Page descriptors distinguish code, data and read-only data. The reference makes code/read-only pages read-only unless a special writable-code mode is enabled.
9. **Continue loading after patches.** The C++ design separates initial load from a continuation phase so title patches can be applied before final PE/import/protection setup.

## Required safety invariants for Rust

- Never trust `header_size`, optional-header count, security offsets, descriptor counts, PE offsets, section counts, or compressed-block lengths without checked arithmetic and bounds validation.
- A failed parse must not mutate the guest address space.
- Parse metadata into owned Rust structures before mapping guest memory.
- Validate each nested table against its containing structure, not only against the overall file.
- Treat malformed input as a recoverable error; do not panic or index unchecked slices.
- Preserve endianness explicitly. XEX metadata and PPC instruction words are big-endian; PE fields have their own format conventions and must be parsed field-by-field rather than by casting host structs.
- Decryption/decompression should produce a temporary validated image before guest memory is committed, so a malformed image cannot leave partial mappings.
- Keep cryptographic and compression implementations behind small, testable interfaces; record upstream license/notice obligations before choosing dependencies.

## Planned Rust milestones

- **LDR-1 (current):** fixed XEX2 header and optional-header table parsing, malformed-input tests, CLI metadata inspection.
- **LDR-2 (complete):** optional-header lookup and safe XEX2 security-info parsing, including page descriptor bounds and metadata CLI output.
- **LDR-3 (complete, metadata only):** PE header parser for the Xbox 360 PPCBE/32-bit/Xbox-subsystem profile and section-table parsing with malformed-input tests.
- **LDR-4 (complete, planning only):** image layout plan computes guest ranges and section permissions without mutating memory.
- **LDR-5 (complete, restricted):** unencrypted XEX2 payload extraction with no compression or basic compression, PE validation, and layout planning for synthetic fixtures. This does not map memory or boot a title.
- **LDR-6:** compression and encryption paths, then imports, patches, and boot integration.

No retail game fixture is included in the repository. Tests must use synthetic, legally safe fixtures and must not claim game compatibility until the entire boot pipeline is integrated and tested.
