use std::path::{Path, PathBuf};
use std::process::Command;

use lxst_core::{CodecKind, Frame, LxstPacket, Profile, Signal, SignallingStatus};
use serde_json::Value;

const SKIP_ENV: &str = "SKIP_PYTHON_LXST_INTEROP";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate is under rsLXST/crates/lxst-core")
        .to_path_buf()
}

fn fixture_script() -> PathBuf {
    repo_root().join("tools/fixtures/lxst_wire_fixtures.py")
}

fn should_skip() -> bool {
    std::env::var(SKIP_ENV).map(|v| v == "1").unwrap_or(false)
}

fn python_fixtures() -> Vec<Value> {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping Python LXST wire parity");
        return Vec::new();
    }

    let output = Command::new("python3")
        .arg(fixture_script())
        .output()
        .expect("spawn Python fixture generator");

    assert!(
        output.status.success(),
        "Python fixture generator failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).expect("fixture JSON")
}

fn decode_with_python(packet_hex: &str) -> Value {
    let output = Command::new("python3")
        .arg(fixture_script())
        .arg("--decode-hex")
        .arg(packet_hex)
        .output()
        .expect("spawn Python fixture decoder");

    assert!(
        output.status.success(),
        "Python fixture decoder failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).expect("decode JSON")
}

fn signal_values(packet: &LxstPacket) -> Vec<u32> {
    packet.signals.iter().map(|s| s.wire_value()).collect()
}

#[test]
fn rust_decodes_python_lxst_packets() {
    for fixture in python_fixtures() {
        let name = fixture["name"].as_str().expect("fixture name");
        let packet_hex = fixture["packet_hex"].as_str().expect("packet hex");
        let packet_bytes = hex::decode(packet_hex).expect("packet hex decodes");
        let packet = LxstPacket::decode(&packet_bytes)
            .unwrap_or_else(|e| panic!("failed to decode fixture {name}: {e}"));

        let expected_signals: Vec<u32> = fixture["signals"]
            .as_array()
            .expect("signals array")
            .iter()
            .map(|v| v.as_u64().expect("signal int") as u32)
            .collect();
        assert_eq!(signal_values(&packet), expected_signals, "{name}");

        let expected_frames = fixture["frames"].as_array().expect("frames array");
        assert_eq!(packet.frames.len(), expected_frames.len(), "{name}");

        for (frame, expected) in packet.frames.iter().zip(expected_frames) {
            let codec = expected["codec"].as_u64().expect("codec") as u8;
            assert_eq!(frame.codec.wire_id(), codec, "{name}");
            let payload = expected["payload_hex"].as_str().expect("payload hex");
            assert_eq!(hex::encode(&frame.payload), payload, "{name}");
        }

        if fixture["canonical"].as_bool().unwrap_or(false) {
            assert_eq!(hex::encode(packet.encode().unwrap()), packet_hex, "{name}");
        }
    }
}

#[test]
fn python_decodes_rust_lxst_packets() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping Python LXST wire parity");
        return;
    }

    let packets = [
        LxstPacket::signalling([
            Signal::from(SignallingStatus::Available),
            Signal::from(Profile::QualityMedium),
        ]),
        LxstPacket::frame(Frame::new(CodecKind::Raw, [0x41, 0x01, 0x02, 0x03, 0x04])),
        LxstPacket {
            signals: vec![
                Signal::from(SignallingStatus::Established),
                Signal::from(Profile::LatencyUltraLow),
            ],
            frames: vec![
                Frame::new(CodecKind::Raw, [0x41, 0x01, 0x02, 0x03, 0x04]),
                Frame::new(CodecKind::Codec2, [0x04, 0xAA, 0xBB]),
            ],
        },
    ];

    for packet in packets {
        let packet_hex = hex::encode(packet.encode().expect("encode packet"));
        let decoded = decode_with_python(&packet_hex);
        let signals: Vec<u32> = decoded["signals"]
            .as_array()
            .expect("signals")
            .iter()
            .map(|v| v.as_u64().expect("signal") as u32)
            .collect();
        assert_eq!(signals, signal_values(&packet));

        let frames = decoded["frames"].as_array().expect("frames");
        assert_eq!(frames.len(), packet.frames.len());
        for (frame, expected) in packet.frames.iter().zip(frames) {
            assert_eq!(
                expected["codec"].as_u64().expect("codec") as u8,
                frame.codec.wire_id()
            );
            assert_eq!(
                expected["payload_hex"].as_str().expect("payload"),
                hex::encode(&frame.payload)
            );
        }
    }
}
