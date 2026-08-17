# Third-party licenses

rsLXST is licensed under AGPL-3.0-or-later as described in [LICENSE](LICENSE).
Third-party components retain their own licenses; the rsLXST license does not
replace those terms or notices.

[`THIRD_PARTY_LICENSES.json`](THIRD_PARTY_LICENSES.json) is the complete,
CI-checked inventory of registry third-party normal/build
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

rsLXST consumes the exact crates.io package `opus-rs` 0.1.29 from
[`restsend/opus-rs`](https://github.com/restsend/opus-rs), associated with
upstream commit `95f8b76430beb8c1bed067354d519c918ceade21` and locked crate
checksum `eadaf84d53a021172447a8771d74f805ac3719a61041dafa95720ef86b8e271d`.
It is BSD-3-Clause, is not locally modified, and is configured with explicit
heap-backed codec state. The preserved license and Opus patent notice are in
[`third_party/opus-rs-0.1.29-COPYING`](third_party/opus-rs-0.1.29-COPYING).

This inventory records source and license evidence; it is not legal advice and
does not by itself replace any third-party notice required for a distributed
binary or source bundle.
