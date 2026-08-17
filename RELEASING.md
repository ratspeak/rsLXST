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

Before a component source release, verify that the tree is clean and run:

```sh
python3 tools/check-source-release.py
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo doc --workspace --no-deps --locked
```

The committed `Cargo.lock` is part of the qualified source. Release-oriented
commands must use `--locked`; an unexpected lockfile change is a dependency-set
change that requires review. Rust 1.85 remains the declared minimum and is
checked separately in CI.

Tag creation, artifact upload, registry publication, and downstream integration
tagging are separate operations and are not implied by passing these checks.
