use thiserror::Error;

use crate::{Error as WireError, RawAudioFrame, RawFrameHeader};

#[derive(Debug, Error, PartialEq)]
pub enum SyntheticError {
    #[error("synthetic source sample rate must be greater than zero")]
    InvalidSampleRate,
    #[error("synthetic source frame sample count must be greater than zero")]
    InvalidFrameSamples,
    #[error("synthetic source parameter must be finite")]
    NonFiniteParameter,
    #[error("raw frame error: {0}")]
    Raw(#[from] WireError),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyntheticSourceKind {
    Silence,
    Ramp { start: f32, step: f32 },
    Sine { frequency_hz: f32, amplitude: f32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyntheticSource {
    channels: u8,
    sample_rate_hz: u32,
    frame_samples: usize,
    kind: SyntheticSourceKind,
    cursor_samples: u64,
}

impl SyntheticSource {
    pub fn new(
        channels: u8,
        sample_rate_hz: u32,
        frame_samples: usize,
        kind: SyntheticSourceKind,
    ) -> Result<Self, SyntheticError> {
        RawFrameHeader::new(channels, crate::RawBitDepth::Float16)?;
        if sample_rate_hz == 0 {
            return Err(SyntheticError::InvalidSampleRate);
        }
        if frame_samples == 0 {
            return Err(SyntheticError::InvalidFrameSamples);
        }
        validate_kind(kind)?;

        Ok(Self {
            channels,
            sample_rate_hz,
            frame_samples,
            kind,
            cursor_samples: 0,
        })
    }

    pub const fn channels(&self) -> u8 {
        self.channels
    }

    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    pub const fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    pub const fn cursor_samples(&self) -> u64 {
        self.cursor_samples
    }

    pub fn next_raw_frame(&mut self) -> Result<RawAudioFrame, SyntheticError> {
        let mut samples = Vec::with_capacity(self.frame_samples * usize::from(self.channels));

        for frame_index in 0..self.frame_samples {
            let absolute_sample = self.cursor_samples + frame_index as u64;
            let base = self.sample_at(absolute_sample);
            for channel in 0..self.channels {
                samples.push(channel_sample(base, channel));
            }
        }

        self.cursor_samples += self.frame_samples as u64;
        Ok(RawAudioFrame::new(self.channels, samples)?)
    }

    fn sample_at(&self, absolute_sample: u64) -> f32 {
        match self.kind {
            SyntheticSourceKind::Silence => 0.0,
            SyntheticSourceKind::Ramp { start, step } => start + step * absolute_sample as f32,
            SyntheticSourceKind::Sine {
                frequency_hz,
                amplitude,
            } => {
                let t = absolute_sample as f32 / self.sample_rate_hz as f32;
                amplitude * (std::f32::consts::TAU * frequency_hz * t).sin()
            }
        }
    }
}

fn channel_sample(base: f32, channel: u8) -> f32 {
    if channel == 0 {
        base
    } else {
        // Deterministic but small channel separation for fixture validation.
        base + f32::from(channel) * 0.001
    }
}

fn validate_kind(kind: SyntheticSourceKind) -> Result<(), SyntheticError> {
    match kind {
        SyntheticSourceKind::Silence => Ok(()),
        SyntheticSourceKind::Ramp { start, step } => {
            if start.is_finite() && step.is_finite() {
                Ok(())
            } else {
                Err(SyntheticError::NonFiniteParameter)
            }
        }
        SyntheticSourceKind::Sine {
            frequency_hz,
            amplitude,
        } => {
            if frequency_hz.is_finite() && amplitude.is_finite() {
                Ok(())
            } else {
                Err(SyntheticError::NonFiniteParameter)
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawFrameCollector {
    frames: Vec<RawAudioFrame>,
    sample_frames: usize,
}

impl RawFrameCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, frame: RawAudioFrame) {
        self.sample_frames += frame.sample_frames();
        self.frames.push(frame);
    }

    pub fn frames(&self) -> &[RawAudioFrame] {
        &self.frames
    }

    pub const fn sample_frames(&self) -> usize {
        self.sample_frames
    }

    pub fn into_frames(self) -> Vec<RawAudioFrame> {
        self.frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_source_generates_deterministic_interleaved_channels() {
        let mut source = SyntheticSource::new(
            2,
            48_000,
            3,
            SyntheticSourceKind::Ramp {
                start: 0.0,
                step: 0.5,
            },
        )
        .unwrap();

        let first = source.next_raw_frame().unwrap();
        let second = source.next_raw_frame().unwrap();

        assert_eq!(
            first,
            RawAudioFrame::new(2, vec![0.0, 0.001, 0.5, 0.501, 1.0, 1.001]).unwrap()
        );
        assert_eq!(
            second,
            RawAudioFrame::new(2, vec![1.5, 1.501, 2.0, 2.001, 2.5, 2.501]).unwrap()
        );
        assert_eq!(source.cursor_samples(), 6);
    }

    #[test]
    fn sine_source_generates_expected_quarter_wave_samples() {
        let mut source = SyntheticSource::new(
            1,
            4,
            5,
            SyntheticSourceKind::Sine {
                frequency_hz: 1.0,
                amplitude: 1.0,
            },
        )
        .unwrap();

        let frame = source.next_raw_frame().unwrap();
        let expected = [0.0, 1.0, 0.0, -1.0, 0.0];
        for (sample, expected) in frame.samples.iter().zip(expected) {
            assert!((sample - expected).abs() < 0.000_001);
        }
    }

    #[test]
    fn collector_tracks_frames_and_sample_count() {
        let mut collector = RawFrameCollector::new();
        collector.push(RawAudioFrame::new(1, vec![0.0, 1.0]).unwrap());
        collector.push(RawAudioFrame::new(2, vec![0.0, 0.1, 0.2, 0.3]).unwrap());

        assert_eq!(collector.frames().len(), 2);
        assert_eq!(collector.sample_frames(), 4);
    }

    #[test]
    fn invalid_source_parameters_fail_explicitly() {
        assert_eq!(
            SyntheticSource::new(1, 0, 1, SyntheticSourceKind::Silence),
            Err(SyntheticError::InvalidSampleRate)
        );
        assert_eq!(
            SyntheticSource::new(1, 48_000, 0, SyntheticSourceKind::Silence),
            Err(SyntheticError::InvalidFrameSamples)
        );
        assert_eq!(
            SyntheticSource::new(
                1,
                48_000,
                1,
                SyntheticSourceKind::Ramp {
                    start: f32::NAN,
                    step: 1.0,
                },
            ),
            Err(SyntheticError::NonFiniteParameter)
        );
    }
}
