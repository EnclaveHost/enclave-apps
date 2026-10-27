#![cfg(unix)]
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "snapshot-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn run(dir: &Temp, mode: &str, input: &str, output: &str, owner: &str) -> bool {
    let config = serde_json::json!({"snapshotEncryption":{"masterKey":"$TEST_SNAPSHOT_SECRET","owner":owner,"deployment":"dep"}});
    let mut child = Command::new(env!("CARGO_BIN_EXE_snapshot-crypto"))
        .args([mode, "risc-box", "bucket", "private/session", input, output])
        .current_dir(&dir.0)
        .env("TEST_SNAPSHOT_SECRET", "ab".repeat(32))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(config.to_string().as_bytes())
        .unwrap();
    child.wait().unwrap().success()
}
#[test]
fn offline_migration_preserves_source_and_never_overwrites() {
    let d = Temp::new();
    let plain = b"private VM snapshot";
    fs::write(d.0.join("legacy"), plain).unwrap();
    assert!(run(&d, "seal", "legacy", "encrypted", "alice"));
    assert!(fs::read(d.0.join("encrypted"))
        .unwrap()
        .starts_with(b"EVMSNAP1"));
    assert!(run(&d, "open", "encrypted", "restored", "alice"));
    assert_eq!(fs::read(d.0.join("restored")).unwrap(), plain);
    assert_eq!(
        fs::metadata(d.0.join("restored"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(fs::read(d.0.join("legacy")).unwrap(), plain);
    assert!(!run(&d, "seal", "legacy", "restored", "alice"));
    assert_eq!(fs::read(d.0.join("restored")).unwrap(), plain);
}
#[test]
fn failed_restore_never_publishes_and_cleans_partial_plaintext() {
    let d = Temp::new();
    fs::write(d.0.join("legacy"), vec![123; 1024 * 1024 + 19]).unwrap();
    assert!(run(&d, "seal", "legacy", "encrypted", "alice"));
    assert!(!run(&d, "open", "encrypted", "wrong-owner", "bob"));
    let mut bytes = fs::read(d.0.join("encrypted")).unwrap();
    bytes.pop();
    fs::write(d.0.join("truncated"), bytes).unwrap();
    assert!(!run(&d, "open", "truncated", "incomplete", "alice"));
    assert!(!d.0.join("wrong-owner").exists());
    assert!(!d.0.join("incomplete").exists());
    assert!(fs::read_dir(&d.0).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".snapshot-")));
}
