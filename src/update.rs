use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const LATEST: &str = "https://github.com/Agent-Commander/Agent-Command/releases/latest";
const INSTALLER: &str =
    "https://raw.githubusercontent.com/Agent-Commander/Agent-Command/main/install/install";

fn parse(version: &str) -> Option<[u64; 3]> {
    let core = version.trim().trim_start_matches('v');
    let mut parts = core.split(['-', '+']).next()?.split('.');
    let mut numbers = [0; 3];
    for number in &mut numbers {
        match parts.next() {
            Some(part) => *number = part.parse().ok()?,
            None => break,
        }
    }
    parts.next().is_none().then_some(numbers)
}

pub fn newer(latest: &str, current: &str) -> Option<String> {
    let (latest, current) = (parse(latest)?, parse(current)?);
    (latest > current).then(|| latest.map(|number| number.to_string()).join("."))
}

fn tag(headers: &str) -> Option<&str> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        let (_, tag) = value.trim().rsplit_once("/releases/tag/")?;
        name.eq_ignore_ascii_case("location").then_some(tag)
    })
}

pub fn watch(enabled: bool) -> Option<Receiver<String>> {
    if cfg!(debug_assertions) || !enabled {
        return None;
    }
    let (found, results) = mpsc::channel();
    thread::spawn(move || found.send(newer(&latest_tag()?, CURRENT)?).ok());
    Some(results)
}

fn latest_tag() -> Option<String> {
    let headers = Command::new("curl")
        .args(["--silent", "--head", "--max-time", "5", LATEST])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    tag(&String::from_utf8_lossy(&headers.stdout)).map(str::to_owned)
}

pub const DEVELOPMENT_REFUSAL: &str =
    "agent-command update is unavailable in development builds. Rebuild from source instead.";

pub fn run(args: &[String]) -> io::Result<()> {
    if !args.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Usage: agent-command update",
        ));
    }
    if cfg!(debug_assertions) {
        return Err(io::Error::other(DEVELOPMENT_REFUSAL));
    }
    apply(latest_tag().as_deref())
}

fn apply(latest: Option<&str>) -> io::Result<()> {
    let Some(latest) = latest else {
        return Err(io::Error::other(
            "Cannot read the latest release. Try again later.",
        ));
    };
    let Some(latest) = newer(latest, CURRENT) else {
        println!("Agent Command {CURRENT} is up to date.");
        return Ok(());
    };
    if cfg!(windows) {
        print!("{}", notice(&latest));
        return Ok(());
    }
    if install(&latest) {
        Ok(())
    } else {
        Err(io::Error::other("The update did not finish."))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Applied {
    Installed,
    HandedOff,
}

pub(crate) fn finished_message(latest: &str, applied: Applied) -> String {
    match applied {
        Applied::Installed => {
            format!("Agent Command updated to {latest}. Run agent-command to open the new version.")
        }
        Applied::HandedOff => format!(
            "Agent Command {latest} is installing in a new PowerShell window and opens when it finishes."
        ),
    }
}

pub(crate) fn apply_in_app(latest: &str) -> io::Result<Applied> {
    if cfg!(debug_assertions) {
        return Err(io::Error::other(DEVELOPMENT_REFUSAL));
    }
    if cfg!(windows) {
        let script = windows_updater_script(std::process::id(), custom_dir().as_deref());
        return hand_off(&script).map(|()| Applied::HandedOff);
    }
    if install(latest) {
        Ok(Applied::Installed)
    } else {
        Err(io::Error::other("the installer did not finish"))
    }
}

pub(crate) fn windows_updater_script(pid: u32, dir: Option<&Path>) -> String {
    format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue; {}",
        command(true, dir)
    )
}

fn windows_launcher(script: &str) -> String {
    format!(
        "Start-Process powershell -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-EncodedCommand','{}'",
        encoded_command(script)
    )
}

fn encoded_command(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&bytes)
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| {
            value | u32::from(*byte) << (16 - 8 * index)
        });
        for position in 0..4 {
            if position <= chunk.len() {
                let sextet = (value >> (18 - 6 * position)) & 0x3f;
                encoded.push(char::from(ALPHABET[sextet as usize]));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

fn hand_off(script: &str) -> io::Result<()> {
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(windows_launcher(script))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "the PowerShell updater did not start ({status})"
                )))
            }
        })
}

