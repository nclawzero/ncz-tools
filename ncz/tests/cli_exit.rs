use std::process::Command;

#[test]
fn invalid_argument_exits_with_usage_code() {
    let status = Command::new(env!("CARGO_BIN_EXE_ncz"))
        .arg("--definitely-not-a-real-ncz-arg")
        .status()
        .unwrap();

    assert_eq!(status.code(), Some(1));
}
