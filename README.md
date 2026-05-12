<div align="center">

# rsLXST

**Rust LXST telephony and media streaming for Reticulum.**

[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)
[![LXST 0.4.5](https://img.shields.io/badge/target-LXST%200.4.5-success.svg)](https://github.com/markqvist/LXST)
[![Status](https://img.shields.io/badge/status-library-yellow.svg)](#feature-status)

[rsLXMF](https://github.com/ratspeak/rsLXMF) |
[Ratspeak](https://github.com/ratspeak/Ratspeak) |
[rsReticulum](https://github.com/ratspeak/rsReticulum) |
[Reticulum Manual](https://reticulum.network/manual/) 

</div>

---

rsLXST is a Rust implementation of [LXST](https://github.com/markqvist/LXST), the Lightweight Extensible Signal
Transport used for real-time voice calls and other media streams over Reticulum. This is not a
fork of LXST; it is LXST written in a different language with interoperability
as the primary focus. Python LXST remains the source-of-truth
implementation, do not treat this repository as one.

The current rsLXST is experimental and incomplete. It provides LXST wire codecs,
Reticulum link media packet boundaries, a telephony runtime, and Opus stream
integration for applications (such as Ratspeak).

The first public target is interoperable Opus
telephony, not complete feature parity with
the reference implementation LXST.

## Contents

- [Release Scope](#release-scope)
- [Build It](#build-it)
- [Test It](#test-it)
- [Crate Layout](#crate-layout)
- [Using Telephony](#using-telephony)
- [Contributing](#contributing)
- [License](#license)

## Release Scope

The experimental release is to cover basic voice calls, with several features still unsupported:

- `rnphone` parity and `rnphone-rs` usage.
- Full Codec2 support.
- Deeper audio support: microphone/source backends, filters, AGC, etc.
- Broadcast, stream, and non-telephony LXST primitives.

Those are expected future work. They should not be implied by the first public
Opus telephony release.

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
cd rsLXST
```

### macOS

Install Rust with `rustup`, then install Apple's command-line build tools:

```bash
xcode-select --install
```

Build the workspace:

```bash
cd rsLXST
cargo build --release
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
cd rsLXST
cargo build --release
```

### Windows

Install Rust with the MSVC toolchain. If Rust or Cargo asks for Visual Studio
Build Tools, install the "Desktop development with C++" workload.

Build from PowerShell:

```powershell
cd rsLXST
cargo build --release
```

## Test It

Run the workspace test gate:

```bash
cargo test --workspace
```

Run the local CI gate:

```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

The test gate covers wire codecs, telephony state, profile metadata, Opus
stream boundaries, malformed-input handling, the local service runtime, Python
LXST wire parity, Reticulum destination parity, and live headless LXST
Telephone interop. The Python tests expect upstream LXST at `../upstream/LXST`
or `LXST_UPSTREAM_DIR`, and upstream Reticulum at `../upstream/Reticulum`,
`RETICULUM_UPSTREAM_DIR`, or sibling `../rsReticulum`.

For full Python Opus media interop, install a native Opus runtime as well as
the Python reference dependencies:

```bash
python -m pip install numpy cryptography pyserial cffi
```

## Crate Layout

| Crate | Purpose |
| --- | --- |
| `lxst-core` | LXST constants, telephony profiles, signalling values, codec IDs, MessagePack packets, Raw audio frames, Opus encode/decode state, stream packetization, synthetic sources, and jitter buffers. This crate has no Reticulum runtime dependency. |
| `lxst-rns` | The Reticulum link-packet boundary for no-receipt LXST signalling and media over active links. It packs outbound LXST packets and decodes inbound link plaintext into typed LXST packet/frame events. |
| `lxst-telephony` | The telephony runtime and service layer. It owns call state, caller policy, Reticulum destination registration, announce discovery, outgoing link establishment, typed control/event channels, Opus transmit/receive stream boundaries, timeout handling, and shutdown teardown. |

## Using Telephony

Applications normally use `lxst-telephony` through `TelephonyService`, not by
manually translating Reticulum events. The service registers the local
`lxst.telephony` destination, emits startup/periodic announces, owns the call
runtime, and exposes typed control and event channels.

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

For Opus calls, applications supply and receive `RawAudioFrame` values through
`StartOpusStream` and `StartOpusReceiveStream`. rsLXST enforces the negotiated
LXST profile and reports profile changes, source/sink closure, frame drops, and
call-end stream shutdown explicitly.

Applications still own platform integration:

- contact or peer lookup
- UI and call controls
- microphone/camera/speaker permissions
- device selection
- audio session lifecycle
- capture/playback and resampling into `RawAudioFrame`
- settings persistence
- mobile foreground/background behavior

Ratspeak uses this boundary for its native voice-call feature.

## Contributing

If the issue or contribution belongs upstream as well, start there. Python LXST
and Reticulum remain the reference implementations.

PRs are closed for now until I have time to catch up on everything.

## License

Licensed under the GNU Affero General
Public License v3.0 or later. See [LICENSE](LICENSE).
