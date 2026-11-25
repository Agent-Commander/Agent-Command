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

#[test]
fn specialist_subcommands_spawn_list_status_and_remove() {
    let root = std::env::temp_dir().join(format!("agent-cli-specialist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let base = root.join("plain");
    std::fs::create_dir_all(&base).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agent-command"))
            .arg("specialist")
            .args(args)
            .env("SUB_AUTH_DIR", root.join("state"))
            .current_dir(&base)
            .output()
            .unwrap()
    };
    let spawned = run(&["spawn", "echo", "writer", "Draft", "notes"]);
    assert!(spawned.status.success(), "{spawned:?}");
    assert!(String::from_utf8_lossy(&spawned.stdout)
        .contains("Spawned echo (writer, auto-edits) in directory"));
    let listed = String::from_utf8_lossy(&run(&["list"]).stdout).into_owned();
    assert!(
        listed.starts_with("echo\twriter\tIDLE\tdirectory"),
        "{listed}"
    );
    let status = String::from_utf8_lossy(&run(&["status", "echo"]).stdout).into_owned();
    assert!(status.contains("task: Draft notes"), "{status}");
    assert!(status.contains("mode: auto-edits"), "{status}");
    assert!(
        status.contains("review: revision 0 · round 0/3 · author unknown"),
        "{status}"
    );
    let refused = run(&[
        "spawn",
        "bolt",
        "builder",
        "--mode",
        "worktree-auto",
        "Ship",
    ]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not an isolated git worktree"));
    assert!(!root
        .join("state/agent-command-orchestrator-v1/workspaces/bolt")
        .exists());
    let bypass = run(&["spawn", "rogue", "builder", "--mode", "full-access"]);
    assert!(!bypass.status.success());
    let reviewed = run(&["review", "echo", "edge cases"]);
    assert!(!reviewed.status.success());
    assert!(String::from_utf8_lossy(&reviewed.stderr).contains("no recorded changes"));
    let playtester = run(&["spawn", "quinn", "playtester"]);
    assert!(String::from_utf8_lossy(&playtester.stdout).contains("(playtester, read-only)"));
    assert!(!root
        .join("state/agent-command-orchestrator-v1/workspaces/echo")
        .exists());
    assert!(status.contains(&base.canonicalize().unwrap().display().to_string()));
    let diff = run(&["diff", "echo"]);
    assert!(!diff.status.success());
    assert!(String::from_utf8_lossy(&diff.stderr).contains("plain directory"));
    let pr = run(&["pr", "echo", "--commit"]);
    assert!(!pr.status.success());
    assert!(String::from_utf8_lossy(&pr.stderr).contains("No PR for echo"));
    assert!(run(&["settle", "echo"]).status.success());
    let listed = String::from_utf8_lossy(&run(&["list"]).stdout).into_owned();
    assert!(
        listed.trim_end().ends_with("no branch\tsettled"),
        "{listed}"
    );
    assert!(!run(&["settle", "echo"]).status.success());
    assert!(run(&["unsettle", "echo"]).status.success());
    assert!(run(&["remove", "echo"]).status.success());
    assert!(base.is_dir());
    assert!(!run(&["status", "echo"]).status.success());
    assert!(!run(&["unknown"]).status.success());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn triage_reports_json_without_a_terminal_and_never_prints_secrets() {
    let base = std::env::temp_dir().join(format!("agent-cli-triage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let config = base.join("config");
    std::fs::create_dir_all(config.join("agent-command")).unwrap();
    std::fs::write(
        config.join("agent-command/settings.toml"),
        "auto_resume_on_reset = true\nfallback_profile = \"codex/backup\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agent-command"))
        .args(["triage", "--json"])
        .env("SUB_AUTH_DIR", base.join("profiles"))
        .env("XDG_CONFIG_HOME", &config)
        .env("TERM_PROGRAM", "sk-proj-0123456789abcdefghij")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("sk-proj-0123456789abcdefghij"));
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["stdout_terminal"], false);
    assert_eq!(report["settings"]["auto_resume_on_reset"], true);
    assert_eq!(report["settings"]["fallback_profile"], "codex/backup");
    assert!(report["catalog"]
        .as_array()
        .is_some_and(|lines| !lines.is_empty()));
    let usage = Command::new(env!("CARGO_BIN_EXE_agent-command"))
        .args(["triage", "--pipe", "gemini"])
        .output()
        .unwrap();
    assert!(!usage.status.success());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn mcp_stdio_binary_answers_initialize_tools_and_refuses_traversal() {
    use std::io::Write;
    let root = std::env::temp_dir().join(format!("agent-cli-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let base = root.join("plain");
    std::fs::create_dir_all(&base).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-command"))
        .args(["mcp", "--root"])
        .arg(&base)
        .env("SUB_AUTH_DIR", root.join("state"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"create_thread","arguments":{"name":"nova","role":"r","workspace":"../.."}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"create_thread","arguments":{"name":"echo","role":"writer"}}}"#,
    ];
    let mut stdin = child.stdin.take().unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    let line = |id: u32| {
        text.lines()
            .find(|line| {
                line.contains(&format!("\"id\":{id},")) || line.contains(&format!("\"id\":{id}}}"))
            })
            .unwrap_or_else(|| panic!("no response {id} in {text}"))
            .to_owned()
    };
    assert!(line(1).contains("\"protocolVersion\":\"2025-06-18\""));
    assert!(line(2).contains("delegate_task"));
    assert!(line(3).contains("-32602"));
    assert!(line(4).contains("\"isError\":false"));
    assert!(!root
        .join("state/agent-command-orchestrator-v1/workspaces/echo")
        .exists());
    assert!(base.is_dir());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn daemon_prints_service_files_and_refuses_wildcard_binds() {
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agent-command"))
            .arg("daemon")
            .args(args)
            .current_dir(std::env::temp_dir())
            .output()
            .unwrap()
    };
    for (os, marker) in [
        ("linux", "ExecStart="),
        ("macos", "<key>ProgramArguments</key>"),
        ("windows", "schtasks /Create"),
    ] {
        let printed = run(&["--print-service", os]);
        assert!(printed.status.success(), "{printed:?}");
        assert!(
            String::from_utf8_lossy(&printed.stdout).contains(marker),
            "{os}"
        );
    }
    let refused = run(&["--bind", "0.0.0.0:47100", "--allow-remote"]);
    assert!(!refused.status.success());
    let remote = run(&["--bind", "192.0.2.1:47100"]);
    assert!(!remote.status.success());
    assert!(String::from_utf8_lossy(&remote.stderr).contains("--allow-remote"));
}

#[test]
fn connect_requires_a_token_for_remote_daemons() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-command"))
        .args(["connect", "http://192.0.2.1:47100", "--once"])
        .env_remove("AGENT_COMMAND_TOKEN")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("No daemon token"));
}
