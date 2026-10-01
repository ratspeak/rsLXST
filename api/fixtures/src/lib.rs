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
