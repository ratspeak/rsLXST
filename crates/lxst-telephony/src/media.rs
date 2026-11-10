//! Host PCM adapter. The same bounded native Codec2 backend and packet framing
//! as handheld firmware; original Opus controls/events remain compatible.
use crate::Error;
use lxst_codec2::{Codec, Mode};
use lxst_core::{
    AudioCodec, Codec2Backend, Codec2Mode, Codec2PacketCodec, CodecKind, Frame, OpusEncoderState,
    OpusPacketDecoder, Profile, RawAudioFrame,
};

struct Native(Box<Codec>);
impl Codec2Backend for Native {
    type Error = &'static str;
    fn mode(&self) -> Codec2Mode {
        match self.0.mode() {
            Mode::Rate1600 => Codec2Mode::Mode1600,
            Mode::Rate3200 => Codec2Mode::Mode3200,
            Mode::Rate700C => Codec2Mode::Mode700C,
        }
    }
    fn encode_native(&mut self, pcm: &[i16], bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.0
            .encode(pcm, bytes)
            .map_err(|_| "invalid native frame")
    }
    fn decode_native(&mut self, bytes: &[u8], pcm: &mut [i16]) -> Result<(), Self::Error> {
        self.0
            .decode(bytes, pcm)
            .map_err(|_| "invalid native frame")
    }
}
pub(crate) struct Codec2Audio {
    codec: Codec2PacketCodec<Native>,
    pcm: Vec<i16>,
}
impl Codec2Audio {
    fn new(profile: Profile) -> Result<Self, Error> {
        let mode = match profile {
            Profile::BandwidthUltraLow => Mode::Rate700C,
            Profile::BandwidthVeryLow => Mode::Rate1600,
            Profile::BandwidthLow => Mode::Rate3200,
            _ => return Err(Error::Audio("unsupported native Codec2 profile".into())),
        };
        let mut storage = Box::<Codec>::new_uninit();
        Codec::initialise(&mut storage, mode);
        // SAFETY: initialise completed in final storage above.
        let backend = Native(unsafe { storage.assume_init() });
        let codec =
            Codec2PacketCodec::new(profile, backend).map_err(|e| Error::Audio(e.to_string()))?;
        Ok(Self {
            pcm: vec![0; codec.sample_frames()],
            codec,
        })
    }
    fn encode(&mut self, frame: &RawAudioFrame) -> Result<Frame, Error> {
        if frame.channels != 1
            || frame.samples.len() != self.pcm.len()
            || frame.samples.iter().any(|v| !v.is_finite())
        {
            return Err(Error::Audio(
                "invalid Codec2 PCM shape or non-finite sample".into(),
            ));
        }
        for (to, from) in self.pcm.iter_mut().zip(&frame.samples) {
            *to = (from.clamp(-1.0, 1.0) * 32768.0)
                .round()
                .clamp(-32768.0, 32767.0) as i16;
        }
        let mut payload = vec![0; self.codec.payload_bytes()];
        let result = self
            .codec
            .encode_into(&self.pcm, &mut payload)
            .map_err(|e| Error::Audio(e.to_string()));
        self.pcm.fill(0);
        result?;
        Ok(Frame::new(CodecKind::Codec2, payload))
    }
}
pub(crate) enum AudioEncoder {
    Opus(Box<OpusEncoderState>),
    Codec2(Codec2Audio),
}
impl AudioEncoder {
    pub(crate) fn new(profile: Profile) -> Result<Self, Error> {
        match profile.audio_codec() {
            AudioCodec::Opus(_) => Ok(Self::Opus(Box::new(OpusEncoderState::new(profile)?))),
            AudioCodec::Codec2(_) => Ok(Self::Codec2(Codec2Audio::new(profile)?)),
        }
    }
    pub(crate) fn encode_frame(&mut self, frame: &RawAudioFrame) -> Result<Frame, Error> {
        match self {
            Self::Opus(codec) => Ok(codec.encode_frame(frame)?),
            Self::Codec2(codec) => codec.encode(frame),
        }
    }
}
const CODEC2_SAMPLE_RATE_HZ: u32 = 8_000;

/// Follows the codec and Codec2 mode of each received frame, as Python `LinkSource`
/// does, and converts the output to the local profile's sample rate and channels.
pub(crate) struct ReceiveDecoder {
    sample_rate_hz: u32,
    channels: u8,
    opus: Option<OpusPacketDecoder>,
    codec2: Vec<(Codec2Mode, Box<Codec>)>,
}

impl ReceiveDecoder {
    pub(crate) fn new(profile: Profile) -> Self {
        Self {
            sample_rate_hz: profile.sample_rate_hz(),
            channels: profile.channels(),
            opus: None,
            codec2: Vec::new(),
        }
    }

    pub(crate) fn decode_frame(&mut self, frame: &Frame) -> Result<RawAudioFrame, Error> {
        match frame.codec {
            CodecKind::Opus => {
                if self.opus.is_none() {
                    self.opus = Some(OpusPacketDecoder::new(self.sample_rate_hz, self.channels)?);
                }
                let decoder = self
                    .opus
                    .as_mut()
                    .expect("Opus decoder exists after creation");
                Ok(decoder.decode_packet(&frame.payload)?)
            }
            CodecKind::Codec2 => self.decode_codec2(&frame.payload),
            other => Err(Error::Audio(format!("unexpected media codec {other:?}"))),
        }
    }

    fn decode_codec2(&mut self, payload: &[u8]) -> Result<RawAudioFrame, Error> {
        let (&header, bytes) = payload
            .split_first()
            .ok_or_else(|| Error::Audio("empty Codec2 frame".into()))?;
        let wire_mode =
            Codec2Mode::from_header(header).map_err(|error| Error::Audio(error.to_string()))?;
        let mode = match wire_mode {
            Codec2Mode::Mode700C => Mode::Rate700C,
            Codec2Mode::Mode1600 => Mode::Rate1600,
            Codec2Mode::Mode3200 => Mode::Rate3200,
            other => {
                return Err(Error::Audio(format!(
                    "unsupported native Codec2 mode {other:?}"
                )));
            }
        };
        let index = match self.codec2.iter().position(|(m, _)| *m == wire_mode) {
            Some(index) => index,
            None => {
                let mut storage = Box::<Codec>::new_uninit();
                Codec::initialise(&mut storage, mode);
                // SAFETY: initialise completed in final storage above.
                self.codec2
                    .push((wire_mode, unsafe { storage.assume_init() }));
                self.codec2.len() - 1
            }
        };
        let codec = &mut self.codec2[index].1;
        // Python decodes every whole native frame in the payload.
        let mut pcm = vec![0i16; mode.samples()];
        let mut samples = Vec::with_capacity(bytes.len() / mode.bytes() * mode.samples());
        for chunk in bytes.chunks_exact(mode.bytes()) {
            codec
                .decode(chunk, &mut pcm)
                .map_err(|_| Error::Audio("invalid native frame".into()))?;
            samples.extend(pcm.iter().map(|v| f32::from(*v) / 32768.0));
        }
        let mono = RawAudioFrame::new(1, samples).map_err(|e| Error::Audio(e.to_string()))?;
        mono.resampled(CODEC2_SAMPLE_RATE_HZ, self.sample_rate_hz)
            .with_channels(self.channels)
            .map_err(|e| Error::Audio(e.to_string()))
    }
}
