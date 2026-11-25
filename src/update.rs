use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const LATEST: &str = "https://github.com/Agent-Commander/Agent-Command/releases/latest";
const RELEASE_DOWNLOAD_BASE: &str =
    "https://github.com/Agent-Commander/Agent-Command/releases/download";
const INSTALLER: &str =
    "https://raw.githubusercontent.com/Agent-Commander/Agent-Command/main/install/install";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate {
        current: String,
    },
    Available {
        tag: String,
        current: String,
    },
    Downloading {
        pct: u8,
    },
    Failed(String),
}

impl UpdateState {
    pub fn pill_label(&self) -> String {
        match self {
            Self::Idle => format!("v{CURRENT}"),
            Self::Checking => format!("v{CURRENT} · checking…"),
            Self::UpToDate { current } => format!("v{current} ✓"),
            Self::Available { tag, .. } => {
                format!("⬆ Update v{} (F6)", tag.trim_start_matches('v'))
            }
            Self::Downloading { pct } => format!("Updating… {pct}%"),
            Self::Failed(_) => format!("v{CURRENT} · update check offline (retry)"),
        }
    }

    pub fn available_version(&self) -> Option<&str> {
        match self {
            Self::Available { tag, .. } => Some(tag.trim_start_matches('v')),
            _ => None,
        }
    }
}

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
    thread::spawn(move || found.send(newer(&latest_tag().ok()?, CURRENT)?).ok());
    Some(results)
}

pub fn check_now() -> Receiver<UpdateState> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let state = evaluate_release_check(latest_tag(), CURRENT);
        let _ = sender.send(state);
    });
    receiver
}

pub fn evaluate_release_check(latest: io::Result<String>, current: &str) -> UpdateState {
    match latest {
        Ok(found_tag) => match newer(&found_tag, current) {
            Some(version) => UpdateState::Available {
                tag: format!("v{version}"),
                current: current.to_owned(),
            },
            None => UpdateState::UpToDate {
                current: current.to_owned(),
            },
        },
        Err(error) => UpdateState::Failed(error.to_string()),
    }
}

fn curl_binary() -> &'static str {
    if cfg!(windows) {
        "curl.exe"
    } else {
        "curl"
    }
}

fn latest_tag() -> io::Result<String> {
    let headers = Command::new(curl_binary())
        .args(["--silent", "--head", "--max-time", "5", LATEST])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !headers.status.success() {
        let stderr = String::from_utf8_lossy(&headers.stderr).trim().to_owned();
        return Err(io::Error::other(if stderr.is_empty() {
            format!("Release check failed ({})", headers.status)
        } else {
            stderr
        }));
    }
    tag(&String::from_utf8_lossy(&headers.stdout))
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("Release redirect missing version tag"))
}

pub fn old_binary_path(executable: &Path) -> PathBuf {
    let mut name = OsString::from(executable.as_os_str());
    name.push(".old");
    PathBuf::from(name)
}

pub fn cleanup_stale_old_binary() {
    if let Ok(current) = env::current_exe() {
        let _ = cleanup_old_at(&current);
    }
}

pub fn cleanup_old_at(executable: &Path) -> io::Result<bool> {
    let stale = old_binary_path(executable);
    match fs::remove_file(&stale) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn parse_sha256_manifest(manifest: &str) -> Option<String> {
    let token = manifest.split_whitespace().next()?;
    (token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| token.to_ascii_lowercase())
}

pub fn verify_sha256_bytes(bytes: &[u8], expected_hex: &str) -> io::Result<()> {
    let expected = parse_sha256_manifest(expected_hex).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "SHA-256 manifest is malformed.")
    })?;
    let actual: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Download checksum mismatch (expected {expected}, got {actual})."),
        ));
    }
    Ok(())
}

