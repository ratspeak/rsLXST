# Third-party licenses

rsLXST is used under the license in [LICENSE](LICENSE), reproduced here:

```
LXST Copyright 2026 Mark Qvist.

This implementation of LXST is derived from the LXST reference implementation,
and used under license for Ratspeak, for non-commercial use only.
No derivatives or use outside Ratspeak is permitted.

For commercial licensing inquiries, contact license@unsigned.io
```

The adapted `lxst-codec2` crate retains the separate terms described below.
Third-party components retain their own licenses; the rsLXST license does not
replace those terms or notices.

[`THIRD_PARTY_LICENSES.json`](THIRD_PARTY_LICENSES.json) is the complete,
CI-checked inventory of adapted Codec2 and registry third-party normal/build
dependencies reachable from the locked workspace across declared targets. It
excludes dev-only packages and first-party path packages. The inventory is
generated from Cargo metadata and `Cargo.lock` with:

```sh
python3 tools/check-third-party-licenses.py --write
```

Review changes before committing them. Ordinary qualification uses the script
without `--write` and fails if the lockfile, dependency graph, declared license,
repository, or source changes.

## Upstream Opus implementation

rsLXST consumes the exact crates.io package `opus-rs` 0.1.42 from
[`restsend/opus-rs`](https://github.com/restsend/opus-rs), associated with
upstream commit `5d8230f362b2618b9e730e6201c0d8f98961dfe1` and locked crate
checksum `bafbcaf2ed381cdf68ced4ed8d16517de90ffd7424e36f0b9e45d67fbe30b1f9`.
It is BSD-3-Clause, is not locally modified, and is configured with explicit
heap-backed codec state. The preserved license and Opus patent notice are in
[`third_party/opus-rs-0.1.42-COPYING`](third_party/opus-rs-0.1.42-COPYING).

This inventory records source and license evidence; it is not legal advice and
does not by itself replace any third-party notice required for a distributed
binary or source bundle.

## Adapted Codec2 implementation

`crates/lxst-codec2` retains the pure-Rust codec2 0.3.1 implementation's
LGPL-2.1-only AND MIT declaration and embedded KISS FFT BSD-3-Clause notice.
Its [NOTICE](crates/lxst-codec2/NOTICE) and
[immutable provenance](crates/lxst-codec2/UPSTREAM.json) describe the bounded
storage, math and state-ownership modifications. This path dependency is
explicitly inventoried; it is not treated as original first-party DSP.
