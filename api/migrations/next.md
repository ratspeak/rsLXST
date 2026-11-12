# Migrating to the next rsLXST release (draft)

This draft covers source changes since the current compatibility floor (commit
`1c03765`, LXST 0.6.0 signalling). rsLXST now runs on the rsReticulum release
that follows Reticulum 1.6.0. The release number belongs to the owner.

## Telephony: outgoing call links

LXST 0.6.0 opens the call with a plain `RNS.Link(call_destination, ...)`
(`LXST/Primitives/Telephony.py:840-842`). On Reticulum 1.6.0 that link request
carries a stamp at the link cost the destination announced
(`RNS/Link.py:363`). Generating that stamp is CPU work, so building the link
request is now async:

| Old | New |
| --- | --- |
| `fn TelephonyRnsEndpoint::begin_outgoing_link_with_mode(..) -> Result<LinkId, Error>` | `async fn`; add `.await` |
| `fn TelephonyRnsEndpoint::begin_outgoing_link_with_remote_pubkey(..) -> Result<LinkId, Error>` | `async fn`; add `.await` |

```rust
// old
let link_id = endpoint.begin_outgoing_link_with_mode(&mut core, identity, pubkey, profile, mode, hops)?;
// new
let link_id = endpoint.begin_outgoing_link_with_mode(&mut core, identity, pubkey, profile, mode, hops).await?;
```

To stamp off the caller's task, call
`prepare_outgoing_link_request(&transport_tx, identity, hops).await?` and pass
its `OutgoingLinkRequest` to the synchronous
`TelephonyRnsEndpoint::begin_prepared_outgoing_link`.

`Error` gains `LinkStamp(rns_link::stamp::LinkStampError)`, so exhaustive
matches on `lxst_telephony::Error` need a new arm.

## Behaviour an operator sees

- The telephony destination announces through the stamped announce builder.
  Calls stamp and check link requests according to the Reticulum `[stamps]`
  settings, as upstream does.
- Call links signal the next hop's MTU when Reticulum's `link_mtu_discovery`
  is on (`RNS/Link.py:392-397`).
- The Medium voice profile still encodes wideband SILK at its 8 kbps ceiling.
- `opus-rs` is pinned at 0.1.42 (was 0.1.29).

