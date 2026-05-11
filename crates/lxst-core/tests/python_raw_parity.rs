use std::path::{Path, PathBuf};
use std::process::Command;

use lxst_core::{RawAudioFrame, RawBitDepth};
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
    repo_root().join("tools/fixtures/lxst_raw_fixtures.py")
}

fn should_skip() -> bool {
    std::env::var(SKIP_ENV).map(|v| v == "1").unwrap_or(false)
}

fn python_fixtures() -> Vec<Value> {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping Python LXST Raw parity");
        return Vec::new();
    }

    let output = Command::new("python3")
        .arg(fixture_script())
        .output()
        .expect("spawn Python Raw fixture generator");

    assert!(
        output.status.success(),
        "Python Raw fixture generator failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).expect("fixture JSON")
}

fn decode_with_python(payload_hex: &str) -> Value {
    let output = Command::new("python3")
        .arg(fixture_script())
        .arg("--decode-hex")
        .arg(payload_hex)
        .output()
        .expect("spawn Python Raw fixture decoder");

    assert!(
        output.status.success(),
        "Python Raw fixture decoder failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).expect("decode JSON")
}

fn bit_depth(value: &Value) -> RawBitDepth {
    match value["bitdepth_header"].as_u64().expect("bitdepth header") {
        0 => RawBitDepth::Float16,
        1 => RawBitDepth::Float32,
        2 => RawBitDepth::Float64,
        3 => RawBitDepth::Float128,
        other => panic!("unknown bitdepth header {other}"),
    }
}

fn samples(value: &Value) -> Vec<f32> {
    value["samples"]
        .as_array()
        .expect("samples")
        .iter()
        .map(|sample| sample.as_f64().expect("sample") as f32)
        .collect()
}

fn assert_samples_close(actual: &[f32], expected: &[f32], name: &str) {
    assert_eq!(actual.len(), expected.len(), "{name}");
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        let delta = (actual - expected).abs();
        assert!(
            delta <= 0.000_976_562_5,
            "{name} sample {index}: actual {actual} expected {expected} delta {delta}",
        );
    }
}

#[test]
fn rust_decodes_python_raw_payloads() {
    for fixture in python_fixtures() {
        let name = fixture["name"].as_str().expect("fixture name");
        let payload_hex = fixture["payload_hex"].as_str().expect("payload hex");
        let payload = hex::decode(payload_hex).expect("payload hex decodes");
        let raw = RawAudioFrame::from_payload(&payload)
            .unwrap_or_else(|e| panic!("failed to decode fixture {name}: {e}"));
        let depth = bit_depth(&fixture);
        let expected_samples = samples(&fixture);

        assert_eq!(
            raw.channels,
            fixture["channels"].as_u64().expect("channels") as u8,
            "{name}",
        );
        assert_eq!(
            raw.sample_frames(),
            fixture["sample_frames"].as_u64().expect("sample frames") as usize,
            "{name}",
        );
        assert_samples_close(&raw.samples, &expected_samples, name);
        assert_eq!(
            hex::encode(raw.to_payload(depth).unwrap()),
            payload_hex,
            "{name}"
        );
    }
}

#[test]
fn python_decodes_rust_raw_payloads() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping Python LXST Raw parity");
        return;
    }

    let cases = [
        (
            RawAudioFrame::new(2, vec![0.0, 0.5, -0.25, 1.0]).unwrap(),
            RawBitDepth::Float16,
        ),
        (
            RawAudioFrame::new(1, vec![0.25, -1.5, 2.0]).unwrap(),
            RawBitDepth::Float32,
        ),
        (
            RawAudioFrame::new(3, vec![0.0, 0.125, -0.5, 1.0, -1.0, 0.25]).unwrap(),
            RawBitDepth::Float64,
        ),
    ];

    for (raw, depth) in cases {
        let payload_hex = hex::encode(raw.to_payload(depth).expect("encode Raw payload"));
        let decoded = decode_with_python(&payload_hex);

        assert_eq!(
            decoded["channels"].as_u64().expect("channels") as u8,
            raw.channels
        );
        assert_eq!(
            decoded["sample_frames"].as_u64().expect("sample frames") as usize,
            raw.sample_frames(),
        );
        assert_eq!(bit_depth(&decoded), depth);
        assert_samples_close(&samples(&decoded), &raw.samples, &payload_hex);
    }
}
