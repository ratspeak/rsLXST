# opus-rs provenance

This directory is a locally modified, vendored copy of the BSD-3-Clause
`opus-rs` implementation. It is not an unmodified upstream 0.1.19 checkout.

## Upstream baseline

- Repository: <https://github.com/restsend/opus-rs>
- Upstream commit: `a61f6d3f93623080e3e146ec267602802abb6313`
- Upstream manifest version: `0.1.19`
- crates.io package SHA-256:
  `6511297abde7ca183099fb30e50b8f81c3bde45ab85812f68bb4db3d97f20a0c`
- Upstream `src` Git tree:
  `d0725f0bdcc570c142480fd3fbfed39838bdeebc`
- Imported into rsLXST by commit:
  `6bafada6d9a9f03e635df06724817771439e7edf`
- License: BSD-3-Clause, preserved verbatim in [COPYING](COPYING)
- Current COPYING SHA-256:
  `f9116d266d13dfd1350182113b59e007a32180642d158f789becb92bb4abef4b`

The published package's `.cargo_vcs_info.json` identifies the same upstream
commit. The import retained upstream production source, README, and COPYING;
omitted upstream development-only tests, fixtures, examples, fuzz targets,
benchmarks, and dev dependencies; added an explicit README manifest field; and
included an import-time decoder scratch-buffer correction for 60 ms SILK
frames. Rust formatting also changed portions of `src/lib.rs` during import.

## Local delta ledger

| rsLXST commit | Classification | Effect |
| --- | --- | --- |
| `6bafada6d9a9f03e635df06724817771439e7edf` | import + safety | import upstream 0.1.19; size SILK PCM scratch space for 60 ms and channel count |
| `cd84f705354b42142c9f2cde31760182009dd1c0` | mechanical | satisfy the repository's strict Clippy policy |
| `7ac244cafde428945838dd709794468d4654fc89` | portability | prevent x86_64 AVX intrinsics from compiling on 32-bit x86 targets |
| `e9f90874f22c86ef48b78a4b9d59c8927d16d0cd` | safety | reject malformed empty-CBR and over-duration packets before unsafe buffer use; add fuzz coverage |
| `47827c9e2adb1fbdff84b12e687581f9c8991344` | mechanical | keep vendored source clean under workspace strict Clippy |
| `3205cb73b91f34c368a26002475fce0161275109` | release policy | declare Rust 1.85 and prevent accidental registry publication |

At the Wave B decision-spike baseline, the current vendored `src` Git tree is
`c07ad8b1cbaf770a915b12d6f19ef52ffa77b7b2`; its path-and-content SHA-256 is
`9468dbcfea089350057c0bb16deef8e50abd008f02e6ac3e3115e749f15e1e53`.
Any future source refresh must
replace this ledger with a reviewable upstream base plus retained-patch list,
preserve COPYING, refresh the third-party inventory, and pass the rsLXST Opus,
malformed-input, target, and interop gates. A matching manifest version alone
is never sufficient provenance.
