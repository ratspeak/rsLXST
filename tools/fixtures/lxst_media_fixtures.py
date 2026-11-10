#!/usr/bin/env python3
"""Generate LXST media-path vectors by calling upstream Python LXST.

usage: python tools/fixtures/lxst_media_fixtures.py crates/lxst-telephony/tests/fixtures/media_vectors.json

Upstream checkouts come from LXST_UPSTREAM_DIR / RETICULUM_UPSTREAM_DIR, or the
workspace upstream directory (see tools/common/lxst_reference.py). Real Opus and
Codec2 codecs are required: the generator refuses to run with the Codec2 stub.

Emits buffer_frames and frame_time_ms per profile, raw128 (Raw(bitdepth=128) bytes,
platform-specific) and mixed_batch (one packet switching codec mid-batch, with the
sample counts upstream's per-frame codec switching decodes, Network.py LinkSource).
"""

from __future__ import annotations

import json
import platform
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "common"))
from lxst_reference import git_output, prepare_python_path, upstream_lxst_root, upstream_reticulum_root  # noqa: E402

try:
    import pycodec2  # noqa: F401  loaded first so the locator never installs its Codec2 stub
except ImportError:
    raise SystemExit("pycodec2 is not installed; Codec2 vectors need the real codec")

prepare_python_path()

import numpy as np  # noqa: E402
import RNS  # noqa: E402
from LXST._version import __version__ as LXST_VERSION  # noqa: E402
from LXST.Codecs import Codec2, Opus, Raw, codec_header_byte, codec_type  # noqa: E402
from LXST.Primitives.Telephony import Profiles  # noqa: E402

assert LXST_VERSION == "0.6.0", LXST_VERSION
assert RNS.__version__ == "1.6.0", RNS.__version__


class Src:  # encoders read only samplerate from their source
    def __init__(self, rate):
        self.samplerate = rate


class Sink:  # decoders read samplerate and channels from their sink
    def __init__(self, rate, channels):
        self.samplerate = rate
        self.channels = channels


def tone(rate, ms):
    n = int(rate * ms / 1000)
    t = np.arange(n) / rate
    return (0.3 * np.sin(2 * np.pi * 440 * t)).reshape(n, 1).astype("float32")


def main() -> None:
    out_path = Path(sys.argv[1])
    lxst_root = upstream_lxst_root()
    out = {
        "generator": "tools/fixtures/lxst_media_fixtures.py",
        "lxst_version": LXST_VERSION,
        "lxst_commit": git_output(["rev-parse", "HEAD"], lxst_root),
        "rns_version": RNS.__version__,
        "rns_commit": git_output(["rev-parse", "HEAD"], upstream_reticulum_root()),
    }
    profiles = [Profiles.BANDWIDTH_ULTRA_LOW, Profiles.BANDWIDTH_VERY_LOW, Profiles.BANDWIDTH_LOW,
                Profiles.QUALITY_MEDIUM, Profiles.QUALITY_HIGH, Profiles.QUALITY_MAX,
                Profiles.LATENCY_ULTRA_LOW, Profiles.LATENCY_LOW]
    out["buffer_frames"] = {str(p): Profiles.get_buffer_frames(p) for p in profiles}
    out["frame_time_ms"] = {str(p): Profiles.get_frame_time(p) for p in profiles}

    samples = np.array([[0.0], [0.5], [-0.25], [1.0]], dtype="float32")
    encoded = Raw(channels=1, bitdepth=128).encode(samples)
    decoded = Raw().decode(encoded)
    out["raw128"] = {"machine": platform.machine(),
                     "longdouble_bits": int(np.finfo(np.longdouble).bits),
                     "longdouble_nmant": int(np.finfo(np.longdouble).nmant),
                     "samples": samples[:, 0].tolist(), "payload_hex": encoded.hex(),
                     "decoded": [float(x) for x in decoded[:, 0]]}

    frames = []
    opus = Opus(profile=Opus.PROFILE_VOICE_MEDIUM)
    opus.source = Src(24000)
    frames.append(("opus_60ms", codec_header_byte(Opus) + opus.encode(tone(24000, 60))))
    codec2 = Codec2(mode=Codec2.CODEC2_3200)
    codec2.source = Src(8000)
    frames.append(("codec2_3200_200ms", codec_header_byte(Codec2) + codec2.encode(np.repeat(tone(8000, 200), 2, axis=1))))
    frames.append(("opus_20ms", codec_header_byte(Opus) + opus.encode(tone(24000, 20))))

    sink = Sink(48000, 1)
    codec = Opus()
    codec.sink = sink
    batch = []
    for name, wire in frames:
        frame_codec = codec_type(wire[0])
        if type(codec) != frame_codec:
            codec = frame_codec()
            codec.sink = sink
        samples_out = codec.decode(wire[1:])
        batch.append({"name": name, "codec": int(wire[0]), "wire_hex": wire.hex(),
                      "decoded_sample_frames": int(samples_out.shape[0])})
    out["mixed_batch"] = batch

    lines = []
    for key, value in out.items():
        if isinstance(value, list):
            items = ",\n".join("  " + json.dumps(item) for item in value)
            lines.append(f"{json.dumps(key)}: [\n{items}\n ]")
        else:
            lines.append(f"{json.dumps(key)}: {json.dumps(value)}")
    out_path.write_text("{\n " + ",\n ".join(lines) + "\n}\n")


if __name__ == "__main__":
    main()
