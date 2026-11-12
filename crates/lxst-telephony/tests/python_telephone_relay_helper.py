#!/usr/bin/env python3
"""The headless Telephone helper, plus path hop counts and per-frame codec headers.

Commands added to tools/interop/lxst_telephone_helper.py:
  {"cmd": "hops", "destination_hash": hex, "timeout": s}
      -> HOPS with RNS.Transport.hops_to
  {"cmd": "send_call_frames", "count": n}
      encodes n 48 kHz sine frames with a fresh copy of the Telephone's transmit
      codec, paced at its target frame time -> CALL_FRAMES_DONE
  {"cmd": "profile_codec", "profile": p}
      -> PROFILE_CODEC, the codec header byte, channels and frame time Telephony.Profiles gives p
Events added:
  FRAME_SENT     {"codec", "stereo" (Opus TOC s bit), "digest"} of each frame Packetizer puts on the wire
  FRAME_RECEIVED {"frames": [{"codec", "stereo", "digest"}]} of each packet LinkSource reads
"""

from __future__ import annotations

import hashlib
import math
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "tools" / "interop"))
import lxst_telephone_helper as helper  # noqa: E402

OPUS = 0x01

_setup_python_stack = helper.setup_python_stack
_handle_command = helper.handle_command


def frame_fields(frame: bytes) -> dict:
    return {
        "codec": int(frame[0]),
        "stereo": bool(frame[1] & 0x04) if frame[0] == OPUS and len(frame) > 1 else None,
        "digest": hashlib.sha256(bytes(frame)).hexdigest()[:16],
    }


def setup_python_stack():
    codec2_stubbed = _setup_python_stack()
    import LXST.Network as Network
    import RNS.vendor.umsgpack as mp
    from LXST.Codecs import codec_header_byte

    packetizer_handle_frame = Network.Packetizer.handle_frame
    link_source_packet = Network.LinkSource._packet

    def handle_frame(self, frame, source=None):
        sendable = not (
            type(self.destination) == helper._RNS.Link and not self.destination.status == helper._RNS.Link.ACTIVE
        ) and not self.squelched
        result = packetizer_handle_frame(self, frame, source)
        if sendable:
            helper.emit({"event": "FRAME_SENT", **frame_fields(codec_header_byte(type(self.source.codec)) + frame)})
        return result

    def packet(self, data, packet):
        try:
            unpacked = mp.unpackb(data)
            if type(unpacked) == dict and Network.FIELD_FRAMES in unpacked:
                frames = unpacked[Network.FIELD_FRAMES]
                if type(frames) != list:
                    frames = [frames]
                helper.emit({"event": "FRAME_RECEIVED", "frames": [frame_fields(frame) for frame in frames]})
        except Exception:
            pass
        return link_source_packet(self, data, packet)

    Network.Packetizer.handle_frame = handle_frame
    Network.LinkSource._packet = packet
    return codec2_stubbed


def send_call_frames(command: dict) -> None:
    np = helper._np
    telephone = helper._telephone
    profile = telephone.active_profile
    codec = type(telephone.transmit_codec)(profile=telephone.transmit_codec.profile)
    frame_ms = telephone.target_frame_time_ms

    class Source:
        samplerate = 48000
        channels = 1

    samples = 48000 * frame_ms // 1000
    sent = 0
    started = time.time()
    for index in range(int(command["count"])):
        t = (np.arange(samples) + index * samples) / 48000.0
        frame = (0.25 * np.sin(2 * math.pi * 440.0 * t)).astype(np.float32).reshape((samples, 1))
        # send_packetized_frame rebinds codec.source to the packetizer's source.
        codec.source = Source()
        sent += bool(helper.send_packetized_frame(codec, codec.encode(frame)))
        time.sleep(max(0.0, started + (index + 1) * frame_ms / 1000.0 - time.time()))
    helper.emit({"event": "CALL_FRAMES_DONE", "id": command.get("id"), "profile": profile, "frame_ms": frame_ms, "sent": sent})


def profile_codec(command: dict) -> None:
    from LXST.Codecs import codec_header_byte

    Profiles = helper._Telephony.Profiles
    profile = int(command["profile"])
    helper.emit(
        {
            "event": "PROFILE_CODEC",
            "id": command.get("id"),
            "profile": profile,
            "codec": codec_header_byte(type(Profiles.get_codec(profile)))[0],
            "channels": Profiles.get_codec(profile).channels,
            "frame_ms": Profiles.get_frame_time(profile),
        }
    )


def handle_command(command: dict, args) -> bool:
    if command.get("cmd") == "profile_codec":
        profile_codec(command)
        return True
    if command.get("cmd") == "send_call_frames":
        threading.Thread(target=send_call_frames, args=(command,), daemon=True).start()
        return True
    if command.get("cmd") != "hops":
        return _handle_command(command, args)
    RNS = helper._RNS
    destination_hash = bytes.fromhex(command["destination_hash"])
    deadline = time.time() + float(command.get("timeout", 10))
    requested = 0.0
    while not RNS.Transport.has_path(destination_hash) and time.time() < deadline:
        if time.time() - requested > 1.0:
            RNS.Transport.request_path(destination_hash)
            requested = time.time()
        time.sleep(0.1)
    helper.emit(
        {
            "event": "HOPS",
            "id": command.get("id"),
            "has_path": bool(RNS.Transport.has_path(destination_hash)),
            "hops": RNS.Transport.hops_to(destination_hash),
        }
    )
    return True


helper.setup_python_stack = setup_python_stack
helper.handle_command = handle_command

if __name__ == "__main__":
    raise SystemExit(helper.main())
