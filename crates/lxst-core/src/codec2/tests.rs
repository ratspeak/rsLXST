use super::*;
use std::cell::Cell;
use std::rc::Rc;

struct Backend {
    mode: Codec2Mode,
    calls: Rc<Cell<usize>>,
    fail_at: Option<usize>,
}

impl Codec2Backend for Backend {
    type Error = &'static str;

    fn mode(&self) -> Codec2Mode {
        self.mode
    }

    fn encode_native(&mut self, pcm: &[i16], encoded: &mut [u8]) -> Result<(), Self::Error> {
        let count = self.calls.get() + 1;
        self.calls.set(count);
        assert_eq!(
            pcm.len(),
            if self.mode == Codec2Mode::Mode3200 {
                160
            } else {
                320
            }
        );
        assert_eq!(
            encoded.len(),
            if self.mode == Codec2Mode::Mode700C {
                4
            } else {
                8
            }
        );
        if self.fail_at == Some(count) {
            return Err("injected codec failure");
        }
        encoded.fill(count as u8);
        Ok(())
    }

    fn decode_native(&mut self, encoded: &[u8], pcm: &mut [i16]) -> Result<(), Self::Error> {
        let mut scratch = [0; 8];
        self.encode_native(pcm, &mut scratch[..encoded.len()])?;
        pcm.fill(i16::from(encoded[0]));
        Ok(())
    }
}

fn codec(
    profile: Profile,
    fail_at: Option<usize>,
) -> (Codec2PacketCodec<Backend>, Rc<Cell<usize>>) {
    let AudioCodec::Codec2(mode) = profile.audio_codec() else {
        panic!("test profile")
    };
    let calls = Rc::new(Cell::new(0));
    let backend = Backend {
        mode,
        calls: calls.clone(),
        fail_at,
    };
    (Codec2PacketCodec::new(profile, backend).unwrap(), calls)
}

#[test]
fn profile_packets_preserve_native_frame_padding_and_output_tails() {
    for (profile, samples, bytes, count) in [
        (Profile::BandwidthUltraLow, 3200, 41, 10),
        (Profile::BandwidthVeryLow, 2560, 65, 8),
        (Profile::BandwidthLow, 1600, 81, 10),
    ] {
        let (mut encoder, calls) = codec(profile, None);
        assert_eq!(encoder.profile(), profile);
        assert_eq!(encoder.sample_frames(), samples);
        assert_eq!(encoder.payload_bytes(), bytes);
        let pcm = vec![100; samples];
        let mut payload = [0xED; 96];
        assert_eq!(encoder.encode_into(&pcm, &mut payload).unwrap(), bytes);
        assert_eq!(calls.get(), count);
        assert_eq!(payload[0], encoder.mode().header());
        assert!(payload[bytes..].iter().all(|x| *x == 0xED));
        for (i, frame) in payload[1..bytes]
            .chunks_exact((bytes - 1) / count)
            .enumerate()
        {
            assert!(frame.iter().all(|x| usize::from(*x) == i + 1));
        }
        let (mut decoder, calls) = codec(profile, None);
        let mut decoded = vec![-123; samples + 8];
        assert_eq!(
            decoder
                .decode_into(&payload[..bytes], &mut decoded)
                .unwrap(),
            samples
        );
        assert_eq!(calls.get(), count);
        assert!(decoded[samples..].iter().all(|x| *x == -123));
        for (i, frame) in decoded[..samples].chunks_exact(samples / count).enumerate() {
            assert!(frame.iter().all(|x| *x == (i + 1) as i16));
        }
    }
}

#[test]
fn reject_non_codec2_profiles_and_backend_mode_mismatch() {
    for profile in Profile::ORDER {
        let backend = Backend {
            mode: Codec2Mode::Mode1600,
            calls: Rc::new(Cell::new(0)),
            fail_at: None,
        };
        let result = Codec2PacketCodec::new(profile, backend);
        match profile.audio_codec() {
            AudioCodec::Opus(_) => assert!(
                matches!(result, Err(Codec2PacketError::NonCodec2Profile(p)) if p == profile)
            ),
            AudioCodec::Codec2(Codec2Mode::Mode1600) => assert!(result.is_ok()),
            AudioCodec::Codec2(_) => assert!(matches!(
                result,
                Err(Codec2PacketError::BackendModeMismatch { .. })
            )),
        }
    }
}

#[test]
fn all_malformed_lengths_headers_and_capacities_fail_before_backend_or_output_mutation() {
    for profile in [
        Profile::BandwidthUltraLow,
        Profile::BandwidthVeryLow,
        Profile::BandwidthLow,
    ] {
        let (mut codec, calls) = codec(profile, None);
        let mut payload = vec![0; codec.payload_bytes() + 1];
        payload[0] = codec.mode().header();
        let mut output = vec![-123; codec.sample_frames()];
        for length in 0..=payload.len() {
            if length == codec.payload_bytes() {
                continue;
            }
            assert!(matches!(
                codec.decode_into(&payload[..length], &mut output),
                Err(Codec2PacketError::PayloadLength { .. })
            ));
        }
        payload.pop();
        for header in 0..=255 {
            if header == codec.mode().header() {
                continue;
            }
            payload[0] = header;
            assert!(matches!(
                codec.decode_into(&payload, &mut output),
                Err(Codec2PacketError::ModeHeader { .. })
            ));
        }
        payload[0] = codec.mode().header();
        for capacity in [0, 1, codec.sample_frames() - 1] {
            assert!(matches!(
                codec.decode_into(&payload, &mut output[..capacity]),
                Err(Codec2PacketError::OutputCapacity { .. })
            ));
        }
        assert!(output.iter().all(|x| *x == -123));
        payload.fill(0xEA);
        for samples in [0, 1, codec.sample_frames() - 1, codec.sample_frames() + 1] {
            assert!(matches!(
                codec.encode_into(&vec![0; samples], &mut payload),
                Err(Codec2PacketError::SampleCount { .. })
            ));
        }
        for capacity in [0, 1, codec.payload_bytes() - 1] {
            assert!(matches!(
                codec.encode_into(&output, &mut payload[..capacity]),
                Err(Codec2PacketError::OutputCapacity { .. })
            ));
        }
        assert!(payload.iter().all(|x| *x == 0xEA));
        assert_eq!(calls.get(), 0);
        // A rejected input must not poison a valid stream.
        assert!(codec.encode_into(&output, &mut payload).is_ok());
    }
}

#[test]
fn backend_failure_latches_both_directions_until_replacement() {
    for encode_first in [true, false] {
        let (mut codec, calls) = codec(Profile::BandwidthVeryLow, Some(3));
        let mut payload = [0; 65];
        payload[0] = Codec2Mode::Mode1600.header();
        let mut pcm = [0; 2560];
        let result = if encode_first {
            codec.encode_into(&pcm, &mut payload)
        } else {
            codec.decode_into(&payload, &mut pcm)
        };
        assert_eq!(
            result,
            Err(Codec2PacketError::Backend("injected codec failure"))
        );
        assert_eq!(calls.get(), 3);
        assert_eq!(
            codec.encode_into(&pcm, &mut payload),
            Err(Codec2PacketError::Failed)
        );
        assert_eq!(
            codec.decode_into(&payload, &mut pcm),
            Err(Codec2PacketError::Failed)
        );
        assert_eq!(calls.get(), 3);
    }
}
