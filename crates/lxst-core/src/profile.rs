use crate::wire::{Codec2Mode, CodecKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum SignallingStatus {
    Busy = 0x00,
    Rejected = 0x01,
    Calling = 0x02,
    Available = 0x03,
    Ringing = 0x04,
    Connecting = 0x05,
    Established = 0x06,
}

impl SignallingStatus {
    pub const AUTO_STATUS_CODES: [Self; 5] = [
        Self::Calling,
        Self::Available,
        Self::Ringing,
        Self::Connecting,
        Self::Established,
    ];

    pub const fn wire_value(self) -> u32 {
        self as u32
    }

    pub const fn from_wire(value: u32) -> Option<Self> {
        match value {
            0x00 => Some(Self::Busy),
            0x01 => Some(Self::Rejected),
            0x02 => Some(Self::Calling),
            0x03 => Some(Self::Available),
            0x04 => Some(Self::Ringing),
            0x05 => Some(Self::Connecting),
            0x06 => Some(Self::Established),
            _ => None,
        }
    }

    pub fn is_auto_status(self) -> bool {
        Self::AUTO_STATUS_CODES.contains(&self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Profile {
    BandwidthUltraLow = 0x10,
    BandwidthVeryLow = 0x20,
    BandwidthLow = 0x30,
    QualityMedium = 0x40,
    QualityHigh = 0x50,
    QualityMax = 0x60,
    LatencyUltraLow = 0x70,
    LatencyLow = 0x80,
}

impl Profile {
    pub const DEFAULT: Self = Self::QualityMedium;

    pub const ORDER: [Self; 8] = [
        Self::BandwidthUltraLow,
        Self::BandwidthVeryLow,
        Self::BandwidthLow,
        Self::QualityMedium,
        Self::QualityHigh,
        Self::QualityMax,
        Self::LatencyLow,
        Self::LatencyUltraLow,
    ];

    pub const fn wire_value(self) -> u32 {
        self as u32
    }

    pub const fn from_wire(value: u32) -> Option<Self> {
        match value {
            0x10 => Some(Self::BandwidthUltraLow),
            0x20 => Some(Self::BandwidthVeryLow),
            0x30 => Some(Self::BandwidthLow),
            0x40 => Some(Self::QualityMedium),
            0x50 => Some(Self::QualityHigh),
            0x60 => Some(Self::QualityMax),
            0x70 => Some(Self::LatencyUltraLow),
            0x80 => Some(Self::LatencyLow),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::BandwidthUltraLow => "Ultra Low Bandwidth",
            Self::BandwidthVeryLow => "Very Low Bandwidth",
            Self::BandwidthLow => "Low Bandwidth",
            Self::QualityMedium => "Medium Quality",
            Self::QualityHigh => "High Quality",
            Self::QualityMax => "Super High Quality",
            Self::LatencyLow => "Low Latency",
            Self::LatencyUltraLow => "Ultra Low Latency",
        }
    }

    pub const fn abbreviation(self) -> &'static str {
        match self {
            Self::BandwidthUltraLow => "ULBW",
            Self::BandwidthVeryLow => "VLBW",
            Self::BandwidthLow => "LBW",
            Self::QualityMedium => "MQ",
            Self::QualityHigh => "HQ",
            Self::QualityMax => "SHQ",
            Self::LatencyLow => "LL",
            Self::LatencyUltraLow => "ULL",
        }
    }

    pub const fn frame_time_ms(self) -> u16 {
        match self {
            Self::BandwidthUltraLow => 400,
            Self::BandwidthVeryLow => 320,
            Self::BandwidthLow => 200,
            Self::QualityMedium => 60,
            Self::QualityHigh => 60,
            Self::QualityMax => 60,
            Self::LatencyLow => 20,
            Self::LatencyUltraLow => 10,
        }
    }

    pub const fn audio_codec(self) -> AudioCodec {
        match self {
            Self::BandwidthUltraLow => AudioCodec::Codec2(Codec2Mode::Mode700C),
            Self::BandwidthVeryLow => AudioCodec::Codec2(Codec2Mode::Mode1600),
            Self::BandwidthLow => AudioCodec::Codec2(Codec2Mode::Mode3200),
            Self::QualityMedium => AudioCodec::Opus(OpusProfile::VoiceMedium),
            Self::QualityHigh => AudioCodec::Opus(OpusProfile::VoiceHigh),
            Self::QualityMax => AudioCodec::Opus(OpusProfile::VoiceMax),
            Self::LatencyLow => AudioCodec::Opus(OpusProfile::VoiceMedium),
            Self::LatencyUltraLow => AudioCodec::Opus(OpusProfile::VoiceMedium),
        }
    }

    pub const fn channels(self) -> u8 {
        self.audio_codec().channels()
    }

    pub const fn sample_rate_hz(self) -> u32 {
        self.audio_codec().sample_rate_hz()
    }

    pub const fn sample_frames_per_packet(self) -> usize {
        ((self.sample_rate_hz() as usize) * (self.frame_time_ms() as usize)) / 1000
    }

    pub const fn opus_payload_ceiling_bytes(self) -> Option<usize> {
        match self.audio_codec() {
            AudioCodec::Opus(profile) => Some(profile.max_bytes_per_frame_ms(self.frame_time_ms())),
            AudioCodec::Codec2(_) => None,
        }
    }

    pub fn next(self) -> Self {
        let index = Self::ORDER
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        Self::ORDER[(index + 1) % Self::ORDER.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    Opus(OpusProfile),
    Codec2(Codec2Mode),
}

impl AudioCodec {
    pub const fn codec_kind(self) -> CodecKind {
        match self {
            Self::Opus(_) => CodecKind::Opus,
            Self::Codec2(_) => CodecKind::Codec2,
        }
    }

    pub const fn channels(self) -> u8 {
        match self {
            Self::Opus(profile) => profile.channels(),
            Self::Codec2(_) => 1,
        }
    }

    pub const fn sample_rate_hz(self) -> u32 {
        match self {
            Self::Opus(profile) => profile.sample_rate(),
            Self::Codec2(_) => 8_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum OpusProfile {
    VoiceLow = 0x00,
    VoiceMedium = 0x01,
    VoiceHigh = 0x02,
    VoiceMax = 0x03,
    AudioMin = 0x04,
    AudioLow = 0x05,
    AudioMedium = 0x06,
    AudioHigh = 0x07,
    AudioMax = 0x08,
}

impl OpusProfile {
    pub const VALID_FRAME_MS: [f32; 6] = [2.5, 5.0, 10.0, 20.0, 40.0, 60.0];
    pub const FRAME_QUANTA_MS: f32 = 2.5;
    pub const FRAME_MAX_MS: f32 = 60.0;

    pub const fn channels(self) -> u8 {
        match self {
            Self::VoiceLow | Self::VoiceMedium | Self::VoiceHigh => 1,
            Self::VoiceMax => 2,
            Self::AudioMin | Self::AudioLow => 1,
            Self::AudioMedium | Self::AudioHigh | Self::AudioMax => 2,
        }
    }

    pub const fn sample_rate(self) -> u32 {
        match self {
            Self::VoiceLow => 8_000,
            Self::VoiceMedium => 24_000,
            Self::VoiceHigh | Self::VoiceMax => 48_000,
            Self::AudioMin => 8_000,
            Self::AudioLow => 12_000,
            Self::AudioMedium => 24_000,
            Self::AudioHigh | Self::AudioMax => 48_000,
        }
    }

    pub const fn application(self) -> OpusApplication {
        match self {
            Self::VoiceLow | Self::VoiceMedium | Self::VoiceHigh | Self::VoiceMax => {
                OpusApplication::Voip
            }
            Self::AudioMin
            | Self::AudioLow
            | Self::AudioMedium
            | Self::AudioHigh
            | Self::AudioMax => OpusApplication::Audio,
        }
    }

    pub const fn bitrate_ceiling(self) -> u32 {
        match self {
            Self::VoiceLow => 6_000,
            Self::VoiceMedium => 8_000,
            Self::VoiceHigh => 16_000,
            Self::VoiceMax => 32_000,
            Self::AudioMin => 8_000,
            Self::AudioLow => 14_000,
            Self::AudioMedium => 28_000,
            Self::AudioHigh => 56_000,
            Self::AudioMax => 128_000,
        }
    }

    pub const fn max_bytes_per_frame_ms(self, frame_duration_ms: u16) -> usize {
        let numerator = (self.bitrate_ceiling() as usize) * (frame_duration_ms as usize);
        numerator.div_ceil(8_000)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpusApplication {
    Voip,
    Audio,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_order_matches_python_next_profile_order() {
        assert_eq!(Profile::QualityMax.next(), Profile::LatencyLow);
        assert_eq!(Profile::LatencyLow.next(), Profile::LatencyUltraLow);
        assert_eq!(Profile::LatencyUltraLow.next(), Profile::BandwidthUltraLow);
    }

    #[test]
    fn telephony_profile_mapping_matches_python_reference() {
        assert_eq!(
            Profile::BandwidthUltraLow.audio_codec(),
            AudioCodec::Codec2(Codec2Mode::Mode700C)
        );
        assert_eq!(
            Profile::BandwidthVeryLow.audio_codec(),
            AudioCodec::Codec2(Codec2Mode::Mode1600)
        );
        assert_eq!(
            Profile::BandwidthLow.audio_codec(),
            AudioCodec::Codec2(Codec2Mode::Mode3200)
        );
        assert_eq!(
            Profile::LatencyUltraLow.audio_codec(),
            AudioCodec::Opus(OpusProfile::VoiceMedium)
        );
        assert_eq!(Profile::LatencyUltraLow.frame_time_ms(), 10);
        assert_eq!(Profile::LatencyLow.frame_time_ms(), 20);
    }

    #[test]
    fn telephony_profile_audio_budgets_match_python_tables() {
        assert_eq!(Profile::BandwidthUltraLow.channels(), 1);
        assert_eq!(Profile::BandwidthUltraLow.sample_rate_hz(), 8_000);
        assert_eq!(Profile::BandwidthUltraLow.sample_frames_per_packet(), 3_200);
        assert_eq!(
            Profile::BandwidthUltraLow.opus_payload_ceiling_bytes(),
            None
        );

        assert_eq!(Profile::QualityMedium.channels(), 1);
        assert_eq!(Profile::QualityMedium.sample_rate_hz(), 24_000);
        assert_eq!(Profile::QualityMedium.sample_frames_per_packet(), 1_440);
        assert_eq!(
            Profile::QualityMedium.opus_payload_ceiling_bytes(),
            Some(60)
        );

        assert_eq!(Profile::QualityHigh.channels(), 1);
        assert_eq!(Profile::QualityHigh.sample_rate_hz(), 48_000);
        assert_eq!(Profile::QualityHigh.sample_frames_per_packet(), 2_880);
        assert_eq!(Profile::QualityHigh.opus_payload_ceiling_bytes(), Some(120));

        assert_eq!(Profile::QualityMax.channels(), 2);
        assert_eq!(Profile::QualityMax.sample_rate_hz(), 48_000);
        assert_eq!(Profile::QualityMax.sample_frames_per_packet(), 2_880);
        assert_eq!(Profile::QualityMax.opus_payload_ceiling_bytes(), Some(240));

        assert_eq!(Profile::LatencyLow.sample_frames_per_packet(), 480);
        assert_eq!(Profile::LatencyLow.opus_payload_ceiling_bytes(), Some(20));
        assert_eq!(Profile::LatencyUltraLow.sample_frames_per_packet(), 240);
        assert_eq!(
            Profile::LatencyUltraLow.opus_payload_ceiling_bytes(),
            Some(10)
        );
    }

    #[test]
    fn opus_max_bytes_per_frame_matches_python_formula() {
        assert_eq!(OpusProfile::VoiceMedium.max_bytes_per_frame_ms(60), 60);
        assert_eq!(OpusProfile::VoiceHigh.max_bytes_per_frame_ms(60), 120);
        assert_eq!(OpusProfile::VoiceMax.max_bytes_per_frame_ms(60), 240);
        assert_eq!(OpusProfile::AudioMax.max_bytes_per_frame_ms(60), 960);
    }
}
