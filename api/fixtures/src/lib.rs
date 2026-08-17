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