pub fn one_line_notice(latest: &str) -> String {
    notice(latest)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn custom_dir() -> Option<PathBuf> {
    let real = |path: PathBuf| {
        if cfg!(windows) {
            Some(path)
        } else {
            fs::canonicalize(path).ok()
        }
    };
    let dir = real(env::current_exe().ok()?)?.parent()?.to_owned();
    let default = if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(|root| Path::new(&root).join(r"AgentCommand\bin"))
    } else {
        env::var_os("HOME").map(|home| Path::new(&home).join(".local/bin"))
    };
    (default.and_then(real).as_ref() != Some(&dir)).then_some(dir)
}

fn command(windows: bool, dir: Option<&Path>) -> String {
    match (windows, dir.map(|dir| dir.display().to_string())) {
        (false, None) => format!("curl -fsSL {INSTALLER}.sh | sh"),
        (false, Some(dir)) => format!(
            "curl -fsSL {INSTALLER}.sh | AGENT_COMMAND_INSTALL_DIR='{}' sh",
            dir.replace('\'', r"'\''")
        ),
        (true, None) => format!("irm {INSTALLER}.ps1 | iex"),
        (true, Some(dir)) => format!(
            "$env:AGENT_COMMAND_INSTALL_DIR = '{}'; irm {INSTALLER}.ps1 | iex",
            dir.replace('\'', "''")
        ),
    }
}

pub fn notice(latest: &str) -> String {
    let run = if cfg!(windows) {
        "run this in PowerShell"
    } else {
        "run"
    };
    format!(
        "Agent Command {latest} is available (you have {CURRENT}). To update, {run}:\n\n  {}\n",
        command(cfg!(windows), custom_dir().as_deref())
    )
}

