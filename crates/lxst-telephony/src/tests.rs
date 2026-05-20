use super::*;
use lxst_core::{
    CodecKind, OpusDecoderState, OpusEncoderState, SyntheticSource, SyntheticSourceKind,
};
use rns_crypto::ed25519::Ed25519PrivateKey;
use rns_identity::announce::AnnounceData;
use rns_link::link::LinkState;
use rns_transport::{
    constants::InterfaceMode,
    messages::{
        AnnounceRpcEntry, InterfaceRole, PathTableRpcEntry, TransportMessage, TransportQuery,
        TransportQueryResponse,
    },
};

fn link(byte: u8) -> LinkId {
    [byte; 16]
}

fn identity(byte: u8) -> IdentityHash {
    [byte; 16]
}

fn announce_event(
    destination_hash: [u8; 16],
    hops: u8,
    public_key: Option<[u8; 64]>,
) -> AnnounceHandlerEvent {
    AnnounceHandlerEvent {
        destination_hash,
        identity_hash: None,
        announce_packet_hash: [0; 32],
        is_path_response: false,
        hops,
        app_data: None,
        public_key,
        ratchet: None,
        name_hash: [0; 10],
    }
}

fn announce_entry(
    destination_hash: [u8; 16],
    hops: u8,
    public_key: Option<[u8; 64]>,
) -> AnnounceRpcEntry {
    AnnounceRpcEntry {
        dest_hash: destination_hash,
        hops,
        app_data: None,
        timestamp: 1234.0,
        public_key,
        ratchet: None,
        name_hash: name_hash(TELEPHONY_DESTINATION_NAME),
        is_path_response: false,
        retained: false,
    }
}

fn path_entry(destination_hash: [u8; 16], hops: u8) -> PathTableRpcEntry {
    PathTableRpcEntry {
        hash: destination_hash,
        timestamp: 1235.0,
        via: None,
        hops,
        expires: 3600.0,
        interface: "test".to_string(),
        interface_id: 1,
        interface_mode: InterfaceMode::Full,
        interface_role: InterfaceRole::Normal,
    }
}

fn packet(signals: impl IntoIterator<Item = Signal>) -> Vec<u8> {
    LxstPacket::signalling(signals).encode().unwrap()
}

fn synthetic_frame_for_profile(profile: Profile) -> RawAudioFrame {
    SyntheticSource::new(
        profile.channels(),
        profile.sample_rate_hz(),
        profile.sample_frames_per_packet(),
        SyntheticSourceKind::Sine {
            frequency_hz: 440.0,
            amplitude: 0.25,
        },
    )
    .unwrap()
    .next_raw_frame()
    .unwrap()
}

fn active_link_pair() -> (Link, Link) {
    let dest_hash = [0xAA; 16];
    let identity_key = Ed25519PrivateKey::generate();
    let identity_pub = identity_key.public_key();

    let (mut initiator, request_data) = Link::new_initiator(dest_hash, 1);
    let (mut responder, proof_data) =
        Link::new_responder(&request_data, &identity_key, dest_hash, 1).unwrap();

    let rtt_data = initiator
        .validate_proof(&proof_data, &identity_pub, &identity_pub.to_bytes())
        .unwrap();
    responder.receive_rtt_packet(&rtt_data).unwrap();

    (initiator, responder)
}

fn established_outgoing_service(
    profile: Profile,
    remote_identity_byte: u8,
    event_capacity: usize,
) -> (
    TelephonyService,
    Link,
    LinkId,
    mpsc::Sender<DestinationEvent>,
    mpsc::Receiver<TelephonyServiceEvent>,
) {
    let (service, receiver, link_id, link_event_tx, service_events, _transport_rx) =
        established_outgoing_service_with_transport(profile, remote_identity_byte, event_capacity);
    (service, receiver, link_id, link_event_tx, service_events)
}

fn established_outgoing_service_with_transport(
    profile: Profile,
    remote_identity_byte: u8,
    event_capacity: usize,
) -> (
    TelephonyService,
    Link,
    LinkId,
    mpsc::Sender<DestinationEvent>,
    mpsc::Receiver<TelephonyServiceEvent>,
    mpsc::Receiver<TransportMessage>,
) {
    let (sender, receiver) = active_link_pair();
    let link_id = sender.link_id;
    let local_identity = Identity::new();
    let remote_identity = identity(remote_identity_byte);

    let (transport_tx, mut transport_rx) = mpsc::channel(32);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (link_event_tx, event_rx) = mpsc::channel(4);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );

    let mut core = TelephonyRuntimeCore::new();
    core.start_outgoing_call(link_id, remote_identity, Some(profile))
        .unwrap();
    for status in [
        SignallingStatus::Available,
        SignallingStatus::Ringing,
        SignallingStatus::Connecting,
        SignallingStatus::Established,
    ] {
        core.accept_lxst_plaintext(link_id, &packet([Signal::from(status)]))
            .unwrap();
    }

    let (_control_tx, control_rx) = mpsc::channel(1);
    let (event_tx, service_events) = mpsc::channel(event_capacity);
    (
        TelephonyService::new(endpoint, core, control_rx, event_tx),
        receiver,
        link_id,
        link_event_tx,
        service_events,
        transport_rx,
    )
}

fn queue_inbound_opus_frame(
    link_event_tx: &mpsc::Sender<DestinationEvent>,
    receiver: &Link,
    link_id: LinkId,
    encoder: &mut OpusEncoderState,
    frame: RawAudioFrame,
) {
    let opus_frame = encoder.encode_frame(&frame).unwrap();
    let plaintext = LxstPacket::frame(opus_frame).encode().unwrap();
    let encrypted = receiver.encrypt(&plaintext).unwrap();
    link_event_tx
        .try_send(DestinationEvent::InboundPacket {
            raw: Bytes::from(link_data_packet(
                link_id,
                rns_wire::context::PacketContext::None,
                &encrypted,
            )),
            interface_id: 1,
        })
        .unwrap();
}

fn take_outbound(
    rx: &mut mpsc::Receiver<TransportMessage>,
) -> (rns_wire::header::PacketHeader, Vec<u8>) {
    let message = rx.try_recv().unwrap();
    let TransportMessage::Outbound(outbound) = message else {
        panic!("expected Outbound, got {message:?}");
    };
    let (header, data_offset) = rns_wire::header::PacketHeader::unpack(&outbound.raw).unwrap();
    (header, outbound.raw[data_offset..].to_vec())
}

fn collect_ready_service_events(
    rx: &mut mpsc::Receiver<TelephonyServiceEvent>,
) -> Vec<TelephonyServiceEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn assert_deregistered_link(rx: &mut mpsc::Receiver<TransportMessage>, link_id: LinkId) {
    let deregister_link = rx.try_recv().unwrap();
    let TransportMessage::DeregisterDestination {
        hash: deregistered_hash,
    } = deregister_link
    else {
        panic!("expected DeregisterDestination, got {deregister_link:?}");
    };
    assert_eq!(deregistered_hash, link_id);
}

fn assert_deregistered_destination(
    rx: &mut mpsc::Receiver<TransportMessage>,
    destination_hash: [u8; 16],
) {
    let deregister = rx.try_recv().unwrap();
    let TransportMessage::DeregisterDestination { hash } = deregister else {
        panic!("expected DeregisterDestination, got {deregister:?}");
    };
    assert_eq!(hash, destination_hash);
}

fn link_proof_packet(link_id: LinkId, proof_data: &[u8]) -> Vec<u8> {
    let header = rns_wire::header::PacketHeader {
        flags: rns_wire::flags::PacketFlags {
            header_type: rns_wire::flags::HeaderType::Header1,
            context_flag: false,
            transport_type: rns_wire::flags::TransportType::Broadcast,
            destination_type: rns_wire::flags::DestinationType::Link,
            packet_type: rns_wire::flags::PacketType::Proof,
        },
        hops: 0,
        transport_id: None,
        destination_hash: link_id,
        context: rns_wire::context::PacketContext::Lrproof,
    };
    let mut raw = header.pack();
    raw.extend_from_slice(proof_data);
    raw
}

