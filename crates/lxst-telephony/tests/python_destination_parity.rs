use std::path::{Path, PathBuf};
use std::process::Command;

use lxst_core::TELEPHONY_DESTINATION_NAME;
use lxst_telephony::telephony_destination_hash;
use serde_json::Value;

const SKIP_ENV: &str = "SKIP_PYTHON_LXST_INTEROP";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate is under rsLXST/crates/lxst-telephony")
        .to_path_buf()
}

fn fixture_script() -> PathBuf {
    repo_root().join("tools/fixtures/lxst_destination_fixtures.py")
}

fn should_skip() -> bool {
    std::env::var(SKIP_ENV).map(|v| v == "1").unwrap_or(false)
}

#[test]
fn rust_destination_hash_matches_python_rns_lxst_telephony() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping Python RNS destination parity");
        return;
    }

    let output = Command::new("python3")
        .arg(fixture_script())
        .output()
        .expect("spawn Python destination fixture generator");

    assert!(
        output.status.success(),
        "Python destination fixture generator failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let fixture: Value = serde_json::from_slice(&output.stdout).expect("fixture JSON");
    assert_eq!(
        fixture["expanded_name"].as_str().expect("expanded name"),
        TELEPHONY_DESTINATION_NAME
    );
    assert_eq!(
        fixture["destination_hash"]
            .as_str()
            .expect("destination hash"),
        fixture["hash_from_name"].as_str().expect("hash from name")
    );

    let identity_hash = hex::decode(fixture["identity_hash"].as_str().expect("identity hash"))
        .expect("identity hash hex");
    let identity_hash: [u8; 16] = identity_hash
        .try_into()
        .expect("Python RNS identity hashes are 16 bytes");

    assert_eq!(
        hex::encode(telephony_destination_hash(&identity_hash)),
        fixture["destination_hash"]
            .as_str()
            .expect("destination hash")
    );
}