pub fn install(latest: &str) -> bool {
    println!("Updating Agent Command {CURRENT} to {latest}...");
    let dir = custom_dir();
    let script = Command::new("curl")
        .args(["-fsSL", &format!("{INSTALLER}.sh")])
        .stderr(Stdio::inherit())
        .output()
        .ok()
        .filter(|fetched| fetched.status.success());
    let finished = script.is_some_and(|script| {
        let mut installer = Command::new("sh");
        installer
            .args(["-c", &String::from_utf8_lossy(&script.stdout)])
            .env("AGENT_COMMAND_NO_LAUNCH", "1");
        if let Some(dir) = &dir {
            installer.env("AGENT_COMMAND_INSTALL_DIR", dir);
        }
        installer.status().is_ok_and(|status| status.success())
    });
    if !finished {
        eprintln!(
            "\nThe update did not finish. To try again, run:\n\n  {}\n",
            command(false, dir.as_deref())
        );
    }
    finished
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_newer_versions_count() {
        assert_eq!(newer("v0.2.0", "0.1.0").as_deref(), Some("0.2.0"));
        assert_eq!(newer("v0.10.0", "0.9.9").as_deref(), Some("0.10.0"));
        assert_eq!(newer("v1.0", "0.9.9").as_deref(), Some("1.0.0"));
        for (latest, current) in [
            ("v0.1.0", "0.1.0"),
            ("v0.1.0", "0.2.0"),
            ("v0.2.0-rc1", "0.2.0"),
            ("v1.2.3.4", "0.1.0"),
            ("nightly", "0.1.0"),
            ("", "0.1.0"),
        ] {
            assert_eq!(newer(latest, current), None, "{latest} over {current}");
        }
    }

    #[test]
    fn tag_comes_from_the_redirect() {
        let headers = "HTTP/2 302 \r\nserver: github.com\r\nlocation: https://github.com/Agent-Commander/Agent-Command/releases/tag/v0.2.0\r\n\r\n";
        assert_eq!(tag(headers), Some("v0.2.0"));
        assert_eq!(
            tag(&headers.replace("location", "Location")),
            Some("v0.2.0")
        );
        assert_eq!(tag(&headers.replace("/tag/v0.2.0", "")), None);
        assert_eq!(tag(""), None);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn update_command_refuses_or_no_ops_without_a_newer_version() {
        let refused = run(&[]).unwrap_err();
        assert_eq!(refused.to_string(), DEVELOPMENT_REFUSAL);
        assert_eq!(
            run(&["now".into()]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(apply(Some(&format!("v{CURRENT}"))).is_ok());
        assert!(apply(Some("v0.0.1")).is_ok());
        assert!(apply(Some("nightly")).is_ok());
        assert!(apply(None).is_err());
        assert!(watch(true).is_none());
    }

    #[test]
    fn one_line_notice_keeps_the_command_on_a_single_line() {
        let line = one_line_notice("9.9.9");
        assert!(!line.contains('\n'));
        assert!(line.starts_with("Agent Command 9.9.9 is available"));
        assert!(line.contains(INSTALLER));
    }

    #[test]
    fn windows_updater_waits_for_this_process_then_runs_the_official_installer() {
        let script = windows_updater_script(4242, None);
        assert!(
            script.starts_with("Wait-Process -Id 4242 -ErrorAction SilentlyContinue; "),
            "{script}"
        );
        assert!(
            script.ends_with(&format!("irm {INSTALLER}.ps1 | iex")),
            "{script}"
        );
        assert_eq!(
            format!("{INSTALLER}.ps1"),
            "https://raw.githubusercontent.com/Agent-Commander/Agent-Command/main/install/install.ps1"
        );
        let custom = windows_updater_script(7, Some(Path::new(r"C:\Tools\it's here")));
        assert!(
            custom.contains(r"$env:AGENT_COMMAND_INSTALL_DIR = 'C:\Tools\it''s here'; irm "),
            "{custom}"
        );
        assert!(custom.find("Wait-Process") < custom.find("AGENT_COMMAND_INSTALL_DIR"));
    }

    #[test]
    fn windows_launcher_passes_the_script_as_an_encoded_command() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(encoded_command("ab"), "YQBiAA==");
        let launcher = windows_launcher(&windows_updater_script(1, None));
        assert!(launcher.starts_with("Start-Process powershell -ArgumentList "));
        assert!(launcher.contains("'-EncodedCommand','"));
        assert!(!launcher.contains('|') && !launcher.contains('"'));
    }

    #[test]
    fn finished_messages_name_the_new_version() {
        assert!(finished_message("9.9.9", Applied::Installed).contains("updated to 9.9.9"));
        assert!(finished_message("9.9.9", Applied::HandedOff).contains("PowerShell window"));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn in_app_update_refuses_in_development_builds() {
        assert_eq!(
            apply_in_app("9.9.9").unwrap_err().to_string(),
            DEVELOPMENT_REFUSAL
        );
    }

    #[test]
    fn command_matches_the_readme_and_names_other_folders() {
        let readme = include_str!("../README.md");
        assert!(readme.contains(&command(false, None)));
        assert!(readme.contains(&command(true, None)));
        assert!(command(false, Some(Path::new("/opt/it's here")))
            .ends_with(r"| AGENT_COMMAND_INSTALL_DIR='/opt/it'\''s here' sh"));
        assert!(command(true, Some(Path::new(r"C:\it's here")))
            .starts_with(r"$env:AGENT_COMMAND_INSTALL_DIR = 'C:\it''s here'; irm "));
    }
}
