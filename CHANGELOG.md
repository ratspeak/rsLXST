# Changelog

## Unreleased

## 0.4.0 - 2026-11-13

### Added

- **Call modes**: calls can be full or half duplex, as in LXST 0.6.0, and switch during a call. `TelephonyControl::Call` takes a `mode`; `None` is full duplex.
- **Stamped calls**: call announces and link requests carry stamps when Reticulum's `[stamps]` settings ask for them, and call links use Link MTU discovery.

### Fixed and improved

- Calls connect as in LXST 0.6.0: the callee is established as soon as it answers, and a lost setup packet no longer ends a call. Check `audio_started` to know when audio is running.
- An outgoing call ends if the callee doesn't ring within `connect_timeout` (5 seconds by default), and the line is busy while a call looks for its path.
- `lxst-embedded` sessions use LXST 0.6.0's ring, wait, and connect times; change them with `SessionConfig::times`.
- The first announce goes out 5 seconds after start, with no startup burst by default.
- `opus-rs` 0.1.42 fixes a decoder crash on a crafted packet, and a damaged frame is now skipped instead of dropping its whole packet.

### Build and compatibility

- rsLXST now uses rsReticulum `~1.5.0`, whose default storage builds a bundled SQLite and needs a C compiler.
- API changes, including the removed answer retry, call modes, and async outgoing link calls, are listed in the [migration guide](api/migrations/next.md).

## 0.3.1 - 2026-11-08

- Telephony wakes on Link events instead of polling, polls once a second when
  idle, and sends announces on their own deadline instead of the idle tick.
- Replaced the AGPL license text with the LXST license.
- Uses the rsReticulum `~1.4.0` dependency line; the `v0.3.0` release used
  `~1.3.0`.
- Retire outgoing calls when Link-request dispatch or handshake queue admission
  fails, and keep established traffic bound to the original interface owner.

## 0.3.0 - 2026-10-12

### Added

- Added native Codec2 telephony streams and revocable push-to-talk input leases, with explicit audio reconfiguration and completion events.

### Embedded sessions

- Reduce 700C encoding work on constrained devices while preserving bit-exact output.

- Implement bounded native Codec2 700C and advertise standard ULBW alongside
  VLBW/LBW, with C-oracle fixtures and bidirectional Python/Rust interoperability.
  Device CPU, stack, acoustic quality and RF usability still require qualification.

- Add a no_std LXST boundary with fixed parser/queue limits and authenticated,
  accepted, audio-ready PTT gating. Share profile IDs, codec aggregation and
  call transitions with the host API. Bound profile fallback and answer retries;
  fence audio reconfiguration completions and require release after a Talk timeout.

### Codec foundations

- Add an opt-in, allocation-free `no_std` native Rust Codec2 crate for 1600/3200
  bit/s. Initialize in caller-owned storage; isolate decoder randomness; use the
  same Rust math on host and MCU; reject wrong native frame sizes. Correct the
  inherited real-FFT input-copy truncation. Preserve upstream source/license
  provenance. Target audio, CPU/stack and live integration remain separate gates.

- Add a fixed-profile Codec2 packet adapter over caller-owned PCM16/output
  buffers and an application-supplied native backend. Reject malformed lengths,
  unexpected modes and insufficient capacity before codec processing; stop
  further processing after a backend failure. Preserve native 700C frame padding.
  Native Codec2 telephony streams use this adapter boundary.

### Build and compatibility

- Raised the host source-build minimum to Rust 1.89 and qualified the corresponding protocol dependency update.
- Kept the standalone `lxst-codec2` and `lxst-embedded` minimum at Rust 1.87, with an independent MCU library check.
- Added variants to the experimental telephony event, control, and error enums; downstream exhaustive matches must handle the new cases.

- Migrated rsReticulum compatibility to the explicit `~1.3.0` line for nullable
  interface discovery and bounded RNode announce admission. Codec dependencies,
  telephony APIs and the standalone embedded compiler minimum are unchanged.


## 0.2.0 - 2026-08-17

- Document the existing registered `TelephonyService` seam as the canonical
  experimental embedding boundary, with compiled canonical/retained consumer
  contracts and no public API or runtime behavior change.

- Classified all three pre-1.0 packages as experimental and added pinned,
  CI-enforced public API snapshots without changing visibility or signatures.
- Replace the locally modified Opus 0.1.19 snapshot with exact upstream
  `opus-rs` 0.1.29 using heap-backed codec state, raise the unified stack MSRV
  to Rust 1.87, retain Android ARMv7 support, and explicitly exclude Android
  i686 from the supported artifact matrix.
- Record the exact vendored `opus-rs` lineage and local patch ledger, and add a
  locked, CI-checked production dependency license inventory.

- Replaced finite telephony announce handlers with validated destination
  recall and one deadline-bound path request, while preserving cached-path
  refresh, stale-path removal, final path confirmation, and hop refresh.
- Pinned every established telephony Link to the immutable interface that
  completed its handshake; wrong-interface traffic is rejected before crypto,
  accounting, or audio processing, and interface loss closes the call.
- Moved signalling, identification, keepalives, and close packets to ordered
  typed Link endpoint delivery. Final teardown now drains before atomic endpoint
  and temporary-destination removal.
- Kept realtime media bounded and receipt-free with exact-interface
  best-effort delivery and explicit backpressure drop accounting.
- Made live Python Telephone/TCP interoperability tests opt-in so the ordinary
  locked workspace test gate remains local and non-live.
- Advanced the experimental source boundary to 0.2 for the current telephony,
  exact-Link, Opus, and rsReticulum 1.2 integration APIs.

## 0.1.2 - 2026-07-26

- Updated the Reticulum dependency baseline to rsReticulum 1.1.0.
- Aligned telephony compatibility fixtures with current interface and
  responder Link registration behaviour.
