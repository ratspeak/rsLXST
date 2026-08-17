# Rust API

rsLXST is experimental and remains below 1.0. Its current application boundary
for telephony is the existing `lxst_telephony::TelephonyService` service seam:

- construct it with `TelephonyService::registered` or
  `TelephonyService::registered_with_config`;
- retain the `TelephonyServiceParts::control_tx` and `event_rx` handles;
- drive the owned service with `TelephonyService::run`; and
- use `TelephonyControl`, `request_answer`, and `TelephonyServiceEvent` without
  translating raw Reticulum events in the application.

The compiled `lxst-telephony` `service` example exercises this path.

## Stability

All three packages are experimental:

- `lxst-core` contains codec, profile, stream, signalling, and wire concepts;
- `lxst-rns` binds media packets to Reticulum Links; and
- `lxst-telephony` contains the service boundary plus extensive runtime and
  state-machine machinery.

Selecting `TelephonyService` does not stabilize the whole package or change
channel capacities, backpressure, event ordering, cancellation, timeouts,
media, exact-Link, or shutdown behavior. `TelephonyRnsEndpoint`,
`TelephonyRuntimeCore`, `TelephonyCommand`, `TelephonyDriveStep`,
`TelephonyService::new`, and `TelephonyService::with_config` remain available
for compatibility and specialist testing, but are implementation-level APIs
rather than the recommended application construction path.

## Compatibility checks

The `api/` directory contains the evidence used by CI:

- `stability.json` records package tiers, source commits, snapshot hashes, and
  the current review decision;
- `snapshots/` records the explicit all-feature Apple ARM64 Rust API and the
  manifest, feature, dependency, target, and MSRV contract; and
- `fixtures/` compiles the recommended and retained paths as an external
  consumer.

These checks catch accidental changes, but they do not replace Android and
Apple target builds, media tests, Python Telephone interoperability, or manual
review. The API snapshot omits auto-derived, auto-trait, and blanket
implementations and is not by itself a complete SemVer verdict.

Run the checks with:

```sh
python3 tools/check-api-baseline.py
python3 tools/check-api-manifest.py
python3 tools/check-api-compatibility.py
cargo check --manifest-path api/fixtures/Cargo.toml --locked
```

Snapshot updates require a clean source commit and an explicit review recorded
in `api/stability.json`. Additions, removals, deprecations, platform impact, and
version consequences must be reviewed before accepting new evidence.
