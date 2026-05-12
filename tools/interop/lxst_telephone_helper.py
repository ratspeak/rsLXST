#!/usr/bin/env python3
"""Headless Python LXST Telephone helper for Rust interop tests."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "common"))
from lxst_reference import import_rns_lxst, upstream_lxst_root  # noqa: E402

_emit_lock = threading.Lock()
_telephone = None
_RNS = None
_LXST = None
_Telephony = None
_np = None
_opus_available = False


def emit(event: dict) -> None:
    with _emit_lock:
        print(json.dumps(event, sort_keys=True, separators=(",", ":")), flush=True)


def build_rns_config(tcp_role: str, tcp_host: str, tcp_port: int, enable_transport: bool) -> str:
    transport = "yes" if enable_transport else "no"
    if tcp_port <= 0:
        return f"""[reticulum]
  enable_transport = {transport}
  share_instance = no

[interfaces]
"""

    if tcp_role == "server":
        return f"""[reticulum]
  enable_transport = {transport}
  share_instance = no

[interfaces]
  [[TCP Server]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = {tcp_host}
    listen_port = {tcp_port}
"""

    return f"""[reticulum]
  enable_transport = {transport}
  share_instance = no

[interfaces]
  [[TCP Client]]
    type = TCPClientInterface
    enabled = yes
    target_host = {tcp_host}
    target_port = {tcp_port}
"""


def python_opus_available(Telephony) -> bool:
    try:
        Telephony.Opus()
        return True
    except Exception:
        return False


def patch_headless_audio(Telephony, LXST, np, opus_available: bool) -> None:
    from LXST.Codecs import Null
    from LXST.Sinks import LocalSink
    from LXST.Sources import LocalSource

    class HeadlessLineSink(LocalSink):
        def __init__(self, preferred_device=None, autodigest=True, low_latency=False):
            self.preferred_device = preferred_device
            self.autodigest = autodigest
            self.low_latency = low_latency
            self.should_run = False
            self.samplerate = 48000
            self.channels = 1

        def start(self):
            self.should_run = True

        def stop(self):
            self.should_run = False

        def enable_low_latency(self):
            self.low_latency = True

        def handle_frame(self, frame, source):
            return None

    class HeadlessLineSource(LocalSource):
        def __init__(
            self,
            preferred_device=None,
            target_frame_ms=60,
            codec=None,
            sink=None,
            filters=None,
            gain=0.0,
            ease_in=0.0,
            skip=0.0,
            **_kwargs,
        ):
            self.preferred_device = preferred_device
            self.target_frame_ms = target_frame_ms
            self.codec = codec or Null()
            self.sink = sink
            self.filters = filters or []
            self.gain = gain
            self.ease_in = ease_in
            self.skip = skip
            self.should_run = False
            self.pipeline = None
            self.samplerate = 48000
            self.channels = 1

        def start(self):
            self.should_run = True

        def stop(self):
            self.should_run = False

    class HeadlessToneSource(HeadlessLineSource):
        def __init__(self, *args, frequency=382, **kwargs):
            super().__init__(*args, **kwargs)
            self.frequency = frequency
            self.running = False

        def start(self):
            self.should_run = True
            self.running = True

        def stop(self):
            self.should_run = False
            self.running = False

    class HeadlessOpusFileSource(HeadlessLineSource):
        pass

    class HeadlessMixer(LocalSource, LocalSink):
        def __init__(self, target_frame_ms=60, samplerate=None, codec=None, sink=None, gain=0.0):
            self.target_frame_ms = target_frame_ms
            self.samplerate = samplerate or 48000
            self.channels = 1
            self.codec = codec or Null()
            self.sink = sink
            self.gain = gain
            self.muted = False
            self.should_run = False
            self.pipeline = None
            self.incoming_frames = {}

        def start(self):
            self.should_run = True

        def stop(self):
            self.should_run = False

        def set_gain(self, gain=None):
            self.gain = 0.0 if gain is None else float(gain)

        def mute(self, mute=True):
            self.muted = bool(mute)

        def unmute(self, unmute=True):
            self.muted = not bool(unmute)

        def set_source_max_frames(self, source, max_frames):
            self.incoming_frames[source] = max_frames

        def can_receive(self, from_source=None):
            return True

        def handle_frame(self, frame, source, decoded=False):
            samples = frame if decoded else source.codec.decode(frame)
            arr = np.asarray(samples, dtype=np.float32)
            if arr.ndim == 1:
                arr = arr.reshape((-1, 1))
            self.channels = int(arr.shape[1])
            if hasattr(source, "samplerate"):
                self.samplerate = int(source.samplerate)
            flat = [float(v) for v in arr.reshape(-1).tolist()]
            emit(
                {
                    "event": "MEDIA_FRAME",
                    "decoded": True,
                    "shape": [int(arr.shape[0]), int(arr.shape[1])],
                    "samples": flat,
                    "total_samples": len(flat),
                }
            )

    class HeadlessPipeline:
        def __init__(self, source, codec, sink, processor=None):
            self.source = source
            self.sink = sink
            self._codec = None
            self.processor = processor
            self.source.pipeline = self
            self.source.sink = sink
            self.codec = codec
            if hasattr(sink, "source"):
                sink.source = source

        @property
        def codec(self):
            return self.source.codec

        @codec.setter
        def codec(self, codec):
            self._codec = codec
            self.source.codec = codec
            self.source.codec.source = self.source
            self.source.codec.sink = self.sink

        @property
        def running(self):
            return bool(getattr(self.source, "should_run", False))

        def start(self):
            self.source.start()

        def stop(self):
            self.source.stop()

    class NoopFilter:
        def __init__(self, *args, **kwargs):
            pass

        def handle_frame(self, frame, *args, **kwargs):
            return frame

    Telephony.LineSink = HeadlessLineSink
    Telephony.LineSource = HeadlessLineSource
    Telephony.OpusFileSource = HeadlessOpusFileSource
    Telephony.ToneSource = HeadlessToneSource
    Telephony.Mixer = HeadlessMixer
    Telephony.Pipeline = HeadlessPipeline
    Telephony.BandPass = NoopFilter
    Telephony.AGC = NoopFilter

    if not opus_available:
        original_get_codec = Telephony.Profiles.get_codec

        def get_codec(profile):
            if profile in (
                Telephony.Profiles.QUALITY_MEDIUM,
                Telephony.Profiles.QUALITY_HIGH,
                Telephony.Profiles.QUALITY_MAX,
                Telephony.Profiles.LATENCY_LOW,
                Telephony.Profiles.LATENCY_ULTRA_LOW,
            ):
                return Null()
            return original_get_codec(profile)

        Telephony.Profiles.get_codec = staticmethod(get_codec)


def patch_signalling_events(Telephony) -> None:
    original = Telephony.Telephone.signalling_received

    def signalling_received(self, signals, source):
        emit({"event": "SIGNALS", "signals": [int(signal) for signal in signals]})
        return original(self, signals, source)

    Telephony.Telephone.signalling_received = signalling_received


def setup_python_stack():
    global _RNS, _LXST, _Telephony, _np, _opus_available
    _RNS, _LXST, codec2_stubbed = import_rns_lxst()
    import numpy as np
    import LXST.Primitives.Telephony as Telephony

    _opus_available = python_opus_available(Telephony)
    patch_headless_audio(Telephony, _LXST, np, _opus_available)
    patch_signalling_events(Telephony)
    _Telephony = Telephony
    _np = np
    return codec2_stubbed


def snapshot_event():
    telephone = _telephone
    active = None
    if telephone and telephone.active_call:
        remote = telephone.active_call.get_remote_identity()
        active = {
            "identity_hash": bytes(remote.hash).hex() if remote else None,
            "profile": getattr(telephone.active_call, "profile", None),
            "status": telephone.call_status,
            "answered": bool(getattr(telephone.active_call, "answered", False)),
        }

    return {
        "event": "SNAPSHOT",
        "active_call": active,
        "active_profile": telephone.active_profile if telephone else None,
        "busy": bool(telephone.busy) if telephone else False,
        "call_status": telephone.call_status if telephone else None,
        "link_count": len(telephone.links) if telephone else 0,
    }


def emit_ready(identity, telephone, codec2_stubbed):
    from LXST._version import __version__ as lxst_version

    emit(
        {
            "event": "READY",
            "app_name": "lxst",
            "primitive_name": "telephony",
            "lxst_version": lxst_version,
            "lxst_root": str(upstream_lxst_root()),
            "codec2_stubbed": bool(codec2_stubbed),
            "headless_audio": True,
            "native_filters_disabled": True,
            "identity_hash": bytes(identity.hash).hex(),
            "destination_hash": bytes(telephone.destination.hash).hex(),
            "expanded_name": f"lxst.telephony.{bytes(identity.hash).hex()}",
            "opus_available": _opus_available,
        }
    )


def remote_identity_hash(remote_identity):
    return bytes(remote_identity.hash).hex() if remote_identity else None


def configure_callbacks(telephone):
    def ringing(remote_identity):
        emit({"event": "RINGING", "identity_hash": remote_identity_hash(remote_identity)})

    def established(remote_identity):
        emit({"event": "ESTABLISHED", "identity_hash": remote_identity_hash(remote_identity)})

    def ended(remote_identity):
        emit({"event": "ENDED", "identity_hash": remote_identity_hash(remote_identity)})

    def busy(remote_identity):
        emit({"event": "BUSY", "identity_hash": remote_identity_hash(remote_identity)})

    def rejected(remote_identity):
        emit({"event": "REJECTED", "identity_hash": remote_identity_hash(remote_identity)})

    telephone.set_ringing_callback(ringing)
    telephone.set_established_callback(established)
    telephone.set_ended_callback(ended)
    telephone.set_busy_callback(busy)
    telephone.set_rejected_callback(rejected)


def setup_reticulum(storage: Path, tcp_role: str, tcp_host: str, tcp_port: int, enable_transport: bool):
    if storage.exists():
        shutil.rmtree(storage)
    storage.mkdir(parents=True, exist_ok=True)
    config_dir = storage / "rns"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config").write_text(
        build_rns_config(tcp_role, tcp_host, tcp_port, enable_transport),
        encoding="utf-8",
    )

    loglevel = os.environ.get("LXST_PYTHON_LOGLEVEL")
    if loglevel:
        resolved_loglevel = int(loglevel) if loglevel.isdigit() else getattr(_RNS, loglevel)
        _RNS.logfile = str(storage / "rns_debug.log")
        return _RNS.Reticulum(
            configdir=str(config_dir),
            loglevel=resolved_loglevel,
            logdest=_RNS.LOG_FILE,
        )

    return _RNS.Reticulum(configdir=str(config_dir))


def create_telephone(args):
    global _telephone
    identity = _RNS.Identity()
    telephone = _Telephony.Telephone(
        identity,
        ring_time=args.ring_time,
        wait_time=args.wait_time,
    )
    configure_callbacks(telephone)
    _telephone = telephone
    return identity, telephone


def active_remote_identity():
    if not _telephone or not _telephone.active_call:
        return None
    return _telephone.active_call.get_remote_identity()


def recall_identity_for_destination(destination_hash: bytes, timeout: float):
    identity = _RNS.Identity.recall(destination_hash)
    if identity:
        return identity

    _RNS.Transport.request_path(destination_hash)
    deadline = time.time() + timeout
    while time.time() < deadline:
        identity = _RNS.Identity.recall(destination_hash)
        if identity:
            return identity
        time.sleep(0.1)
    return None


def send_packetized_frame(codec, encoded_frame: bytes) -> bool:
    if not _telephone or not _telephone.active_call:
        return False
    packetizer = getattr(_telephone.active_call, "packetizer", None)
    if packetizer is None:
        return False
    source = getattr(packetizer, "source", None)
    if source is None:
        class Source:
            pass

        source = Source()
        packetizer.source = source
    source.codec = codec
    source.codec.source = source
    packetizer.handle_frame(encoded_frame)
    return True


def handle_command(command: dict, args) -> bool:
    cmd = command.get("cmd")

    if cmd == "shutdown":
        return False

    if cmd == "snapshot":
        event = snapshot_event()
        if "id" in command:
            event["id"] = command["id"]
        emit(event)
        return True

    if cmd == "announce":
        _telephone.announce()
        emit(
            {
                "event": "ANNOUNCED",
                "id": command.get("id"),
                "destination_hash": bytes(_telephone.destination.hash).hex(),
            }
        )
        return True

    if cmd == "answer":
        identity = active_remote_identity()
        accepted = bool(identity and _telephone.answer(identity))
        emit({"event": "ANSWERED", "id": command.get("id"), "accepted": accepted})
        return True

    if cmd == "hangup":
        _telephone.hangup()
        emit({"event": "HUNG_UP", "id": command.get("id")})
        return True

    if cmd == "set_busy":
        busy = bool(command.get("busy"))
        _telephone.set_busy(busy)
        emit({"event": "BUSY_SET", "id": command.get("id"), "busy": busy})
        return True

    if cmd == "call":
        destination_hash = bytes.fromhex(command["target_destination_hash"])
        identity = recall_identity_for_destination(destination_hash, args.wait_time)
        if identity is None:
            emit(
                {
                    "event": "ERROR",
                    "id": command.get("id"),
                    "message": "target identity was not recalled before timeout",
                }
            )
            return True
        _telephone.call(identity, profile=command.get("profile"))
        emit(
            {
                "event": "CALL_REQUESTED",
                "id": command.get("id"),
                "target_destination_hash": destination_hash.hex(),
            }
        )
        return True

    if cmd == "send_raw_frame":
        from LXST.Codecs import Raw

        channels = int(command.get("channels", 1))
        bitdepth = int(command.get("bitdepth", 32))
        samples = _np.asarray(command.get("samples", []), dtype=_np.float32)
        frame = samples.reshape((len(samples) // channels, channels))
        codec = Raw(channels=channels, bitdepth=bitdepth)
        encoded = codec.encode(frame)
        sent = send_packetized_frame(codec, encoded)
        emit({"event": "RAW_FRAME_SENT", "id": command.get("id"), "sent": sent})
        return True

    if cmd == "send_opus_frame":
        if not _opus_available:
            emit(
                {
                    "event": "OPUS_FRAME_SENT",
                    "id": command.get("id"),
                    "sent": False,
                    "error": "opus_unavailable",
                }
            )
            return True

        from LXST.Codecs import Opus

        profile = int(command.get("profile", Opus.PROFILE_VOICE_MEDIUM))
        channels = int(command.get("channels", 1))
        samplerate = int(command.get("samplerate", 24000))
        samples = _np.asarray(command.get("samples", []), dtype=_np.float32)
        frame = samples.reshape((len(samples) // channels, channels))
        codec = Opus(profile=profile)

        class Source:
            pass

        source = Source()
        source.samplerate = samplerate
        source.channels = channels
        codec.source = source
        encoded = codec.encode(frame)
        sent = send_packetized_frame(codec, encoded)
        emit({"event": "OPUS_FRAME_SENT", "id": command.get("id"), "sent": sent})
        return True

    if cmd == "switch_profile":
        profile = int(command["profile"])
        _telephone.switch_profile(profile)
        emit(
            {
                "event": "PROFILE_SWITCHED",
                "id": command.get("id"),
                "profile": _telephone.active_profile,
            }
        )
        return True

    emit({"event": "ERROR", "message": f"unknown command: {cmd}"})
    return True


def run_command_loop(args):
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            command = json.loads(line)
            if not handle_command(command, args):
                break
        except Exception as exc:  # noqa: BLE001
            emit({"event": "ERROR", "message": str(exc)})


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=["self-test", "host"], required=True)
    parser.add_argument("--storage-dir", required=True)
    parser.add_argument("--tcp-role", choices=["server", "client"], default="client")
    parser.add_argument("--tcp-host", default="127.0.0.1")
    parser.add_argument("--tcp-port", type=int, default=0)
    parser.add_argument("--ring-time", type=float, default=20.0)
    parser.add_argument("--wait-time", type=float, default=20.0)
    parser.add_argument("--enable-transport", action="store_true")
    args = parser.parse_args()

    try:
        codec2_stubbed = setup_python_stack()
        setup_reticulum(
            Path(args.storage_dir),
            args.tcp_role,
            args.tcp_host,
            args.tcp_port,
            args.enable_transport,
        )
        identity, telephone = create_telephone(args)
        emit_ready(identity, telephone, codec2_stubbed)
        if args.mode == "self-test":
            emit(snapshot_event())
        else:
            run_command_loop(args)
        telephone.teardown()
        emit({"event": "STOPPED"})
        return 0
    except Exception as exc:  # noqa: BLE001
        emit({"event": "FATAL", "message": str(exc)})
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
