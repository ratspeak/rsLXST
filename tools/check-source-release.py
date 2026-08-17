#!/usr/bin/env python3
"""Check source-release metadata that must stay true for this workspace."""

from __future__ import annotations

import json
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
EXPECTED_PACKAGES = {"lxst-core", "lxst-rns", "lxst-telephony"}
EXPECTED_MSRV = "1.87"
EXPECTED_RETICULUM_VERSION = "1.1.0"
EXPECTED_RETICULUM_COMMIT = "cfb7253f99439367212744f322b72b549a44bc62"
EXPECTED_OPUS_DEPENDENCY = (
    'opus-rs = { version = "=0.1.29", default-features = false, '
    'features = ["heap"] }'
)
SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")


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

reticulum_requirements: set[str] = set()
for name, package in sorted(packages.items()):
    if package["publish"] != []:
        fail(f"{name} must declare publish = false")
    if package["rust_version"] != EXPECTED_MSRV:
        fail(
            f"{name} must declare Rust {EXPECTED_MSRV} "
            f"(found {package['rust_version']!r})"
        )
    for dependency in package["dependencies"]:
        if dependency["name"].startswith("rns-"):
            reticulum_requirements.add(dependency["req"])

if len(reticulum_requirements) != 1:
    fail(
        "rsReticulum dependencies must share one compatibility requirement "
        f"(found {sorted(reticulum_requirements)})"
    )
reticulum_requirement = reticulum_requirements.pop()
if reticulum_requirement != f"^{EXPECTED_RETICULUM_VERSION}":
    fail(
        "rsReticulum compatibility requirement is "
        f"{reticulum_requirement!r}, expected ^{EXPECTED_RETICULUM_VERSION}"
    )

if not (ROOT / "Cargo.lock").is_file():
    fail("Cargo.lock must be committed for reproducible workspace builds")

if "## Unreleased" not in (ROOT / "CHANGELOG.md").read_text(encoding="utf-8"):
    fail("CHANGELOG.md must retain an Unreleased section")

root_manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
if EXPECTED_OPUS_DEPENDENCY not in root_manifest:
    fail("workspace must consume exact upstream opus-rs 0.1.29 with heap state")
if (ROOT / "vendor/opus-rs").exists():
    fail("local opus-rs vendor/fork must not exist")

subprocess.run(
    [sys.executable, "tools/check-third-party-licenses.py"],
    cwd=ROOT,
    check=True,
)

workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
version_match = re.search(
    r"^\s*RSLXST_RSRETICULUM_VERSION:\s*(\S+)\s*$", workflow, re.MULTILINE
)
commit_match = re.search(
    r"^\s*RSLXST_RSRETICULUM_COMMIT:\s*([0-9a-f]+)\s*$", workflow, re.MULTILINE
)
if not version_match or version_match.group(1) != EXPECTED_RETICULUM_VERSION:
    fail("CI rsReticulum compatibility version does not match the manifest contract")
if not commit_match or commit_match.group(1) != EXPECTED_RETICULUM_COMMIT:
    fail("CI rsReticulum source is not pinned to the qualified commit")

action_uses = re.findall(r"^\s*-\s+uses:\s+([^\s#]+)", workflow, re.MULTILINE)
if not action_uses:
    fail("CI workflow contains no external actions")
for action in action_uses:
    if action.startswith("./"):
        continue
    if "@" not in action or not SHA_PATTERN.fullmatch(action.rsplit("@", 1)[1]):
        fail(f"CI action is not pinned to a full commit: {action}")

reticulum_checkouts = []
lines = workflow.splitlines()
for index, line in enumerate(lines):
    match = re.match(r"^(\s*)-\s+uses:\s+([^\s#]+)", line)
    if not match:
        continue
    indentation = len(match.group(1))
    block = [line]
    for following in lines[index + 1 :]:
        if following.strip() and len(following) - len(following.lstrip()) <= indentation:
            break
        block.append(following)
    text = "\n".join(block)
    if "repository: ${{ github.repository_owner }}/rsReticulum" in text:
        reticulum_checkouts.append(text)

if not reticulum_checkouts:
    fail("CI workflow contains no rsReticulum checkout")
expected_ref = "ref: ${{ env.RSLXST_RSRETICULUM_COMMIT }}"
for checkout in reticulum_checkouts:
    if expected_ref not in checkout:
        fail("every CI rsReticulum checkout must use the qualified commit")

for target in (
    "aarch64-linux-android",
    "armv7-linux-androideabi",
    "x86_64-linux-android",
    "aarch64-apple-ios",
):
    if target not in workflow:
        fail(f"CI does not qualify supported target {target}")
if "i686-linux-android" in workflow:
    fail("Android i686 is unsupported and must not appear in the CI target matrix")

reticulum_root = ROOT.parent / "rsReticulum"
if not reticulum_root.is_dir():
    fail(f"rsReticulum sibling checkout is missing at {reticulum_root}")
actual_reticulum_commit = subprocess.run(
    ["git", "rev-parse", "HEAD"],
    cwd=reticulum_root,
    check=True,
    capture_output=True,
    text=True,
).stdout.strip()
if actual_reticulum_commit != EXPECTED_RETICULUM_COMMIT:
    fail(
        f"rsReticulum sibling is {actual_reticulum_commit}, "
        f"expected {EXPECTED_RETICULUM_COMMIT}"
    )

subprocess.run(
    [sys.executable, "tools/check-api-baseline.py", "--metadata-only"],
    cwd=ROOT,
    check=True,
)

print("source-release contract: ok")
