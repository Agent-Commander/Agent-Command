use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-uninstall-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn installer_script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("install")
        .join(name)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const MARKER_AND_PATH: &str = "# Agent Command\nexport PATH=\"$HOME/.local/bin:$PATH\"\n";

    fn write_executable(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn fake_agent_command(path: &Path) {
        write_executable(path, "echo 'agent-command 9.9.9'");
    }

    fn fake_cargo(tools: &Path) {
        write_executable(
            &tools.join("cargo"),
            "if [ \"$1 $2\" = 'install --list' ]; then echo 'agent-command v9.9.9:'; else touch \"$0.ran\"; fi",
        );
    }

    fn uninstall(home: &Path, tools: &Path, install_dir: Option<&Path>) -> Output {
        let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
        let mut command = Command::new("sh");
        command
            .arg(installer_script("uninstall.sh"))
            .env("HOME", home)
            .env("PATH", path)
            .env_remove("SUB_AUTH_DIR")
            .env_remove("AGENT_COMMAND_THEME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("AGENT_COMMAND_INSTALL_DIR");
        if let Some(dir) = install_dir {
            command.env("AGENT_COMMAND_INSTALL_DIR", dir);
        }
        command.output().unwrap()
    }

    #[test]
    fn default_install_removes_binary_empty_bin_and_only_installer_profile_lines() {
        let home = scratch("sh-profile");
        let tools = home.join("tools");
        fake_cargo(&tools);
        fake_agent_command(&home.join(".local/bin/agent-command"));
        let profile = format!("alias keep=1\n\n{MARKER_AND_PATH}export EDITOR=vi\n");
        let bashrc = "# Agent Command\nexport PATH=\"$HOME/other:$PATH\"\nexport PATH=\"$HOME/.local/bin:$PATH\"\n";
        fs::write(home.join(".profile"), &profile).unwrap();
        fs::write(home.join(".bashrc"), bashrc).unwrap();

        let first = uninstall(&home, &tools, None);
        assert!(first.status.success(), "{}", stderr(&first));
        assert!(!home.join(".local/bin").exists());
        assert!(home.join(".local").exists());
        assert_eq!(
            fs::read_to_string(home.join(".profile")).unwrap(),
            "alias keep=1\n\nexport EDITOR=vi\n"
        );
        assert_eq!(fs::read_to_string(home.join(".bashrc")).unwrap(), bashrc);
        assert!(!home.join(".zshrc").exists());
        let text = stdout(&first);
        assert!(text.contains(&format!(
            "Kept sign-ins and app data: {}/.local/share/sub-auth",
            home.display()
        )));
        assert!(text.contains(&format!(
            "Kept theme settings: {}/.config/agent-command/theme.toml",
            home.display()
        )));
        assert!(text.contains("agent-command auth logout PROVIDER PROFILE"));
        assert!(text.contains("Next step: cargo uninstall agent-command"));
        assert!(!tools.join("cargo.ran").exists());

        let second = uninstall(&home, &tools, None);
        assert!(second.status.success(), "{}", stderr(&second));
        assert_eq!(
            fs::read_to_string(home.join(".profile")).unwrap(),
            "alias keep=1\n\nexport EDITOR=vi\n"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn custom_install_dir_keeps_profiles_and_refuses_unrelated_file() {
        let home = scratch("sh-install-dir");
        let tools = home.join("tools");
        let custom = home.join("custom");
        fs::create_dir_all(&tools).unwrap();
        fake_agent_command(&custom.join("agent-command"));
        fs::write(custom.join("other-tool"), "keep").unwrap();
        let profile = format!("\n{MARKER_AND_PATH}");
        fs::write(home.join(".profile"), &profile).unwrap();

        let first = uninstall(&home, &tools, Some(&custom));
        assert!(first.status.success(), "{}", stderr(&first));
        assert!(!custom.join("agent-command").exists());
        assert!(custom.join("other-tool").exists());
        assert_eq!(fs::read_to_string(home.join(".profile")).unwrap(), profile);
        assert!(uninstall(&home, &tools, Some(&custom)).status.success());

        let unrelated = home.join("unrelated");
        write_executable(
            &unrelated.join("agent-command"),
            "echo 'something else 1.0'",
        );
        let refused = uninstall(&home, &tools, Some(&unrelated));
        assert!(!refused.status.success());
        assert!(stderr(&refused).contains("Refusing to remove unrelated file"));
        assert!(unrelated.join("agent-command").exists());
        fs::remove_dir_all(home).unwrap();
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    fn powershell() -> &'static str {
        let available = |name: &str| {
            Command::new(name)
                .args(["-NoProfile", "-Command", "exit 0"])
                .status()
                .is_ok_and(|status| status.success())
        };
        if available("pwsh") {
            "pwsh"
        } else {
            "powershell"
        }
    }

    fn run_powershell(script: &str) -> String {
        let output = Command::new(powershell())
            .args(["-NoProfile", "-Command", script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output).trim().to_string()
    }

    fn user_path() -> String {
        run_powershell("[Environment]::GetEnvironmentVariable('Path', 'User')")
    }

    fn set_user_path(value: &str) {
        let quoted = value.replace('\'', "''");
        run_powershell(&format!(
            "[Environment]::SetEnvironmentVariable('Path', '{quoted}', 'User')"
        ));
    }

    struct RestoreUserPath(String);

    impl Drop for RestoreUserPath {
        fn drop(&mut self) {
            set_user_path(&self.0);
        }
    }

    fn copy_agent_command(bin: &Path) {
        fs::create_dir_all(bin).unwrap();
        fs::copy(
            env!("CARGO_BIN_EXE_agent-command"),
            bin.join("agent-command.exe"),
        )
        .unwrap();
    }

    fn uninstall(local_app_data: &Path, install_dir: Option<&Path>) -> Output {
        let mut command = Command::new(powershell());
        command
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(installer_script("uninstall.ps1"))
            .env("LOCALAPPDATA", local_app_data)
            .env_remove("AGENT_COMMAND_INSTALL_DIR");
        if let Some(dir) = install_dir {
            command.env("AGENT_COMMAND_INSTALL_DIR", dir);
        }
        command.output().unwrap()
    }

    #[test]
    fn default_install_removes_exe_empty_folders_and_only_its_user_path_entry() {
        let local = scratch("ps-local");
        let bin = local.join("AgentCommand").join("bin");
        copy_agent_command(&bin);
        let original = user_path();
        let restore = RestoreUserPath(original.clone());
        set_user_path(&format!("{};{original}", bin.display()));

        let first = uninstall(&local, None);
        assert!(first.status.success(), "{}", stderr(&first));
        assert!(!local.join("AgentCommand").exists());
        assert!(local.exists());
        assert_eq!(user_path(), original);
        let text = stdout(&first);
        assert!(text.contains("Kept sign-ins and app data:"));
        assert!(text.contains("agent-command auth logout PROVIDER PROFILE"));

        let second = uninstall(&local, None);
        assert!(second.status.success(), "{}", stderr(&second));
        assert_eq!(user_path(), original);
        drop(restore);
        fs::remove_dir_all(local).unwrap();
    }

    #[test]
    fn custom_install_dir_removes_exe_and_refuses_unrelated_file() {
        let local = scratch("ps-install-dir");
        let custom = local.join("custom");
        copy_agent_command(&custom);

        let first = uninstall(&local, Some(&custom));
        assert!(first.status.success(), "{}", stderr(&first));
        assert!(!custom.exists());
        assert!(uninstall(&local, Some(&custom)).status.success());

        let unrelated = local.join("unrelated");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("agent-command.exe"), "not agent command").unwrap();
        let refused = uninstall(&local, Some(&unrelated));
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&[refused.stdout, refused.stderr].concat())
                .contains("Refusing to remove unrelated file")
        );
        assert!(unrelated.join("agent-command.exe").exists());
        fs::remove_dir_all(local).unwrap();
    }
}