fn link_data_packet(
    link_id: LinkId,
    context: rns_wire::context::PacketContext,
    body: &[u8],
) -> Vec<u8> {
    let header = rns_wire::header::PacketHeader {
        flags: rns_wire::flags::PacketFlags {
            header_type: rns_wire::flags::HeaderType::Header1,
            context_flag: false,
            transport_type: rns_wire::flags::TransportType::Broadcast,
            destination_type: rns_wire::flags::DestinationType::Link,
            packet_type: rns_wire::flags::PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: link_id,
        context,
    };
    let mut raw = header.pack();
    raw.extend_from_slice(body);
    raw
}

#[test]
fn telephony_destination_uses_full_lxst_name() {
    let identity = Identity::new();
    let destination = telephony_inbound_destination(&identity).unwrap();

    assert_eq!(destination.app_name, TELEPHONY_DESTINATION_NAME);
    assert_eq!(destination.hash, telephony_destination_hash(&identity.hash));
    assert_eq!(destination.proof_strategy, ProofStrategy::ProveNone);
}

#[test]
fn caller_access_policy_matches_python_allow_and_block_order() {
    let allowed = identity(0x11);
    let blocked = identity(0x22);
    let policy = CallerAccessPolicy {
        allowed: CallerAllowPolicy::List(vec![allowed, blocked]),
        blocked: vec![blocked],
    };

    assert!(policy.is_allowed(&allowed));
    assert!(!policy.is_allowed(&blocked));
    assert!(!policy.is_allowed(&identity(0x33)));
    assert!(
        !CallerAccessPolicy {
            allowed: CallerAllowPolicy::None,
            blocked: Vec::new(),
        }
        .is_allowed(&allowed)
    );
}

#[test]
fn incoming_call_identify_answer_and_hangup_emit_python_ordered_commands() {
    let mut core = TelephonyRuntimeCore::new();
    let link_id = link(0xAA);
    let remote = identity(0x10);

    assert_eq!(
        core.incoming_link_established(link_id),
        vec![TelephonyCommand::SendSignal {
            link_id,
            signal: Signal::from(SignallingStatus::Available),
        }]
    );
    assert_eq!(core.pending_link_count(), 1);

    assert_eq!(
        core.caller_identified(link_id, remote).unwrap(),
        vec![
            TelephonyCommand::ResetDialingPipelines { link_id },
            TelephonyCommand::SendSignal {
                link_id,
                signal: Signal::from(SignallingStatus::Ringing),
            },
            TelephonyCommand::RingIncomingCall {
                link_id,
                remote_identity: remote,
            },
        ]
    );
    assert_eq!(core.pending_link_count(), 0);
    assert_eq!(
        core.active_call().unwrap().call.status(),
        SignallingStatus::Ringing
    );

    assert_eq!(
        core.answer_active().unwrap(),
        vec![
            TelephonyCommand::SelectProfile {
                link_id,
                profile: Profile::DEFAULT,
            },
            TelephonyCommand::SendSignal {
                link_id,
                signal: Signal::from(SignallingStatus::Connecting),
            },
            TelephonyCommand::OpenAudioPipelines { link_id },
            TelephonyCommand::SendSignal {
                link_id,
                signal: Signal::from(SignallingStatus::Established),
            },
            TelephonyCommand::StartAudioPipelines { link_id },
        ]
    );

    assert_eq!(
        core.hangup_active(false).unwrap(),
        vec![
            TelephonyCommand::TeardownLink { link_id },
            TelephonyCommand::StopAudioPipelines { link_id },
            TelephonyCommand::CallTerminated {
                link_id,
                reason: None,
            },
        ]
    );
    assert!(core.active_call().is_none());
}

#[test]
fn blocked_or_busy_incoming_call_sends_busy_and_tears_down() {
    let blocked = identity(0x44);
    let mut core = TelephonyRuntimeCore::with_access_policy(CallerAccessPolicy {
        allowed: CallerAllowPolicy::All,
        blocked: vec![blocked],
    });
    let link_id = link(0xBB);

    core.incoming_link_established(link_id);
    assert_eq!(
        core.caller_identified(link_id, blocked).unwrap(),
        vec![
            TelephonyCommand::SendSignal {
                link_id,
                signal: Signal::from(SignallingStatus::Busy),
            },
            TelephonyCommand::TeardownLink { link_id },
        ]
    );
    assert!(core.active_call().is_none());

    core.start_outgoing_call(link(0xCC), identity(0x55), None)
        .unwrap();
    assert_eq!(
        core.incoming_link_established(link(0xDD)),
        vec![
            TelephonyCommand::SendSignal {
                link_id: link(0xDD),
                signal: Signal::from(SignallingStatus::Busy),
            },
            TelephonyCommand::TeardownLink {
                link_id: link(0xDD)
            },
        ]
    );
}

#[test]
fn outgoing_signalling_from_lxst_packets_drives_call_setup() {
    let mut core = TelephonyRuntimeCore::new();
    let link_id = link(0xCC);
    core.start_outgoing_call(link_id, identity(0x77), Some(Profile::LatencyLow))
        .unwrap();

    let available = core
        .accept_lxst_plaintext(
            link_id,
            &packet([Signal::from(SignallingStatus::Available)]),
        )
        .unwrap();
    assert_eq!(
        available.commands,
        vec![TelephonyCommand::IdentifyLocalIdentity { link_id }]
    );

    let ringing = core
        .accept_lxst_plaintext(link_id, &packet([Signal::from(SignallingStatus::Ringing)]))
        .unwrap();
    assert_eq!(
        ringing.commands,
        vec![
            TelephonyCommand::SelectProfile {
                link_id,
                profile: Profile::LatencyLow,
            },
            TelephonyCommand::PrepareDialingPipelines { link_id },
            TelephonyCommand::SendSignal {
                link_id,
                signal: Signal::from(Profile::LatencyLow),
            },
            TelephonyCommand::StartDialTone { link_id },
        ]
    );

    let connecting = core
        .accept_lxst_plaintext(
            link_id,
            &packet([Signal::from(SignallingStatus::Connecting)]),
        )
        .unwrap();
    assert_eq!(
        connecting.commands,
        vec![
            TelephonyCommand::ResetDialingPipelines { link_id },
            TelephonyCommand::OpenAudioPipelines { link_id },
        ]
    );

    let established = core
        .accept_lxst_plaintext(
            link_id,
            &packet([Signal::from(SignallingStatus::Established)]),
        )
        .unwrap();
    assert_eq!(
        established.commands,
        vec![TelephonyCommand::StartAudioPipelines { link_id }]
    );
    assert_eq!(
        core.active_call().unwrap().call.status(),
        SignallingStatus::Established
    );
}

#[test]
fn local_profile_switch_updates_active_call_and_signals_remote() {
    let mut core = TelephonyRuntimeCore::new();
    let link_id = link(0xC7);
    core.start_outgoing_call(link_id, identity(0xC8), Some(Profile::QualityMedium))
        .unwrap();
    core.receive_signal(link_id, Signal::from(SignallingStatus::Established))
        .unwrap();

    assert_eq!(
        core.switch_active_profile(Profile::QualityHigh).unwrap(),
        vec![
            TelephonyCommand::SendSignal {
                link_id,
                signal: Signal::from(Profile::QualityHigh),
            },
            TelephonyCommand::SwitchProfile {
                link_id,
                profile: Profile::QualityHigh,
            },
        ]
    );
    assert_eq!(
        core.active_call().unwrap().call.profile(),
        Some(Profile::QualityHigh)
    );
    assert!(
        core.switch_active_profile(Profile::QualityHigh)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn incoming_call_ignores_status_signalling_until_answered() {
    let mut core = TelephonyRuntimeCore::new();
    let link_id = link(0xEE);

    core.incoming_link_established(link_id);
    core.caller_identified(link_id, identity(0x88)).unwrap();

    let step = core
        .accept_lxst_plaintext(
            link_id,
            &packet([Signal::from(SignallingStatus::Established)]),
        )
        .unwrap();
    assert_eq!(
        step.commands,
        vec![TelephonyCommand::IgnoredSignal {
            link_id,
            signal: Signal::from(SignallingStatus::Established),
        }]
    );
    assert_eq!(
        core.active_call().unwrap().call.status(),
        SignallingStatus::Ringing
    );
}

#[test]
fn remote_busy_terminates_and_clears_active_call() {
    let mut core = TelephonyRuntimeCore::new();
    let link_id = link(0xAB);
    core.start_outgoing_call(link_id, identity(0x99), None)
        .unwrap();

    assert_eq!(
        core.receive_signal(link_id, Signal::from(SignallingStatus::Busy))
            .unwrap(),
        vec![
            TelephonyCommand::StopAudioPipelines { link_id },
            TelephonyCommand::TeardownLink { link_id },
            TelephonyCommand::CallTerminated {
                link_id,
                reason: Some(SignallingStatus::Busy),
            },
        ]
    );
    assert!(core.active_call().is_none());
    assert!(matches!(
        core.receive_signal(link_id, Signal::from(SignallingStatus::Available)),
        Err(Error::NoActiveCall)
    ));
}

#[test]
fn link_closed_stops_active_call_and_forgets_pending_links() {
    let mut core = TelephonyRuntimeCore::new();
    let pending = link(0x01);
    core.incoming_link_established(pending);
    assert!(core.link_closed(pending).is_empty());
    assert_eq!(core.pending_link_count(), 0);

    let active = link(0x02);
    core.start_outgoing_call(active, identity(0x02), None)
        .unwrap();
    assert_eq!(
        core.link_closed(active),
        vec![
            TelephonyCommand::StopAudioPipelines { link_id: active },
            TelephonyCommand::CallTerminated {
                link_id: active,
                reason: None,
            },
        ]
    );
    assert!(core.active_call().is_none());
}

#[test]
fn runtime_snapshot_reports_busy_pending_and_active_call_state() {
    let mut core = TelephonyRuntimeCore::new();
    let pending = link(0xA1);
    core.set_external_busy(true);
    core.incoming_link_established(pending);

    assert_eq!(
        core.snapshot(),
        TelephonyRuntimeSnapshot {
            external_busy: true,
            pending_link_count: 0,
            active_call: None,
        }
    );

    core.set_external_busy(false);
    core.incoming_link_established(pending);
    core.caller_identified(pending, identity(0xA2)).unwrap();
    assert_eq!(
        core.snapshot(),
        TelephonyRuntimeSnapshot {
            external_busy: false,
            pending_link_count: 0,
            active_call: Some(ActiveCallSnapshot {
                link_id: pending,
                remote_identity: identity(0xA2),
                role: CallRole::Incoming,
                status: SignallingStatus::Ringing,
                profile: None,
                answered: false,
            }),
        }
    );
}

#[test]
fn signalling_packet_helper_packs_single_signal() {
    let encoded = signalling_packet(Signal::from(SignallingStatus::Available))
        .encode()
        .unwrap();
    assert_eq!(
        LxstPacket::decode(&encoded).unwrap().signals,
        vec![Signal::from(SignallingStatus::Available)]
    );
}

#[test]
fn send_signal_commands_convert_back_to_lxst_packets() {
    let link_id = link(0xB1);
    let commands = vec![
        TelephonyCommand::ResetDialingPipelines { link_id },
        TelephonyCommand::SendSignal {
            link_id,
            signal: Signal::from(SignallingStatus::Ringing),
        },
        TelephonyCommand::SendSignal {
            link_id,
            signal: Signal::from(Profile::LatencyUltraLow),
        },
        TelephonyCommand::StartDialTone { link_id },
    ];

    let packets = signalling_packets_from_commands(&commands);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].0, link_id);
    assert_eq!(
        packets[0].1.signals,
        vec![Signal::from(SignallingStatus::Ringing)]
    );
    assert_eq!(
        packets[1].1.signals,
        vec![Signal::from(Profile::LatencyUltraLow)]
    );
}

#[test]
fn service_events_report_incoming_calls_and_terminations() {
    let link_id = link(0xB3);
    let remote_identity = identity(0xB4);
    assert_eq!(
        service_events_from_commands(&[
            TelephonyCommand::RingIncomingCall {
                link_id,
                remote_identity,
            },
            TelephonyCommand::CallTerminated {
                link_id,
                reason: Some(SignallingStatus::Rejected),
            },
            TelephonyCommand::StopAudioPipelines { link_id },
        ]),
        vec![
            TelephonyServiceEvent::IncomingCall {
                link_id,
                remote_identity,
            },
            TelephonyServiceEvent::CallTerminated {
                link_id,
                reason: Some(SignallingStatus::Rejected),
            },
        ]
    );
}

#[test]
fn link_event_adapter_feeds_runtime_core() {
    let mut core = TelephonyRuntimeCore::new();
    let link_id = link(0xF1);
    let remote = identity(0xF2);

    assert_eq!(
        core.accept_link_event(TelephonyLinkEvent::IncomingLinkEstablished { link_id })
            .unwrap()
            .commands,
        vec![TelephonyCommand::SendSignal {
            link_id,
            signal: Signal::from(SignallingStatus::Available),
        }]
    );

    assert_eq!(
        core.accept_link_event(TelephonyLinkEvent::RemoteIdentified {
            link_id,
            remote_identity: remote,
        })
        .unwrap()
        .commands,
        vec![
            TelephonyCommand::ResetDialingPipelines { link_id },
            TelephonyCommand::SendSignal {
                link_id,
                signal: Signal::from(SignallingStatus::Ringing),
            },
            TelephonyCommand::RingIncomingCall {
                link_id,
                remote_identity: remote,
            },
        ]
    );

    assert_eq!(
        core.accept_link_event(TelephonyLinkEvent::LinkClosed { link_id })
            .unwrap()
            .commands,
        vec![
            TelephonyCommand::StopAudioPipelines { link_id },
            TelephonyCommand::CallTerminated {
                link_id,
                reason: None,
            },
        ]
    );
}

#[test]
fn generic_link_established_event_uses_current_call_role() {
    let mut core = TelephonyRuntimeCore::new();
    let outgoing_link = link(0xC1);
    core.start_outgoing_call(outgoing_link, identity(0xC2), None)
        .unwrap();

    assert_eq!(
        core.accept_link_event(TelephonyLinkEvent::LinkEstablished {
            link_id: outgoing_link,
        })
        .unwrap()
        .commands,
        Vec::new(),
    );

    let incoming_link = link(0xC3);
    assert_eq!(
        core.accept_link_event(TelephonyLinkEvent::LinkEstablished {
            link_id: incoming_link,
        })
        .unwrap()
        .commands,
        vec![
            TelephonyCommand::SendSignal {
                link_id: incoming_link,
                signal: Signal::from(SignallingStatus::Busy),
            },
            TelephonyCommand::TeardownLink {
                link_id: incoming_link,
            },
        ]
    );
}

#[test]
fn rns_endpoint_registers_lxst_telephony_destination_and_channels() {
    let identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(1);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &identity).unwrap();

    assert_eq!(
        endpoint.destination_hash,
        telephony_destination_hash(&identity.hash)
    );
    assert!(endpoint.link_established_rx.try_recv().is_err());
    assert!(endpoint.link_identified_rx.try_recv().is_err());
    assert!(endpoint.link_packet_rx.try_recv().is_err());
    assert!(endpoint.link_closed_rx.try_recv().is_err());

    let registered = transport_rx.try_recv().unwrap();
    let TransportMessage::RegisterDestination {
        hash,
        app_name,
        delivery_tx: Some(_),
    } = registered
    else {
        panic!("expected RegisterDestination, got {registered:?}");
    };

    assert_eq!(hash, endpoint.destination_hash);
    assert_eq!(app_name, TELEPHONY_DESTINATION_NAME);
}

