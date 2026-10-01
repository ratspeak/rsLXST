use lxst_embedded::wire::{self, Error, Frame, Packet};
use lxst_embedded::{CodecKind, Profile, Signal};

#[test]
fn bounded_wire_matches_full_rust_canonical_packets() {
    let signal_values = [0, 6, 127, 128, 255, 256, 65535, u32::MAX];
    for count in 0..=8 {
        let signals: Vec<_> = signal_values[..count]
            .iter()
            .map(|v| Signal::from_wire(*v))
            .collect();
        for lengths in [vec![], vec![0], vec![64], vec![255], vec![0, 64, 80, 120]] {
            let payloads: Vec<_> = lengths.iter().map(|n| vec![0x5a; *n]).collect();
            let frames: Vec<_> = payloads
                .iter()
                .map(|p| Frame {
                    codec: CodecKind::Codec2,
                    payload: p,
                })
                .collect();
            let host = lxst_core::LxstPacket {
                signals: signals
                    .iter()
                    .map(|s| lxst_core::Signal::from_wire(s.wire_value()))
                    .collect(),
                frames: payloads
                    .iter()
                    .map(|p| lxst_core::Frame::new(lxst_core::CodecKind::Codec2, p.clone()))
                    .collect(),
            };
            let expected = host.encode().unwrap();
            let mut encoded = [0xa5; 512];
            let size = wire::encode(&signals, &frames, &mut encoded).unwrap();
            assert_eq!(encoded[..size], expected);
            assert!(encoded[size..].iter().all(|v| *v == 0xa5));
            let decoded = Packet::decode(&encoded[..size]).unwrap();
            assert_eq!(decoded.signals(), signals);
            assert_eq!(decoded.frames(), frames);
            for end in 0..size {
                assert!(Packet::decode(&encoded[..end]).is_err());
            }
            let mut too_small = vec![0xa5; size - 1];
            assert_eq!(
                wire::encode(&signals, &frames, &mut too_small),
                Err(Error::OutputCapacity)
            );
            assert!(too_small.iter().all(|v| *v == 0xa5));
        }
    }
}

#[test]
fn equivalent_integer_and_container_forms_remain_compatible() {
    for bytes in [
        vec![0x81, 0, 4],
        vec![0xde, 0, 1, 0xcc, 0, 0xdc, 0, 1, 0xd1, 0, 4],
        vec![
            0xdf, 0, 0, 0, 1, 0, 0xdd, 0, 0, 0, 1, 0xd3, 0, 0, 0, 0, 0, 0, 0, 4,
        ],
    ] {
        let packet = Packet::decode(&bytes).unwrap();
        assert_eq!(packet.signals()[0].wire_value(), 4);
        assert_eq!(
            lxst_core::LxstPacket::decode(&bytes).unwrap().signals[0].wire_value(),
            4
        );
    }
    for bytes in [
        vec![0x81, 1, 0xc5, 0, 2, 2, 4],
        vec![0x81, 1, 0xc6, 0, 0, 0, 2, 2, 4],
    ] {
        assert_eq!(Packet::decode(&bytes).unwrap().frames()[0].payload, &[4]);
    }
    // Unknown metadata is skipped, bounded by packet bytes and nesting depth.
    let bytes = [0x82, 8, 0x92, 0xc0, 0x81, 0xa1, b'a', 0xc7, 1, 3, 8, 0, 4];
    assert_eq!(Packet::decode(&bytes).unwrap().signals()[0].wire_value(), 4);
}

#[test]
fn hostile_shapes_are_rejected_without_unbounded_work() {
    for bytes in [
        vec![0x81, 0, 0xff],
        vec![0x81, 0, 0xd0, 0xff],
        vec![0x81, 0, 0xcf, 1, 0, 0, 0, 0, 0, 0, 0],
        vec![0x81, 0, 0xdd, 255, 255, 255, 255],
        vec![0x81, 1, 0xc6, 255, 255, 255, 255],
        vec![0x81, 1, 0xc4, 0],
        vec![0x81, 1, 0xc4, 1, 255],
        vec![0x82, 0, 4, 0, 4],
        vec![0x81, 0, 4, 0],
        vec![0x81, 1, 0x95],
        vec![0x81, 0, 0x99],
        vec![
            0x81, 8, 0x91, 0x91, 0x91, 0x91, 0x91, 0x91, 0x91, 0x91, 0x91, 0,
        ],
    ] {
        assert!(Packet::decode(&bytes).is_err(), "accepted {bytes:x?}");
    }
    let mut output = [0xa5; 512];
    let frames = [Frame {
        codec: CodecKind::Null,
        payload: &[],
    }];
    assert_eq!(
        wire::encode(&[], &frames, &mut output),
        Err(Error::NonTransmittableCodec(CodecKind::Null))
    );
    assert_eq!(output, [0xa5; 512]);
    let mut random = 123_u32;
    let mut bytes = [0; 520];
    for iteration in 0..100000 {
        let length = iteration % bytes.len();
        for byte in &mut bytes[..length] {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            *byte = (random >> 24) as u8;
        }
        let _ = Packet::decode(&bytes[..length]);
    }
}

#[test]
fn shared_transitions_retain_full_rust_actions() {
    use lxst_embedded::{CallState, SignallingStatus};
    let mut embedded = CallState::outgoing(Some(Profile::BandwidthVeryLow));
    let mut host = lxst_core::TelephonyCall::outgoing(Some(lxst_core::Profile::BandwidthVeryLow));
    for signal in [3, 4, 5, 6, 5, 6, 0x11f, 0x12f, 0, 1, u32::MAX] {
        let a = embedded.receive_signal(Signal::from_wire(signal));
        let b = host.receive_signal(lxst_core::Signal::from_wire(signal));
        assert_eq!(format!("{:?}", a.as_slice()), format!("{b:?}"));
        assert_eq!(embedded.status().wire_value(), host.status().wire_value());
    }
    // Explore combinations including adversarial order to prove the shared
    // fixed action bound; session policy separately restricts allowed ordering.
    for first in 0..512 {
        for second in [0, 3, 4, 5, 6, 0x11f, 0x12f, 0x13f] {
            let mut call = CallState::incoming();
            call.caller_identified(false, true);
            call.receive_signal(Signal::from_wire(first));
            call.answer();
            call.receive_signal(Signal::from_wire(second));
            call.retry_answer();
            call.hangup(false);
            assert_eq!(call.status(), SignallingStatus::Available);
        }
    }
}
