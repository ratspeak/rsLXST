use lxst_core::{
    CodecKind, Error, FIELD_FRAMES, FIELD_SIGNALLING, Frame, LxstPacket, RawAudioFrame, RawBitDepth,
};
use proptest::prelude::*;
use rmpv::Value;

fn encode_value(value: Value) -> Vec<u8> {
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, &value).expect("encode msgpack value");
    out
}

#[test]
fn malformed_msgpack_shapes_fail_deterministically() {
    let cases = [
        (
            Value::Array(vec![]),
            Error::RootNotMap,
            "root array is not an LXST packet",
        ),
        (
            Value::Map(vec![(Value::from(-1), Value::from(0))]),
            Error::InvalidFieldKey,
            "negative field key",
        ),
        (
            Value::Map(vec![(Value::from(999), Value::from(0))]),
            Error::InvalidFieldKey,
            "oversized field key",
        ),
        (
            Value::Map(vec![(
                Value::from(FIELD_SIGNALLING as u64),
                Value::String("bad".into()),
            )]),
            Error::InvalidSignal,
            "non-integer signal",
        ),
        (
            Value::Map(vec![(
                Value::from(FIELD_FRAMES as u64),
                Value::Array(vec![Value::from(1)]),
            )]),
            Error::InvalidFieldType {
                field: FIELD_FRAMES,
            },
            "non-bytes frame in list",
        ),
        (
            Value::Map(vec![(
                Value::from(FIELD_FRAMES as u64),
                Value::Binary(vec![]),
            )]),
            Error::EmptyFrame,
            "empty media frame",
        ),
        (
            Value::Map(vec![(
                Value::from(FIELD_FRAMES as u64),
                Value::Binary(vec![0x03]),
            )]),
            Error::UnknownCodec(0x03),
            "unknown media codec",
        ),
        (
            Value::Map(vec![(
                Value::from(FIELD_FRAMES as u64),
                Value::Binary(vec![0xFF]),
            )]),
            Error::NonTransmittableCodec(CodecKind::Null),
            "null media codec is not transmittable",
        ),
    ];

    for (value, expected, name) in cases {
        assert_eq!(
            LxstPacket::decode(&encode_value(value)),
            Err(expected),
            "{name}"
        );
    }
}

#[test]
fn decoder_tolerates_trailing_bytes_like_python_umsgpack() {
    let mut encoded = LxstPacket::frame(Frame::new(CodecKind::Raw, [0x00, 0x00]))
        .encode()
        .unwrap();
    encoded.extend_from_slice(b"trailing");

    let decoded = LxstPacket::decode(&encoded).unwrap();
    assert_eq!(decoded.frames.len(), 1);
}

#[test]
fn raw_payload_errors_are_explicit() {
    assert_eq!(
        RawAudioFrame::from_payload(&[0x40, 0x00]),
        Err(Error::InvalidRawSampleBytes {
            bytes_per_sample: 4,
        })
    );
    assert_eq!(
        RawAudioFrame::new(2, vec![0.0]),
        Err(Error::InvalidRawSampleCount {
            samples: 1,
            channels: 2,
        })
    );
    assert_eq!(
        RawAudioFrame::new(1, vec![0.0])
            .unwrap()
            .to_frame(RawBitDepth::Float16)
            .unwrap()
            .codec,
        CodecKind::Raw
    );
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let result = std::panic::catch_unwind(|| {
            let _ = LxstPacket::decode(&bytes);
        });
        prop_assert!(result.is_ok());
    }
}