pub fn swap_executable_in_place(current_exe: &Path, staged_exe: &Path) -> io::Result<PathBuf> {
    let backup = old_binary_path(current_exe);
    let _ = fs::remove_file(&backup);
    let parent = current_exe.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Current executable has no parent directory.",
        )
    })?;
    let temp_target = parent.join(format!("agent-command.{}.new", uuid::Uuid::new_v4()));
    let mut staged_bytes = Vec::new();
    File::open(staged_exe)?.read_to_end(&mut staged_bytes)?;
    let mut file = File::create_new(&temp_target)?;
    file.write_all(&staged_bytes)?;
    file.sync_all()?;
    drop(file);

    if let Err(error) = fs::rename(current_exe, &backup) {
        let _ = fs::remove_file(&temp_target);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp_target, current_exe) {
        let _ = fs::rename(&backup, current_exe);
        let _ = fs::remove_file(&temp_target);
        return Err(error);
    }
    Ok(backup)
}

fn windows_asset_name() -> io::Result<&'static str> {
    match env::consts::ARCH {
        "x86_64" => Ok("agent-command-x86_64-pc-windows-msvc.zip"),
        "aarch64" => Ok("agent-command-aarch64-pc-windows-msvc.zip"),
        arch => Err(io::Error::other(format!(
            "Unsupported Windows architecture {arch} for self-update."
        ))),
    }
}

fn release_asset_url(tag: &str, asset: &str) -> String {
    if let Ok(base) = env::var("AGENT_COMMAND_RELEASE_URL") {
        let trimmed = base.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return format!("{trimmed}/{asset}");
        }
    }
    let normalized = if tag.starts_with('v') {
        tag.to_owned()
    } else {
        format!("v{tag}")
    };
    format!("{RELEASE_DOWNLOAD_BASE}/{normalized}/{asset}")
}

fn curl_download(url: &str, destination: &Path) -> io::Result<()> {
    let output = Command::new(curl_binary())
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-time",
            "120",
            "--output",
        ])
        .arg(destination)
        .arg(url)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(io::Error::other(if stderr.is_empty() {
            format!("Download failed ({})", output.status)
        } else {
            stderr
        }));
    }
    Ok(())
}

