#!/usr/bin/env python3
"""Generate and decode LXST wire fixtures with the Python reference stack."""

from __future__ import annotations

import argparse
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "common"))
from lxst_reference import import_rns_lxst, json_dump  # noqa: E402


def decode_packet(packet: bytes):
    RNS, _LXST, _stubbed = import_rns_lxst()
    from RNS.vendor import umsgpack as mp  # noqa: WPS433

    unpacked = mp.unpackb(packet)
    signals = unpacked.get(0x00, [])
    if not isinstance(signals, list):
        signals = [signals]

    raw_frames = unpacked.get(0x01, [])
    if raw_frames and not isinstance(raw_frames, list):
        raw_frames = [raw_frames]

    frames = []
    for frame in raw_frames:
        frame = bytes(frame)
        frames.append(
            {
                "codec": frame[0],
                "payload_hex": frame[1:].hex(),
            }
        )

    return {
        "signals": [int(signal) for signal in signals],
        "frames": frames,
    }


def generate_fixtures():
    _RNS, LXST, _stubbed = import_rns_lxst()
    from RNS.vendor import umsgpack as mp  # noqa: WPS433

    cases = [
        (
            "available_signal",
            {LXST.Network.FIELD_SIGNALLING: [0x03]},
            True,
        ),
        (
            "preferred_mq_signal",
            {LXST.Network.FIELD_SIGNALLING: [0xFF + 0x40]},
            True,
        ),
        (
            "single_raw_frame",
            {LXST.Network.FIELD_FRAMES: bytes([0x00, 0x41, 0x01, 0x02, 0x03, 0x04])},
            True,
        ),
        (
            "mixed_signals_and_frames",
            {
                LXST.Network.FIELD_SIGNALLING: [0x06, 0xFF + 0x80],
                LXST.Network.FIELD_FRAMES: [
                    bytes([0x00, 0x00, 0x00, 0x80]),
                    bytes([0x02, 0x04, 0xAA, 0xBB]),
                ],
            },
            True,
        ),
    ]

    fixtures = []
    for name, data, canonical in cases:
        packet = mp.packb(data)
        decoded = decode_packet(packet)
        fixtures.append(
            {
                "name": name,
                "packet_hex": packet.hex(),
                "signals": decoded["signals"],
                "frames": decoded["frames"],
                "canonical": canonical,
            }
        )
    return fixtures


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--decode-hex", default="")
    args = parser.parse_args()

    if args.decode_hex:
        json_dump(decode_packet(bytes.fromhex(args.decode_hex)))
    else:
        json_dump(generate_fixtures())


if __name__ == "__main__":
    main()
