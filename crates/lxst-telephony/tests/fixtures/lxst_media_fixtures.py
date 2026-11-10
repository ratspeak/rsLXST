"""LXST 0.6.0 media-path vectors, produced only by calling upstream code.

usage: PYTHONPATH=<RNS 1.6.0>:<LXST 0.6.0> python crates/lxst-telephony/tests/fixtures/lxst_media_fixtures.py <out.json>

Emits:
  buffer_frames: Profiles.get_buffer_frames(p) for every profile constant.
  telephone_timeouts: Telephone.RING_TIME / WAIT_TIME / CONNECT_TIME.
  raw128: upstream Raw(bitdepth=128).encode of fixed samples (platform-specific; records
          platform.machine() and numpy longdouble info) and Raw().decode of those bytes.
  mixed_batch: wire frames (codec header byte + payload) produced by upstream encoders for
          one LXST packet that switches codec mid-batch: Opus VOICE_MEDIUM 60 ms, Codec2 3200
          200 ms, Opus VOICE_MEDIUM 20 ms; plus the per-frame sample counts upstream decodes
          with LinkSource-style per-frame codec switching (Network.py:114-135).
"""
import json, platform, sys
import numpy as np
import RNS
import LXST
from LXST._version import __version__ as LXST_VERSION
from LXST.Codecs import Raw, Opus, Codec2, codec_header_byte, codec_type
from LXST.Primitives.Telephony import Profiles, Telephone

assert LXST_VERSION == "0.6.0", LXST_VERSION
assert RNS.__version__ == "1.6.0", RNS.__version__


class Src:  # encoder "source" attribute: only samplerate is read
    def __init__(self, rate): self.samplerate = rate


class Sink:  # decoder "sink": samplerate/channels are read
    def __init__(self, rate, ch): self.samplerate = rate; self.channels = ch


def tone(rate, ms):
    n = int(rate * ms / 1000)
    t = np.arange(n) / rate
    return (0.3 * np.sin(2 * np.pi * 440 * t)).reshape(n, 1).astype("float32")


out = {"lxst_version": LXST_VERSION, "rns_version": RNS.__version__,
       "generator": "crates/lxst-telephony/tests/fixtures/lxst_media_fixtures.py"}
profiles = {k: getattr(Profiles, k) for k in ["BANDWIDTH_ULTRA_LOW", "BANDWIDTH_VERY_LOW", "BANDWIDTH_LOW",
            "QUALITY_MEDIUM", "QUALITY_HIGH", "QUALITY_MAX", "LATENCY_ULTRA_LOW", "LATENCY_LOW"]}
out["buffer_frames"] = {str(v): Profiles.get_buffer_frames(v) for v in profiles.values()}
out["frame_time_ms"] = {str(v): Profiles.get_frame_time(v) for v in profiles.values()}
out["telephone_timeouts"] = {"RING_TIME": Telephone.RING_TIME, "WAIT_TIME": Telephone.WAIT_TIME,
                             "CONNECT_TIME": Telephone.CONNECT_TIME}

samples = np.array([[0.0], [0.5], [-0.25], [1.0]], dtype="float32")
raw = Raw(channels=1, bitdepth=128)
enc = raw.encode(samples)
dec = Raw().decode(enc)
out["raw128"] = {"machine": platform.machine(), "longdouble_bits": int(np.finfo(np.longdouble).bits),
                 "longdouble_nmant": int(np.finfo(np.longdouble).nmant),
                 "samples": samples[:, 0].tolist(), "payload_hex": enc.hex(),
                 "decoded": [float(x) for x in dec[:, 0]]}

frames = []
o = Opus(profile=Opus.PROFILE_VOICE_MEDIUM); o.source = Src(24000)
frames.append(("opus_60ms", codec_header_byte(Opus) + o.encode(tone(24000, 60))))
c = Codec2(mode=Codec2.CODEC2_3200); c.source = Src(8000)
frames.append(("codec2_3200_200ms", codec_header_byte(Codec2) + c.encode(np.repeat(tone(8000, 200), 2, axis=1))))
frames.append(("opus_20ms", codec_header_byte(Opus) + o.encode(tone(24000, 20))))

# Upstream receive-side per-frame codec switching, as in LinkSource._packet.
sink = Sink(48000, 1)
codec = Opus(); codec.sink = sink
decoded = []
for name, fb in frames:
    ft = codec_type(fb[0])
    if type(codec) != ft:
        codec = ft(); codec.sink = sink
    d = codec.decode(fb[1:])
    decoded.append({"name": name, "codec": int(fb[0]), "wire_hex": fb.hex(), "decoded_sample_frames": int(d.shape[0])})
out["mixed_batch"] = decoded
open(sys.argv[1], "w").write(json.dumps(out, indent=1))
print(json.dumps(out, indent=1)[:3000])
