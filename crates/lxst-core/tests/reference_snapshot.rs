use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate is under rsLXST/crates/lxst-core")
        .to_path_buf()
}

fn python_interpreter() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| {
        if cfg!(windows) {
            "python".to_string()
        } else {
            "python3".to_string()
        }
    })
}

#[test]
fn python_lxst_reference_snapshot_is_available() {
    let script = repo_root().join("tools/reference/lxst_reference_snapshot.py");
    let output = Command::new(python_interpreter())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(script)
        .output()
        .expect("spawn Python LXST reference snapshot");

    assert!(
        output.status.success(),
        "Python LXST reference snapshot failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let snapshot: Value = serde_json::from_slice(&output.stdout).expect("snapshot JSON");
    let lock_path = repo_root().join("tools/reference/lxst_reference_lock.json");
    let locked: Value =
        serde_json::from_slice(&fs::read(lock_path).expect("read LXST reference lock"))
            .expect("reference lock JSON");

    assert_eq!(
        snapshot["remote"].as_str(),
        Some("https://github.com/markqvist/LXST.git")
    );
    assert_eq!(snapshot["dirty"].as_bool(), Some(false));
    assert_eq!(snapshot["missing_files"].as_array().unwrap().len(), 0);
    assert_eq!(
        snapshot["remote"], locked["remote"],
        "LXST upstream remote changed"
    );
    assert_eq!(
        snapshot["commit"], locked["commit"],
        "LXST source-of-truth commit changed; review upstream diff and update tools/reference/lxst_reference_lock.json intentionally"
    );
    assert_eq!(
        snapshot["package_version"], locked["package_version"],
        "LXST package version changed"
    );

    let files = snapshot["files"].as_object().expect("files object");
    let locked_files = locked["files"].as_object().expect("locked files object");
    for rel in [
        "LXST/_version.py",
        "LXST/Network.py",
        "LXST/Primitives/Telephony.py",
        "LXST/Codecs/__init__.py",
        "LXST/Codecs/Raw.py",
        "LXST/Codecs/Opus.py",
        "LXST/Codecs/Codec2.py",
    ] {
        let entry = files.get(rel).unwrap_or_else(|| panic!("missing {rel}"));
        let locked_entry = locked_files
            .get(rel)
            .unwrap_or_else(|| panic!("missing locked {rel}"));
        let sha = entry["sha256"].as_str().expect("sha256");
        assert_eq!(sha.len(), 64, "{rel}");
        assert!(entry["bytes"].as_u64().unwrap_or(0) > 0, "{rel}");
        assert_eq!(entry, locked_entry, "LXST reference file changed: {rel}");
    }
}
