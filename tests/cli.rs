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
    for scope in ["agents/coding (optional)", "models/free"] {
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

#[cfg(unix)]
#[test]
fn model_pins_round_trip_through_versioned_state_and_refuse_aliases() {
    use std::os::unix::fs::DirBuilderExt;
    let root = std::env::temp_dir().join(format!("agent-cli-pins-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root.join("claude/work"))
        .unwrap();
    let model = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agent-command"))
            .arg("model")
            .args(arguments)
            .env("SUB_AUTH_DIR", &root)
            .output()
            .unwrap()
    };
    let stdout =
        |output: &std::process::Output| String::from_utf8_lossy(&output.stdout).into_owned();
    let pinned = model(&["pin", "claude", "work", "claude-opus-5-5", "high"]);
    assert!(pinned.status.success(), "{pinned:?}");
    assert_eq!(
        stdout(&pinned),
        "Pinned claude/work · claude-opus-5-5 high\n"
    );
    assert_eq!(
        stdout(&model(&["show"])),
        "claude/work · claude-opus-5-5 high\n"
    );
    let state = root.join("agent-command-routing-v1/native-pins-v1.json");
    assert!(std::fs::read_to_string(&state)
        .unwrap()
        .starts_with(r#"{"schema":"agent-command-native-pins-v1""#));
    for refused in [
        &["pin", "claude", "work", "opus"][..],
        &["pin", "claude", "work", "claude-opus-5-5", "ultra"],
        &["pin", "claude", "absent", "claude-opus-5-5"],
        &["pin", "claude", "work", "claude-opus-5-5-high"],
    ] {
        assert!(!model(refused).status.success(), "{refused:?}");
    }
    assert_eq!(
        stdout(&model(&["pin", "claude", "work", "claude-opus-5-5"])),
        "Pinned claude/work · claude-opus-5-5 AA default setting\n"
    );
    assert_eq!(
        stdout(&model(&["unpin", "claude", "work"])),
        "Unpinned claude/work\n"
    );
    assert_eq!(stdout(&model(&["show"])), "");
    std::fs::remove_dir_all(root).unwrap();
}
