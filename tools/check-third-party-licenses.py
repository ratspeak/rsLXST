#!/usr/bin/env python3
"""Generate or verify the locked non-dev third-party dependency inventory."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
INVENTORY = ROOT / "THIRD_PARTY_LICENSES.json"
UPSTREAM_OPUS_COMMIT = "95f8b76430beb8c1bed067354d519c918ceade21"
UPSTREAM_OPUS_VERSION = "0.1.29"
UPSTREAM_OPUS_LICENSE = "BSD-3-Clause"
UPSTREAM_OPUS_REPOSITORY = "https://github.com/restsend/opus-rs"
UPSTREAM_OPUS_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
UPSTREAM_OPUS_CRATE_CHECKSUM = (
    "eadaf84d53a021172447a8771d74f805ac3719a61041dafa95720ef86b8e271d"
)
UPSTREAM_OPUS_COPYING = "third_party/opus-rs-0.1.29-COPYING"
UPSTREAM_OPUS_COPYING_SHA256 = (
    "67c6f0a4bac3019fb08948838d7203bf661a629416f69057081c6f39db5e96a5"
)


def fail(message: str) -> None:
    print(f"third-party license inventory: {message}", file=sys.stderr)
    raise SystemExit(1)


def load_metadata() -> dict:
    return json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )


def build_inventory() -> dict:
    metadata = load_metadata()
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    reachable = set(metadata["workspace_members"])
    pending = list(reachable)

    while pending:
        package_id = pending.pop()
        for dependency in nodes[package_id]["deps"]:
            if not any(
                dep_kind["kind"] in (None, "build")
                for dep_kind in dependency["dep_kinds"]
            ):
                continue
            if dependency["pkg"] not in reachable:
                reachable.add(dependency["pkg"])
                pending.append(dependency["pkg"])

    entries = []
    upstream_opus_count = 0
    for package_id in reachable:
        package = packages[package_id]
        source = package["source"]
        if source is None:
            continue
        if not package["license"]:
            fail(f"{package['name']} {package['version']} has no declared license")

        entry = {
            "name": package["name"],
            "version": package["version"],
            "license": package["license"],
            "repository": package["repository"],
            "source": source,
        }
        if package["name"] == "opus-rs":
            upstream_opus_count += 1
            expected_manifest = (
                UPSTREAM_OPUS_VERSION,
                UPSTREAM_OPUS_LICENSE,
                UPSTREAM_OPUS_REPOSITORY,
                UPSTREAM_OPUS_SOURCE,
            )
            actual_manifest = (
                package["version"],
                package["license"],
                package["repository"],
                package["source"],
            )
            if actual_manifest != expected_manifest:
                fail(
                    "upstream opus-rs manifest provenance changed "
                    f"(expected {expected_manifest!r}, found {actual_manifest!r})"
                )
            entry.update(
                {
                    "crateChecksum": UPSTREAM_OPUS_CRATE_CHECKSUM,
                    "upstreamCommit": UPSTREAM_OPUS_COMMIT,
                    "licenseText": UPSTREAM_OPUS_COPYING,
                }
            )
        entries.append(entry)

    if upstream_opus_count != 1:
        fail(f"expected one upstream opus-rs package, found {upstream_opus_count}")

    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    locked_opus = [
        package
        for package in lock["package"]
        if package["name"] == "opus-rs" and package["version"] == UPSTREAM_OPUS_VERSION
    ]
    if len(locked_opus) != 1:
        fail(f"expected one locked opus-rs {UPSTREAM_OPUS_VERSION} package")
    if locked_opus[0].get("source") != UPSTREAM_OPUS_SOURCE:
        fail("locked opus-rs source is not crates.io")
    if locked_opus[0].get("checksum") != UPSTREAM_OPUS_CRATE_CHECKSUM:
        fail("locked opus-rs crate checksum changed")
    copying_hash = hashlib.sha256(
        (ROOT / UPSTREAM_OPUS_COPYING).read_bytes()
    ).hexdigest()
    if copying_hash != UPSTREAM_OPUS_COPYING_SHA256:
        fail(f"preserved opus-rs COPYING changed (found {copying_hash})")

    entries.sort(key=lambda entry: (entry["name"], entry["version"], entry["source"]))
    lockfile_hash = hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest()
    return {
        "schemaVersion": 1,
        "scope": (
            "All registry third-party normal/build dependencies "
            "reachable from rsLXST workspace members across declared targets; "
            "dev-only and first-party path packages are excluded."
        ),
        "cargoLockSha256": lockfile_hash,
        "packages": entries,
    }


expected = json.dumps(build_inventory(), indent=2, sort_keys=False) + "\n"
if len(sys.argv) == 2 and sys.argv[1] == "--write":
    INVENTORY.write_text(expected, encoding="utf-8")
elif len(sys.argv) != 1:
    fail("usage: check-third-party-licenses.py [--write]")
elif not INVENTORY.is_file():
    fail("THIRD_PARTY_LICENSES.json is missing; run with --write")
elif INVENTORY.read_text(encoding="utf-8") != expected:
    fail("THIRD_PARTY_LICENSES.json is stale; review changes and run with --write")
else:
    print(f"third-party license inventory: ok ({len(json.loads(expected)['packages'])} packages)")
