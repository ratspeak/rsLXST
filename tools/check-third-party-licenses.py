#!/usr/bin/env python3
"""Generate or verify the locked non-dev third-party dependency inventory."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
INVENTORY = ROOT / "THIRD_PARTY_LICENSES.json"
VENDORED_OPUS_COMMIT = "a61f6d3f93623080e3e146ec267602802abb6313"
VENDORED_OPUS_VERSION = "0.1.19"
VENDORED_OPUS_LICENSE = "BSD-3-Clause"
VENDORED_OPUS_REPOSITORY = "https://github.com/restsend/opus-rs"
VENDORED_OPUS_COPYING_SHA256 = (
    "f9116d266d13dfd1350182113b59e007a32180642d158f789becb92bb4abef4b"
)
VENDORED_OPUS_SOURCE_SHA256 = (
    "9468dbcfea089350057c0bb16deef8e50abd008f02e6ac3e3115e749f15e1e53"
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


def directory_sha256(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        digest.update(path.relative_to(root).as_posix().encode("utf-8"))
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


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
    vendored_opus_count = 0
    for package_id in reachable:
        package = packages[package_id]
        source = package["source"]
        is_vendored_opus = (
            package["name"] == "opus-rs"
            and source is None
            and Path(package["manifest_path"]).is_relative_to(ROOT / "vendor")
        )
        if source is None and not is_vendored_opus:
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
        if is_vendored_opus:
            vendored_opus_count += 1
            expected_manifest = (
                VENDORED_OPUS_VERSION,
                VENDORED_OPUS_LICENSE,
                VENDORED_OPUS_REPOSITORY,
            )
            actual_manifest = (
                package["version"],
                package["license"],
                package["repository"],
            )
            if actual_manifest != expected_manifest:
                fail(
                    "vendored opus-rs manifest provenance changed "
                    f"(expected {expected_manifest!r}, found {actual_manifest!r})"
                )
            entry.update(
                {
                    "source": (
                        "vendored+https://github.com/restsend/opus-rs#"
                        f"{VENDORED_OPUS_COMMIT}"
                    ),
                    "provenance": "vendor/opus-rs/PROVENANCE.md",
                    "licenseText": "vendor/opus-rs/COPYING",
                }
            )
        entries.append(entry)

    if vendored_opus_count != 1:
        fail(f"expected one vendored opus-rs package, found {vendored_opus_count}")

    copying_hash = hashlib.sha256(
        (ROOT / "vendor/opus-rs/COPYING").read_bytes()
    ).hexdigest()
    if copying_hash != VENDORED_OPUS_COPYING_SHA256:
        fail(f"vendored opus-rs COPYING changed (found {copying_hash})")
    source_hash = directory_sha256(ROOT / "vendor/opus-rs/src")
    if source_hash != VENDORED_OPUS_SOURCE_SHA256:
        fail(
            "vendored opus-rs source changed without a provenance decision "
            f"(found {source_hash})"
        )

    provenance = (ROOT / "vendor/opus-rs/PROVENANCE.md").read_text(encoding="utf-8")
    for value in (
        VENDORED_OPUS_COMMIT,
        VENDORED_OPUS_COPYING_SHA256,
        VENDORED_OPUS_SOURCE_SHA256,
    ):
        if value not in provenance:
            fail(f"vendored opus-rs provenance does not record {value}")

    entries.sort(key=lambda entry: (entry["name"], entry["version"], entry["source"]))
    lockfile_hash = hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest()
    return {
        "schemaVersion": 1,
        "scope": (
            "All registry and vendored third-party normal/build dependencies "
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
