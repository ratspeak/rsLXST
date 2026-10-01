// Shared directly with lxst-embedded. The enclosing wire module supplies its
// own error type; wire identifiers and conversions have one implementation.
use crate::profile::{Profile, SignallingStatus};
use crate::wire::Error;
const PREFERRED_PROFILE_BASE: u32 = 0xFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CodecKind {
    Raw = 0x00,
    Opus = 0x01,
    Codec2 = 0x02,
    Null = 0xFF,
}

impl CodecKind {
    pub const fn wire_id(self) -> u8 {
        self as u8
    }

    pub const fn from_wire(id: u8) -> Result<Self, Error> {
        match id {
            0x00 => Ok(Self::Raw),
            0x01 => Ok(Self::Opus),
            0x02 => Ok(Self::Codec2),
            0xFF => Ok(Self::Null),
            other => Err(Error::UnknownCodec(other)),
        }
    }

    pub const fn is_transmittable(self) -> bool {
        !matches!(self, Self::Null)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    Status(SignallingStatus),
    PreferredProfile(Profile),
    Raw(u32),
}

impl Signal {
    pub const fn wire_value(self) -> u32 {
        match self {
            Self::Status(status) => status.wire_value(),
            Self::PreferredProfile(profile) => PREFERRED_PROFILE_BASE + profile.wire_value(),
            Self::Raw(value) => value,
        }
    }

    pub const fn from_wire(value: u32) -> Self {
        if let Some(status) = SignallingStatus::from_wire(value) {
            Self::Status(status)
        } else if value >= PREFERRED_PROFILE_BASE {
            let profile_value = value - PREFERRED_PROFILE_BASE;
            if let Some(profile) = Profile::from_wire(profile_value) {
                Self::PreferredProfile(profile)
            } else {
                Self::Raw(value)
            }
        } else {
            Self::Raw(value)
        }
    }
}

impl From<SignallingStatus> for Signal {
    fn from(value: SignallingStatus) -> Self {
        Self::Status(value)
    }
}

impl From<Profile> for Signal {
    fn from(value: Profile) -> Self {
        Self::PreferredProfile(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Codec2Mode {
    Mode700C = 0x00,
    Mode1200 = 0x01,
    Mode1300 = 0x02,
    Mode1400 = 0x03,
    Mode1600 = 0x04,
    Mode2400 = 0x05,
    Mode3200 = 0x06,
}

impl Codec2Mode {
    pub const fn from_header(byte: u8) -> Result<Self, Error> {
        match byte {
            0x00 => Ok(Self::Mode700C),
            0x01 => Ok(Self::Mode1200),
            0x02 => Ok(Self::Mode1300),
            0x03 => Ok(Self::Mode1400),
            0x04 => Ok(Self::Mode1600),
            0x05 => Ok(Self::Mode2400),
            0x06 => Ok(Self::Mode3200),
            other => Err(Error::UnknownCodec2Mode(other)),
        }
    }

    pub const fn header(self) -> u8 {
        self as u8
    }

    pub const fn bitrate(self) -> u16 {
        match self {
            Self::Mode700C => 700,
            Self::Mode1200 => 1200,
            Self::Mode1300 => 1300,
            Self::Mode1400 => 1400,
            Self::Mode1600 => 1600,
            Self::Mode2400 => 2400,
            Self::Mode3200 => 3200,
        }
    }
}