#[test]
fn rns_endpoint_try_step_feeds_established_and_packet_events() {
    let local_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(1);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _registered = transport_rx.try_recv().unwrap();

    let (established_tx, established_rx) = mpsc::channel(1);
    endpoint
        .manager
        .set_link_established_channel(established_tx.clone());
    endpoint.link_established_rx = established_rx;

    let link_id = link(0xD1);
    established_tx.try_send(link_id).unwrap();
    let mut core = TelephonyRuntimeCore::new();
    assert_eq!(
        endpoint.try_step(&mut core).unwrap().unwrap().commands,
        vec![TelephonyCommand::SendSignal {
            link_id,
            signal: Signal::from(SignallingStatus::Available),
        }]
    );

    let (packet_tx, packet_rx) = mpsc::channel(1);
    endpoint.manager.set_link_packet_channel(packet_tx.clone());
    endpoint.link_packet_rx = packet_rx;
    let remote = identity(0xD2);
    core.caller_identified(link_id, remote).unwrap();
    packet_tx
        .try_send((
            signalling_packet(Signal::from(Profile::LatencyLow))
                .encode()
                .unwrap(),
            link_id,
        ))
        .unwrap();

    assert_eq!(
        endpoint.try_step(&mut core).unwrap().unwrap().commands,
        vec![TelephonyCommand::SelectProfile {
            link_id,
            profile: Profile::LatencyLow,
        }]
    );

    let (closed_tx, closed_rx) = mpsc::channel(1);
    endpoint.manager.set_link_closed_channel(closed_tx.clone());
    endpoint.link_closed_rx = closed_rx;
    closed_tx.try_send(link_id).unwrap();
    assert_eq!(
        endpoint.try_step(&mut core).unwrap().unwrap().commands,
        vec![
            TelephonyCommand::StopAudioPipelines { link_id },
            TelephonyCommand::CallTerminated {
                link_id,
                reason: None,
            }
        ]
    );
}

#[test]
fn rns_endpoint_try_drive_once_executes_step_commands() {
    let local_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(1);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _registered = transport_rx.try_recv().unwrap();

    let link_id = link(0xE1);
    let mut core = TelephonyRuntimeCore::new();
    core.start_outgoing_call(link_id, identity(0xE2), None)
        .unwrap();

    let (packet_tx, packet_rx) = mpsc::channel(1);
    endpoint.manager.set_link_packet_channel(packet_tx.clone());
    endpoint.link_packet_rx = packet_rx;
    packet_tx
        .try_send((
            signalling_packet(Signal::from(Profile::LatencyLow))
                .encode()
                .unwrap(),
            link_id,
        ))
        .unwrap();

    let driven = endpoint.try_drive_once(&mut core).unwrap().unwrap();
    assert_eq!(
        driven.step.commands,
        vec![TelephonyCommand::SelectProfile {
            link_id,
            profile: Profile::LatencyLow,
        }]
    );
    assert_eq!(driven.effects, vec![TelephonyCommandEffect::Noop]);
}

#[test]
fn rns_endpoint_try_drive_ready_pumps_reticulum_handshake_events() {
    let local_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(16);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();

    let registered = transport_rx.try_recv().unwrap();
    let TransportMessage::RegisterDestination {
        delivery_tx: Some(destination_tx),
        ..
    } = registered
    else {
        panic!("expected RegisterDestination, got {registered:?}");
    };

    let (mut initiator, request_data) = Link::new_initiator(endpoint.destination_hash, 1);
    let link_id = initiator.link_id;
    destination_tx
        .try_send(DestinationEvent::LinkRequest {
            raw: build_link_request_packet(endpoint.destination_hash, &request_data),
            interface_id: 7,
        })
        .unwrap();

    let mut core = TelephonyRuntimeCore::new();
    assert!(endpoint.try_drive_ready(&mut core).unwrap().is_empty());

    let (proof_header, proof_data) = take_outbound(&mut transport_rx);
    assert_eq!(proof_header.destination_hash, link_id);
    assert_eq!(
        proof_header.flags.packet_type,
        rns_wire::flags::PacketType::Proof
    );

    let register_link = transport_rx.try_recv().unwrap();
    let TransportMessage::RegisterLink {
        link_id: registered_link_id,
        destination_hash: registered_destination_hash,
        interface_id,
        initiator: registered_initiator,
        ..
    } = register_link
    else {
        panic!("expected RegisterLink, got {register_link:?}");
    };
    assert_eq!(registered_link_id, link_id);
    assert_eq!(registered_destination_hash, endpoint.destination_hash);
    assert_eq!(interface_id, 7);
    assert!(!registered_initiator);

    let local_public_key = local_identity.get_public_key();
    let mut local_ed25519_public_key = [0u8; 32];
    local_ed25519_public_key.copy_from_slice(&local_public_key[32..64]);
    let verify_key = Ed25519PublicKey::from_bytes(&local_ed25519_public_key).unwrap();
    let rtt_data = initiator
        .validate_proof(&proof_data, &verify_key, &local_ed25519_public_key)
        .unwrap();

    destination_tx
        .try_send(DestinationEvent::InboundPacket {
            raw: Bytes::from(link_data_packet(
                link_id,
                rns_wire::context::PacketContext::Lrrtt,
                &rtt_data,
            )),
            interface_id: 7,
        })
        .unwrap();

    let driven = endpoint.try_drive_ready(&mut core).unwrap();
    assert_eq!(driven.len(), 1);
    assert_eq!(
        driven[0].step.commands,
        vec![TelephonyCommand::SendSignal {
            link_id,
            signal: Signal::from(SignallingStatus::Available),
        }]
    );
    assert_eq!(
        driven[0].effects,
        vec![TelephonyCommandEffect::QueuedLinkPacket {
            link_id,
            kind: QueuedLinkPacketKind::LxstData,
        }]
    );

    let (available_header, available_data) = take_outbound(&mut transport_rx);
    assert_eq!(available_header.destination_hash, link_id);
    let available_plaintext = initiator.decrypt(&available_data).unwrap();
    assert_eq!(
        LxstPacket::decode(&available_plaintext).unwrap().signals,
        vec![Signal::from(SignallingStatus::Available)]
    );
}

#[tokio::test]
async fn discover_remote_telephony_peer_uses_recent_announce_pubkey() {
    let local_identity = Identity::new();
    let remote_identity = Identity::new();
    let remote_hash = remote_identity.hash;
    let remote_public_key = remote_identity.get_public_key();
    let destination_hash = telephony_destination_hash(&remote_hash);

    let (transport_tx, mut transport_rx) = mpsc::channel(16);
    let endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.recv().await.unwrap();

    let discovery = endpoint.discover_remote_telephony_peer(remote_hash, Duration::from_secs(1));
    let transport = async {
        let register = transport_rx.recv().await.unwrap();
        let TransportMessage::RegisterAnnounceHandler {
            aspect_filter: Some(aspect_filter),
            ..
        } = register
        else {
            panic!("expected RegisterAnnounceHandler, got {register:?}");
        };
        assert_eq!(aspect_filter, TELEPHONY_DESTINATION_NAME);

        let rpc = transport_rx.recv().await.unwrap();
        let TransportMessage::Rpc { query, response_tx } = rpc else {
            panic!("expected Rpc, got {rpc:?}");
        };
        assert!(matches!(query, TransportQuery::GetRecentAnnounces));
        response_tx
            .send(TransportQueryResponse::Announces(vec![announce_entry(
                destination_hash,
                3,
                Some(remote_public_key),
            )]))
            .unwrap();

        let request_path = transport_rx.recv().await.unwrap();
        let TransportMessage::RequestPath {
            destination_hash: requested_hash,
        } = request_path
        else {
            panic!("expected RequestPath, got {request_path:?}");
        };
        assert_eq!(requested_hash, destination_hash);

        let deregister = transport_rx.recv().await.unwrap();
        let TransportMessage::DeregisterAnnounceHandler {
            aspect_filter: Some(aspect_filter),
        } = deregister
        else {
            panic!("expected DeregisterAnnounceHandler, got {deregister:?}");
        };
        assert_eq!(aspect_filter, TELEPHONY_DESTINATION_NAME);
    };

    let (peer, ()) = tokio::join!(discovery, transport);
    assert_eq!(
        peer.unwrap(),
        RemoteTelephonyPeer {
            identity_hash: remote_hash,
            destination_hash,
            public_key: remote_public_key,
            hops: 3,
        }
    );
}

#[tokio::test]
async fn discover_remote_telephony_peer_skips_keyless_recent_announce() {
    let local_identity = Identity::new();
    let remote_identity = Identity::new();
    let remote_hash = remote_identity.hash;
    let remote_public_key = remote_identity.get_public_key();
    let destination_hash = telephony_destination_hash(&remote_hash);

    let (transport_tx, mut transport_rx) = mpsc::channel(16);
    let endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.recv().await.unwrap();

    let discovery = endpoint.discover_remote_telephony_peer(remote_hash, Duration::from_secs(1));
    let transport = async {
        let register = transport_rx.recv().await.unwrap();
        let TransportMessage::RegisterAnnounceHandler {
            aspect_filter: Some(aspect_filter),
            callback_tx,
            ..
        } = register
        else {
            panic!("expected RegisterAnnounceHandler, got {register:?}");
        };
        assert_eq!(aspect_filter, TELEPHONY_DESTINATION_NAME);

        let rpc = transport_rx.recv().await.unwrap();
        let TransportMessage::Rpc { query, response_tx } = rpc else {
            panic!("expected Rpc, got {rpc:?}");
        };
        assert!(matches!(query, TransportQuery::GetRecentAnnounces));
        response_tx
            .send(TransportQueryResponse::Announces(vec![announce_entry(
                destination_hash,
                3,
                None,
            )]))
            .unwrap();

        let rpc = transport_rx.recv().await.unwrap();
        let TransportMessage::Rpc { query, response_tx } = rpc else {
            panic!("expected DropPath Rpc, got {rpc:?}");
        };
        assert!(matches!(
            query,
            TransportQuery::DropPath {
                dest
            } if dest == destination_hash
        ));
        response_tx.send(TransportQueryResponse::Ok).unwrap();

        let request_path = transport_rx.recv().await.unwrap();
        let TransportMessage::RequestPath {
            destination_hash: requested_hash,
        } = request_path
        else {
            panic!("expected RequestPath, got {request_path:?}");
        };
        assert_eq!(requested_hash, destination_hash);

        callback_tx
            .send(announce_event(destination_hash, 2, None))
            .await
            .unwrap();
        callback_tx
            .send(announce_event(destination_hash, 1, Some(remote_public_key)))
            .await
            .unwrap();

        let deregister = transport_rx.recv().await.unwrap();
        let TransportMessage::DeregisterAnnounceHandler {
            aspect_filter: Some(aspect_filter),
        } = deregister
        else {
            panic!("expected DeregisterAnnounceHandler, got {deregister:?}");
        };
        assert_eq!(aspect_filter, TELEPHONY_DESTINATION_NAME);
    };

    let (peer, ()) = tokio::join!(discovery, transport);
    assert_eq!(
        peer.unwrap(),
        RemoteTelephonyPeer {
            identity_hash: remote_hash,
            destination_hash,
            public_key: remote_public_key,
            hops: 1,
        }
    );
}

#[tokio::test]
async fn await_path_to_identity_uses_telephony_destination_hash() {
    let local_identity = Identity::new();
    let remote_identity = Identity::new();
    let remote_hash = remote_identity.hash;
    let destination_hash = telephony_destination_hash(&remote_hash);

    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.recv().await.unwrap();

    let await_path = endpoint.await_path_to_identity(remote_hash, Duration::from_secs(1));
    let transport = async {
        let message = transport_rx.recv().await.unwrap();
        let TransportMessage::AwaitPath { dest, reply } = message else {
            panic!("expected AwaitPath, got {message:?}");
        };
        assert_eq!(dest, destination_hash);
        reply.send(true).unwrap();
    };

    let (result, ()) = tokio::join!(await_path, transport);
    result.unwrap();
}

