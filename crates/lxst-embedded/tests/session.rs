use lxst_embedded::{
    CallMode, CallRole, CallState, EndReason, Event, Profile, ProfileSet, Session, SessionConfig,
    Signal, SignallingStatus as Status, TelephonyAction as Action,
};

#[test]
fn crossed_profile_offers_converge_before_media_without_an_echo_loop() {
    for (preferred, offered) in [
        (Profile::BandwidthLow, Profile::BandwidthVeryLow),
        (Profile::BandwidthLow, Profile::BandwidthUltraLow),
        (Profile::BandwidthUltraLow, Profile::BandwidthVeryLow),
        (Profile::BandwidthVeryLow, Profile::BandwidthUltraLow),
        (Profile::BandwidthVeryLow, Profile::BandwidthLow),
    ] {
        let mut cfg = config();
        cfg.preferred = preferred;
        let mut receiver = Session::new(CallRole::Incoming, cfg, 0).unwrap();
        let mut caller = CallState::outgoing(Some(offered));
        receiver.link_established();
        caller.receive_signal(Status::Available.into());
        let mut in_flight = Vec::new();
        // Delay the caller's response until it has received both RINGING and
        // the receiver preference, reproducing the actual TCP interop race.
        for event in receiver.peer_verified(1).as_slice() {
            if let Event::Transition(Action::SendSignal(signal)) = event {
                for action in caller.receive_signal(*signal).as_slice() {
                    match action {
                        Action::SendSignal(signal) => in_flight.push(*signal),
                        Action::SendSignalPair(first, second) => {
                            in_flight.push(*first);
                            in_flight.push(*second);
                        }
                        _ => {}
                    }
                }
            }
        }
        assert_eq!(caller.profile(), Some(preferred));
        assert_eq!(
            in_flight,
            vec![Signal::from(offered), Signal::from(CallMode::FullDuplex)]
        );
        for signal in in_flight {
            for event in receiver.receive_signal(signal, 2).as_slice() {
                if let Event::Transition(Action::SendSignal(signal)) = event {
                    caller.receive_signal(*signal);
                }
            }
        }
        assert_eq!(receiver.profile(), Some(offered));
        assert_eq!(caller.profile(), receiver.profile());
        assert!(!receiver.media_ready());
        assert!(
            receiver
                .receive_signal(offered.into(), 3)
                .as_slice()
                .is_empty()
        );
        let answer = receiver.answer(4);
        receiver.audio_ready(receiver.audio_generation(), true);
        for event in answer.as_slice() {
            if let Event::Transition(Action::SendSignal(signal)) = event {
                for action in caller.receive_signal(*signal).as_slice() {
                    if let Action::SendSignal(signal) = action {
                        receiver.receive_signal(*signal, 5);
                    }
                }
            }
        }
        assert_eq!(caller.status(), Status::Established);
        assert_eq!(caller.profile(), receiver.profile());
        assert!(receiver.media_ready());
        assert!(!receiver.transmitting());
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        allowed: ProfileSet::CODEC2,
        preferred: Profile::BandwidthVeryLow,
        capture: true,
        playback: true,
    }
}
fn connect(role: CallRole) -> Session {
    let mut session = Session::new(role, config(), 0).unwrap();
    session.link_established();
    session.peer_verified(1);
    match role {
        CallRole::Incoming => {
            session.answer(5);
        }
        CallRole::Outgoing => {
            for status in [Status::Available, Status::Ringing, Status::Connecting] {
                session.receive_signal(status.into(), 5);
            }
        }
    }
    session.audio_ready(session.audio_generation(), true);
    session.receive_signal(Status::Established.into(), 6);
    assert!(session.media_ready());
    session
}
#[test]
fn media_requires_identity_acceptance_correct_order_readiness_and_fresh_press() {
    for role in [CallRole::Incoming, CallRole::Outgoing] {
        let mut session = Session::new(role, config(), 0).unwrap();
        session.set_transmitting(true, 0);
        session.audio_ready(session.audio_generation(), true);
        for status in [
            Status::Available,
            Status::Ringing,
            Status::Connecting,
            Status::Established,
        ] {
            session.receive_signal(status.into(), 1);
        }
        assert!(!session.media_ready());
        session.link_established();
        session.receive_signal(Status::Established.into(), 2);
        assert!(!session.media_ready());
        session.peer_verified(3);
        session.receive_signal(Status::Established.into(), 4);
        assert!(!session.media_ready());
        if role == CallRole::Incoming {
            session.answer(5);
        } else {
            for status in [Status::Available, Status::Ringing, Status::Connecting] {
                session.receive_signal(status.into(), 5);
            }
        }
        session.receive_signal(Status::Established.into(), 6);
        assert!(session.media_ready());
        assert!(!session.transmitting());
        // A press held across establishment is not a new capture request.
        assert!(session.set_transmitting(true, 7).as_slice().is_empty());
        session.set_transmitting(false, 8);
        assert_eq!(
            session.set_transmitting(true, 9).as_slice(),
            &[Event::FlushMedia, Event::Capture(true)]
        );
        assert!(!session.playback_allowed());
        assert_eq!(
            session.set_transmitting(false, 10).as_slice(),
            &[Event::Capture(false), Event::FlushMedia]
        );
        assert!(session.playback_allowed());
    }
}
#[test]
fn timeout_teardown_and_stale_setup_never_restart_capture() {
    let mut session = connect(CallRole::Outgoing);
    session.set_transmitting(true, 100);
    assert_eq!(
        session.tick(30100).as_slice(),
        &[Event::Capture(false), Event::FlushMedia]
    );
    assert!(session.set_transmitting(true, 30101).as_slice().is_empty());
    session.set_transmitting(false, 30102);
    session.set_transmitting(true, 30103);
    let old = session.audio_generation();
    let events = session.receive_signal(Profile::BandwidthLow.into(), 30104);
    assert_eq!(
        &events.as_slice()[..2],
        &[Event::Capture(false), Event::FlushMedia]
    );
    assert!(!session.media_ready());
    session.audio_ready(old, true);
    assert!(!session.media_ready());
    session.audio_ready(session.audio_generation(), true);
    assert!(session.media_ready());
    assert!(session.set_transmitting(true, 30105).as_slice().is_empty());
    session.set_transmitting(false, 30106);
    session.set_transmitting(true, 30107);
    let events = session.end(EndReason::RouteLost);
    assert_eq!(events.as_slice()[0], Event::Capture(false));
    assert_eq!(
        events.as_slice().last(),
        Some(&Event::Ended(EndReason::RouteLost))
    );
    assert!(session.set_transmitting(true, 30108).as_slice().is_empty());
    assert!(
        session
            .audio_ready(session.audio_generation(), true)
            .as_slice()
            .is_empty()
    );
    assert!(!session.media_ready());
}
#[test]
fn unsupported_profile_has_one_bounded_offer_then_closes() {
    let mut session = connect(CallRole::Outgoing);
    session.set_transmitting(true, 10);
    let events = session.receive_signal(Profile::QualityHigh.into(), 11);
    assert_eq!(
        events.as_slice(),
        &[
            Event::Capture(false),
            Event::FlushMedia,
            Event::Transition(Action::SendSignal(Profile::BandwidthVeryLow.into())),
        ]
    );
    assert!(!session.media_ready());
    assert!(session.tick(5010).as_slice().is_empty());
    session.tick(5011);
    assert_eq!(session.ended(), Some(EndReason::ProfileUnsupported));
    let mut session = connect(CallRole::Incoming);
    session.receive_signal(Profile::QualityHigh.into(), 20);
    session.receive_signal(Profile::BandwidthVeryLow.into(), 21);
    assert!(session.media_ready());
    let events = session.receive_signal(Profile::QualityHigh.into(), 22);
    assert_eq!(session.ended(), None);
    assert!(
        events
            .as_slice()
            .contains(&Event::Transition(Action::SendSignal(
                Profile::BandwidthVeryLow.into()
            )))
    );
    session.receive_signal(Profile::QualityMax.into(), 23);
    assert_eq!(session.ended(), Some(EndReason::ProfileUnsupported));
}
#[test]
fn answer_establishes_without_retries_and_one_direction_devices_work() {
    let mut session = Session::new(CallRole::Incoming, config(), 0).unwrap();
    session.link_established();
    session.peer_verified(1);
    let answer = session.answer(2);
    let sent = answer
        .as_slice()
        .iter()
        .filter(|event| matches!(event, Event::Transition(Action::SendSignal(_))))
        .count();
    assert_eq!(sent, 2);
    assert_eq!(session.status(), Status::Established);
    for n in 1..=20 {
        assert!(session.tick(2 + n * 1000).as_slice().is_empty());
    }
    assert_eq!(session.ended(), None);
    for (capture, playback) in [(true, false), (false, true)] {
        let mut cfg = config();
        cfg.capture = capture;
        cfg.playback = playback;
        let mut session = Session::new(CallRole::Incoming, cfg, 0).unwrap();
        session.link_established();
        session.peer_verified(1);
        session.answer(2);
        session.audio_ready(session.audio_generation(), true);
        session.receive_signal(Status::Established.into(), 3);
        assert_eq!(session.playback_allowed(), playback);
        session.set_transmitting(true, 4);
        assert_eq!(session.transmitting(), capture);
    }
}
#[test]
fn adversarial_event_sequences_remain_bounded() {
    let mut rng = 401_u32;
    for role in [CallRole::Incoming, CallRole::Outgoing] {
        for _ in 0..1000 {
            let mut session = Session::new(role, config(), 0).unwrap();
            for now in 0..100 {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                match rng % 10 {
                    0 => {
                        session.link_established();
                    }
                    1 => {
                        session.peer_verified(now);
                    }
                    2 => {
                        session.answer(now);
                    }
                    3 => {
                        session.audio_ready(session.audio_generation(), true);
                    }
                    4 => {
                        session.set_transmitting(true, now);
                    }
                    5 => {
                        session.set_transmitting(false, now);
                    }
                    6 => {
                        session.tick(now);
                    }
                    _ => {
                        session.receive_signal(Signal::from_wire((rng >> 16) % 512), now);
                    }
                }
                if session.transmitting() {
                    assert!(session.media_ready());
                }
            }
        }
    }
}

