#[cfg(unix)]
#[test]
fn provider_lifecycle_uses_only_selected_profiles() {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

    let workspace = std::env::temp_dir().join(format!("agent-command-auth-{}", std::process::id()));
    fs::create_dir(&workspace).unwrap();
    let profiles = workspace.join("profiles");
    let tools = workspace.join("tools");
    fs::create_dir(&tools).unwrap();
    for provider in ["claude", "codex"] {
        let executable = tools.join(provider);
        fs::write(&executable, r#"#!/bin/sh
test -z "$OPENAI_API_KEY$ANTHROPIC_AUTH_TOKEN$CLAUDECODE$CODEX_THREAD_ID" || exit 71
case "${0##*/}" in
claude)
    dir="$CLAUDE_CONFIG_DIR"
    test -z "$CODEX_HOME" || exit 72
    test "$1" != auth || shift
    ;;
codex)
    dir="$CODEX_HOME"
    test -z "$CLAUDE_CONFIG_DIR" || exit 73
    test "$2" = "cli_auth_credentials_store='file'" || exit 74
    test "$4" = "forced_login_method='chatgpt'" || exit 75
    shift 4
    ;;
esac
case "$1 $2" in
    'login status') test -f "$dir/signed-in" && echo 'Logged in using ChatGPT' >&2 ;;
    'status --json') test -f "$dir/signed-in" && echo '{"loggedIn":true,"authMethod":"claude.ai","email":"Pilot@Example.test"}' ;;
    'login '| 'login --claudeai') touch "$dir/signed-in" ;;
    'logout ') rm "$dir/signed-in" ;;
    '--help ') printf '%s\n' 'provider help' ;;
    *) exit 76 ;;
esac
"#).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let command = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_agent-command"))
            .args(args)
            .env(
                "PATH",
                std::env::join_paths([tools.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
                    .unwrap(),
            )
            .env("SUB_AUTH_DIR", &profiles)
            .env("CLAUDECODE", "synthetic")
            .env("CODEX_THREAD_ID", "synthetic")
            .env("OPENAI_API_KEY", "synthetic")
            .env("ANTHROPIC_AUTH_TOKEN", "synthetic")
            .env("CODEX_HOME", workspace.join("default-codex"))
            .env("CLAUDE_CONFIG_DIR", workspace.join("default-claude"))
            .output()
            .unwrap()
    };
    for provider in ["claude", "codex"] {
        for profile in ["alpha", "beta"] {
            assert!(command(&["auth", "login", provider, profile])
                .status
                .success());
            let directory = profiles.join(provider).join(profile);
            assert_eq!(
                directory.metadata().unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(command(&["auth", "status", provider, "alpha"])
            .status
            .success());
        let run = command(&["auth", "run", provider, "alpha", "--", "--help"]);
        assert!(run.status.success());
        assert_eq!(run.stdout, b"provider help\n");
        assert!(command(&["auth", "logout", provider, "alpha"])
            .status
            .success());
        assert!(!command(&["auth", "status", provider, "alpha"])
            .status
            .success());
        assert!(command(&["auth", "status", provider, "beta"])
            .status
            .success());
    }
    let automatic = command(&["auth", "login", "claude"]);
    assert!(automatic.status.success());
    assert_eq!(
        automatic.stdout,
        b"claude/pilot@example.test is signed in\n"
    );
    let fallback = command(&["auth", "login", "codex"]);
    assert!(fallback.status.success());
    assert_eq!(fallback.stdout, b"codex/main is signed in\n");
    assert!(command(&["auth", "status", "claude", "pilot@example.test"])
        .status
        .success());
    assert_eq!(
        command(&["auth", "list"]).stdout,
        b"claude alpha\nclaude beta\nclaude pilot@example.test\ncodex alpha\ncodex beta\ncodex main\n"
    );
    assert!(!workspace.join("default-claude").exists());
    assert!(!workspace.join("default-codex").exists());
    let unsafe_profile = profiles.join("codex/beta");
    fs::set_permissions(&unsafe_profile, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!command(&["auth", "status", "codex", "beta"])
        .status
        .success());
    std::os::unix::fs::symlink(profiles.join("codex/alpha"), profiles.join("codex/link")).unwrap();
    assert!(!command(&["auth", "login", "codex", "link"])
        .status
        .success());
    fs::remove_dir_all(workspace).unwrap();
}
