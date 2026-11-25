use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::Sender,
    thread,
    time::Instant,
};

use crate::{auth, branches, bridge, github, update};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChangedFile {
    pub status: String,
    pub path: String,
    pub added: u64,
    pub removed: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitSnapshot {
    pub branch: Option<String>,
    pub dirty: bool,
    pub files: Vec<ChangedFile>,
    pub unified_diff: String,
}

pub enum AppEvent {
    AuthDetected {
        detections: Vec<auth::AuthDetection>,
        profiles: Vec<auth::Profile>,
    },
    GitHubDetected(github::GitHubAuthStatus),
    UpdateChecked(update::UpdateState),
    GitSnapshotReady(GitSnapshot),
    ShellFinished {
        command: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
        duration_ms: u64,
    },
    WorkerWake(bridge::BackgroundResult),
}

pub fn collect_git_snapshot(workspace: &Path) -> GitSnapshot {
    let Some(branch) = branches::current_branch(workspace) else {
        return GitSnapshot::default();
    };
    let stat = branches::working_tree_stat(workspace).unwrap_or_default();
    let porcelain = branches::git(workspace, &["status", "--porcelain"]).unwrap_or_default();
    let dirty = !porcelain.trim().is_empty() || !stat.files.is_empty();
    let mut status_by_path = std::collections::HashMap::new();
    for line in porcelain.lines() {
        if line.len() >= 4 {
            let code = line[..2].trim();
            let path = line[3..].trim();
            let badge = if code.contains('?') {
                "A"
            } else if code.contains('D') {
                "D"
            } else {
                "M"
            };
            status_by_path.insert(path.to_owned(), badge.to_owned());
        }
    }
    let files: Vec<ChangedFile> = stat
        .files
        .into_iter()
        .map(|f| {
            let status = status_by_path.get(&f.path).cloned().unwrap_or_else(|| {
                if f.removed == 0 && f.added > 0 {
                    "A"
                } else {
                    "M"
                }
                .into()
            });
            ChangedFile {
                status,
                path: f.path,
                added: f.added,
                removed: f.removed,
            }
        })
        .collect();
    let unified_diff = branches::unified_diff(workspace, None).unwrap_or_default();
    GitSnapshot {
        branch: Some(branch),
        dirty,
        files,
        unified_diff,
    }
}

pub fn spawn_startup_probes(
    workspace: PathBuf,
    profile_root: Option<PathBuf>,
    check_updates: bool,
    tx: Sender<AppEvent>,
) {
    let auth_tx = tx.clone();
    thread::spawn(move || {
        let detections = auth::detect_system_defaults();
        let profiles = profile_root
            .as_deref()
            .and_then(|root| auth::all_profiles(root).ok())
            .unwrap_or_default();
        let _ = auth_tx.send(AppEvent::AuthDetected {
            detections,
            profiles,
        });
    });

    let gh_tx = tx.clone();
    let gh_workspace = workspace.clone();
    thread::spawn(move || {
        let status = github::detect(&gh_workspace, None);
        let _ = gh_tx.send(AppEvent::GitHubDetected(status));
    });

    if check_updates {
        spawn_update_check(tx.clone());
    }

    spawn_git_refresh(workspace, tx);
}

pub fn spawn_update_check(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let rx = update::check_now();
        if let Ok(state) = rx.recv() {
            let _ = tx.send(AppEvent::UpdateChecked(state));
        }
    });
}

pub fn spawn_git_refresh(workspace: PathBuf, tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let snapshot = collect_git_snapshot(&workspace);
        let _ = tx.send(AppEvent::GitSnapshotReady(snapshot));
    });
}

pub fn spawn_shell_command(workspace: PathBuf, command: String, tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let started = Instant::now();
        let output = if env::consts::OS == "windows" {
            Command::new("cmd")
                .args(["/C", &command])
                .current_dir(&workspace)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
        } else {
            Command::new("sh")
                .args(["-lc", &command])
                .current_dir(&workspace)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        match output {
            Ok(out) => {
                let _ = tx.send(AppEvent::ShellFinished {
                    command,
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    exit_code: out.status.code(),
                    duration_ms,
                });
            }
            Err(error) => {
                let _ = tx.send(AppEvent::ShellFinished {
                    command,
                    stdout: String::new(),
                    stderr: error.to_string(),
                    exit_code: None,
                    duration_ms,
                });
            }
        }
    });
}