#[tokio::test]
async fn endpoint_announce_sends_lxst_telephony_announce_with_public_key() {
    let identity = Identity::new();
    let destination_hash = telephony_destination_hash(&identity.hash);
    let public_key = identity.get_public_key();

    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let endpoint = TelephonyRnsEndpoint::register(transport_tx, &identity).unwrap();
    let _listener_registration = transport_rx.recv().await.unwrap();

    endpoint.announce().unwrap();

    let (header, payload) = take_outbound(&mut transport_rx);
    assert_eq!(header.destination_hash, destination_hash);
    assert_eq!(
        header.flags.packet_type,
        rns_wire::flags::PacketType::Announce
    );
    assert_eq!(header.context, rns_wire::context::PacketContext::None);

    let announce = AnnounceData::unpack(&payload, header.flags.context_flag).unwrap();
    assert_eq!(announce.public_key, public_key);
    assert_eq!(announce.name_hash, name_hash(TELEPHONY_DESTINATION_NAME));
    let announced_identity = announce.validate(&destination_hash).unwrap();
    assert_eq!(announced_identity.hash, identity.hash);
}

#[tokio::test]
async fn begin_outgoing_link_discovers_announce_and_sends_link_request() {
    let local_identity = Identity::new();
    let remote_identity = Identity::new();
    let remote_hash = remote_identity.hash;
    let remote_public_key = remote_identity.get_public_key();
    let destination_hash = telephony_destination_hash(&remote_hash);

    let (transport_tx, mut transport_rx) = mpsc::channel(16);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.recv().await.unwrap();
    let mut core = TelephonyRuntimeCore::new();

    let call = endpoint.begin_outgoing_link(
        &mut core,
        remote_hash,
        Some(Profile::LatencyLow),
        Duration::from_secs(1),
    );
    let transport = async {
        let register = transport_rx.recv().await.unwrap();
        let TransportMessage::RegisterAnnounceHandler {
            aspect_filter: Some(aspect_filter),
            receive_path_responses,
            callback_tx,
        } = register
        else {
            panic!("expected RegisterAnnounceHandler, got {register:?}");
        };
        assert_eq!(aspect_filter, TELEPHONY_DESTINATION_NAME);
        assert!(receive_path_responses);

        let rpc = transport_rx.recv().await.unwrap();
        let TransportMessage::Rpc { query, response_tx } = rpc else {
            panic!("expected Rpc, got {rpc:?}");
        };
        assert!(matches!(query, TransportQuery::GetRecentAnnounces));
        response_tx
            .send(TransportQueryResponse::Announces(Vec::new()))
            .unwrap();

        let rpc = transport_rx.recv().await.unwrap();
        let TransportMessage::Rpc { query, response_tx } = rpc else {
            panic!("expected DropPath Rpc, got {rpc:?}");
        };
        assert!(matches!(
            query,
            TransportQuery::DropPath {
                dest
            } if dest == destination_hash
        ));
        response_tx.send(TransportQueryResponse::Ok).unwrap();

        let request_path = transport_rx.recv().await.unwrap();
        let TransportMessage::RequestPath {
            destination_hash: requested_hash,
        } = request_path
        else {
            panic!("expected discovery RequestPath, got {request_path:?}");
        };
        assert_eq!(requested_hash, destination_hash);

        callback_tx
            .send(announce_event(destination_hash, 2, Some(remote_public_key)))
            .await
            .unwrap();

        let deregister = transport_rx.recv().await.unwrap();
        let TransportMessage::DeregisterAnnounceHandler {
            aspect_filter: Some(aspect_filter),
        } = deregister
        else {
            panic!("expected DeregisterAnnounceHandler, got {deregister:?}");
        };
        assert_eq!(aspect_filter, TELEPHONY_DESTINATION_NAME);

        let await_path = transport_rx.recv().await.unwrap();
        let TransportMessage::AwaitPath { dest, reply } = await_path else {
            panic!("expected AwaitPath, got {await_path:?}");
        };
        assert_eq!(dest, destination_hash);
        reply.send(true).unwrap();

        let rpc = transport_rx.recv().await.unwrap();
        let TransportMessage::Rpc { query, response_tx } = rpc else {
            panic!("expected path table Rpc, got {rpc:?}");
        };
        assert!(matches!(query, TransportQuery::GetPathTable));
        response_tx
            .send(TransportQueryResponse::PathTable(vec![path_entry(
                destination_hash,
                2,
            )]))
            .unwrap();

        let request_path = transport_rx.recv().await.unwrap();
        let TransportMessage::RequestPath {
            destination_hash: requested_hash,
        } = request_path
        else {
            panic!("expected link RequestPath, got {request_path:?}");
        };
        assert_eq!(requested_hash, destination_hash);

        let register_link = transport_rx.recv().await.unwrap();
        let TransportMessage::RegisterDestination {
            hash: registered_link_id,
            delivery_tx: Some(_),
            ..
        } = register_link
        else {
            panic!("expected link RegisterDestination, got {register_link:?}");
        };

        let (request_header, request_data) = take_outbound(&mut transport_rx);
        assert_eq!(request_header.destination_hash, destination_hash);
        assert_eq!(
            request_header.flags.packet_type,
            rns_wire::flags::PacketType::LinkRequest
        );
        assert!(!request_data.is_empty());

        registered_link_id
    };

    let (link_id, registered_link_id) = tokio::join!(call, transport);
    let link_id = link_id.unwrap();
    assert_eq!(link_id, registered_link_id);
    assert_eq!(core.active_call().unwrap().link_id, link_id);
    assert_eq!(core.active_call().unwrap().remote_identity, remote_hash);
}

#[tokio::test]
async fn telephony_service_registered_helper_wires_channels_and_lifecycle() {
    let local_identity = Identity::new();
    let destination_hash = telephony_destination_hash(&local_identity.hash);
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let TelephonyServiceParts {
        service,
        control_tx,
        mut event_rx,
    } = TelephonyService::registered_with_config(
        transport_tx,
        &local_identity,
        TelephonyServiceConfig {
            poll_interval: Duration::from_millis(100),
            announce_on_start: false,
            ..TelephonyServiceConfig::default()
        },
        TelephonyServiceChannelConfig {
            control_capacity: 0,
            event_capacity: 0,
        },
    )
    .unwrap();

    let register = transport_rx.recv().await.unwrap();
    let TransportMessage::RegisterDestination {
        hash: registered_hash,
        app_name,
        delivery_tx: Some(_),
    } = register
    else {
        panic!("expected RegisterDestination, got {register:?}");
    };
    assert_eq!(registered_hash, destination_hash);
    assert_eq!(app_name, TELEPHONY_DESTINATION_NAME);

    let service_task = tokio::spawn(service.run());
    control_tx.send(TelephonyControl::Shutdown).await.unwrap();

    let stopped = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped, TelephonyServiceEvent::Stopped);
    service_task.await.unwrap();

    assert_deregistered_destination(&mut transport_rx, destination_hash);
}

#[tokio::test]
async fn telephony_service_announces_on_start() {
    let local_identity = Identity::new();
    let destination_hash = telephony_destination_hash(&local_identity.hash);
    let (transport_tx, mut transport_rx) = mpsc::channel(8);
    let TelephonyServiceParts {
        service,
        control_tx,
        mut event_rx,
    } = TelephonyService::registered_with_config(
        transport_tx,
        &local_identity,
        TelephonyServiceConfig {
            poll_interval: Duration::from_millis(10),
            announce_interval: None,
            ..TelephonyServiceConfig::default()
        },
        TelephonyServiceChannelConfig::default(),
    )
    .unwrap();

    let _listener_registration = transport_rx.recv().await.unwrap();
    let service_task = tokio::spawn(service.run());

    let announce = timeout(Duration::from_secs(1), transport_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let TransportMessage::Outbound(announce) = announce else {
        panic!("expected telephony announce Outbound, got {announce:?}");
    };
    let (header, data_offset) = rns_wire::header::PacketHeader::unpack(&announce.raw).unwrap();
    assert_eq!(header.destination_hash, destination_hash);
    assert_eq!(
        header.flags.packet_type,
        rns_wire::flags::PacketType::Announce
    );
    let announce_data =
        AnnounceData::unpack(&announce.raw[data_offset..], header.flags.context_flag).unwrap();
    announce_data.validate(&destination_hash).unwrap();

    control_tx.send(TelephonyControl::Shutdown).await.unwrap();
    let stopped = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped, TelephonyServiceEvent::Stopped);
    service_task.await.unwrap();
}

#[tokio::test]
async fn telephony_service_retries_startup_announces_before_regular_interval() {
    let local_identity = Identity::new();
    let destination_hash = telephony_destination_hash(&local_identity.hash);
    let (transport_tx, mut transport_rx) = mpsc::channel(8);
    let TelephonyServiceParts {
        service,
        control_tx,
        mut event_rx,
    } = TelephonyService::registered_with_config(
        transport_tx,
        &local_identity,
        TelephonyServiceConfig {
            poll_interval: Duration::from_millis(5),
            announce_interval: None,
            startup_announce_retry_interval: Some(Duration::from_millis(20)),
            startup_announce_retries: 2,
            ..TelephonyServiceConfig::default()
        },
        TelephonyServiceChannelConfig::default(),
    )
    .unwrap();

    let _listener_registration = transport_rx.recv().await.unwrap();
    let service_task = tokio::spawn(service.run());

    for _ in 0..3 {
        let announce = timeout(Duration::from_secs(1), transport_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let TransportMessage::Outbound(announce) = announce else {
            panic!("expected telephony announce Outbound, got {announce:?}");
        };
        let (header, data_offset) = rns_wire::header::PacketHeader::unpack(&announce.raw).unwrap();
        assert_eq!(header.destination_hash, destination_hash);
        assert_eq!(
            header.flags.packet_type,
            rns_wire::flags::PacketType::Announce
        );
        AnnounceData::unpack(&announce.raw[data_offset..], header.flags.context_flag)
            .unwrap()
            .validate(&destination_hash)
            .unwrap();
    }

    assert!(
        timeout(Duration::from_millis(60), transport_rx.recv())
            .await
            .is_err(),
        "service should stop startup announces after configured retry count"
    );

    control_tx.send(TelephonyControl::Shutdown).await.unwrap();
    let stopped = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped, TelephonyServiceEvent::Stopped);
    service_task.await.unwrap();
}

#[tokio::test]
async fn telephony_service_announce_control_queues_telephony_announce() {
    let local_identity = Identity::new();
    let destination_hash = telephony_destination_hash(&local_identity.hash);
    let (transport_tx, mut transport_rx) = mpsc::channel(8);
    let TelephonyServiceParts {
        service,
        control_tx,
        mut event_rx,
    } = TelephonyService::registered_with_config(
        transport_tx,
        &local_identity,
        TelephonyServiceConfig {
            announce_on_start: false,
            announce_interval: None,
            ..TelephonyServiceConfig::default()
        },
        TelephonyServiceChannelConfig::default(),
    )
    .unwrap();

    let _listener_registration = transport_rx.recv().await.unwrap();
    let service_task = tokio::spawn(service.run());

    control_tx.send(TelephonyControl::Announce).await.unwrap();
    let announce = timeout(Duration::from_secs(1), transport_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let TransportMessage::Outbound(announce) = announce else {
        panic!("expected telephony announce Outbound, got {announce:?}");
    };
    let (header, data_offset) = rns_wire::header::PacketHeader::unpack(&announce.raw).unwrap();
    assert_eq!(header.destination_hash, destination_hash);
    assert_eq!(
        header.flags.packet_type,
        rns_wire::flags::PacketType::Announce
    );
    AnnounceData::unpack(&announce.raw[data_offset..], header.flags.context_flag)
        .unwrap()
        .validate(&destination_hash)
        .unwrap();

    control_tx.send(TelephonyControl::Shutdown).await.unwrap();
    let stopped = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped, TelephonyServiceEvent::Stopped);
    service_task.await.unwrap();
}

