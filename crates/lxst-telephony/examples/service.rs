//! Construct the canonical experimental telephony service boundary.

use lxst_telephony::{TelephonyControl, TelephonyService, TelephonyServiceEvent};
use rns_identity::identity::Identity;
use rns_transport::messages::TransportMessage;
use tokio::sync::mpsc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (transport_tx, _transport_rx) = mpsc::channel::<TransportMessage>(32);
    let identity = Identity::new();
    let parts = TelephonyService::registered(transport_tx, &identity)?;

    let _service: TelephonyService = parts.service;
    let _control: mpsc::Sender<TelephonyControl> = parts.control_tx;
    let _events: mpsc::Receiver<TelephonyServiceEvent> = parts.event_rx;
    Ok(())
}
