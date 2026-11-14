use std::process::Command;

#[test]
fn command_routes_to_ui_outside_checkout() {
    let binary = env!("CARGO_BIN_EXE_agent-command");
    let output = Command::new(binary)
        .args(["--help"])
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: agent-command"));

    let path = std::env::temp_dir().join(format!("agent-cli-{}.svg", std::process::id()));
    let status = Command::new(binary)
        .args(["--snapshot"])
        .arg(&path)
        .current_dir(std::env::temp_dir())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(std::fs::read_to_string(&path).unwrap().contains("#111419"));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn invalid_command_fails_without_opening_terminal() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-command"))
        .arg("unknown")
        .output()
        .unwrap();
    assert!(!output.status.success());
}