#[tokio::test]
async fn telephony_service_outgoing_discovery_does_not_block_controls() {
    let local_identity = Identity::new();
    let remote_identity = Identity::new();
    let remote_hash = remote_identity.hash;
    let local_destination_hash = telephony_destination_hash(&local_identity.hash);
    let remote_destination_hash = telephony_destination_hash(&remote_hash);

    let (transport_tx, mut transport_rx) = mpsc::channel(16);
    let endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.recv().await.unwrap();

    let (control_tx, control_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let service = TelephonyService::with_config(
        endpoint,
        TelephonyRuntimeCore::new(),
        control_rx,
        event_tx,
        TelephonyServiceConfig {
            poll_interval: Duration::from_millis(20),
            announce_on_start: false,
            announce_interval: None,
            ..TelephonyServiceConfig::default()
        },
    );
    let service_task = tokio::spawn(service.run());

    control_tx
        .send(TelephonyControl::Call {
            remote_identity: remote_hash,
            profile: Some(Profile::QualityMedium),
            discovery_timeout: Duration::from_millis(100),
        })
        .await
        .unwrap();

    let event = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        TelephonyServiceEvent::OutgoingCallPending {
            remote_identity: remote_hash,
        }
    );

    let register = transport_rx.recv().await.unwrap();
    assert!(matches!(
        register,
        TransportMessage::RegisterAnnounceHandler {
            aspect_filter: Some(_),
            receive_path_responses: true,
            ..
        }
    ));

    let rpc = transport_rx.recv().await.unwrap();
    let TransportMessage::Rpc { query, response_tx } = rpc else {
        panic!("expected GetRecentAnnounces Rpc, got {rpc:?}");
    };
    assert!(matches!(query, TransportQuery::GetRecentAnnounces));

    control_tx.send(TelephonyControl::Announce).await.unwrap();
    let announce = timeout(Duration::from_secs(1), transport_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let TransportMessage::Outbound(announce) = announce else {
        panic!("expected telephony announce while discovery is pending, got {announce:?}");
    };
    let (header, _data_offset) = rns_wire::header::PacketHeader::unpack(&announce.raw).unwrap();
    assert_eq!(header.destination_hash, local_destination_hash);
    assert_eq!(
        header.flags.packet_type,
        rns_wire::flags::PacketType::Announce
    );

    response_tx
        .send(TransportQueryResponse::Announces(Vec::new()))
        .unwrap();
    let rpc = transport_rx.recv().await.unwrap();
    let TransportMessage::Rpc { query, response_tx } = rpc else {
        panic!("expected DropPath Rpc, got {rpc:?}");
    };
    assert!(matches!(
        query,
        TransportQuery::DropPath {
            dest
        } if dest == remote_destination_hash
    ));
    response_tx.send(TransportQueryResponse::Ok).unwrap();

    let request_path = transport_rx.recv().await.unwrap();
    let TransportMessage::RequestPath { destination_hash } = request_path else {
        panic!("expected RequestPath, got {request_path:?}");
    };
    assert_eq!(destination_hash, remote_destination_hash);

    let event = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        TelephonyServiceEvent::OutgoingCallFailed {
            remote_identity: remote_hash,
            message: Error::RemoteTelephonyPeerNotDiscovered.to_string(),
        }
    );

    control_tx.send(TelephonyControl::Shutdown).await.unwrap();
    let stopped = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped, TelephonyServiceEvent::Stopped);
    service_task.await.unwrap();
}

#[tokio::test]
async fn telephony_service_call_control_discovers_peer_and_emits_started() {
    let local_identity = Identity::new();
    let remote_identity = Identity::new();
    let remote_hash = remote_identity.hash;
    let local_destination_hash = telephony_destination_hash(&local_identity.hash);
    let remote_public_key = remote_identity.get_public_key();
    let destination_hash = telephony_destination_hash(&remote_hash);

    let (transport_tx, mut transport_rx) = mpsc::channel(16);
    let endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.recv().await.unwrap();

    let (control_tx, control_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let service = TelephonyService::with_config(
        endpoint,
        TelephonyRuntimeCore::new(),
        control_rx,
        event_tx,
        TelephonyServiceConfig {
            poll_interval: Duration::from_millis(100),
            announce_on_start: false,
            ..TelephonyServiceConfig::default()
        },
    );
    let service_task = tokio::spawn(service.run());

    control_tx
        .send(TelephonyControl::Call {
            remote_identity: remote_hash,
            profile: Some(Profile::LatencyLow),
            discovery_timeout: Duration::from_secs(1),
        })
        .await
        .unwrap();

    let event = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        TelephonyServiceEvent::OutgoingCallPending {
            remote_identity: remote_hash,
        }
    );

    let register = transport_rx.recv().await.unwrap();
    let TransportMessage::RegisterAnnounceHandler {
        aspect_filter: Some(aspect_filter),
        receive_path_responses,
        callback_tx,
    } = register
    else {
        panic!("expected RegisterAnnounceHandler, got {register:?}");
    };
    assert_eq!(aspect_filter, TELEPHONY_DESTINATION_NAME);
    assert!(receive_path_responses);

    let rpc = transport_rx.recv().await.unwrap();
    let TransportMessage::Rpc { query, response_tx } = rpc else {
        panic!("expected Rpc, got {rpc:?}");
    };
    assert!(matches!(query, TransportQuery::GetRecentAnnounces));
    response_tx
        .send(TransportQueryResponse::Announces(Vec::new()))
        .unwrap();

    let rpc = transport_rx.recv().await.unwrap();
    let TransportMessage::Rpc { query, response_tx } = rpc else {
        panic!("expected DropPath Rpc, got {rpc:?}");
    };
    assert!(matches!(
        query,
        TransportQuery::DropPath {
            dest
        } if dest == destination_hash
    ));
    response_tx.send(TransportQueryResponse::Ok).unwrap();

    let request_path = transport_rx.recv().await.unwrap();
    let TransportMessage::RequestPath {
        destination_hash: requested_hash,
    } = request_path
    else {
        panic!("expected discovery RequestPath, got {request_path:?}");
    };
    assert_eq!(requested_hash, destination_hash);

    callback_tx
        .send(announce_event(destination_hash, 1, Some(remote_public_key)))
        .await
        .unwrap();

    let deregister = transport_rx.recv().await.unwrap();
    let TransportMessage::DeregisterAnnounceHandler {
        aspect_filter: Some(aspect_filter),
    } = deregister
    else {
        panic!("expected DeregisterAnnounceHandler, got {deregister:?}");
    };
    assert_eq!(aspect_filter, TELEPHONY_DESTINATION_NAME);

    let await_path = transport_rx.recv().await.unwrap();
    let TransportMessage::AwaitPath { dest, reply } = await_path else {
        panic!("expected AwaitPath, got {await_path:?}");
    };
    assert_eq!(dest, destination_hash);
    reply.send(true).unwrap();

    let rpc = transport_rx.recv().await.unwrap();
    let TransportMessage::Rpc { query, response_tx } = rpc else {
        panic!("expected path table Rpc, got {rpc:?}");
    };
    assert!(matches!(query, TransportQuery::GetPathTable));
    response_tx
        .send(TransportQueryResponse::PathTable(vec![path_entry(
            destination_hash,
            1,
        )]))
        .unwrap();

    let request_path = transport_rx.recv().await.unwrap();
    let TransportMessage::RequestPath {
        destination_hash: requested_hash,
    } = request_path
    else {
        panic!("expected link RequestPath, got {request_path:?}");
    };
    assert_eq!(requested_hash, destination_hash);

    let register_link = transport_rx.recv().await.unwrap();
    let TransportMessage::RegisterDestination {
        hash: registered_link_id,
        delivery_tx: Some(_),
        ..
    } = register_link
    else {
        panic!("expected link RegisterDestination, got {register_link:?}");
    };

    let (request_header, _) = take_outbound(&mut transport_rx);
    assert_eq!(
        request_header.flags.packet_type,
        rns_wire::flags::PacketType::LinkRequest
    );

    let event = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        TelephonyServiceEvent::OutgoingCallStarted {
            link_id: registered_link_id,
            remote_identity: remote_hash,
        }
    );

    let event = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        TelephonyServiceEvent::Snapshot(TelephonyRuntimeSnapshot {
            external_busy: false,
            pending_link_count: 0,
            active_call: Some(ActiveCallSnapshot {
                link_id: registered_link_id,
                remote_identity: remote_hash,
                role: CallRole::Outgoing,
                status: SignallingStatus::Calling,
                profile: Some(Profile::LatencyLow),
                answered: false,
            }),
        })
    );

    control_tx.send(TelephonyControl::Shutdown).await.unwrap();
    let event = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let TelephonyServiceEvent::Drive(step) = event else {
        panic!("expected Drive event, got {event:?}");
    };
    assert_eq!(
        step.step.commands,
        vec![
            TelephonyCommand::TeardownLink {
                link_id: registered_link_id,
            },
            TelephonyCommand::StopAudioPipelines {
                link_id: registered_link_id,
            },
            TelephonyCommand::CallTerminated {
                link_id: registered_link_id,
                reason: None,
            },
        ]
    );
    assert_eq!(
        step.effects,
        vec![
            TelephonyCommandEffect::Noop,
            TelephonyCommandEffect::Noop,
            TelephonyCommandEffect::Noop,
        ]
    );

    let event = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        TelephonyServiceEvent::CallTerminated {
            link_id: registered_link_id,
            reason: None,
        }
    );

    let event = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        TelephonyServiceEvent::Snapshot(TelephonyRuntimeSnapshot {
            external_busy: false,
            pending_link_count: 0,
            active_call: None,
        })
    );

    let stopped = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped, TelephonyServiceEvent::Stopped);
    service_task.await.unwrap();

    assert_deregistered_link(&mut transport_rx, registered_link_id);
    assert_deregistered_destination(&mut transport_rx, local_destination_hash);
}

#[tokio::test]
async fn telephony_service_send_opus_frames_queues_decodeable_quality_media() {
    let (sender, receiver) = active_link_pair();
    let link_id = sender.link_id;
    let profile = Profile::QualityMedium;
    let frame = synthetic_frame_for_profile(profile);
    let local_identity = Identity::new();
    let remote_identity = identity(0xA7);

    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (_event_tx, event_rx) = mpsc::channel(1);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );

    let mut core = TelephonyRuntimeCore::new();
    core.start_outgoing_call(link_id, remote_identity, Some(profile))
        .unwrap();
    for status in [
        SignallingStatus::Available,
        SignallingStatus::Ringing,
        SignallingStatus::Connecting,
        SignallingStatus::Established,
    ] {
        core.accept_lxst_plaintext(link_id, &packet([Signal::from(status)]))
            .unwrap();
    }

    let (_control_tx, control_rx) = mpsc::channel(1);
    let (event_tx, mut service_events) = mpsc::channel(4);
    let mut service = TelephonyService::new(endpoint, core, control_rx, event_tx);

    service
        .send_opus_frames(profile, vec![frame.clone()])
        .await
        .unwrap();
    let first_encoder_generation = service.media.opus_encoder_generation;
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::MediaSent {
            link_id,
            frames: 1,
            packets: 1,
        }
    );

    let (header, encrypted) = take_outbound(&mut transport_rx);
    assert_eq!(header.destination_hash, link_id);
    assert_eq!(header.context, rns_wire::context::PacketContext::None);

    let plaintext = receiver.decrypt(&encrypted).unwrap();
    let packet = LxstPacket::decode(&plaintext).unwrap();
    assert_eq!(packet.frames.len(), 1);
    assert_eq!(packet.frames[0].codec, CodecKind::Opus);
    assert_eq!(packet.frames[0].payload[0] & 0x03, 0x03);
    assert_eq!(packet.frames[0].payload[1] & 0x3F, 3);
    assert!(packet.frames[0].payload.len() <= profile.opus_payload_ceiling_bytes().unwrap());

    let mut decoder = OpusDecoderState::new(profile).unwrap();
    let decoded = decoder.decode_frame(&packet.frames[0]).unwrap();
    assert_eq!(decoded.channels, profile.channels());
    assert_eq!(decoded.sample_frames(), profile.sample_frames_per_packet());

    service
        .send_opus_frames(profile, vec![synthetic_frame_for_profile(profile)])
        .await
        .unwrap();
    assert_eq!(
        service.media.opus_encoder_generation,
        first_encoder_generation
    );
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::MediaSent {
            link_id,
            frames: 1,
            packets: 1,
        }
    );
    let (_header, _encrypted) = take_outbound(&mut transport_rx);

    assert!(matches!(
        service
            .send_opus_frames(
                Profile::LatencyLow,
                vec![synthetic_frame_for_profile(Profile::LatencyLow)]
            )
            .await,
        Err(Error::MediaProfileMismatch {
            active: Profile::QualityMedium,
            requested: Profile::LatencyLow,
        })
    ));

    let _commands = service.core.hangup_active(false).unwrap();
    service.refresh_active_timeout();
    assert!(service.media.opus_encoder.is_none());
}

