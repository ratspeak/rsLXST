# Migrating to rsLXST 0.4 (draft)

This draft covers source and behaviour changes since rsLXST 0.3.1. Telephony
now follows LXST 0.6.0 signalling, and rsLXST 0.4 runs on rsReticulum 1.5,
which follows Reticulum 1.6.0.

Move rsReticulum dependencies to `~1.5.0` together with rsLXST 0.4; types from
two rsReticulum lines do not mix. rsReticulum types that appear in rsLXST
signatures (`TransportMessage`, `Link`, `LinkManager`, and `Identity`) change
in 1.5; rsReticulum's own migration guide covers them.

## Telephony: answering

LXST 0.6.0 establishes the callee as soon as it answers: CONNECTING and
ESTABLISHED are sent once, with no retry and no wait for the caller's echo.

| Old | New |
| --- | --- |
| `lxst_core::TelephonyCall::retry_answer()` | removed; `answer()` returns the whole answer and ends with `StartAudioPipelines` |
| `lxst_embedded::CallState::retry_answer()` | removed; as above |
| `TelephonyServiceConfig::answer_retry_interval` | removed; delete the field |
| `TelephonyServiceConfig::answer_timeout` | removed; delete the field |
| (none) | `TelephonyServiceConfig::connect_timeout`; a literal without `..TelephonyServiceConfig::default()` must set it |

`request_answer` and `TelephonyControl::Answer` now reply with an ESTABLISHED
snapshot where they replied with CONNECTING.

A caller is ESTABLISHED once it has handled CONNECTING, so a lost ESTABLISHED
no longer ends the call. Its audio still starts only on the callee's
ESTABLISHED, so `status()` no longer shows that audio is running: check
`audio_started()` (`TelephonyCall`, `CallState`) or wait for
`StartAudioPipelines`.

## Telephony: call modes

Full and half duplex are signalled with the profile when the callee rings.

```rust
// old
TelephonyControl::Call { remote_identity, profile, discovery_timeout }
// new; `None` is full duplex
TelephonyControl::Call { remote_identity, profile, mode: None, discovery_timeout }
```

These types gain variants or fields, so exhaustive matches need new arms and
struct literals need the new fields:

| Type | Added |
| --- | --- |
| `Signal` (`lxst-core`, `lxst-embedded`) | `PreferredMode(u8)`; wire values `0xF0` to `0xFE` decode to it, not to `Raw` |
| `TelephonyAction` (`lxst-core`, `lxst-embedded`) | `SendSignalPair`, `SelectMode`, `SwitchMode` |
| `TelephonyCommand` | `SendSignalPair`, `SelectMode`, `SwitchMode` |
| `TelephonyControl` | `SwitchMode`, `SquelchTransmit`, `DisableRemoteModeFollow` |
| `ActiveCallSnapshot` | `mode`, `transmit_squelched` |

A caller that sees RINGING now emits one `SendSignalPair(profile, mode)` where
it emitted `SendSignal(profile)`. Both signals travel in one packet:
`TelephonyCommand::signal()` returns `None` for the pair, so build the packet
with `TelephonyCommand::to_lxst_packet()`.

## Telephony: outgoing call links

LXST 0.6.0 opens the call with a plain `RNS.Link(call_destination, ...)`
(`LXST/Primitives/Telephony.py:840-842`). On Reticulum 1.6.0 that link request
carries a stamp at the link cost the destination announced
(`RNS/Link.py:363`). Generating that stamp is CPU work, so building the link
request is now async:

| Old | New |
| --- | --- |
| `fn TelephonyRnsEndpoint::begin_outgoing_link_with_remote_pubkey(..) -> Result<LinkId, Error>` | `async fn`; add `.await` |
| (none) | `async fn TelephonyRnsEndpoint::begin_outgoing_link_with_mode(..)`, the same call with an `Option<CallMode>` |

```rust
// old
let link_id = endpoint.begin_outgoing_link_with_remote_pubkey(&mut core, identity, pubkey, profile, hops)?;
// new
let link_id = endpoint.begin_outgoing_link_with_remote_pubkey(&mut core, identity, pubkey, profile, hops).await?;
```

`begin_outgoing_link_with_remote_pubkey` no longer sends a path request, and
the new calls send none. When no path is known, call
`request_path_to_identity` first, or use `begin_outgoing_link`, which discovers
the peer and its path.

To stamp off the caller's task, call
`prepare_outgoing_link_request(&transport_tx, identity, hops).await?` and pass
its `OutgoingLinkRequest` to the synchronous
`TelephonyRnsEndpoint::begin_prepared_outgoing_link`.

`Error` gains `LinkStamp(rns_link::stamp::LinkStampError)`, so exhaustive
matches on `lxst_telephony::Error` need a new arm.

## Embedded sessions

`SessionConfig` gains `times: CallTimes`; add `times: CallTimes::DEFAULT` to
struct literals. The defaults are LXST 0.6.0's: an incoming call rings for
60 seconds (was 90), and an outgoing call ends unless the callee rings within
5 seconds and the call is established within 70 seconds of the session start.
The 10 second answer deadline and its three retries are gone.

## Behaviour an operator sees

- Outgoing calls end unless the callee rings within `connect_timeout`
  (5 seconds by default) of the call request. The 70 second
  `outgoing_call_timeout` also runs from the call request, path discovery
  included.
- The line is busy while an outgoing call discovers its path.
- The first announce follows 5 seconds after the service starts, and
  `startup_announce_retries` defaults to 0.
- The telephony destination announces through the stamped announce builder.
  Calls stamp and check link requests according to the Reticulum `[stamps]`
  settings, as upstream does.
- Call links signal the next hop's MTU when Reticulum's `link_mtu_discovery`
  is on (`RNS/Link.py:392-397`).
- Received audio is decoded by the codec of each frame. A frame that fails to
  decode is reported and skipped; the rest of its packet still plays.
- Raw frames at `RawBitDepth::Float128` carry IEEE binary128 samples, as
  numpy's `float128` does on Linux aarch64. rsLXST 0.3.1 wrote an `f64`
  padded to 16 bytes.
- The Medium voice profile still encodes wideband SILK at its 8 kbps ceiling.
- `opus-rs` is pinned at 0.1.42 (was 0.1.29). A peer packet that panics the
  Opus decoder is rejected, and the decoder is reset.
