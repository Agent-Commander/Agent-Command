use crate::auth::Profile;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub(crate) struct Progress {
    pub(crate) assistant_messages: u64,
    pub(crate) completed_tool_events: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Checkpoint {
    version: u8,
    mission: Uuid,
    workspace: [u8; 32],
    interrupted_profile: String,
    progress: Progress,
}

impl Checkpoint {
    pub(crate) fn save(
        profile: &Profile,
        mission: Uuid,
        workspace: &Path,
        progress: Progress,
    ) -> io::Result<Self> {
        let state = profile.checkpoint_directory()?.canonicalize()?;
        if state.starts_with(workspace.canonicalize()?) {
            return Err(io::Error::other(
                "Place isolated profile state outside the Git workspace",
            ));
        }
        let checkpoint = Self {
            version: 1,
            mission,
            workspace: workspace_digest(workspace)?,
            interrupted_profile: profile.to_string(),
            progress,
        };
        let path = state.join(format!("{mission}.json"));
        write_new(&path, &checkpoint)?;
        Ok(checkpoint)
    }

    pub(crate) fn reserve(
        &self,
        profile: &Profile,
        workspace: &Path,
        alternate: &Profile,
    ) -> io::Result<()> {
        if self.version != 1
            || self.interrupted_profile != profile.to_string()
            || profile.provider != alternate.provider
            || self.interrupted_profile == alternate.to_string()
        {
            return Err(io::Error::other(
                "Checkpoint does not match this continuation",
            ));
        }
        let hash = Sha256::digest(serde_json::to_vec(self)?);
        let path = profile
            .checkpoint_directory()?
            .join(format!("{}-{hash:x}.claim", self.mission));
        if path.try_exists()? {
            return Err(claimed(&path));
        }
        if self.workspace != workspace_digest(workspace)? {
            return Err(io::Error::other(
                "Workspace changed since the interruption; continuation stopped",
            ));
        }
        write_new(&path, &alternate.to_string()).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                claimed(&path)
            } else {
                error
            }
        })
    }

    pub(crate) fn prompt(&self, task: &str) -> String {
        format!(
            "Continue the interrupted task using the existing workspace. The previous run emitted {} assistant messages and {} completed tool events. Inspect the retained work before acting, do not redo completed work or repeat external actions, and stop if prior side effects are uncertain.\n\nOriginal task:\n{}",
            self.progress.assistant_messages, self.progress.completed_tool_events, task
        )
    }
}

fn claimed(path: &Path) -> io::Error {
    let target = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<String>(&bytes).ok());
    io::Error::other(target.map_or_else(
        || "Continuation already claimed for this checkpoint".into(),
        |profile| format!("Continuation already claimed for this checkpoint by {profile}"),
    ))
}

fn write_new(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.sync_all()?;
    #[cfg(unix)]
    File::open(path.parent().expect("state path has a parent"))?.sync_all()?;
    Ok(())
}

fn git(workspace: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(workspace)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            "Continuation needs a Git workspace with an accessible index",
        ));
    }
    Ok(output.stdout)
}

fn add_field(hash: &mut Sha256, data: &[u8]) {
    hash.update((data.len() as u64).to_le_bytes());
    hash.update(data);
}

fn workspace_digest(workspace: &Path) -> io::Result<[u8; 32]> {
    let relative = git(workspace, &["rev-parse", "--show-prefix"])?;
    if relative != b"\n" && relative != b"\r\n" {
        return Err(io::Error::other(
            "Continuation requires the root of the Git workspace",
        ));
    }
    let mut hash = Sha256::new();
    add_field(
        &mut hash,
        workspace.canonicalize()?.as_os_str().as_encoded_bytes(),
    );
    let head = git(workspace, &["rev-parse", "--verify", "HEAD"])?;
    let index = git(workspace, &["ls-files", "-z", "--stage"])?;
    let files = git(
        workspace,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--deduplicate",
        ],
    )?;
    add_field(&mut hash, &head);
    add_field(&mut hash, &index);
    for entry in files
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        add_field(&mut hash, entry);
        let path = relative_path(entry)?;
        let mut file_path = workspace.to_path_buf();
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            file_path.push(component);
            match fs::symlink_metadata(&file_path) {
                Ok(metadata) if redirects(&metadata) => {
                    return Err(io::Error::other(
                        "Continuation cannot verify symbolic links",
                    ));
                }
                Ok(metadata) if components.peek().is_none() && metadata.is_file() => {
                    hash.update([1]);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        hash.update((metadata.permissions().mode() & 0o111).to_le_bytes());
                    }
                    let mut file = File::open(&file_path)?;
                    let mut file_hash = Sha256::new();
                    let mut buffer = [0; 16384];
                    loop {
                        let read = file.read(&mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        file_hash.update(&buffer[..read]);
                    }
                    hash.update(file_hash.finalize());
                }
                Ok(metadata) if components.peek().is_some() && metadata.is_dir() => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    hash.update([0]);
                    break;
                }
                Err(error) => return Err(error),
                Ok(_) => {
                    return Err(io::Error::other(
                        "Continuation cannot verify this workspace entry",
                    ))
                }
            }
        }
    }
    Ok(hash.finalize().into())
}

fn relative_path(entry: &[u8]) -> io::Result<PathBuf> {
    let path = PathBuf::from(std::str::from_utf8(entry).map_err(io::Error::other)?);
    if !path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(io::Error::other(
            "Continuation cannot verify this workspace path",
        ));
    }
    Ok(path)
}

#[cfg(unix)]
fn redirects(metadata: &fs::Metadata) -> bool {
    metadata.is_symlink()
}

#[cfg(windows)]
fn redirects(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}