#[tokio::test]
async fn telephony_service_pumps_owned_opus_stream_in_bounded_batches() {
    let (sender, receiver) = active_link_pair();
    let link_id = sender.link_id;
    let profile = Profile::LatencyLow;
    let local_identity = Identity::new();
    let remote_identity = identity(0xA9);

    let (transport_tx, mut transport_rx) = mpsc::channel(16);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (_event_tx, event_rx) = mpsc::channel(1);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );

    let mut core = TelephonyRuntimeCore::new();
    core.start_outgoing_call(link_id, remote_identity, Some(profile))
        .unwrap();
    for status in [
        SignallingStatus::Available,
        SignallingStatus::Ringing,
        SignallingStatus::Connecting,
        SignallingStatus::Established,
    ] {
        core.accept_lxst_plaintext(link_id, &packet([Signal::from(status)]))
            .unwrap();
    }

    let (_control_tx, control_rx) = mpsc::channel(1);
    let (event_tx, mut service_events) = mpsc::channel(8);
    let mut service = TelephonyService::with_config(
        endpoint,
        core,
        control_rx,
        event_tx,
        TelephonyServiceConfig {
            media_frames_per_tick: 2,
            ..TelephonyServiceConfig::default()
        },
    );
    let (frame_tx, frame_rx) = mpsc::channel(8);
    for _ in 0..5 {
        frame_tx
            .try_send(synthetic_frame_for_profile(profile))
            .unwrap();
    }
    drop(frame_tx);

    service.start_opus_stream(profile, frame_rx).await.unwrap();
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusTransmitStreamStarted { link_id, profile }
    );

    let mut decoder = OpusDecoderState::new(profile).unwrap();
    for expected_frames in [2, 2, 1] {
        assert!(service.pump_opus_stream().await);
        assert_eq!(
            service_events.recv().await.unwrap(),
            TelephonyServiceEvent::MediaSent {
                link_id,
                frames: expected_frames,
                packets: expected_frames,
            }
        );

        for _ in 0..expected_frames {
            let (header, encrypted) = take_outbound(&mut transport_rx);
            assert_eq!(header.destination_hash, link_id);
            assert_eq!(header.context, rns_wire::context::PacketContext::None);
            let plaintext = receiver.decrypt(&encrypted).unwrap();
            let packet = LxstPacket::decode(&plaintext).unwrap();
            assert_eq!(packet.frames.len(), 1);
            assert_eq!(packet.frames[0].codec, CodecKind::Opus);
            let decoded = decoder.decode_frame(&packet.frames[0]).unwrap();
            assert_eq!(decoded.channels, profile.channels());
            assert_eq!(decoded.sample_frames(), profile.sample_frames_per_packet());
        }
    }

    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusTransmitStreamStopped {
            link_id,
            profile,
            reason: OpusTransmitStreamStopReason::SourceClosed,
        }
    );
    assert!(service.media.opus_transmit_stream.is_none());
    assert_eq!(service.media.opus_encoder_generation, 1);
}

#[tokio::test]
async fn telephony_service_decodes_inbound_opus_frames_with_call_profile() {
    let (sender, receiver) = active_link_pair();
    let link_id = sender.link_id;
    let profile = Profile::QualityMedium;
    let local_identity = Identity::new();
    let remote_identity = identity(0xA8);

    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (link_event_tx, event_rx) = mpsc::channel(4);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );

    let mut core = TelephonyRuntimeCore::new();
    core.start_outgoing_call(link_id, remote_identity, Some(profile))
        .unwrap();
    for status in [
        SignallingStatus::Available,
        SignallingStatus::Ringing,
        SignallingStatus::Connecting,
        SignallingStatus::Established,
    ] {
        core.accept_lxst_plaintext(link_id, &packet([Signal::from(status)]))
            .unwrap();
    }

    let (_control_tx, control_rx) = mpsc::channel(1);
    let (event_tx, mut service_events) = mpsc::channel(8);
    let mut service = TelephonyService::new(endpoint, core, control_rx, event_tx);
    let mut encoder = OpusEncoderState::new(profile).unwrap();

    for expected_generation in [1, 1] {
        let opus_frame = encoder
            .encode_frame(&synthetic_frame_for_profile(profile))
            .unwrap();
        let plaintext = LxstPacket::frame(opus_frame).encode().unwrap();
        let encrypted = receiver.encrypt(&plaintext).unwrap();
        link_event_tx
            .try_send(DestinationEvent::InboundPacket {
                raw: Bytes::from(link_data_packet(
                    link_id,
                    rns_wire::context::PacketContext::None,
                    &encrypted,
                )),
                interface_id: 1,
            })
            .unwrap();

        assert!(service.drive_ready().await);
        assert!(matches!(
            service_events.recv().await.unwrap(),
            TelephonyServiceEvent::Drive(_)
        ));
        assert_eq!(
            service_events.recv().await.unwrap(),
            TelephonyServiceEvent::MediaReceived { link_id, frames: 1 }
        );
        let decoded = service_events.recv().await.unwrap();
        let TelephonyServiceEvent::OpusFramesReceived {
            link_id: decoded_link,
            profile: decoded_profile,
            frames,
        } = decoded
        else {
            panic!("expected decoded Opus frames event, got {decoded:?}");
        };
        assert_eq!(decoded_link, link_id);
        assert_eq!(decoded_profile, profile);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].channels, profile.channels());
        assert_eq!(
            frames[0].sample_frames(),
            profile.sample_frames_per_packet()
        );
        assert!(matches!(
            service_events.recv().await.unwrap(),
            TelephonyServiceEvent::Snapshot(_)
        ));
        assert_eq!(service.media.opus_decoder_generation, expected_generation);
    }
}

#[tokio::test]
async fn telephony_service_delivers_decoded_opus_to_receive_stream() {
    let profile = Profile::LatencyLow;
    let (mut service, receiver, link_id, link_event_tx, mut service_events) =
        established_outgoing_service(profile, 0xAA, 16);
    let (sink_tx, mut sink_rx) = mpsc::channel(4);

    service.start_opus_receive_stream(sink_tx).await.unwrap();
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStarted { link_id, profile }
    );

    let mut encoder = OpusEncoderState::new(profile).unwrap();
    queue_inbound_opus_frame(
        &link_event_tx,
        &receiver,
        link_id,
        &mut encoder,
        synthetic_frame_for_profile(profile),
    );

    assert!(service.drive_ready().await);
    assert!(matches!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::Drive(_)
    ));
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::MediaReceived { link_id, frames: 1 }
    );
    assert!(matches!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusFramesReceived {
            link_id: event_link,
            profile: event_profile,
            frames,
        } if event_link == link_id
            && event_profile == profile
            && frames.len() == 1
    ));
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamFrames {
            link_id,
            profile,
            frames: 1,
            dropped: 0,
        }
    );
    assert!(matches!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::Snapshot(_)
    ));

    let delivered = sink_rx.try_recv().unwrap();
    assert_eq!(delivered.channels, profile.channels());
    assert_eq!(
        delivered.sample_frames(),
        profile.sample_frames_per_packet()
    );

    assert!(
        service
            .stop_opus_receive_stream(OpusReceiveStreamStopReason::Requested)
            .await
    );
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStopped {
            link_id,
            profile,
            reason: OpusReceiveStreamStopReason::Requested,
        }
    );
}

#[tokio::test]
async fn telephony_service_reports_receive_stream_backpressure() {
    let profile = Profile::LatencyLow;
    let (mut service, receiver, link_id, link_event_tx, mut service_events) =
        established_outgoing_service(profile, 0xAB, 16);
    let (sink_tx, mut sink_rx) = mpsc::channel(1);
    let queued = synthetic_frame_for_profile(profile);
    sink_tx.try_send(queued.clone()).unwrap();

    service.start_opus_receive_stream(sink_tx).await.unwrap();
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStarted { link_id, profile }
    );

    let mut encoder = OpusEncoderState::new(profile).unwrap();
    queue_inbound_opus_frame(
        &link_event_tx,
        &receiver,
        link_id,
        &mut encoder,
        synthetic_frame_for_profile(profile),
    );

    assert!(service.drive_ready().await);
    assert!(matches!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::Drive(_)
    ));
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::MediaReceived { link_id, frames: 1 }
    );
    assert!(matches!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusFramesReceived { .. }
    ));
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamFrames {
            link_id,
            profile,
            frames: 0,
            dropped: 1,
        }
    );
    assert!(service.media.opus_receive_stream.is_some());
    assert_eq!(sink_rx.try_recv().unwrap(), queued);
}

#[tokio::test]
async fn telephony_service_stops_receive_stream_when_sink_closes() {
    let profile = Profile::LatencyLow;
    let (mut service, receiver, link_id, link_event_tx, mut service_events) =
        established_outgoing_service(profile, 0xAC, 16);
    let (sink_tx, sink_rx) = mpsc::channel(1);
    drop(sink_rx);

    service.start_opus_receive_stream(sink_tx).await.unwrap();
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStarted { link_id, profile }
    );

    let mut encoder = OpusEncoderState::new(profile).unwrap();
    queue_inbound_opus_frame(
        &link_event_tx,
        &receiver,
        link_id,
        &mut encoder,
        synthetic_frame_for_profile(profile),
    );

    assert!(service.drive_ready().await);
    assert!(matches!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::Drive(_)
    ));
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::MediaReceived { link_id, frames: 1 }
    );
    assert!(matches!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusFramesReceived { .. }
    ));
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStopped {
            link_id,
            profile,
            reason: OpusReceiveStreamStopReason::SinkClosed,
        }
    );
    assert!(service.media.opus_receive_stream.is_none());
}

#[tokio::test]
async fn telephony_service_stops_media_streams_when_call_ends() {
    let profile = Profile::LatencyLow;
    let (mut service, _receiver, link_id, _link_event_tx, mut service_events, _transport_rx) =
        established_outgoing_service_with_transport(profile, 0xAD, 16);
    let (_source_tx, source_rx) = mpsc::channel(1);
    let (sink_tx, _sink_rx) = mpsc::channel(1);

    service.start_opus_stream(profile, source_rx).await.unwrap();
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusTransmitStreamStarted { link_id, profile }
    );
    service.start_opus_receive_stream(sink_tx).await.unwrap();
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStarted { link_id, profile }
    );

    let commands = service.core.hangup_active(false);
    service.control_commands(commands).await.unwrap();
    assert!(matches!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::Drive(_)
    ));
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::CallTerminated {
            link_id,
            reason: None,
        }
    );
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusTransmitStreamStopped {
            link_id,
            profile,
            reason: OpusTransmitStreamStopReason::CallEnded,
        }
    );
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStopped {
            link_id,
            profile,
            reason: OpusReceiveStreamStopReason::CallEnded,
        }
    );
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::Snapshot(TelephonyRuntimeSnapshot {
            external_busy: false,
            pending_link_count: 0,
            active_call: None,
        })
    );
    assert!(service.media.opus_transmit_stream.is_none());
    assert!(service.media.opus_receive_stream.is_none());
}

#[tokio::test]
async fn telephony_service_stops_media_streams_on_remote_link_close() {
    let profile = Profile::LatencyLow;
    let (mut service, _receiver, link_id, link_event_tx, mut service_events, mut transport_rx) =
        established_outgoing_service_with_transport(profile, 0xAF, 16);
    let (_source_tx, source_rx) = mpsc::channel(1);
    let (sink_tx, _sink_rx) = mpsc::channel(1);

    service.start_opus_stream(profile, source_rx).await.unwrap();
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusTransmitStreamStarted { link_id, profile }
    );
    service.start_opus_receive_stream(sink_tx).await.unwrap();
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStarted { link_id, profile }
    );

    link_event_tx
        .try_send(DestinationEvent::LinkClosed { link_id })
        .unwrap();
    assert!(service.drive_ready().await);

    assert_deregistered_link(&mut transport_rx, link_id);
    assert!(matches!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::Drive(_)
    ));
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::CallTerminated {
            link_id,
            reason: None,
        }
    );
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusTransmitStreamStopped {
            link_id,
            profile,
            reason: OpusTransmitStreamStopReason::CallEnded,
        }
    );
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStopped {
            link_id,
            profile,
            reason: OpusReceiveStreamStopReason::CallEnded,
        }
    );
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::Snapshot(TelephonyRuntimeSnapshot {
            external_busy: false,
            pending_link_count: 0,
            active_call: None,
        })
    );
    assert!(service.media.opus_transmit_stream.is_none());
    assert!(service.media.opus_receive_stream.is_none());
}

