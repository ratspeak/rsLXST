#!/usr/bin/env python3
"""Snapshot the pinned upstream LXST reference used by rsLXST tests."""

from __future__ import annotations

import hashlib
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "common"))
from lxst_reference import git_output, json_dump, upstream_lxst_root  # noqa: E402


REFERENCE_FILES = [
    "LXST/_version.py",
    "LXST/Network.py",
    "LXST/Primitives/Telephony.py",
    "LXST/Codecs/__init__.py",
    "LXST/Codecs/Raw.py",
    "LXST/Codecs/Opus.py",
    "LXST/Codecs/Codec2.py",
]


def package_version(root: Path) -> str:
    version_globals = {}
    exec((root / "LXST" / "_version.py").read_text(encoding="utf-8"), version_globals)
    return version_globals["__version__"]


def git_blob(root: Path, rel: str) -> bytes:
    return subprocess.check_output(["git", "show", f"HEAD:{rel}"], cwd=str(root))


def main() -> None:
    root = upstream_lxst_root()
    missing = [rel for rel in REFERENCE_FILES if not (root / rel).is_file()]
    files = {}
    for rel in REFERENCE_FILES:
        path = root / rel
        if path.is_file():
            blob = git_blob(root, rel)
            files[rel] = {
                "bytes": len(blob),
                "sha256": hashlib.sha256(blob).hexdigest(),
            }

    json_dump(
        {
            "upstream": "../upstream/LXST",
            "remote": git_output(["config", "--get", "remote.origin.url"], root),
            "commit": git_output(["rev-parse", "HEAD"], root),
            "dirty": bool(git_output(["status", "--short", "--untracked-files=no"], root)),
            "package_version": package_version(root),
            "missing_files": missing,
            "files": files,
        }
    )


if __name__ == "__main__":
    main()
