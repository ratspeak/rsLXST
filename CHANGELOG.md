# Changelog

## Unreleased

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

## 0.1.2 - 2026-07-26

- Updated the Reticulum dependency baseline to rsReticulum 1.1.0.
- Aligned telephony compatibility fixtures with current interface and
  responder Link registration behaviour.
