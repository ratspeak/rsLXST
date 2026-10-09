# Changelog

## Unreleased

### Embedded sessions

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
  Codec backend selection and live Codec2 telephony remain separate work.

### Build and compatibility

- Raised the host source-build minimum to Rust 1.89 and qualified the corresponding protocol dependency update.
- Kept the standalone `lxst-codec2` and `lxst-embedded` minimum at Rust 1.87, with an independent MCU library check.

- Restricted rsReticulum compatibility to the 1.3 patch line and updated the pinned source. Codec dependencies and telephony APIs are unchanged.


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
