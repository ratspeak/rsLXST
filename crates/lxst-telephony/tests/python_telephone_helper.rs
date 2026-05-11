use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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

fn helper_script() -> PathBuf {
    repo_root().join("tools/interop/lxst_telephone_helper.py")
}

fn temp_storage(name: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{name}-{}-{now}", std::process::id()))
}

fn should_skip() -> bool {
    std::env::var(SKIP_ENV).map(|v| v == "1").unwrap_or(false)
}

#[test]
fn python_telephone_helper_loads_reference_and_creates_destination() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping Python LXST Telephone helper self-test");
        return;
    }

    let storage = temp_storage("rs-lxst-python-telephone-helper");
    let output = Command::new("python3")
        .arg(helper_script())
        .arg("--mode")
        .arg("self-test")
        .arg("--storage-dir")
        .arg(&storage)
        .output()
        .expect("spawn Python LXST Telephone helper");

    let _ = fs::remove_dir_all(&storage);

    assert!(
        output.status.success(),
        "Python LXST Telephone helper failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("helper JSON line"))
        .collect();

    let ready = events
        .iter()
        .find(|event| event["event"] == "READY")
        .expect("READY event");
    assert_eq!(ready["app_name"], "lxst");
    assert_eq!(ready["primitive_name"], "telephony");
    assert_eq!(ready["lxst_version"], "0.4.5");
    assert_eq!(
        ready["lxst_root"].as_str(),
        Some("/Users/Games/Desktop/main/upstream/LXST")
    );
    assert!(ready["codec2_stubbed"].is_boolean());
    assert_eq!(ready["headless_audio"], true);
    assert_eq!(ready["native_filters_disabled"], true);

    let identity_hash = hex::decode(ready["identity_hash"].as_str().expect("identity hash"))
        .expect("identity hash hex");
    let identity_hash: [u8; 16] = identity_hash
        .try_into()
        .expect("Python RNS identity hashes are 16 bytes");
    assert_eq!(
        hex::encode(telephony_destination_hash(&identity_hash)),
        ready["destination_hash"]
            .as_str()
            .expect("destination hash")
    );
    assert_eq!(
        ready["expanded_name"].as_str().expect("expanded name"),
        format!("lxst.telephony.{}", hex::encode(identity_hash))
    );

    let snapshot = events
        .iter()
        .find(|event| event["event"] == "SNAPSHOT")
        .expect("SNAPSHOT event");
    assert_eq!(snapshot["active_call"], Value::Null);
    assert_eq!(snapshot["busy"], false);
    assert_eq!(snapshot["call_status"], 3);
    assert_eq!(snapshot["link_count"], 0);

    assert!(
        events.iter().any(|event| event["event"] == "STOPPED"),
        "helper did not stop cleanly"
    );
}