#[test]
fn authenticated_matching_media_confirms_standard_profile_fallback_without_echo() {
    let mut session = connect(CallRole::Outgoing);
    session.receive_signal(Profile::QualityMedium.into(), 100);
    assert!(!session.media_ready());
    session.receive_profile_media(Profile::BandwidthLow, 200);
    assert!(!session.media_ready());
    session.receive_profile_media(Profile::BandwidthVeryLow, 201);
    assert!(session.media_ready());
    assert!(!session.transmitting());
    session.tick(6000);
    assert_eq!(session.ended(), None);

    let mut late = connect(CallRole::Outgoing);
    late.receive_signal(Profile::QualityMedium.into(), 100);
    late.receive_profile_media(Profile::BandwidthVeryLow, 5100);
    late.tick(5100);
    assert_eq!(late.ended(), Some(EndReason::ProfileUnsupported));

    let mut ringing = Session::new(CallRole::Incoming, config(), 0).unwrap();
    ringing.link_established();
    ringing.peer_verified(1);
    ringing.receive_signal(Profile::QualityMedium.into(), 100);
    ringing.receive_profile_media(Profile::BandwidthVeryLow, 200);
    ringing.tick(5100);
    assert_eq!(ringing.ended(), Some(EndReason::ProfileUnsupported));
}

