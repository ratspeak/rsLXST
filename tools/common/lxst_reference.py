#!/usr/bin/env python3
"""Shared helpers for rsLXST Python reference interop tests."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import types
from pathlib import Path

sys.dont_write_bytecode = True


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def main_root() -> Path:
    return repo_root().parent


def upstream_lxst_root() -> Path:
    return Path(os.environ.get("LXST_UPSTREAM_DIR", main_root() / "upstream" / "LXST")).resolve()


def upstream_reticulum_root() -> Path:
    configured = os.environ.get("RETICULUM_UPSTREAM_DIR")
    if configured:
        return Path(configured).resolve()

    upstream = main_root() / "upstream" / "Reticulum"
    if upstream.exists():
        return upstream.resolve()

    sibling = main_root() / "rsReticulum"
    return sibling.resolve()


def install_pycodec2_stub() -> bool:
    """Install a minimal pycodec2 stub so LXST imports without Codec2 installed."""

    if "pycodec2" in sys.modules:
        return False

    class Codec2:
        def __init__(self, mode):
            self.mode = mode

        def samples_per_frame(self):
            return {
                700: 320,
                1200: 320,
                1300: 320,
                1400: 320,
                1600: 320,
                2400: 160,
                3200: 160,
            }.get(self.mode, 160)

        def bytes_per_frame(self):
            return {
                700: 4,
                1200: 6,
                1300: 7,
                1400: 7,
                1600: 8,
                2400: 6,
                3200: 8,
            }.get(self.mode, 8)

        def encode(self, frame):
            return b"\x00" * self.bytes_per_frame()

        def decode(self, frame):
            return [0] * self.samples_per_frame()

    module = types.ModuleType("pycodec2")
    module.Codec2 = Codec2
    sys.modules["pycodec2"] = module
    return True


def configure_cffi_tmpdir() -> None:
    tmpdir = Path(os.environ.get("CFFI_TMPDIR", Path(tempfile.gettempdir()) / "rs-lxst-cffi"))
    tmpdir.mkdir(parents=True, exist_ok=True)
    os.environ["CFFI_TMPDIR"] = str(tmpdir)


def prepare_python_path() -> bool:
    configure_cffi_tmpdir()
    codec2_stubbed = install_pycodec2_stub()
    for path in [str(upstream_lxst_root()), str(upstream_reticulum_root())]:
        if path not in sys.path:
            sys.path.insert(0, path)
    return codec2_stubbed


def import_rns_lxst():
    codec2_stubbed = prepare_python_path()
    import RNS  # noqa: WPS433

    loglevel = os.environ.get("LXST_PYTHON_LOGLEVEL")
    if loglevel and os.environ.get("LXST_PYTHON_LOG_TO_STDOUT") == "1":
        try:
            RNS.loglevel = int(loglevel)
        except ValueError:
            resolved = getattr(RNS, loglevel, None)
            if resolved is not None:
                RNS.loglevel = resolved
    elif hasattr(RNS, "LOG_CRITICAL"):
        RNS.loglevel = RNS.LOG_CRITICAL

    import LXST  # noqa: WPS433

    return RNS, LXST, codec2_stubbed


def json_dump(value) -> None:
    print(json.dumps(value, sort_keys=True, separators=(",", ":")))


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def git_output(args: list[str], cwd: Path) -> str:
    return subprocess.check_output(["git", *args], cwd=str(cwd), text=True).strip()
