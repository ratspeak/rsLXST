#!/usr/bin/env python3
"""Check source-release metadata that must stay true for this workspace."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
EXPECTED_PACKAGES = {"lxst-core", "lxst-rns", "lxst-telephony", "opus-rs"}
EXPECTED_MSRV = "1.85"


def fail(message: str) -> None:
    print(f"source-release contract: {message}", file=sys.stderr)
    raise SystemExit(1)


metadata = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
)

packages = {package["name"]: package for package in metadata["packages"]}
if set(packages) != EXPECTED_PACKAGES:
    fail(
        "package inventory changed; update this check deliberately "
        f"(expected {sorted(EXPECTED_PACKAGES)}, found {sorted(packages)})"
    )

for name, package in sorted(packages.items()):
    if package["publish"] != []:
        fail(f"{name} must declare publish = false")
    if package["rust_version"] != EXPECTED_MSRV:
        fail(
            f"{name} must declare Rust {EXPECTED_MSRV} "
            f"(found {package['rust_version']!r})"
        )

if not (ROOT / "Cargo.lock").is_file():
    fail("Cargo.lock must be committed for reproducible workspace builds")

if "## Unreleased" not in (ROOT / "CHANGELOG.md").read_text(encoding="utf-8"):
    fail("CHANGELOG.md must retain an Unreleased section")

print("source-release contract: ok")
