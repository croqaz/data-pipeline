//! The real binary: exit codes and streams through the shim in main.rs.

use assert_cmd::Command;

#[test]
fn version_help_and_errors() {
    let mut cmd = Command::cargo_bin("pipeline").unwrap();
    let out = cmd.arg("--version").assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.starts_with("pipeline "));

    Command::cargo_bin("pipeline").unwrap().assert().failure().code(2);

    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("pipeline")
        .unwrap()
        .args(["count", "--db", dir.path().join("none").to_str().unwrap()])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("not a pipeline store"));
}
