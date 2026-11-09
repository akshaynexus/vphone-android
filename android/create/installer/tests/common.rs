#![allow(dead_code)]
use std::path::PathBuf;
use std::process::Command;

pub fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

/// Test scratch space: /tmp is nearly full on the development host.
pub fn tmp() -> tempfile::TempDir {
    let base = repo().join("android/build/create-steps/tmp");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new().prefix("t").tempdir_in(base).unwrap()
}

pub fn assets() -> PathBuf {
    repo().join("android/build/create-steps")
}

pub fn guest_resources() -> PathBuf {
    assets().join("release/x/VPhone.bundle/Contents/Resources/guest-resources")
}

pub fn sshtars() -> PathBuf {
    assets().join("sshtars/x")
}

/// Run python3 in isolated mode on an inline program; stdout on success.
pub fn python(program: &str, args: &[&str]) -> String {
    let out = Command::new("python3")
        .arg("-I")
        .arg("-c")
        .arg(program)
        .args(args)
        .output()
        .expect("python3");
    assert!(
        out.status.success(),
        "python failed: {}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run a repo script file with python3 in isolated mode.
pub fn python_tool(script: &str, args: &[&str]) -> String {
    let out = Command::new("python3")
        .arg("-I")
        .arg(repo().join(script))
        .args(args)
        .output()
        .expect("python3");
    assert!(
        out.status.success(),
        "{script} failed: {}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
