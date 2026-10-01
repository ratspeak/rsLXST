use lxst_embedded::{
    CallRole, EndReason, Event, Profile, ProfileSet, Session, SessionConfig, Signal,
    SignallingStatus as Status, TelephonyAction as Action,
};

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
    session.receive_signal(Profile::QualityHigh.into(), 22);
    assert_eq!(session.ended(), Some(EndReason::ProfileUnsupported));
}
#[test]
fn retries_are_bounded_and_one_direction_devices_work() {
    let mut session = Session::new(CallRole::Incoming, config(), 0).unwrap();
    session.link_established();
    session.peer_verified(1);
    session.answer(2);
    for n in 1..=3 {
        let events = session.tick(2 + n * 1000);
        assert_eq!(events.as_slice().len(), 2);
        assert_eq!(
            events.as_slice()[1],
            Event::Transition(Action::SendSignal(Status::Established.into()))
        );
    }
    assert!(session.tick(4002).as_slice().is_empty());
    session.tick(10002);
    assert_eq!(session.ended(), Some(EndReason::Timeout));
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
