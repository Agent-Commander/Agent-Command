//! Rust-only development supervisor; the app restores its terminal before a rebuild.
use std::{
    path::PathBuf,
    process::{Command, ExitCode},
    thread,
    time::Duration,
};
fn main() -> ExitCode {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let output = match Command::new("cargo")
            .args([
                "build",
                "--bin",
                "agent-command",
                "--message-format=json-render-diagnostics",
            ])
            .current_dir(&root)
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                eprintln!("Could not run cargo: {error}");
                return ExitCode::FAILURE;
            }
        };
        if !output.status.success() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            eprintln!("Fix the build and save a source file to retry. Ctrl+C exits.");
            let old = agent_command::source_stamp(&root);
            while agent_command::source_stamp(&root) == old {
                thread::sleep(Duration::from_millis(500));
            }
            continue;
        }
        let executable = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).ok()?;
                value.get("executable")?.as_str().map(str::to_owned)
            })
            .next_back();
        let Some(executable) = executable else {
            eprintln!("Cargo did not report an executable.");
            return ExitCode::FAILURE;
        };
        match Command::new(executable)
            .env("AGENT_COMMAND_DEV", "1")
            .current_dir(&root)
            .status()
        {
            Ok(status) if status.code() == Some(75) => continue,
            Ok(status) if status.success() => return ExitCode::SUCCESS,
            Ok(_) => return ExitCode::FAILURE,
            Err(error) => {
                eprintln!("Launch failed: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
}
