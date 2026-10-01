//! Host PCM adapter. The same bounded native Codec2 backend and packet framing
//! as handheld firmware; original Opus controls/events remain compatible.
use crate::Error;
use lxst_codec2::{Codec, Mode};
use lxst_core::{
    AudioCodec, Codec2Backend, Codec2Mode, Codec2PacketCodec, CodecKind, Frame, OpusDecoderState,
    OpusEncoderState, Profile, RawAudioFrame,
};

struct Native(Box<Codec>);
impl Codec2Backend for Native {
    type Error = &'static str;
    fn mode(&self) -> Codec2Mode {
        match self.0.mode() {
            Mode::Rate1600 => Codec2Mode::Mode1600,
            Mode::Rate3200 => Codec2Mode::Mode3200,
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
    fn decode(&mut self, frame: &Frame) -> Result<RawAudioFrame, Error> {
        if frame.codec != CodecKind::Codec2 {
            return Err(Error::Audio("unexpected media codec".into()));
        }
        self.codec
            .decode_into(&frame.payload, &mut self.pcm)
            .map_err(|e| Error::Audio(e.to_string()))?;
        let samples = self
            .pcm
            .iter()
            .map(|v| f32::from(*v) / 32768.0)
            .collect::<Vec<_>>();
        self.pcm.fill(0);
        Ok(RawAudioFrame {
            channels: 1,
            samples,
        })
    }
}
pub(crate) enum AudioEncoder {
    Opus(Box<OpusEncoderState>),
    Codec2(Codec2Audio),
}
pub(crate) enum AudioDecoder {
    Opus(Box<OpusDecoderState>),
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
impl AudioDecoder {
    pub(crate) fn new(profile: Profile) -> Result<Self, Error> {
        match profile.audio_codec() {
            AudioCodec::Opus(_) => Ok(Self::Opus(Box::new(OpusDecoderState::new(profile)?))),
            AudioCodec::Codec2(_) => Ok(Self::Codec2(Codec2Audio::new(profile)?)),
        }
    }
    pub(crate) fn decode_frame(&mut self, frame: &Frame) -> Result<RawAudioFrame, Error> {
        match self {
            Self::Opus(codec) => Ok(codec.decode_frame(frame)?),
            Self::Codec2(codec) => codec.decode(frame),
        }
    }
}
