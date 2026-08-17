# Third-party licenses

rsLXST is licensed under AGPL-3.0-or-later as described in [LICENSE](LICENSE).
Third-party components retain their own licenses; the rsLXST license does not
replace those terms or notices.

[`THIRD_PARTY_LICENSES.json`](THIRD_PARTY_LICENSES.json) is the complete,
CI-checked inventory of registry and vendored third-party normal/build
dependencies reachable from the locked workspace across declared targets. It
excludes dev-only packages and first-party path packages. The inventory is
generated from Cargo metadata and `Cargo.lock` with:

```sh
python3 tools/check-third-party-licenses.py --write
```

Review changes before committing them. Ordinary qualification uses the script
without `--write` and fails if the lockfile, dependency graph, declared license,
repository, or source changes.

## Vendored Opus implementation

`vendor/opus-rs` is a locally modified snapshot of
[`restsend/opus-rs`](https://github.com/restsend/opus-rs), originally based on
upstream commit `a61f6d3f93623080e3e146ec267602802abb6313` and the published
`opus-rs` 0.1.19 package. It remains BSD-3-Clause. The preserved license and
Opus patent notice are in [`vendor/opus-rs/COPYING`](vendor/opus-rs/COPYING),
and the exact source/delta record is in
[`vendor/opus-rs/PROVENANCE.md`](vendor/opus-rs/PROVENANCE.md).

This inventory records source and license evidence; it is not legal advice and
does not by itself replace any third-party notice required for a distributed
binary or source bundle.