pub fn self_update_windows(tag: &str, mut progress: impl FnMut(u8)) -> io::Result<PathBuf> {
    let current_exe = env::current_exe()?;
    let asset = windows_asset_name()?;
    let temp = env::temp_dir().join(format!("ac-update-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp)?;

    let result = (|| {
        progress(10);
        let zip_path = temp.join(asset);
        let sha_path = temp.join(format!("{asset}.sha256"));
        curl_download(&release_asset_url(tag, asset), &zip_path)?;
        progress(55);
        curl_download(
            &release_asset_url(tag, &format!("{asset}.sha256")),
            &sha_path,
        )?;
        progress(70);

        let zip_bytes = fs::read(&zip_path)?;
        let sha_text = fs::read_to_string(&sha_path)?;
        verify_sha256_bytes(&zip_bytes, &sha_text)?;
        progress(80);

        let unpacked = temp.join("unpacked");
        fs::create_dir_all(&unpacked)?;
        let tar_output = Command::new("tar.exe")
            .arg("-xf")
            .arg(&zip_path)
            .arg("-C")
            .arg(&unpacked)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        if !tar_output.status.success() {
            let stderr = String::from_utf8_lossy(&tar_output.stderr)
                .trim()
                .to_owned();
            return Err(io::Error::other(format!(
                "Failed to extract update archive: {stderr}"
            )));
        }
        let staged = unpacked.join("agent-command.exe");
        let header = fs::read(&staged)?;
        if header.len() < 2 || &header[..2] != b"MZ" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Extracted update is not a valid Windows executable.",
            ));
        }
        progress(90);
        swap_executable_in_place(&current_exe, &staged)?;
        progress(100);
        Ok(current_exe)
    })();

    let _ = fs::remove_dir_all(&temp);
    result
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
    apply(latest_tag().ok().as_deref())
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
        self_update_windows(&latest, |_| {})?;
        println!(
            "Agent Command updated to {latest}. Restart agent-command to run the new version."
        );
        return Ok(());
    }
    if install(&latest) {
        Ok(())
    } else {
        Err(io::Error::other("The update did not finish."))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Applied {
    Installed,
    ReplacedInPlace { executable: PathBuf },
    HandedOff,
}

pub(crate) fn finished_message(latest: &str, applied: Applied) -> String {
    match applied {
        Applied::Installed => {
            format!("Agent Command updated to {latest}. Run agent-command to open the new version.")
        }
        Applied::ReplacedInPlace { .. } => {
            format!("Agent Command updated to {latest} in place. Relaunching in this terminal.")
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
        let executable = self_update_windows(latest, |_| {})?;
        return Ok(Applied::ReplacedInPlace { executable });
    }
    if install(latest) {
        Ok(Applied::Installed)
    } else {
        Err(io::Error::other("the installer did not finish"))
    }
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

    #[test]
    fn update_state_labels_and_release_evaluation_cover_all_states() {
        assert_eq!(
            evaluate_release_check(Ok("v0.9.0".into()), "0.2.2"),
            UpdateState::Available {
                tag: "v0.9.0".into(),
                current: "0.2.2".into(),
            }
        );
        assert_eq!(
            evaluate_release_check(Ok("v0.2.2".into()), "0.2.2"),
            UpdateState::UpToDate {
                current: "0.2.2".into(),
            }
        );
        assert!(matches!(
            evaluate_release_check(Err(io::Error::other("offline")), "0.2.2"),
            UpdateState::Failed(_)
        ));
        assert!(UpdateState::Checking.pill_label().contains("checking"));
        assert!(UpdateState::UpToDate {
            current: "0.2.2".into()
        }
        .pill_label()
        .contains("v0.2.2 ✓"));
        assert!(UpdateState::Available {
            tag: "v0.3.0".into(),
            current: "0.2.2".into()
        }
        .pill_label()
        .contains("Update v0.3.0"));
        assert!(UpdateState::Downloading { pct: 45 }
            .pill_label()
            .contains("45%"));
        assert!(UpdateState::Failed("net".into())
            .pill_label()
            .contains("offline"));
    }

    #[test]
    fn sha256_verification_and_in_place_rename_swap_and_cleanup_work() {
        let payload = b"MZ_new_binary_payload";
        let expected: String = Sha256::digest(payload)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let manifest = format!("{expected}  agent-command-x86_64-pc-windows-msvc.zip\n");
        assert!(verify_sha256_bytes(payload, &manifest).is_ok());
        assert!(verify_sha256_bytes(b"tampered", &manifest).is_err());
        assert!(verify_sha256_bytes(payload, "not-a-hash").is_err());

        let dir = env::temp_dir().join(format!("ac-swap-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let current = dir.join("agent-command.exe");
        let staged = dir.join("staged.exe");
        fs::write(&current, b"MZ_old_version").unwrap();
        fs::write(&staged, payload).unwrap();

        let backup = swap_executable_in_place(&current, &staged).unwrap();
        assert_eq!(backup, dir.join("agent-command.exe.old"));
        assert_eq!(fs::read(&current).unwrap(), payload);
        assert_eq!(fs::read(&backup).unwrap(), b"MZ_old_version");

        assert!(cleanup_old_at(&current).unwrap());
        assert!(!backup.exists());
        assert!(!cleanup_old_at(&current).unwrap());
        let _ = fs::remove_dir_all(&dir);
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
    fn finished_messages_name_the_new_version() {
        assert!(finished_message("9.9.9", Applied::Installed).contains("updated to 9.9.9"));
        assert!(finished_message(
            "9.9.9",
            Applied::ReplacedInPlace {
                executable: PathBuf::from("agent-command.exe")
            }
        )
        .contains("in place"));
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
