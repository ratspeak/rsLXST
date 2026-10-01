# Bounded Rust Codec2

Allocation-free `no_std` native 1600/3200 bit/s speech at mono 8 kHz PCM16.
This is the pinned Rust Codec2 port with fixed storage, per-instance synthesis
randomness and pure-Rust `libm` math. It has no allocator dependency.

Allocate `MaybeUninit<Codec>` in final aligned storage, then call
`Codec::initialise`. Do not construct a codec temporary on a small task stack.
One serialized owner can alternate encoding and decoding; predictor histories
are independent. Independent sessions require separate states. `reset` erases
histories in place. Native input/output frame lengths must match `Mode` exactly;
length errors do not advance state or modify output.

LXST framing and negotiation belong to `lxst-core`, not this backend. 700C,
Opus, memo containers, audio drivers and real-time target qualification are not
provided here. Codec state plus worst-case nested DSP stack and I/O buffers must
fit the selected device budget. A target build is not a timing/stack measurement.

`cargo test -p lxst-codec2 --release` checks allocation-free initialization,
reset and streaming; rejected inputs; independent duplex histories; hostile
correctly-sized decoder frames; and the real-FFT second-half input regression.

See [NOTICE](NOTICE), [UPSTREAM.json](UPSTREAM.json) and [licenses](licenses/)
for source provenance, modifications and original license notices.