#[tokio::test]
async fn telephony_service_stops_media_streams_on_established_profile_switch() {
    let old_profile = Profile::LatencyLow;
    let new_profile = Profile::QualityMedium;
    let (mut service, receiver, link_id, link_event_tx, mut service_events, _transport_rx) =
        established_outgoing_service_with_transport(old_profile, 0xB0, 16);
    let (_source_tx, source_rx) = mpsc::channel(1);
    let (sink_tx, _sink_rx) = mpsc::channel(1);

    service
        .start_opus_stream(old_profile, source_rx)
        .await
        .unwrap();
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusTransmitStreamStarted {
            link_id,
            profile: old_profile,
        }
    );
    service.start_opus_receive_stream(sink_tx).await.unwrap();
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStarted {
            link_id,
            profile: old_profile,
        }
    );

    let plaintext = packet([Signal::from(new_profile)]);
    let encrypted = receiver.encrypt(&plaintext).unwrap();
    link_event_tx
        .try_send(DestinationEvent::InboundPacket {
            raw: Bytes::from(link_data_packet(
                link_id,
                rns_wire::context::PacketContext::None,
                &encrypted,
            )),
            interface_id: 1,
        })
        .unwrap();
    assert!(service.drive_ready().await);

    assert!(matches!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::Drive(_)
    ));
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusTransmitStreamStopped {
            link_id,
            profile: old_profile,
            reason: OpusTransmitStreamStopReason::ProfileChanged,
        }
    );
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::OpusReceiveStreamStopped {
            link_id,
            profile: old_profile,
            reason: OpusReceiveStreamStopReason::ProfileChanged,
        }
    );
    assert_eq!(
        service_events.recv().await.unwrap(),
        TelephonyServiceEvent::Snapshot(TelephonyRuntimeSnapshot {
            external_busy: false,
            pending_link_count: 0,
            active_call: Some(ActiveCallSnapshot {
                link_id,
                role: CallRole::Outgoing,
                status: SignallingStatus::Established,
                profile: Some(new_profile),
                remote_identity: identity(0xB0),
                answered: false,
            }),
        })
    );
    assert!(service.media.opus_transmit_stream.is_none());
    assert!(service.media.opus_receive_stream.is_none());
}

#[tokio::test]
async fn telephony_service_soaks_bidirectional_opus_stream_loop() {
    let profile = Profile::LatencyLow;
    let (mut service, receiver, link_id, link_event_tx, mut service_events, mut transport_rx) =
        established_outgoing_service_with_transport(profile, 0xAE, 128);
    service.config.media_frames_per_tick = 5;

    let (source_tx, source_rx) = mpsc::channel(16);
    for _ in 0..12 {
        source_tx
            .try_send(synthetic_frame_for_profile(profile))
            .unwrap();
    }
    drop(source_tx);
    let (sink_tx, mut sink_rx) = mpsc::channel(8);
    service.start_opus_stream(profile, source_rx).await.unwrap();
    service.start_opus_receive_stream(sink_tx).await.unwrap();
    assert_eq!(collect_ready_service_events(&mut service_events).len(), 2);

    let mut remote_encoder = OpusEncoderState::new(profile).unwrap();
    let mut outbound_decoder = OpusDecoderState::new(profile).unwrap();
    let mut media_sent_frames = 0;
    let mut delivered_receive_frames = 0;
    let mut decoded_events = 0;
    let mut transmit_stream_stopped = false;

    for expected_outbound_frames in [5, 5, 2] {
        queue_inbound_opus_frame(
            &link_event_tx,
            &receiver,
            link_id,
            &mut remote_encoder,
            synthetic_frame_for_profile(profile),
        );
        assert!(service.drive_ready().await);
        assert!(service.pump_opus_stream().await);

        for _ in 0..expected_outbound_frames {
            let (header, encrypted) = take_outbound(&mut transport_rx);
            assert_eq!(header.destination_hash, link_id);
            assert_eq!(header.context, rns_wire::context::PacketContext::None);
            let plaintext = receiver.decrypt(&encrypted).unwrap();
            let packet = LxstPacket::decode(&plaintext).unwrap();
            assert_eq!(packet.frames.len(), 1);
            assert_eq!(packet.frames[0].codec, CodecKind::Opus);
            let decoded = outbound_decoder.decode_frame(&packet.frames[0]).unwrap();
            assert_eq!(decoded.channels, profile.channels());
            assert_eq!(decoded.sample_frames(), profile.sample_frames_per_packet());
        }

        for event in collect_ready_service_events(&mut service_events) {
            match event {
                TelephonyServiceEvent::MediaSent { frames, .. } => {
                    media_sent_frames += frames;
                }
                TelephonyServiceEvent::OpusFramesReceived { frames, .. } => {
                    decoded_events += frames.len();
                }
                TelephonyServiceEvent::OpusReceiveStreamFrames {
                    frames, dropped, ..
                } => {
                    delivered_receive_frames += frames;
                    assert_eq!(dropped, 0);
                }
                TelephonyServiceEvent::OpusTransmitStreamStopped {
                    reason: OpusTransmitStreamStopReason::SourceClosed,
                    ..
                } => {
                    transmit_stream_stopped = true;
                }
                _ => {}
            }
        }
    }

    assert_eq!(media_sent_frames, 12);
    assert_eq!(decoded_events, 3);
    assert_eq!(delivered_receive_frames, 3);
    assert!(transmit_stream_stopped);
    for _ in 0..3 {
        let frame = sink_rx.try_recv().unwrap();
        assert_eq!(frame.channels, profile.channels());
        assert_eq!(frame.sample_frames(), profile.sample_frames_per_packet());
    }
    assert!(matches!(
        transport_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn outgoing_link_attempt_promotes_after_proof_and_drives_available_signal() {
    let local_identity = Identity::new();
    let remote_identity = Identity::new();
    let remote_signing_key = remote_identity.get_signing_key().unwrap();
    let remote_public_key = remote_identity.get_public_key();
    let remote_hash = remote_identity.hash;
    let destination_hash = telephony_destination_hash(&remote_hash);

    let (transport_tx, mut transport_rx) = mpsc::channel(8);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let mut core = TelephonyRuntimeCore::new();

    let link_id = endpoint
        .begin_outgoing_link_with_remote_pubkey(
            &mut core,
            remote_hash,
            remote_public_key,
            Some(Profile::LatencyLow),
            1,
        )
        .unwrap();

    let request_path = transport_rx.try_recv().unwrap();
    let TransportMessage::RequestPath {
        destination_hash: requested_hash,
    } = request_path
    else {
        panic!("expected RequestPath, got {request_path:?}");
    };
    assert_eq!(requested_hash, destination_hash);

    let register_link = transport_rx.try_recv().unwrap();
    let TransportMessage::RegisterDestination {
        hash: registered_link_id,
        delivery_tx: Some(link_event_tx),
        ..
    } = register_link
    else {
        panic!("expected link RegisterDestination, got {register_link:?}");
    };
    assert_eq!(registered_link_id, link_id);

    let (request_header, request_data) = take_outbound(&mut transport_rx);
    assert_eq!(request_header.destination_hash, destination_hash);
    assert_eq!(
        request_header.flags.packet_type,
        rns_wire::flags::PacketType::LinkRequest
    );

    let (mut responder, proof_data) =
        Link::new_responder(&request_data, &remote_signing_key, destination_hash, 1).unwrap();
    link_event_tx
        .try_send(DestinationEvent::InboundPacket {
            raw: Bytes::from(link_proof_packet(link_id, &proof_data)),
            interface_id: 1,
        })
        .unwrap();

    let driven = endpoint.try_drive_once(&mut core).unwrap().unwrap();
    assert_eq!(driven.step.commands, Vec::new());
    assert_eq!(driven.effects, Vec::new());
    assert!(endpoint.outgoing_attempts.is_empty());
    assert!(endpoint.outgoing_links.contains_key(&link_id));

    let (rtt_header, rtt_data) = take_outbound(&mut transport_rx);
    assert_eq!(rtt_header.context, rns_wire::context::PacketContext::Lrrtt);
    responder.receive_rtt_packet(&rtt_data).unwrap();

    link_event_tx
        .try_send(DestinationEvent::InboundPacket {
            raw: Bytes::from(link_data_packet(
                link_id,
                rns_wire::context::PacketContext::Keepalive,
                &[rns_link::constants::KEEPALIVE_RESPONSE],
            )),
            interface_id: 1,
        })
        .unwrap();
    assert!(endpoint.try_drive_once(&mut core).unwrap().is_none());

    let available = signalling_packet(Signal::from(SignallingStatus::Available))
        .encode()
        .unwrap();
    let encrypted_available = responder.encrypt(&available).unwrap();
    link_event_tx
        .try_send(DestinationEvent::InboundPacket {
            raw: Bytes::from(link_data_packet(
                link_id,
                rns_wire::context::PacketContext::None,
                &encrypted_available,
            )),
            interface_id: 1,
        })
        .unwrap();

    let driven = endpoint.try_drive_once(&mut core).unwrap().unwrap();
    assert_eq!(
        driven.step.commands,
        vec![TelephonyCommand::IdentifyLocalIdentity { link_id }]
    );
    assert_eq!(
        driven.effects,
        vec![TelephonyCommandEffect::QueuedLinkPacket {
            link_id,
            kind: QueuedLinkPacketKind::LinkIdentify,
        }]
    );

    let (identify_header, identify_data) = take_outbound(&mut transport_rx);
    assert_eq!(
        identify_header.context,
        rns_wire::context::PacketContext::LinkIdentify
    );
    assert_eq!(
        responder.handle_identification(&identify_data).unwrap(),
        local_identity.get_public_key()
    );

    let close_data = responder.teardown(CloseReason::DestinationClosed).unwrap();
    link_event_tx
        .try_send(DestinationEvent::InboundPacket {
            raw: Bytes::from(link_data_packet(
                link_id,
                rns_wire::context::PacketContext::LinkClose,
                &close_data,
            )),
            interface_id: 1,
        })
        .unwrap();

    let driven = endpoint.try_drive_once(&mut core).unwrap().unwrap();
    assert_eq!(
        driven.step.commands,
        vec![
            TelephonyCommand::StopAudioPipelines { link_id },
            TelephonyCommand::CallTerminated {
                link_id,
                reason: None,
            },
        ]
    );
    assert_eq!(driven.effects, vec![TelephonyCommandEffect::Noop; 2]);
    assert!(endpoint.outgoing_links.is_empty());

    let deregister_link = transport_rx.try_recv().unwrap();
    let TransportMessage::DeregisterDestination {
        hash: deregistered_hash,
    } = deregister_link
    else {
        panic!("expected DeregisterDestination, got {deregister_link:?}");
    };
    assert_eq!(deregistered_hash, link_id);
}

#[test]
fn execute_send_signal_queues_lxst_link_packet() {
    let (mut sender, receiver) = active_link_pair();
    let link_id = sender.link_id;
    let identity = Identity::new();
    let signing_key = identity.get_signing_key().unwrap();
    let (tx, mut rx) = mpsc::channel(1);

    assert_eq!(
        execute_command_with_link(
            &tx,
            &identity.get_public_key(),
            &signing_key,
            &mut sender,
            &TelephonyCommand::SendSignal {
                link_id,
                signal: Signal::from(SignallingStatus::Available),
            },
        )
        .unwrap(),
        TelephonyCommandEffect::QueuedLinkPacket {
            link_id,
            kind: QueuedLinkPacketKind::LxstData,
        }
    );

    let (header, encrypted) = take_outbound(&mut rx);
    assert_eq!(header.context, rns_wire::context::PacketContext::None);
    assert_eq!(header.destination_hash, link_id);
    let plaintext = receiver.decrypt(&encrypted).unwrap();
    assert_eq!(
        LxstPacket::decode(&plaintext).unwrap().signals,
        vec![Signal::from(SignallingStatus::Available)]
    );
}

#[test]
fn execute_identify_queues_verifiable_link_identify_packet() {
    let (mut sender, mut receiver) = active_link_pair();
    let link_id = sender.link_id;
    let identity = Identity::new();
    let signing_key = identity.get_signing_key().unwrap();
    let (tx, mut rx) = mpsc::channel(1);

    assert_eq!(
        execute_command_with_link(
            &tx,
            &identity.get_public_key(),
            &signing_key,
            &mut sender,
            &TelephonyCommand::IdentifyLocalIdentity { link_id },
        )
        .unwrap(),
        TelephonyCommandEffect::QueuedLinkPacket {
            link_id,
            kind: QueuedLinkPacketKind::LinkIdentify,
        }
    );

    let (header, encrypted) = take_outbound(&mut rx);
    assert_eq!(
        header.context,
        rns_wire::context::PacketContext::LinkIdentify
    );
    assert_eq!(
        receiver.handle_identification(&encrypted).unwrap(),
        identity.get_public_key()
    );
}

#[test]
fn execute_teardown_queues_link_close_packet() {
    let (mut sender, mut receiver) = active_link_pair();
    let link_id = sender.link_id;
    let identity = Identity::new();
    let signing_key = identity.get_signing_key().unwrap();
    let (tx, mut rx) = mpsc::channel(1);

    assert_eq!(
        execute_command_with_link(
            &tx,
            &identity.get_public_key(),
            &signing_key,
            &mut sender,
            &TelephonyCommand::TeardownLink { link_id },
        )
        .unwrap(),
        TelephonyCommandEffect::QueuedLinkPacket {
            link_id,
            kind: QueuedLinkPacketKind::LinkClose,
        }
    );
    assert_eq!(sender.state, LinkState::Closed);

    let (header, encrypted) = take_outbound(&mut rx);
    assert_eq!(header.context, rns_wire::context::PacketContext::LinkClose);
    assert!(receiver.receive_teardown(&encrypted));
    assert_eq!(receiver.state, LinkState::Closed);
}

#[test]
fn outgoing_link_ignores_unauthenticated_remote_close() {
    let (sender, _receiver) = active_link_pair();
    let link_id = sender.link_id;
    let local_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (event_tx, event_rx) = mpsc::channel(1);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );

    event_tx
        .try_send(DestinationEvent::InboundPacket {
            raw: Bytes::from(link_data_packet(
                link_id,
                rns_wire::context::PacketContext::LinkClose,
                &[0xA5; 16],
            )),
            interface_id: 1,
        })
        .unwrap();

    assert!(endpoint.try_recv_link_event().unwrap().is_none());
    let state = endpoint.outgoing_links.get(&link_id).unwrap();
    assert_eq!(state.link.state, LinkState::Active);
    assert!(matches!(
        transport_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn outgoing_link_closes_on_authenticated_remote_close() {
    let (sender, mut receiver) = active_link_pair();
    let link_id = sender.link_id;
    let local_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (event_tx, event_rx) = mpsc::channel(1);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );
    let close_data = receiver.teardown(CloseReason::DestinationClosed).unwrap();

    event_tx
        .try_send(DestinationEvent::InboundPacket {
            raw: Bytes::from(link_data_packet(
                link_id,
                rns_wire::context::PacketContext::LinkClose,
                &close_data,
            )),
            interface_id: 1,
        })
        .unwrap();

    assert_eq!(
        endpoint.try_recv_link_event().unwrap(),
        Some(TelephonyLinkEvent::LinkClosed { link_id })
    );
    assert!(endpoint.outgoing_links.is_empty());
    assert_deregistered_link(&mut transport_rx, link_id);
}

#[test]
fn outgoing_active_transport_close_deregisters_link_destination() {
    let (sender, _receiver) = active_link_pair();
    let link_id = sender.link_id;
    let local_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (event_tx, event_rx) = mpsc::channel(1);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );

    event_tx
        .try_send(DestinationEvent::LinkClosed { link_id })
        .unwrap();

    assert_eq!(
        endpoint.try_recv_link_event().unwrap(),
        Some(TelephonyLinkEvent::LinkClosed { link_id })
    );
    assert!(endpoint.outgoing_links.is_empty());
    assert_deregistered_link(&mut transport_rx, link_id);
}

#[test]
fn outgoing_attempt_transport_close_deregisters_link_destination() {
    let local_identity = Identity::new();
    let remote_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (event_tx, event_rx) = mpsc::channel(1);
    let (link, _request_data) = Link::new_initiator(link(0xA4), 1);
    let link_id = link.link_id;
    endpoint.outgoing_attempts.insert(
        link_id,
        OutgoingLinkAttempt {
            link,
            remote_public_key: remote_identity.get_public_key(),
            event_rx,
        },
    );

    event_tx
        .try_send(DestinationEvent::LinkClosed { link_id })
        .unwrap();

    assert_eq!(
        endpoint.try_recv_link_event().unwrap(),
        Some(TelephonyLinkEvent::LinkClosed { link_id })
    );
    assert!(endpoint.outgoing_attempts.is_empty());
    assert_deregistered_link(&mut transport_rx, link_id);
}

#[test]
fn outgoing_attempt_ignores_shared_instance_announce_request() {
    let local_identity = Identity::new();
    let remote_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (event_tx, event_rx) = mpsc::channel(1);
    let (link, _request_data) = Link::new_initiator(link(0xA6), 1);
    let link_id = link.link_id;
    endpoint.outgoing_attempts.insert(
        link_id,
        OutgoingLinkAttempt {
            link,
            remote_public_key: remote_identity.get_public_key(),
            event_rx,
        },
    );

    event_tx
        .try_send(DestinationEvent::AnnounceRequested(
            rns_transport::link_messages::AnnounceRequest::normal(
                TELEPHONY_DESTINATION_NAME.to_string(),
            ),
        ))
        .unwrap();

    assert_eq!(endpoint.try_recv_link_event().unwrap(), None);
    assert!(endpoint.outgoing_attempts.contains_key(&link_id));
    assert!(transport_rx.try_recv().is_err());
}

#[test]
fn outgoing_active_ignores_broadcast_delivery_proof() {
    let (sender, _receiver) = active_link_pair();
    let link_id = sender.link_id;
    let local_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (event_tx, event_rx) = mpsc::channel(1);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );

    event_tx
        .try_send(DestinationEvent::DeliveryProof {
            msg_id: "lxmf-proof".to_string(),
            rtt: None,
        })
        .unwrap();

    assert_eq!(endpoint.try_recv_link_event().unwrap(), None);
    assert!(endpoint.outgoing_links.contains_key(&link_id));
    assert!(transport_rx.try_recv().is_err());
}

#[test]
fn outgoing_active_ignores_inbound_link_request_event() {
    let (sender, _receiver) = active_link_pair();
    let link_id = sender.link_id;
    let local_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (event_tx, event_rx) = mpsc::channel(1);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );

    event_tx
        .try_send(DestinationEvent::LinkRequest {
            raw: Bytes::from_static(&[0x01, 0x02, 0x03]),
            interface_id: 1,
        })
        .unwrap();

    assert_eq!(endpoint.try_recv_link_event().unwrap(), None);
    assert!(endpoint.outgoing_links.contains_key(&link_id));
    assert!(transport_rx.try_recv().is_err());
}

#[test]
fn outgoing_active_ignores_packet_for_other_destination() {
    let (sender, _receiver) = active_link_pair();
    let link_id = sender.link_id;
    let local_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (event_tx, event_rx) = mpsc::channel(1);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );

    event_tx
        .try_send(DestinationEvent::InboundPacket {
            raw: Bytes::from(link_data_packet(
                link(0xE7),
                rns_wire::context::PacketContext::None,
                &[0x99],
            )),
            interface_id: 1,
        })
        .unwrap();

    assert_eq!(endpoint.try_recv_link_event().unwrap(), None);
    assert!(endpoint.outgoing_links.contains_key(&link_id));
    assert!(transport_rx.try_recv().is_err());
}

