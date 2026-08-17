# Source release policy

rsLXST source releases are qualified from a clean repository checkout. The
workspace is not published to a Cargo registry; every package must retain
`publish = false` unless a separate release policy explicitly changes that
decision.

## Version and tag roles

- The workspace version is the version shared by the three `lxst-*` packages.
- A semantic component-release tag must match that workspace version.
- Ratspeak integration tags identify a tested dependency set. They do not
  replace the component version or changelog and must not move after creation.
- Move the relevant entries out of `Unreleased` only when preparing an
  separately approved semantic component release.

## Source qualification

The Cargo manifests declare rsReticulum `1.2.0` compatibility. CI resolves
that compatibility contract to the immutable rsReticulum commit recorded in
`.github/workflows/ci.yml`; the semantic version describes what the manifests
accept, while the commit identifies the source that qualification actually
uses. Update both values deliberately and prove the new pair in isolated
sibling checkouts whenever the dependency set changes.

Before a component source release, verify that the tree is clean and run:

```sh
python3 tools/check-source-release.py
python3 tools/check-third-party-licenses.py
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo doc --workspace --no-deps --locked
```

The committed `Cargo.lock` is part of the qualified source. Release-oriented
commands must use `--locked`; an unexpected lockfile change is a dependency-set
change that requires review. Rust 1.87 remains the declared minimum and is
checked separately in CI.

The Opus implementation is the exact registry package `opus-rs` 0.1.29 with
its heap-backed codec state feature selected explicitly. rsLXST does not carry
a local codec fork. Android arm64, ARMv7, and x86_64 are qualified targets;
Android i686 is not supported and must not be added to a release matrix without
an upstream compatibility review and a passing target gate.

Tag creation, artifact upload, registry publication, and downstream integration
tagging are separate operations and are not implied by passing these checks.
The checked third-party inventory and every preserved license/provenance record
are source-release inputs; dependency or lockfile changes must refresh and
review them before qualification.
