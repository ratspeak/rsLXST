#!/usr/bin/env python3
"""Generate and decode LXST Raw codec fixtures with upstream Python LXST."""

from __future__ import annotations

import argparse
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "common"))
from lxst_reference import import_rns_lxst, json_dump  # noqa: E402


def raw_to_fixture(name: str, raw, frame):
    payload = raw.encode(frame)
    decoded = decode_payload(payload)
    decoded["name"] = name
    decoded["payload_hex"] = payload.hex()
    return decoded


def decode_payload(payload: bytes):
    import numpy as np  # noqa: WPS433

    _RNS, _LXST, _stubbed = import_rns_lxst()
    from LXST.Codecs import Raw  # noqa: WPS433

    raw = Raw()
    decoded = raw.decode(payload)
    samples = np.asarray(decoded, dtype=np.float32).reshape(-1).tolist()
    return {
        "bitdepth_header": payload[0] >> 6,
        "channels": int(decoded.shape[1]),
        "sample_frames": int(decoded.shape[0]),
        "samples": [float(sample) for sample in samples],
    }


def generate_fixtures():
    import numpy as np  # noqa: WPS433

    _RNS, _LXST, _stubbed = import_rns_lxst()
    from LXST.Codecs import Raw  # noqa: WPS433

    return [
        raw_to_fixture(
            "float16_stereo",
            Raw(channels=2, bitdepth=16),
            np.array([[0.0, 0.5], [-0.25, 1.0]], dtype=np.float32),
        ),
        raw_to_fixture(
            "float32_mono",
            Raw(channels=1, bitdepth=32),
            np.array([[0.25], [-1.5], [2.0]], dtype=np.float32),
        ),
        raw_to_fixture(
            "float64_three_channel",
            Raw(channels=3, bitdepth=64),
            np.array([[0.0, 0.125, -0.5], [1.0, -1.0, 0.25]], dtype=np.float64),
        ),
    ]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--decode-hex", default="")
    args = parser.parse_args()

    if args.decode_hex:
        json_dump(decode_payload(bytes.fromhex(args.decode_hex)))
    else:
        json_dump(generate_fixtures())


if __name__ == "__main__":
    main()