#[test]
fn endpoint_teardown_of_outgoing_link_deregisters_link_destination() {
    let (sender, _receiver) = active_link_pair();
    let link_id = sender.link_id;
    let local_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (_event_tx, event_rx) = mpsc::channel(1);
    endpoint.outgoing_links.insert(
        link_id,
        OutgoingLinkState {
            link: sender,
            event_rx,
        },
    );

    assert_eq!(
        endpoint
            .execute_command(&TelephonyCommand::TeardownLink { link_id })
            .unwrap(),
        TelephonyCommandEffect::QueuedLinkPacket {
            link_id,
            kind: QueuedLinkPacketKind::LinkClose,
        }
    );
    assert!(endpoint.outgoing_links.is_empty());

    let (close_header, close_data) = take_outbound(&mut transport_rx);
    assert_eq!(
        close_header.context,
        rns_wire::context::PacketContext::LinkClose
    );
    assert!(!close_data.is_empty());

    assert_deregistered_link(&mut transport_rx, link_id);
}

#[test]
fn endpoint_teardown_of_outgoing_attempt_deregisters_link_destination() {
    let local_identity = Identity::new();
    let remote_identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(4);
    let mut endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _listener_registration = transport_rx.try_recv().unwrap();
    let (_event_tx, event_rx) = mpsc::channel(1);
    let (link, _request_data) = Link::new_initiator(link(0xA8), 1);
    let link_id = link.link_id;
    endpoint.outgoing_attempts.insert(
        link_id,
        OutgoingLinkAttempt {
            link,
            remote_public_key: remote_identity.get_public_key(),
            event_rx,
        },
    );

    assert_eq!(
        endpoint
            .execute_command(&TelephonyCommand::TeardownLink { link_id })
            .unwrap(),
        TelephonyCommandEffect::Noop
    );
    assert!(endpoint.outgoing_attempts.is_empty());
    assert_deregistered_link(&mut transport_rx, link_id);
}

#[test]
fn rns_endpoint_requests_path_to_outgoing_telephony_destination() {
    let local_identity = Identity::new();
    let remote = identity(0x81);
    let (transport_tx, mut transport_rx) = mpsc::channel(2);
    let endpoint = TelephonyRnsEndpoint::register(transport_tx, &local_identity).unwrap();
    let _registered = transport_rx.try_recv().unwrap();

    endpoint.request_path_to_identity(&remote).unwrap();
    let requested = transport_rx.try_recv().unwrap();
    let TransportMessage::RequestPath { destination_hash } = requested else {
        panic!("expected RequestPath, got {requested:?}");
    };

    assert_eq!(destination_hash, telephony_destination_hash(&remote));
}

#[test]
fn rns_endpoint_deregisters_telephony_destination_on_teardown() {
    let identity = Identity::new();
    let (transport_tx, mut transport_rx) = mpsc::channel(2);
    let endpoint = TelephonyRnsEndpoint::register(transport_tx, &identity).unwrap();
    let _registered = transport_rx.try_recv().unwrap();

    endpoint.deregister_destination().unwrap();
    let deregistered = transport_rx.try_recv().unwrap();
    let TransportMessage::DeregisterDestination { hash } = deregistered else {
        panic!("expected DeregisterDestination, got {deregistered:?}");
    };

    assert_eq!(hash, endpoint.destination_hash);
}

#[test]
fn rns_endpoint_reports_transport_backpressure() {
    let identity = Identity::new();
    let (transport_tx, _transport_rx) = mpsc::channel(1);
    transport_tx
        .try_send(TransportMessage::Shutdown)
        .expect("pre-fill transport queue");

    assert!(matches!(
        TelephonyRnsEndpoint::register(transport_tx, &identity),
        Err(Error::TransportFull)
    ));
}
