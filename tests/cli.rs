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

#[test]
fn catalog_entry_point_shows_bundled_provenance_and_refuses_refresh_without_key() {
    let root = std::env::temp_dir().join(format!("agent-cli-catalog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let command = |action: &str| {
        Command::new(env!("CARGO_BIN_EXE_agent-command"))
            .args(["catalog", action])
            .env("SUB_AUTH_DIR", &root)
            .env_remove("ARTIFICIAL_ANALYSIS_API_KEY")
            .output()
            .unwrap()
    };
    let shown = command("show");
    assert!(shown.status.success());
    let text = String::from_utf8_lossy(&shown.stdout);
    for scope in ["agents/coding", "models/free"] {
        assert!(text.contains(&format!(
            "{scope} · bundled snapshot (cache missing) · version bundled-2026-11-23 · 0 rows · Dated 2026-11-23."
        )));
    }
    let refused = command("refresh");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("Set ARTIFICIAL_ANALYSIS_API_KEY to refresh"));
    assert!(!root.join("agent-command-routing-v1/catalog").exists());
    std::fs::remove_dir_all(root).unwrap();
}
