# Experimental telephony service API

The current canonical application boundary is the existing
`lxst_telephony::TelephonyService` service seam:

- construct it with `TelephonyService::registered` or
  `TelephonyService::registered_with_config`;
- retain the `TelephonyServiceParts::control_tx` and `event_rx` handles;
- drive the owned service with `TelephonyService::run`;
- use `TelephonyControl`, `request_answer`, and `TelephonyServiceEvent` without
  translating raw Reticulum events in the application.

This boundary is experimental. Selection does not stabilize the whole package
or alter the existing channel capacities, backpressure, event ordering,
cancellation, timeout, media, exact-Link, or shutdown behavior.

`TelephonyRnsEndpoint`, `TelephonyRuntimeCore`, `TelephonyCommand`,
`TelephonyDriveStep`, `TelephonyService::new`, and
`TelephonyService::with_config` remain public for compatibility and specialist
testing. They are experimental implementation SPI and are not the recommended
application construction path. No compatibility path is deprecated in this
Wave C milestone.

The compiled `service` example and external fixture exercise the selected and
retained paths. API snapshots, manifest/feature contracts, Android/Apple target
builds, and Python Telephone interoperability are separate evidence layers;
none is replaced by this documentation boundary.