#[test]
fn incoming_ring_times_out_after_python_ring_time() {
    let mut session = Session::new(CallRole::Incoming, config(), 0).unwrap();
    session.link_established();
    session.peer_verified(0);
    session.tick(59_999);
    assert_eq!(session.ended(), None);
    session.tick(60_000);
    assert_eq!(session.ended(), Some(EndReason::Timeout));
}

#[test]
fn outgoing_call_ends_when_callee_does_not_ring_within_connect_time() {
    let mut session = Session::new(CallRole::Outgoing, config(), 0).unwrap();
    session.link_established();
    session.peer_verified(1);
    session.receive_signal(Status::Available.into(), 100);
    session.tick(4_999);
    assert_eq!(session.ended(), None);
    session.tick(5_000);
    assert_eq!(session.ended(), Some(EndReason::Timeout));
}

#[test]
fn outgoing_wait_time_runs_from_call_start_once_ringing() {
    let mut session = Session::new(CallRole::Outgoing, config(), 0).unwrap();
    session.link_established();
    session.peer_verified(1);
    session.receive_signal(Status::Available.into(), 100);
    session.receive_signal(Status::Ringing.into(), 4_000);
    session.tick(69_999);
    assert_eq!(session.ended(), None);
    session.tick(70_000);
    assert_eq!(session.ended(), Some(EndReason::Timeout));
}

#[test]
fn preferred_mode_signal_is_recorded_by_the_session() {
    let mut session = Session::new(CallRole::Incoming, config(), 0).unwrap();
    session.link_established();
    session.peer_verified(1);
    session.receive_signal(Signal::from_wire(0xF2), 2);
    assert_eq!(session.mode(), Some(CallMode::HalfDuplex));
    assert_eq!(session.ended(), None);
}
