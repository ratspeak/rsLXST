<div align="center">

# rsLXST

**Rust LXST telephony and media streaming for Reticulum.**

[![License: LXST license](https://img.shields.io/badge/license-LXST%20license-blue.svg)](LICENSE)
[![Rust 1.89+](https://img.shields.io/badge/rust-1.89%2B-orange.svg)](https://www.rust-lang.org)
[![LXST 0.6.0](https://img.shields.io/badge/target-LXST%200.6.0-success.svg)](https://github.com/markqvist/LXST)
[![Status](https://img.shields.io/badge/status-experimental-yellow.svg)](#release-scope)

[rsLXMF](https://github.com/ratspeak/rsLXMF) |
[Ratspeak](https://github.com/ratspeak/Ratspeak) |
[rsReticulum](https://github.com/ratspeak/rsReticulum) |
[Reticulum Manual](https://reticulum.network/manual/) 

</div>

---

rsLXST is a Rust port of [LXST](https://github.com/markqvist/LXST), Mark Qvist's
Lightweight Extensible Signal Transport for real-time voice calls and other
media streams over Reticulum. It was written from the reference source with
interoperability as the primary focus.

rsLXST is Ratspeak's voice component. The LXST author licenses this port for
use in Ratspeak only, for non-commercial use, with no derivatives or use outside
Ratspeak; see [License](#license). The build, test, and API notes below document
the code for Ratspeak development.

## Contents

- [Release Scope](#release-scope)
- [Build It](#build-it)
- [Test It](#test-it)
- [Crate Layout](#crate-layout)
- [Using Telephony](#using-telephony)
- [Rust API](api/README.md)
- [License](#license)
- [Third-party licenses](#third-party-licenses)

## Release Scope

The implemented application path covers telephony signalling, full- and
half-duplex call modes, and negotiated Opus or native Codec2 media over
Reticulum Links. It does not provide a
`rnphone-rs` client, a general platform microphone/speaker backend, or LXST's
non-telephony features.

## Build It

The current development layout requires `rsReticulum` as a sibling checkout
because rsLXST uses the Rust Reticulum crates directly:

```text
ratspeak-src/
|-- rsReticulum/
`-- rsLXST/
```

If you're starting fresh:

```bash
mkdir ratspeak-src
cd ratspeak-src
git clone https://github.com/ratspeak/rsReticulum
git clone https://github.com/ratspeak/rsLXST
git -C rsReticulum checkout --detach v1.5.0
git -C rsLXST checkout --detach v0.4.0
cd rsLXST
```

The commands below run from that `rsLXST` directory.

### macOS

Install Rust with `rustup`, then install Apple's command-line build tools:

```bash
xcode-select --install
```

Build the workspace:

```bash
cargo build --release --locked
```

### Linux / Raspberry Pi

Install Rust with `rustup`, then install the usual build packages.

Debian, Ubuntu, and Raspberry Pi OS:

```bash
sudo apt update
sudo apt install -y build-essential pkg-config
```

Fedora:

```bash
sudo dnf install gcc make pkgconf-pkg-config
```

Arch:

```bash
sudo pacman -S --needed base-devel pkgconf
```

Build the workspace:

```bash
cargo build --release --locked
```

### Windows

Install Rust with the MSVC toolchain. If Rust or Cargo asks for Visual Studio
Build Tools, install the "Desktop development with C++" workload.

Build from PowerShell:

```powershell
cargo build --release --locked
```

## Test It

Run the workspace test gate:

```bash
cargo test --workspace --locked
```

Run the local CI gate:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
```

The live call tests against Python LXST are not part of ordinary runs. Run
them explicitly with:

```bash
cargo test -p lxst-telephony --test python_telephone_live_interop -- --ignored
```

Python/reference tests additionally need the reference source checkouts. They
expect upstream LXST at `../upstream/LXST` or
`LXST_UPSTREAM_DIR`, and upstream Reticulum at `../upstream/Reticulum`,
`RETICULUM_UPSTREAM_DIR`, or sibling `../rsReticulum`.

For full Python Opus media interop, install a native Opus runtime and activate
a Python virtual environment. Then install the reference dependencies; Python
3.13 and newer also need the `audioop` compatibility package:

```bash
python -m pip install numpy cryptography pyserial cffi "audioop-lts; python_version >= '3.13'"
```

## Crate Layout

| Crate | Purpose |
| --- | --- |
| `lxst-codec2` | Allocation-free native Codec2 700C/1600/3200 processing. |
| `lxst-embedded` | Bounded `no_std` framing, session/PTT policy and Codec2 integration for Ratspeak's constrained devices. |
| `lxst-core` | LXST constants, telephony profiles, signalling values, codec IDs, MessagePack packets, Raw audio frames, Opus encode/decode state, stream packetization, synthetic sources, and jitter buffers. This crate has no Reticulum runtime dependency. |
| `lxst-rns` | The Reticulum link-packet boundary for no-receipt LXST signalling and media over active links. It packs outbound LXST packets and decodes inbound link plaintext into typed LXST packet/frame events. |
| `lxst-telephony` | The telephony runtime and service layer. It owns call state, caller policy, Reticulum destination registration, announce discovery, outgoing link establishment, typed control/event channels, Opus transmit/receive stream boundaries, timeout handling, and shutdown teardown. |

## Using Telephony

Ratspeak uses `lxst-telephony` through `TelephonyService`, not by
manually translating Reticulum events. The service registers the local
`lxst.telephony` destination, emits startup/periodic announces, owns the call
runtime, and exposes typed control and event channels.

This fragment assumes an initialized Reticulum transport, local identity and
remote identity inside an async application:

```rust
use lxst_core::Profile;
use lxst_telephony::{TelephonyControl, TelephonyService};
use tokio::time::Duration;

let parts = TelephonyService::registered(transport_tx, &identity)?;
let control_tx = parts.control_tx.clone();
let mut event_rx = parts.event_rx;

tokio::spawn(parts.service.run());

control_tx
    .send(TelephonyControl::Call {
        remote_identity,
        profile: Some(Profile::QualityMedium),
        mode: None,
        discovery_timeout: Duration::from_secs(8),
    })
    .await?;
```

The service event stream is the app-facing state source. Use
`TelephonyServiceEvent::Snapshot`, `IncomingCall`, `OutgoingCallPending`,
`OutgoingCallStarted`, `OutgoingCallFailed`, `CallTerminated`, stream lifecycle
events, and media events instead of inferring call state from raw Reticulum
traffic. Outgoing announce/path discovery runs asynchronously inside the
service, so an unreachable or non-LXST peer does not block hangup, announce,
media, or shutdown controls while discovery times out.

The compiled [`service` example](crates/lxst-telephony/examples/service.rs)
checks construction and types. It does not run a transport or place a call. The lower-level endpoint and runtime types remain
available for compatible specialist use, but are experimental SPI.

For Opus calls, the host app supplies and receives `RawAudioFrame` values through
`StartOpusStream` and `StartOpusReceiveStream`. rsLXST enforces the negotiated
LXST profile and reports profile changes, source/sink closure, frame drops, and
call-end stream shutdown explicitly.

The host app still owns platform integration:

- contact or peer lookup
- UI and call controls
- microphone/camera/speaker permissions
- device selection
- audio session lifecycle
- capture/playback and resampling into `RawAudioFrame`
- settings persistence
- mobile foreground/background behavior

Ratspeak uses this boundary for its native voice-call feature.

## Embedded protocol boundary

[`lxst-embedded`](crates/lxst-embedded/README.md) shares the trusted host profile
and transition sources, with bounded borrowed MessagePack parsing, fixed media
queues and explicit authenticated-session/PTT gates. It is a no_std embedding
boundary; board drivers, transport ownership and product integration remain
application responsibilities.

## Embedded Codec2 backend

[`lxst-codec2`](crates/lxst-codec2/README.md) supplies bounded native Rust
Codec2 700C, 1600 and 3200 encode/decode with in-place initialization and no
allocator. The host telephony and embedded boundaries accept standard ULBW,
VLBW and LBW profiles. Device timing, stack and audio qualification remain
required before use on hardware. This backend does not implement Opus voice
memo containers.

## License

rsLXST is used under the license in [LICENSE](LICENSE), reproduced here:

```
LXST Copyright 2026 Mark Qvist.

This implementation of LXST is derived from the LXST reference implementation,
and used under license for Ratspeak, for non-commercial use only.
No derivatives or use outside Ratspeak is permitted.

For commercial licensing inquiries, contact license@unsigned.io
```

The adapted `lxst-codec2` crate retains its own [NOTICE](crates/lxst-codec2/NOTICE)
and upstream license files.

## Third-party licenses

Third-party components retain their own licenses. The locked production
dependency inventory, including the exact upstream BSD-3-Clause Opus package,
checksum, source commit, and preserved notice, is documented in
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).

The host workspace requires Rust 1.89 or later. The `lxst-codec2` and
`lxst-embedded` crates retain Rust 1.87 compatibility for Ratspeak's constrained
targets; CI checks that independent minimum without the host-only development
tests.
