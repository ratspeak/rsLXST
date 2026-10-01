# Embedded LXST boundary

`no_std`, allocation-free MessagePack framing, call policy, native Codec2 packet
adaptation and a three-frame arrival-order queue. It supplies no threads, audio
drivers, identities, transport, storage or global state.

Profile tables, wire identifiers, native packet aggregation and call transitions
are compiled directly from the shared sources in `lxst-core/src/shared`. The
host `TelephonyCall` API retains its allocating return values as a facade over
the same fixed action list. Preserve this shared source layout in source bundles;
these unpublished workspace crates are built from the complete repository.

`wire::Packet` borrows validated input with fixed ceilings (512 packet bytes,
eight signals, four frames, 256 bytes per codec frame). It rejects duplicates,
trailing data, excessive nesting and oversized declared containers. Unknown
metadata is skipped within those limits. `wire::encode` matches the full Rust
encoder's canonical MessagePack forms and validates before writing output.
The negotiated Reticulum Link MDU can be smaller than the parser ceiling; the
embedding transport must enforce its exact current MDU before sending.

`Session` wraps the shared transitions with verified-peer, local acceptance,
audio-readiness, profile and PTT gates. The embedding Link owner alone calls
`peer_verified`, after checking the exact identity and interface incarnation.
Opening or answering never starts capture. Only a fresh Talk press can start it;
release, timeout, reconfiguration and teardown stop it. Input edges and worker
completions must carry session/view generations; audio configuration completions
also carry `audio_generation`. Capability sets express previously qualified
local codec/route budgets, not peer capabilities or an extension to LXST.

Unsupported preferences receive one supported suggestion with a deadline;
repeated unsupported preferences close the session. Local PTT does not grant a
remote floor: while capturing, conflicting receive audio is discarded. The
queue uses local expiry times; it cannot infer a sender timestamp, recover packet
order, claim delivery or route speech through LXMF propagation.

The native `Codec2Backend` implementation borrows a codec initialized in final
storage. It never moves a large codec state onto the task stack. Only 1600/3200
are implemented; profile metadata for 700C is not a 700C backend. Opus must be
separately qualified before including it in a device's admitted profile set.
