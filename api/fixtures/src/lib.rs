//! External-consumer compile contract for selected and retained LXST paths.

use rns_identity::identity::Identity;
use rns_transport::messages::TransportMessage;
use tokio::sync::mpsc;

pub mod canonical {
    use super::*;
    use lxst_telephony::{Error, TelephonyService, TelephonyServiceParts};

    pub fn register(
        transport_tx: mpsc::Sender<TransportMessage>,
        identity: &Identity,
    ) -> Result<TelephonyServiceParts, Error> {
        TelephonyService::registered(transport_tx, identity)
    }
}

pub mod retained {
    use lxst_telephony::{TelephonyRnsEndpoint, TelephonyRuntimeCore};

    pub fn raw_assembly_types(
        endpoint: TelephonyRnsEndpoint,
        runtime: TelephonyRuntimeCore,
    ) -> (TelephonyRnsEndpoint, TelephonyRuntimeCore) {
        (endpoint, runtime)
    }
}

pub mod codec2 {
    use lxst_core::{Codec2Backend, Codec2PacketCodec, Codec2PacketError, Profile};

    pub fn construct<B: Codec2Backend>(
        profile: Profile,
        backend: B,
    ) -> Result<Codec2PacketCodec<B>, Codec2PacketError<B::Error>> {
        Codec2PacketCodec::new(profile, backend)
    }

    pub fn encode<B: Codec2Backend>(
        codec: &mut Codec2PacketCodec<B>,
        pcm: &[i16],
        output: &mut [u8],
    ) -> Result<usize, Codec2PacketError<B::Error>> {
        codec.encode_into(pcm, output)
    }

    pub fn decode<B: Codec2Backend>(
        codec: &mut Codec2PacketCodec<B>,
        packet: &[u8],
        output: &mut [i16],
    ) -> Result<usize, Codec2PacketError<B::Error>> {
        codec.decode_into(packet, output)
    }
}

pub mod bounded_codec2 {
    use lxst_codec2::{Codec, Error, Mode};
    use std::mem::MaybeUninit;

    pub fn initialise(storage: &mut MaybeUninit<Codec>, mode: Mode) -> &mut Codec {
        Codec::initialise(storage, mode)
    }
    pub fn roundtrip(
        codec: &mut Codec,
        pcm: &[i16],
        packet: &mut [u8],
        output: &mut [i16],
    ) -> Result<(), Error> {
        codec.encode(pcm, packet)?;
        codec.decode(packet, output)
    }
}

pub mod embedded {
    use lxst_embedded::{CallRole, ConfigError, Session, SessionConfig};
    pub fn construct(role: CallRole, config: SessionConfig) -> Result<Session, ConfigError> {
        Session::new(role, config, 0)
    }
    pub fn parse(
        bytes: &[u8],
    ) -> Result<lxst_embedded::wire::Packet<'_>, lxst_embedded::wire::Error> {
        lxst_embedded::wire::Packet::decode(bytes)
    }
}

pub mod live_audio {
    use lxst_core::{Profile, RawAudioFrame};
    use lxst_telephony::{AudioTransmitGate, LinkId, TelephonyControl};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    pub fn stream(
        link_id: LinkId,
        profile: Profile,
        frames: mpsc::Receiver<RawAudioFrame>,
    ) -> (Arc<AudioTransmitGate>, TelephonyControl) {
        let gate = Arc::new(AudioTransmitGate::new());
        let command = TelephonyControl::StartAudioStream {
            link_id,
            profile,
            frames,
            gate: Some(gate.clone()),
        };
        (gate, command)
    }
}
