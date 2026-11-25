use crate::{
    auth,
    branches::{self, Branch, DiffStat},
    catalog, native,
    permissions::PermissionMode,
    review::{self, ReviewLedger},
    routing,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

pub(crate) const STATE_DIRECTORY: &str = "agent-command-orchestrator-v1";
const SCHEMA: &str = "agent-command-orchestrator-v1";
const ROSTER_FILE: &str = "roster.json";
const ROSTER_LOCK_FILE: &str = ".roster.lock";
const ROSTER_LOCK_WAIT_MS: u64 = 10;
const ROSTER_LOCK_ATTEMPTS: usize = 500;
const ROSTER_LOCK_STALE_SECS: u64 = 10;
pub(crate) fn workspace_hash(workspace: &Path) -> String {
    let canonical = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let normalized = canonical.to_string_lossy().to_ascii_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn workspace_commander_file(workspace: &Path) -> String {
    format!("commander-{}.jsonl", workspace_hash(workspace))
}

fn workspace_transcript_file(workspace: &Path) -> String {
    format!("transcript-{}.jsonl", workspace_hash(workspace))
}
const HISTORY_DIRECTORY: &str = "history";
const WORKTREE_DIRECTORY: &str = "worktrees";
const DIRECTORY_FALLBACK: &str = "workspaces";
pub const COMMANDER: &str = "Commander";
const COMMANDER_ROLE: &str = "entry";
const MAX_NAME_LENGTH: usize = 24;
pub(crate) const FEED_LIMIT: usize = 200;
const CONTEXT_LINES: usize = 12;
const SUMMARY_CHARS: usize = 160;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    #[default]
    Idle,
    Running,
    #[serde(rename = "NEEDS_APPROVAL")]
    NeedsApproval,
    Done,
    Error,
    Snoozed,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Running => "RUNNING",
            Self::NeedsApproval => "NEEDS_APPROVAL",
            Self::Done => "DONE",
            Self::Error => "ERROR",
            Self::Snoozed => "SNOOZED",
        }
    }

    pub fn caption(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::NeedsApproval => "needs approval",
            Self::Done => "done",
            Self::Error => "error",
            Self::Snoozed => "snoozed",
        }
    }

    pub fn needs_attention(self) -> bool {
        matches!(self, Self::NeedsApproval | Self::Error)
    }

    fn inbox_rank(self) -> u8 {
        match self {
            Self::NeedsApproval | Self::Error => 0,
            Self::Running | Self::Snoozed => 1,
            Self::Done => 2,
            Self::Idle => 3,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Isolation {
    Worktree { repository: PathBuf },
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub path: PathBuf,
    pub isolation: Isolation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<Branch>,
}

const NO_WORKSPACE: &str = "it has no isolated workspace";
const DETACHED_WORKTREE: &str = "its worktree is detached (created before named branches)";
const PLAIN_DIRECTORY: &str = "it works in a plain directory outside any Git repository";
const APPROVAL_MARKER: &str = "NEEDS_APPROVAL:";
const DIFF_FILES_SHOWN: usize = 6;

impl Workspace {
    fn named_branch(&self) -> Result<&Branch, &'static str> {
        match (&self.isolation, &self.branch) {
            (Isolation::Worktree { .. }, Some(branch)) => Ok(branch),
            (Isolation::Worktree { .. }, None) => Err(DETACHED_WORKTREE),
            (Isolation::Directory, _) => Err(PLAIN_DIRECTORY),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub at: String,
    pub text: String,
}

impl Entry {
    pub(crate) fn now(text: String) -> Self {
        Self {
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            text,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Member {
    pub name: String,
    pub role: String,
    pub status: Status,
    pub mission: String,
    pub paused: bool,
    pub settled: bool,
    pub touched: u64,
    pub workspace: Option<Workspace>,
    pub changes: Option<DiffStat>,
    pub history: VecDeque<Entry>,
    pub(crate) permission: PermissionMode,
    pub(crate) review: ReviewLedger,
}

impl Member {
    pub fn commander() -> Self {
        Self::new(COMMANDER, COMMANDER_ROLE, None, None)
    }

    pub(crate) fn new(
        name: &str,
        role: &str,
        task: Option<&str>,
        workspace: Option<Workspace>,
    ) -> Self {
        Self {
            name: name.into(),
            role: role.into(),
            status: Status::Idle,
            mission: task.unwrap_or("Standing by").into(),
            paused: false,
            settled: false,
            touched: 0,
            workspace,
            changes: None,
            history: VecDeque::new(),
            permission: PermissionMode::default_for_role(role),
            review: ReviewLedger::default(),
        }
    }

    pub(crate) fn with_permission(mut self, permission: Option<PermissionMode>) -> Self {
        if let Some(permission) = permission {
            self.permission = permission;
        }
        self
    }

    pub fn is_commander(&self) -> bool {
        self.name == COMMANDER
    }

    fn named_branch(&self) -> Result<(&Workspace, &Branch), &'static str> {
        let workspace = self.workspace.as_ref().ok_or(NO_WORKSPACE)?;
        Ok((workspace, workspace.named_branch()?))
    }

    pub(crate) fn diff(&self) -> Result<DiffStat, String> {
        let (workspace, branch) = self
            .named_branch()
            .map_err(|reason| format!("No diff for {}: {reason}.", self.name))?;
        branches::diff_stat(&workspace.path, branch).map_err(|error| error.to_string())
    }

    pub(crate) fn refresh_changes(&mut self) {
        self.changes = self.diff().ok();
    }

    pub fn branch(&self) -> Option<&Branch> {
        self.workspace.as_ref()?.branch.as_ref()
    }

    pub(crate) fn remember(&mut self, entry: Entry) {
        self.history.push_back(entry);
        while self.history.len() > FEED_LIMIT {
            self.history.pop_front();
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RosterFile {
    schema: String,
    specialists: Vec<Record>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    name: String,
    role: String,
    task: String,
    status: Status,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    settled: bool,
    workspace: Option<Workspace>,
    #[serde(default)]
    permission: Option<PermissionMode>,
    #[serde(default)]
    review: ReviewLedger,
}

#[derive(Clone, Debug)]
pub(crate) struct Store {
    root: PathBuf,
}

impl Store {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn existing(&self) -> io::Result<Option<PathBuf>> {
        auth::existing_private_state(&self.root, STATE_DIRECTORY)
    }

    fn directory(&self) -> io::Result<PathBuf> {
        auth::private_state_directory(&self.root, STATE_DIRECTORY)
    }

    fn subdirectory(&self, name: &str) -> io::Result<PathBuf> {
        auth::private_state_directory(&self.directory()?, name)
    }

    pub(crate) fn ledger_path(&self, workspace: &Path) -> io::Result<PathBuf> {
        let digest = Sha256::digest(workspace.to_string_lossy().as_bytes());
        let tag: String = digest
            .iter()
            .take(6)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let stem = crate::ledger::LEDGER_FILE.trim_end_matches(".json");
        Ok(self.directory()?.join(format!("{stem}-{tag}.json")))
    }

    pub(crate) fn load_roster(&self) -> io::Result<Vec<Member>> {
        let Some(directory) = self.existing()? else {
            return Ok(Vec::new());
        };
        let bytes = match fs::read(directory.join(ROSTER_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let roster: RosterFile = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if roster.schema != SCHEMA {
            return Err(io::Error::other(format!(
                "Unsupported roster schema {}",
                roster.schema
            )));
        }
        roster
            .specialists
            .into_iter()
            .map(|record| self.restore(record, &directory))
            .collect()
    }

    fn restore(&self, record: Record, directory: &Path) -> io::Result<Member> {
        let mut member = Member::new(
            &record.name,
            &record.role,
            Some(&record.task),
            record.workspace,
        );
        member.status = match record.status {
            Status::Running => Status::Error,
            status => status,
        };
        member.settled = record.settled;
        member.refresh_changes();
        member.permission = record.permission.unwrap_or(member.permission);
        member.review = record.review;
        member.history = read_entries(&history_path(directory, &record.name))?.into();
        Ok(member)
    }

    pub(crate) fn save_roster(&self, crew: &[Member]) -> io::Result<()> {
        let directory = self.directory()?;
        let _lock = RosterLock::acquire(&directory)?;
        write_roster_file(&directory, crew)
    }

    pub(crate) fn save_roster_merging(&self, crew: &[Member], removed: &[&str]) -> io::Result<()> {
        let directory = self.directory()?;
        let _lock = RosterLock::acquire(&directory)?;
        let existing = read_roster_records(&directory)?;
        let mut merged: Vec<Record> = Vec::new();
        for record in existing {
            let is_removed = removed
                .iter()
                .any(|name| record.name.eq_ignore_ascii_case(name));
            let in_crew = crew.iter().any(|member| {
                !member.is_commander() && member.name.eq_ignore_ascii_case(&record.name)
            });
            if !is_removed && !in_crew {
                merged.push(record);
            }
        }
        for member in crew.iter().filter(|member| !member.is_commander()) {
            merged.push(record_from_member(member));
        }
        write_roster_records(&directory, merged)
    }

    pub(crate) fn load_commander_for_workspace(&self, workspace: &Path) -> io::Result<Vec<Entry>> {
        match self.existing()? {
            Some(directory) => read_entries(&directory.join(workspace_commander_file(workspace))),
            None => Ok(Vec::new()),
        }
    }

    pub(crate) fn append_commander_for_workspace(
        &self,
        workspace: &Path,
        entry: &Entry,
    ) -> io::Result<()> {
        append_entry(
            &self.directory()?.join(workspace_commander_file(workspace)),
            entry,
        )
    }

    pub(crate) fn load_transcript_for_workspace(
        &self,
        workspace: &Path,
    ) -> io::Result<Option<Vec<crate::markdown::TranscriptEntry>>> {
        let Some(directory) = self.existing()? else {
            return Ok(None);
        };
        let path = directory.join(workspace_transcript_file(workspace));
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut entries = VecDeque::new();
        for line in BufReader::new(file).lines() {
            let text = line?;
            if text.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<crate::markdown::TranscriptEntry>(&text) {
                entries.push_back(entry);
                if entries.len() > FEED_LIMIT {
                    entries.pop_front();
                }
            }
        }
        Ok(Some(crate::markdown::Transcript::normalize_loaded(
            entries.into(),
        )))
    }

    pub(crate) fn save_transcript_for_workspace(
        &self,
        workspace: &Path,
        entries: &[crate::markdown::TranscriptEntry],
    ) -> io::Result<()> {
        let path = self.directory()?.join(workspace_transcript_file(workspace));
        let start = entries.len().saturating_sub(FEED_LIMIT);
        let mut buf = Vec::new();
        for entry in &entries[start..] {
            serde_json::to_writer(&mut buf, entry)?;
            buf.push(b'\n');
        }
        let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
        let mut file = fs::File::create_new(&temporary)?;
        file.write_all(&buf)?;
        file.sync_all()?;
        drop(file);
        if fs::rename(&temporary, &path).is_err() {
            fs::write(&path, &buf)?;
            let _ = fs::remove_file(&temporary);
        }
        Ok(())
    }

    pub(crate) fn append_history(&self, name: &str, entry: &Entry) -> io::Result<()> {
        self.subdirectory(HISTORY_DIRECTORY)?;
        append_entry(&history_path(&self.directory()?, name), entry)
    }

    pub(crate) fn isolate(&self, base: &Path, name: &str, topic: &str) -> io::Result<Workspace> {
        let key = name.to_ascii_lowercase();
        match repository_root(base) {
            Some(repository) => {
                let path = self.subdirectory(WORKTREE_DIRECTORY)?.join(&key);
                let branch = branches::add_worktree(&repository, &path, name, topic)?;
                Ok(Workspace {
                    path,
                    isolation: Isolation::Worktree { repository },
                    branch: Some(branch),
                })
            }
            None => {
                let path = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
                Ok(Workspace {
                    path,
                    isolation: Isolation::Directory,
                    branch: None,
                })
            }
        }
    }

    pub(crate) fn isolate_for(
        &self,
        base: &Path,
        name: &str,
        topic: &str,
        mode: PermissionMode,
    ) -> Result<Workspace, String> {
        let workspace = self
            .isolate(base, name, topic)
            .map_err(|error| format!("Cannot isolate {name}: {error}"))?;
        if let Err(refusal) = mode.require_isolation(Some(&workspace.isolation)) {
            let fallback = self
                .existing()
                .ok()
                .flatten()
                .map(|dir| dir.join(DIRECTORY_FALLBACK));
            let _ = release_workspace(&workspace, fallback.as_deref());
            return Err(refusal);
        }
        Ok(workspace)
    }

    fn catalog(&self) -> catalog::Catalog {
        let state = auth::existing_private_state(&self.root, routing::STATE_DIRECTORY)
            .ok()
            .flatten();
        catalog::Catalog::load(state.as_deref())
    }

    fn pins(&self) -> native::Pins {
        native::load_existing(&self.root).unwrap_or_default()
    }

    pub(crate) fn release(&self, member: &Member) -> io::Result<Option<String>> {
        let mut retained = None;
        let existing_dir = self.existing()?;
        let fallback_dir = existing_dir
            .as_ref()
            .map(|directory| directory.join(DIRECTORY_FALLBACK));
        if let Some(workspace) = &member.workspace {
            retained = release_workspace(workspace, fallback_dir.as_deref())?;
        }
        if let Some(directory) = existing_dir {
            remove_if_present(&history_path(&directory, &member.name), |path| {
                fs::remove_file(path)
            })?;
        }
        Ok(retained)
    }
}

struct RosterLock {
    path: PathBuf,
}

impl RosterLock {
    fn acquire(directory: &Path) -> io::Result<Self> {
        let path = directory.join(ROSTER_LOCK_FILE);
        for _ in 0..ROSTER_LOCK_ATTEMPTS {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| {
                            age >= std::time::Duration::from_secs(ROSTER_LOCK_STALE_SECS)
                        });
                    if stale {
                        let _ = fs::remove_file(&path);
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(ROSTER_LOCK_WAIT_MS));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("Timed out waiting for roster lock {}", path.display()),
        ))
    }
}

impl Drop for RosterLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn record_from_member(member: &Member) -> Record {
    Record {
        name: member.name.clone(),
        role: member.role.clone(),
        task: member.mission.clone(),
        status: member.status,
        settled: member.settled,
        workspace: member.workspace.clone(),
        permission: Some(member.permission),
        review: member.review.clone(),
    }
}

fn read_roster_records(directory: &Path) -> io::Result<Vec<Record>> {
    let bytes = match fs::read(directory.join(ROSTER_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let roster: RosterFile = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if roster.schema != SCHEMA {
        return Err(io::Error::other(format!(
            "Unsupported roster schema {}",
            roster.schema
        )));
    }
    Ok(roster.specialists)
}

fn write_roster_file(directory: &Path, crew: &[Member]) -> io::Result<()> {
    let records: Vec<Record> = crew
        .iter()
        .filter(|member| !member.is_commander())
        .map(record_from_member)
        .collect();
    write_roster_records(directory, records)
}

fn write_roster_records(directory: &Path, specialists: Vec<Record>) -> io::Result<()> {
    let roster = RosterFile {
        schema: SCHEMA.into(),
        specialists,
    };
    let path = directory.join(ROSTER_FILE);
    let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let mut file = fs::File::create_new(&temporary)?;
    serde_json::to_writer_pretty(&mut file, &roster)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)
}

pub(crate) fn removal_note(name: &str, retained: Option<&str>) -> String {
    match retained {
        Some(branch) => format!("Removed {name}; kept unmerged branch {branch}."),
        None => format!("Removed {name}."),
    }
}

fn release_workspace(
    workspace: &Workspace,
    managed_fallback: Option<&Path>,
) -> io::Result<Option<String>> {
    match &workspace.isolation {
        Isolation::Worktree { repository } => {
            branches::git(
                repository,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    branches::path_text(&workspace.path)?,
                ],
            )?;
            Ok(workspace
                .branch
                .as_ref()
                .filter(|branch| !branches::delete_if_merged(repository, &branch.name))
                .map(|branch| branch.name.clone()))
        }
        Isolation::Directory => {
            if let Some(managed_root) = managed_fallback {
                let managed = managed_root
                    .canonicalize()
                    .unwrap_or_else(|_| managed_root.to_path_buf());
                let target = workspace
                    .path
                    .canonicalize()
                    .unwrap_or_else(|_| workspace.path.clone());
                if target.starts_with(&managed) && target != managed {
                    remove_if_present(&workspace.path, |path| fs::remove_dir_all(path))?;
                }
            }
            Ok(None)
        }
    }
}

fn remove_if_present(path: &Path, remove: fn(&Path) -> io::Result<()>) -> io::Result<()> {
    match remove(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn history_path(directory: &Path, name: &str) -> PathBuf {
    directory
        .join(HISTORY_DIRECTORY)
        .join(format!("{}.jsonl", name.to_ascii_lowercase()))
}

fn read_entries(path: &Path) -> io::Result<Vec<Entry>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut entries = VecDeque::new();
    for line in BufReader::new(file).lines() {
        if let Ok(entry) = serde_json::from_str::<Entry>(&line?) {
            entries.push_back(entry);
            if entries.len() > FEED_LIMIT {
                entries.pop_front();
            }
        }
    }
    Ok(entries.into())
}

fn append_entry(path: &Path, entry: &Entry) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    file.write_all(&line)
}

fn repository_root(base: &Path) -> Option<PathBuf> {
    branches::git(base, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(PathBuf::from)
}

pub(crate) fn find(crew: &[Member], name: &str) -> Option<usize> {
    crew.iter()
        .position(|member| member.name.eq_ignore_ascii_case(name))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RosterRow {
    Member(usize),
    Settled { count: usize, open: bool },
}

impl RosterRow {
    pub(crate) fn member(self) -> Option<usize> {
        match self {
            Self::Member(index) => Some(index),
            Self::Settled { .. } => None,
        }
    }
}

fn inbox_key(member: &Member) -> (u8, std::cmp::Reverse<u64>) {
    (
        member.status.inbox_rank(),
        std::cmp::Reverse(member.touched),
    )
}

fn sorted_by_inbox(crew: &[Member], settled: bool) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..crew.len())
        .filter(|index| !crew[*index].is_commander() && crew[*index].settled == settled)
        .collect();
    indices.sort_by_key(|index| inbox_key(&crew[*index]));
    indices
}

pub(crate) fn inbox_rows(crew: &[Member], open: bool) -> Vec<RosterRow> {
    let mut rows: Vec<RosterRow> = (0..crew.len())
        .filter(|index| crew[*index].is_commander())
        .map(RosterRow::Member)
        .collect();
    rows.extend(
        sorted_by_inbox(crew, false)
            .into_iter()
            .map(RosterRow::Member),
    );
    let settled = sorted_by_inbox(crew, true);
    if !settled.is_empty() {
        rows.push(RosterRow::Settled {
            count: settled.len(),
            open,
        });
        if open {
            rows.extend(settled.into_iter().map(RosterRow::Member));
        }
    }
    rows
}

pub(crate) fn settle(member: &mut Member, settled: bool) -> Result<String, String> {
    if member.is_commander() {
        return Err("Commander is always active and cannot be settled.".into());
    }
    if settled && member.status == Status::Running {
        return Err(format!(
            "{} is running; settle it after it finishes.",
            member.name
        ));
    }
    if member.settled == settled {
        let state = if settled { "already" } else { "not" };
        return Err(format!("{} is {state} settled.", member.name));
    }
    member.settled = settled;
    Ok(if settled {
        format!(
            "Settled {}; history kept in the Settled drawer.",
            member.name
        )
    } else {
        format!("{} is back in the active queue.", member.name)
    })
}

pub(crate) fn approval_request(reply: &str) -> Option<String> {
    reply.lines().rev().find_map(|line| {
        let question = line.trim().strip_prefix(APPROVAL_MARKER)?.trim();
        Some(if question.is_empty() {
            "approval requested".to_owned()
        } else {
            question.to_owned()
        })
    })
}

pub(crate) fn diff_report(member: &Member, stat: &DiffStat) -> String {
    let branch = member.branch().map_or_else(String::new, |branch| {
        format!(" · {} vs {}", branch.name, branch.base)
    });
    let files: Vec<String> = stat
        .files
        .iter()
        .take(DIFF_FILES_SHOWN)
        .map(|file| format!("{} +{} −{}", file.path, file.added, file.removed))
        .collect();
    let more = stat.files.len().saturating_sub(DIFF_FILES_SHOWN);
    let tail = match (files.is_empty(), more) {
        (true, _) => " · no changes".to_owned(),
        (false, 0) => format!(" · {}", files.join(", ")),
        (false, more) => format!(" · {} and {more} more", files.join(", ")),
    };
    format!("{}{branch} · {}{tail}", member.name, stat.label())
}

pub(crate) fn publish(
    member: &Member,
    commit: bool,
    tools: &branches::Tools,
) -> Result<branches::Published, String> {
    if member.status == Status::Running {
        return Err(format!(
            "{} is running; open the PR after it finishes.",
            member.name
        ));
    }
    let (workspace, branch) = member
        .named_branch()
        .map_err(|reason| format!("No PR for {}: {reason}.", member.name))?;
    branches::publish(
        &branches::PullRequest {
            specialist: &member.name,
            role: &member.role,
            mission: &member.mission,
            worktree: &workspace.path,
            branch,
            commit,
        },
        tools,
    )
}

pub(crate) fn validate_name(name: &str, crew: &[Member]) -> Result<(), String> {
    let well_formed = (1..=MAX_NAME_LENGTH).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !well_formed {
        return Err(format!(
            "Specialist names use 1–{MAX_NAME_LENGTH} ASCII letters, digits, - or _, starting with a letter."
        ));
    }
    if name.eq_ignore_ascii_case(COMMANDER) || find(crew, name).is_some() {
        return Err(format!("{name} is already on the roster."));
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Spawn {
        name: String,
        role: String,
        mode: Option<PermissionMode>,
        task: Option<String>,
    },
    Review {
        name: String,
        focus: Option<String>,
        next_revision: bool,
    },
    Ask {
        name: String,
        task: String,
    },
    Roster,
    Remove {
        name: String,
    },
    Entry {
        profile: String,
    },
    Diff {
        name: Option<String>,
    },
    Pr {
        name: Option<String>,
        commit: bool,
    },
    Settle {
        name: String,
        settled: bool,
    },
    SettledDrawer,
    Settings,
    Connect,
    Update,
    Disconnect {
        target: Option<String>,
    },
    Reconnect {
        target: Option<String>,
    },
    Side {
        question: String,
    },
    Goal {
        criteria: String,
    },
    GoalCancel,
    Mode {
        mode: PermissionMode,
    },
    Help,
    Unknown {
        command: String,
        suggestion: Option<String>,
    },
    Message(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandHelp {
    pub usage: &'static str,
    pub summary: &'static str,
}

const HELP_ENTRIES: [CommandHelp; 21] = [
    CommandHelp {
        usage: "/help",
        summary: "Show all slash commands and routing prefixes.",
    },
    CommandHelp {
        usage: "/spawn NAME ROLE [--mode MODE] [TASK]",
        summary: "Spawn a specialist in a git worktree (or project dir if not a git repo).",
    },
    CommandHelp {
        usage: "/ask NAME TASK",
        summary: "Delegate or queue a task on an existing specialist.",
    },
    CommandHelp {
        usage: "/review NAME [--next-revision] [FOCUS]",
        summary: "Run a read-only cross-family review of a specialist's changes.",
    },
    CommandHelp {
        usage: "/roster",
        summary: "Show active specialists and entry profile.",
    },
    CommandHelp {
        usage: "/diff [NAME]",
        summary: "Show changed files and line counts for a specialist branch.",
    },
    CommandHelp {
        usage: "/pr [NAME] [--commit]",
        summary: "Push a specialist branch and open a pull request with gh.",
    },
    CommandHelp {
        usage: "/settle NAME",
        summary: "Move a finished specialist into the Settled drawer.",
    },
    CommandHelp {
        usage: "/unsettle NAME",
        summary: "Return a settled specialist to the active queue.",
    },
    CommandHelp {
        usage: "/settled",
        summary: "Toggle the Settled drawer.",
    },
    CommandHelp {
        usage: "/remove NAME",
        summary: "Remove a specialist and clean up its worktree.",
    },
    CommandHelp {
        usage: "/entry PROVIDER/PROFILE|auto",
        summary: "Pin or reset the Commander entry profile.",
    },
    CommandHelp {
        usage: "/mode read-only|supervised|auto-edits|workspace-auto",
        summary: "Set Commander permission mode.",
    },
    CommandHelp {
        usage: "/btw QUESTION (alias /side)",
        summary: "Ask a side question without interrupting running work.",
    },
    CommandHelp {
        usage: "/goal CRITERIA | /goal cancel",
        summary: "Pin a multi-turn goal loop or cancel the active goal.",
    },
    CommandHelp {
        usage: "/settings",
        summary: "Open Settings panel.",
    },
    CommandHelp {
        usage: "/connect",
        summary: "Connect or add provider profiles.",
    },
    CommandHelp {
        usage: "/disconnect [TARGET]",
        summary: "Disconnect a provider profile.",
    },
    CommandHelp {
        usage: "/reconnect [TARGET]",
        summary: "Reconnect a provider profile.",
    },
    CommandHelp {
        usage: "/update",
        summary: "Check for and install an Agent Command update.",
    },
    CommandHelp {
        usage: "/hard | /standard | /any | /urgent | /ordinary | /split",
        summary: "Optional routing prefixes placed before a normal message.",
    },
];

const KNOWN_SLASH_COMMANDS: [&str; 27] = [
    "/help",
    "/spawn",
    "/ask",
    "/review",
    "/roster",
    "/diff",
    "/pr",
    "/settle",
    "/unsettle",
    "/settled",
    "/remove",
    "/entry",
    "/mode",
    "/btw",
    "/side",
    "/goal",
    "/settings",
    "/connect",
    "/disconnect",
    "/reconnect",
    "/update",
    "/hard",
    "/standard",
    "/any",
    "/urgent",
    "/ordinary",
    "/split",
];

const ROUTING_DIRECTIVES: [&str; 6] = [
    "/urgent",
    "/ordinary",
    "/any",
    "/standard",
    "/hard",
    "/split",
];

pub fn help_entries() -> &'static [CommandHelp] {
    &HELP_ENTRIES
}

pub fn format_help() -> String {
    let mut lines = vec!["Commands:".to_owned()];
    for entry in HELP_ENTRIES {
        lines.push(format!("  {:<48} {}", entry.usage, entry.summary));
    }
    lines.join("\n")
}

pub fn closest_command(input: &str) -> Option<String> {
    let lowered = input.trim().to_ascii_lowercase();
    if lowered == "/" {
        return Some("/help".into());
    }
    let query = lowered.strip_prefix('/').unwrap_or(&lowered);
    if query.is_empty() {
        return Some("/help".into());
    }
    let mut best: Option<(usize, &str)> = None;
    for candidate in KNOWN_SLASH_COMMANDS {
        let name = candidate.strip_prefix('/').unwrap_or(candidate);
        let distance = levenshtein(query, name);
        let prefix_bonus = name.starts_with(query) && query.len() >= 2;
        if distance <= 2 || prefix_bonus {
            let rank = if prefix_bonus {
                distance.min(1)
            } else {
                distance
            };
            if best.is_none_or(|(best_rank, _)| rank < best_rank) {
                best = Some((rank, candidate));
            }
        }
    }
    best.map(|(_, candidate)| candidate.to_owned())
}

pub fn unknown_command_message(command: &str, suggestion: Option<&str>) -> String {
    match suggestion {
        Some(candidate) => {
            format!("Unknown command {command}. Did you mean {candidate}? Try /help.")
        }
        None => format!("Unknown command {command}. Try /help to see available commands."),
    }
}

fn levenshtein(left: &str, right: &str) -> usize {
    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();
    if left_chars.is_empty() {
        return right_chars.len();
    }
    if right_chars.is_empty() {
        return left_chars.len();
    }
    let mut previous: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current = vec![0; right_chars.len() + 1];
    for (i, left_ch) in left_chars.iter().enumerate() {
        current[0] = i + 1;
        for (j, right_ch) in right_chars.iter().enumerate() {
            let substitution = previous[j] + usize::from(left_ch != right_ch);
            let deletion = previous[j + 1] + 1;
            let insertion = current[j] + 1;
            current[j + 1] = substitution.min(deletion).min(insertion);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right_chars.len()]
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalTurn {
    pub display_label: String,
    pub model_prompt: String,
}

pub fn format_goal_turn(criteria: &str, turn: u32, cap: u32, marker: &str) -> GoalTurn {
    let clean = criteria.split_whitespace().collect::<Vec<_>>().join(" ");
    let display_label = format!("Goal ({turn}/{cap}): {clean}");
    let model_prompt = format!(
        "Pinned goal (turn {turn} of at most {cap}): {clean}\n\n\
         Work toward this goal now. When the goal is fully met, end your reply with a line containing exactly {marker}. \
         Otherwise say briefly what remains; you will get another turn."
    );
    GoalTurn {
        display_label,
        model_prompt,
    }
}

pub fn split_goal_prompt(prompt: &str) -> (String, String) {
    let mut lines = prompt.lines();
    let first = lines.next().unwrap_or_default().trim().to_owned();
    let rest: Vec<&str> = lines
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    (first, rest.join(" "))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetCandidate {
    pub id: String,
    pub name: String,
    pub role: String,
    pub topic: String,
    pub recency_rank: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetMatch {
    Chosen(String),
    Ambiguous(Vec<String>),
    None,
}

const TARGET_AMBIGUITY_MARGIN: f64 = 0.6;
const TARGET_RECENCY_SCALE: f64 = 0.4;
const TARGET_AMBIGUOUS_LIMIT: usize = 3;

const TARGET_STOP_WORDS: [&str; 32] = [
    "the", "and", "for", "with", "this", "that", "from", "into", "about", "your", "have", "what",
    "when", "where", "which", "please", "could", "would", "should", "check", "update", "status",
    "tell", "show", "give", "make", "work", "task", "focus", "switch", "open", "back",
];

pub fn resolve_target(query: &str, candidates: &[TargetCandidate]) -> TargetMatch {
    let terms = extract_topic_terms(query);
    if terms.is_empty() || candidates.is_empty() {
        return TargetMatch::None;
    }
    let mut scored: Vec<(f64, usize, &str)> = candidates
        .iter()
        .filter_map(|candidate| {
            let name_words = fold_words(&candidate.name);
            let role_words = fold_words(&candidate.role);
            let topic_words = fold_words(&candidate.topic);
            let mut topic_score = 0.0;
            for term in &terms {
                if candidate.name.eq_ignore_ascii_case(term) {
                    topic_score += 3.0;
                } else if name_words.iter().any(|word| word == term) {
                    topic_score += 1.5;
                } else if role_words.iter().any(|word| word == term) {
                    topic_score += 1.25;
                } else if topic_words.iter().any(|word| word == term) {
                    topic_score += 1.0;
                }
            }
            if topic_score <= 0.0 {
                return None;
            }
            let recency_boost = TARGET_RECENCY_SCALE / (1.0 + candidate.recency_rank as f64);
            Some((
                topic_score + recency_boost,
                candidate.recency_rank,
                candidate.id.as_str(),
            ))
        })
        .collect();
    if scored.is_empty() {
        return TargetMatch::None;
    }
    scored.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(right.2))
    });
    if scored.len() == 1 || (scored[0].0 - scored[1].0) >= TARGET_AMBIGUITY_MARGIN {
        TargetMatch::Chosen(scored[0].2.to_owned())
    } else {
        TargetMatch::Ambiguous(
            scored
                .into_iter()
                .take(TARGET_AMBIGUOUS_LIMIT)
                .map(|(_, _, id)| id.to_owned())
                .collect(),
        )
    }
}

pub fn resolve_member_target(query: &str, crew: &[Member]) -> TargetMatch {
    let mut active: Vec<&Member> = crew
        .iter()
        .filter(|member| !member.is_commander())
        .collect();
    active.sort_by(|left, right| {
        right
            .touched
            .cmp(&left.touched)
            .then_with(|| left.name.cmp(&right.name))
    });
    let candidates: Vec<TargetCandidate> = active
        .into_iter()
        .enumerate()
        .map(|(rank, member)| TargetCandidate {
            id: member.name.clone(),
            name: member.name.clone(),
            role: member.role.clone(),
            topic: member.mission.clone(),
            recency_rank: rank,
        })
        .collect();
    resolve_target(query, &candidates)
}

fn extract_topic_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for raw in split_token_spans(text) {
        let is_two_letter_acronym =
            raw.len() == 2 && raw.bytes().all(|byte| byte.is_ascii_uppercase());
        let is_version = raw.chars().any(|ch| ch.is_ascii_digit())
            && raw
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-');
        let folded = raw.to_ascii_lowercase();
        let keep = if is_two_letter_acronym || is_version {
            true
        } else {
            folded.chars().count() >= 3 && !TARGET_STOP_WORDS.contains(&folded.as_str())
        };
        if keep && !terms.contains(&folded) {
            terms.push(folded);
        }
    }
    terms
}

fn fold_words(text: &str) -> Vec<String> {
    split_token_spans(text)
        .into_iter()
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn split_token_spans(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    for (index, ch) in text.char_indices() {
        let keep = ch.is_alphanumeric() || ch == '.';
        if keep {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(begin) = start.take() {
            let trimmed = text[begin..index].trim_matches('.');
            if !trimmed.is_empty() {
                tokens.push(trimmed);
            }
        }
    }
    if let Some(begin) = start {
        let trimmed = text[begin..].trim_matches('.');
        if !trimmed.is_empty() {
            tokens.push(trimmed);
        }
    }
    tokens
}

const HELP_USAGE: &str = "Usage: /help";

const SPAWN_USAGE: &str =
    "Usage: /spawn NAME ROLE [--mode read-only|supervised|auto-edits|worktree-auto|workspace-auto] [TASK]";
const REVIEW_USAGE: &str = "Usage: /review NAME [--next-revision] [FOCUS]";
const MODE_FLAG: &str = "--mode";
const NEXT_REVISION_FLAG: &str = "--next-revision";
const ASK_USAGE: &str = "Usage: /ask NAME TASK";
const REMOVE_USAGE: &str = "Usage: /remove NAME";
const ENTRY_USAGE: &str = "Usage: /entry PROVIDER/PROFILE or /entry auto";
const SIDE_USAGE: &str = "Usage: /btw QUESTION (alias /side QUESTION)";
const GOAL_USAGE: &str = "Usage: /goal CRITERIA or /goal cancel";
const DIFF_USAGE: &str = "Usage: /diff [NAME]";
const PR_USAGE: &str = "Usage: /pr [NAME] [--commit]";
const SETTLE_USAGE: &str = "Usage: /settle NAME or /unsettle NAME";
pub(crate) const UPDATE_USAGE: &str = "Usage: /update";
pub(crate) const DISCONNECT_USAGE: &str = "Usage: /disconnect [claude|codex|PROVIDER/PROFILE]";
pub(crate) const RECONNECT_USAGE: &str = "Usage: /reconnect [claude|codex|PROVIDER/PROFILE]";
pub(crate) const COMMIT_FLAG: &str = "--commit";

fn optional_name(rest: &str, usage: &str) -> Result<Option<String>, String> {
    match next_word(rest) {
        None => Ok(None),
        Some((name, "")) => Ok(Some(name.into())),
        Some(_) => Err(usage.into()),
    }
}

fn parse_pr(rest: &str) -> Result<Command, String> {
    let words: Vec<&str> = rest.split_whitespace().collect();
    let commit = words.contains(&COMMIT_FLAG);
    let names: Vec<&str> = words
        .into_iter()
        .filter(|word| *word != COMMIT_FLAG)
        .collect();
    match names[..] {
        [] => Ok(Command::Pr { name: None, commit }),
        [name] if !name.starts_with('-') => Ok(Command::Pr {
            name: Some(name.into()),
            commit,
        }),
        _ => Err(PR_USAGE.into()),
    }
}

fn parse_settle(rest: &str, settled: bool) -> Result<Command, String> {
    match next_word(rest) {
        Some((name, "")) => Ok(Command::Settle {
            name: name.into(),
            settled,
        }),
        _ => Err(SETTLE_USAGE.into()),
    }
}

fn next_word(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if text.is_empty() {
        return None;
    }
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    Some((&text[..end], text[end..].trim()))
}

fn optional(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

fn mode_option(text: &str) -> Result<(Option<PermissionMode>, &str), String> {
    let Some((word, rest)) = next_word(text) else {
        return Ok((None, ""));
    };
    if let Some(value) = word.strip_prefix("--mode=") {
        return Ok((Some(PermissionMode::parse(value)?), rest));
    }
    if word != MODE_FLAG {
        return Ok((None, text.trim()));
    }
    let (value, rest) = next_word(rest).ok_or_else(|| SPAWN_USAGE.to_owned())?;
    Ok((Some(PermissionMode::parse(value)?), rest))
}

fn flag_option<'a>(text: &'a str, flag: &str) -> (bool, &'a str) {
    match next_word(text) {
        Some((word, rest)) if word == flag => (true, rest),
        _ => (false, text.trim()),
    }
}

pub(crate) fn parse_command(input: &str) -> Result<Command, String> {
    let input = input.trim();
    let Some((head, rest)) = next_word(input) else {
        return Ok(Command::Message(String::new()));
    };
    match head {
        "/spawn" => {
            let (name, rest) = next_word(rest).ok_or(SPAWN_USAGE)?;
            let (role, rest) = next_word(rest).ok_or(SPAWN_USAGE)?;
            let (mode, task) = mode_option(rest)?;
            Ok(Command::Spawn {
                name: name.into(),
                role: role.into(),
                mode,
                task: optional(task),
            })
        }
        "/review" => {
            let (name, rest) = next_word(rest).ok_or(REVIEW_USAGE)?;
            let (next_revision, focus) = flag_option(rest, NEXT_REVISION_FLAG);
            Ok(Command::Review {
                name: name.into(),
                focus: optional(focus),
                next_revision,
            })
        }
        "/ask" => match next_word(rest) {
            Some((name, task)) if !task.is_empty() => Ok(Command::Ask {
                name: name.into(),
                task: task.into(),
            }),
            _ => Err(ASK_USAGE.into()),
        },
        "/roster" => Ok(Command::Roster),
        "/settings" => Ok(Command::Settings),
        "/connect" => Ok(Command::Connect),
        "/update" if rest.is_empty() => Ok(Command::Update),
        "/update" => Err(UPDATE_USAGE.into()),
        "/disconnect" => Ok(Command::Disconnect {
            target: optional_name(rest, DISCONNECT_USAGE)?,
        }),
        "/reconnect" => Ok(Command::Reconnect {
            target: optional_name(rest, RECONNECT_USAGE)?,
        }),
        "/btw" | "/side" if !rest.is_empty() => Ok(Command::Side {
            question: rest.into(),
        }),
        "/btw" | "/side" => Err(SIDE_USAGE.into()),
        "/goal" => match rest {
            "" => Err(GOAL_USAGE.into()),
            "cancel" | "stop" => Ok(Command::GoalCancel),
            criteria => Ok(Command::Goal {
                criteria: criteria.into(),
            }),
        },
        "/remove" => match next_word(rest) {
            Some((name, "")) => Ok(Command::Remove { name: name.into() }),
            _ => Err(REMOVE_USAGE.into()),
        },
        "/entry" => match next_word(rest) {
            Some((profile, "")) => Ok(Command::Entry {
                profile: profile.into(),
            }),
            _ => Err(ENTRY_USAGE.into()),
        },
        "/diff" => Ok(Command::Diff {
            name: optional_name(rest, DIFF_USAGE)?,
        }),
        "/pr" => parse_pr(rest),
        "/settle" => parse_settle(rest, true),
        "/unsettle" => parse_settle(rest, false),
        "/settled" if rest.is_empty() => Ok(Command::SettledDrawer),
        "/mode" => match next_word(rest) {
            Some((mode, "")) => Ok(Command::Mode {
                mode: PermissionMode::parse_commander(mode)?,
            }),
            _ => Err(crate::permissions::COMMANDER_MODE_USAGE.into()),
        },
        "/help" | "/?" if rest.is_empty() => Ok(Command::Help),
        "/help" | "/?" => Err(HELP_USAGE.into()),
        other if ROUTING_DIRECTIVES.contains(&other) => Ok(Command::Message(input.into())),
        other if other.starts_with('/') && !other[1..].contains('/') => Ok(Command::Unknown {
            command: other.into(),
            suggestion: closest_command(other),
        }),
        _ => Ok(Command::Message(input.into())),
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Directive {
    Spawn {
        name: String,
        role: String,
        task: Option<String>,
    },
    Delegate {
        name: String,
        task: String,
    },
}

fn fields(text: &str) -> Vec<&str> {
    text.split('|').map(str::trim).collect()
}

fn directive(line: &str) -> Option<Directive> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("SPAWN_SPECIALIST:") {
        return match fields(rest)[..] {
            [name, role] if !name.is_empty() && !role.is_empty() => Some(Directive::Spawn {
                name: name.into(),
                role: role.into(),
                task: None,
            }),
            [name, role, task] if !name.is_empty() && !role.is_empty() => Some(Directive::Spawn {
                name: name.into(),
                role: role.into(),
                task: optional(task),
            }),
            _ => None,
        };
    }
    let rest = line.strip_prefix("DELEGATE:")?;
    match fields(rest)[..] {
        [name, task] if !name.is_empty() && !task.is_empty() => Some(Directive::Delegate {
            name: name.into(),
            task: task.into(),
        }),
        _ => None,
    }
}

pub(crate) fn directives(reply: &str) -> Vec<Directive> {
    reply.lines().filter_map(directive).collect()
}

fn roster_line(member: &Member) -> String {
    format!(
        "- {} ({}, {}): {}",
        member.name,
        member.role,
        member.status.label(),
        member.mission
    )
}

pub(crate) fn roster_summary(crew: &[Member]) -> String {
    crew.iter()
        .map(|member| {
            format!(
                "{} ({}, {})",
                member.name,
                member.role,
                member.status.label()
            )
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn conversational(entry: &&str) -> bool {
    !entry.starts_with("SYSTEM")
}

pub(crate) fn commander_prompt<'a>(
    feed: impl DoubleEndedIterator<Item = &'a str>,
    crew: &[Member],
    message: &str,
) -> String {
    let mut recent: Vec<&str> = feed
        .rev()
        .filter(conversational)
        .take(CONTEXT_LINES)
        .collect();
    recent.reverse();
    let specialists: Vec<String> = crew
        .iter()
        .filter(|member| !member.is_commander())
        .map(roster_line)
        .collect();
    format!(
        "You are Commander, the persistent entry agent of Agent Command. Answer the operator directly and concisely.\n\
         To create a specialist, write a line: SPAWN_SPECIALIST: <name> | <role> | <task>\n\
         To hand work to an existing specialist, write a line: DELEGATE: <name> | <task>\n\
         Specialists:\n{}\n\
         Recent conversation:\n{}\n\
         Operator: {message}",
        if specialists.is_empty() {
            "- none".into()
        } else {
            specialists.join("\n")
        },
        if recent.is_empty() {
            "- none".into()
        } else {
            recent.join("\n")
        },
    )
}

pub(crate) fn specialist_prompt(member: &Member, task: &str) -> String {
    let skip = member.history.len().saturating_sub(CONTEXT_LINES);
    let history: Vec<&str> = member
        .history
        .iter()
        .skip(skip)
        .map(|entry| entry.text.as_str())
        .collect();
    format!(
        "You are {}, an Agent Command specialist with the role: {}. Work only inside the current directory, your isolated workspace on its own Git branch. Finish with a one-line result summary. If you need the operator's approval before going further, finish instead with a line: {APPROVAL_MARKER} <question>\n\
         Your previous work:\n{}\n\
         Task: {task}",
        member.name,
        member.role,
        if history.is_empty() {
            "- none".into()
        } else {
            history.join("\n")
        },
    )
}

pub(crate) fn summary(text: &str) -> String {
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("No output");
    if line.chars().count() > SUMMARY_CHARS {
        format!(
            "{}…",
            line.chars().take(SUMMARY_CHARS - 1).collect::<String>()
        )
    } else {
        line.into()
    }
}

const CLI_USAGE: &str = "Usage: agent-command specialist {list|spawn NAME ROLE [--mode read-only|supervised|auto-edits|worktree-auto] [TASK]|review NAME [--next-revision] [FOCUS]|status NAME|diff NAME|pr NAME [--commit]|settle NAME|unsettle NAME|remove NAME}";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(crate) fn run(args: &[String]) -> io::Result<()> {
    let store = Store::new(auth::root()?);
    run_with(
        &store,
        &std::env::current_dir()?,
        args,
        &mut io::stdout(),
        &branches::Tools::default(),
    )
}

pub(crate) fn branch_label(member: &Member) -> String {
    match (member.branch(), &member.changes) {
        (Some(branch), Some(changes)) => {
            format!("{} vs {} {}", branch.name, branch.base, changes.totals())
        }
        (Some(branch), None) => format!("{} vs {}", branch.name, branch.base),
        (None, _) => "no branch".into(),
    }
}

fn settled_label(member: &Member) -> &'static str {
    if member.settled {
        "settled"
    } else {
        "active"
    }
}

fn write_list(crew: &[Member], out: &mut impl Write) -> io::Result<()> {
    if crew.is_empty() {
        writeln!(out, "No specialists. {CLI_USAGE}")?;
    }
    for row in inbox_rows(crew, true) {
        let Some(index) = row.member() else { continue };
        let member = &crew[index];
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            member.name,
            member.role,
            member.status.label(),
            workspace_label(member),
            member.permission.flag(),
            branch_label(member),
            settled_label(member)
        )?;
    }
    Ok(())
}

fn write_diff(member: &Member, out: &mut impl Write) -> io::Result<()> {
    let stat = member.diff().map_err(invalid)?;
    if let Some(branch) = member.branch() {
        writeln!(
            out,
            "{} · {} vs {} ({})",
            member.name,
            branch.name,
            branch.base,
            short_commit(&branch.base_commit)
        )?;
    }
    for file in &stat.files {
        writeln!(out, "+{}\t-{}\t{}", file.added, file.removed, file.path)?;
    }
    writeln!(out, "{}", stat.label())
}

fn short_commit(commit: &str) -> &str {
    commit.get(..SHORT_COMMIT).unwrap_or(commit)
}

const SHORT_COMMIT: usize = 8;

fn run_with(
    store: &Store,
    base: &Path,
    args: &[String],
    out: &mut impl Write,
    tools: &branches::Tools,
) -> io::Result<()> {
    let mut crew = store.load_roster()?;
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let position = |crew: &[Member], name: &str| find(crew, name).ok_or_else(|| unknown(name));
    match words[..] {
        ["list"] => write_list(&crew, out),
        ["diff", name] => write_diff(&crew[position(&crew, name)?], out),
        ["pr", name] | ["pr", name, COMMIT_FLAG] | ["pr", COMMIT_FLAG, name] => {
            let index = position(&crew, name)?;
            let published =
                publish(&crew[index], words.contains(&COMMIT_FLAG), tools).map_err(invalid)?;
            crew[index].refresh_changes();
            writeln!(out, "{}: {}", crew[index].name, published.describe())?;
            match published.link {
                branches::Link::Opened(_) => Ok(()),
                branches::Link::Missing(reason) => Err(io::Error::other(reason)),
            }
        }
        [verb @ ("settle" | "unsettle"), name] => {
            let index = position(&crew, name)?;
            let note = settle(&mut crew[index], verb == "settle").map_err(invalid)?;
            store.save_roster_merging(&crew, &[])?;
            writeln!(out, "{note}")
        }
        ["spawn", name, role, ref rest @ ..] => {
            validate_name(name, &crew).map_err(invalid)?;
            let rest = rest.join(" ");
            let (mode, task) = mode_option(&rest).map_err(invalid)?;
            let mode = mode.unwrap_or_else(|| PermissionMode::default_for_role(role));
            let topic = if task.is_empty() { role } else { task };
            let workspace = store
                .isolate_for(base, name, topic, mode)
                .map_err(invalid)?;
            let member = Member::new(name, role, optional(task).as_deref(), Some(workspace))
                .with_permission(Some(mode));
            writeln!(
                out,
                "Spawned {name} ({role}, {}) in {}",
                mode.flag(),
                workspace_label(&member)
            )?;
            crew.push(member);
            store.save_roster_merging(&crew, &[])
        }
        ["status", name] => {
            let member = &crew[position(&crew, name)?];
            writeln!(
                out,
                "{}\nrole: {}\nmode: {}\nreview: {}\nstatus: {}\ntask: {}\nworkspace: {}\nbranch: {}\nqueue: {}\nhistory: {} entries",
                member.name,
                member.role,
                member.permission.flag(),
                review_label(member),
                member.status.label(),
                member.mission,
                workspace_label(member),
                branch_label(member),
                settled_label(member),
                member.history.len()
            )?;
            for entry in member.history.iter().rev().take(3).rev() {
                writeln!(out, "  {}  {}", entry.at, entry.text)?;
            }
            Ok(())
        }
        ["review", name, ref rest @ ..] => {
            let index = find(&crew, name).ok_or_else(|| unknown(name))?;
            review_from_cli(store, base, &mut crew, index, &rest.join(" "), out)
        }
        ["remove", name] => {
            let member = crew.remove(position(&crew, name)?);
            let retained = store.release(&member)?;
            store.save_roster_merging(&crew, &[&member.name])?;
            writeln!(out, "{}", removal_note(&member.name, retained.as_deref()))
        }
        _ => Err(invalid(CLI_USAGE)),
    }
}

fn review_from_cli(
    store: &Store,
    base: &Path,
    crew: &mut [Member],
    index: usize,
    arguments: &str,
    out: &mut impl Write,
) -> io::Result<()> {
    let (next_revision, focus) = flag_option(arguments, NEXT_REVISION_FLAG);
    let workspace = crew[index]
        .workspace
        .clone()
        .ok_or_else(|| invalid(format!("{} has no isolated workspace.", crew[index].name)))?;
    let profiles = auth::profiles(&store.root)?;
    let signed_in = review::signed_in_providers(&profiles);
    let plan = review::plan(
        &mut crew[index],
        &review::ReviewRequest {
            focus: (!focus.is_empty()).then_some(focus),
            next_revision,
            signed_in: &signed_in,
            rule_roots: &[&workspace.path, base],
        },
    )
    .map_err(invalid)?;
    store.save_roster(crew)?;
    writeln!(
        out,
        "{} · round {}/{} · read-only",
        plan.note,
        plan.round,
        review::MAX_REVIEW_ROUNDS
    )?;
    let route = review::review_route(&plan, store.catalog(), store.pins());
    review::run_to_completion(profiles, route, workspace.path, out)
}

pub(crate) fn review_label(member: &Member) -> String {
    format!(
        "revision {} · round {}/{} · author {}",
        member.review.revision,
        member.review.rounds,
        review::MAX_REVIEW_ROUNDS,
        member
            .review
            .author
            .map_or("unknown", |provider| provider.binary())
    )
}

fn unknown(name: &str) -> io::Error {
    invalid(format!("No specialist named {name}"))
}

pub(crate) fn display_path(path: &Path) -> String {
    let raw = path.display().to_string();
    if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        raw
    }
}

pub(crate) fn workspace_label(member: &Member) -> String {
    match &member.workspace {
        Some(Workspace {
            path,
            isolation: Isolation::Worktree { .. },
            ..
        }) => format!("git worktree {}", display_path(path)),
        Some(Workspace {
            path,
            isolation: Isolation::Directory,
            ..
        }) => format!("directory {}", display_path(path)),
        None => "preview only".into(),
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::branches::git;

    fn run_with(
        store: &Store,
        base: &Path,
        args: &[String],
        out: &mut impl Write,
    ) -> io::Result<()> {
        super::run_with(store, base, args, out, &branches::Tools::default())
    }

    pub(crate) struct Scratch(pub(crate) PathBuf);

    impl Scratch {
        pub(crate) fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("ac-orch-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn git_repository(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("README"), "fixture\n").unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "README"],
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-qm",
                "Fixture",
            ],
        ] {
            git(path, args).unwrap();
        }
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn parses_slash_commands_and_leaves_messages_alone() {
        assert_eq!(
            parse_command("/spawn nova reviewer Review the patch").unwrap(),
            Command::Spawn {
                name: "nova".into(),
                role: "reviewer".into(),
                mode: None,
                task: Some("Review the patch".into())
            }
        );
        assert_eq!(
            parse_command("/spawn nova reviewer").unwrap(),
            Command::Spawn {
                name: "nova".into(),
                role: "reviewer".into(),
                mode: None,
                task: None
            }
        );
        assert_eq!(
            parse_command("/ask nova  check tests ").unwrap(),
            Command::Ask {
                name: "nova".into(),
                task: "check tests".into()
            }
        );
        assert_eq!(parse_command("/roster").unwrap(), Command::Roster);
        assert_eq!(
            parse_command("/remove nova").unwrap(),
            Command::Remove {
                name: "nova".into()
            }
        );
        assert_eq!(
            parse_command("/entry claude/alpha").unwrap(),
            Command::Entry {
                profile: "claude/alpha".into()
            }
        );
        assert_eq!(
            parse_command("/hard Build it").unwrap(),
            Command::Message("/hard Build it".into())
        );
        assert_eq!(parse_command("/spawn nova").unwrap_err(), SPAWN_USAGE);
        assert_eq!(parse_command("/ask nova").unwrap_err(), ASK_USAGE);
        assert_eq!(parse_command("/remove a b").unwrap_err(), REMOVE_USAGE);
    }

    #[test]
    fn side_and_goal_commands_parse_with_aliases_and_usage() {
        for input in ["/btw what changed?", "/side   what changed?"] {
            assert_eq!(
                parse_command(input).unwrap(),
                Command::Side {
                    question: "what changed?".into()
                }
            );
        }
        assert_eq!(parse_command("/btw").unwrap_err(), SIDE_USAGE);
        assert_eq!(parse_command("/side  ").unwrap_err(), SIDE_USAGE);
        assert_eq!(
            parse_command("/goal all tests pass").unwrap(),
            Command::Goal {
                criteria: "all tests pass".into()
            }
        );
        assert_eq!(parse_command("/goal cancel").unwrap(), Command::GoalCancel);
        assert_eq!(parse_command("/goal stop").unwrap(), Command::GoalCancel);
        assert_eq!(parse_command("/goal").unwrap_err(), GOAL_USAGE);
        assert_eq!(Status::Snoozed.label(), "SNOOZED");
    }

    #[test]
    fn spawn_takes_an_optional_mode_and_review_takes_focus_and_next_revision() {
        assert_eq!(
            parse_command("/spawn bolt builder --mode worktree-auto Ship @codex it").unwrap(),
            Command::Spawn {
                name: "bolt".into(),
                role: "builder".into(),
                mode: Some(PermissionMode::WorktreeAuto),
                task: Some("Ship @codex it".into())
            }
        );
        assert_eq!(
            parse_command("/spawn quinn playtester --mode=read-only").unwrap(),
            Command::Spawn {
                name: "quinn".into(),
                role: "playtester".into(),
                mode: Some(PermissionMode::ReadOnly),
                task: None
            }
        );
        assert!(parse_command("/spawn x builder --mode full-access")
            .unwrap_err()
            .contains("Unknown permission mode full-access"));
        assert_eq!(
            parse_command("/spawn x builder --mode").unwrap_err(),
            SPAWN_USAGE
        );
        assert_eq!(
            parse_command("/review nova --next-revision auth flow").unwrap(),
            Command::Review {
                name: "nova".into(),
                focus: Some("auth flow".into()),
                next_revision: true
            }
        );
        assert_eq!(
            parse_command("/review nova").unwrap(),
            Command::Review {
                name: "nova".into(),
                focus: None,
                next_revision: false
            }
        );
        assert_eq!(parse_command("/review").unwrap_err(), REVIEW_USAGE);
    }

    #[test]
    fn permission_and_review_ledger_persist_and_old_rosters_use_role_defaults() {
        let scratch = Scratch::new("ledger");
        let store = Store::new(scratch.0.join("profiles"));
        let mut nova = Member::new("nova", "builder", Some("Build"), None)
            .with_permission(Some(PermissionMode::AutoAcceptEdits));
        nova.review.record_author(auth::Provider::Claude);
        nova.review.begin_round().unwrap();
        store
            .save_roster(&[Member::commander(), nova.clone()])
            .unwrap();
        let restored = &store.load_roster().unwrap()[0];
        assert_eq!(restored.permission, PermissionMode::AutoAcceptEdits);
        assert_eq!(restored.review, nova.review);
        let roster = scratch
            .0
            .join("profiles")
            .join(STATE_DIRECTORY)
            .join(ROSTER_FILE);
        fs::write(
            &roster,
            r#"{"schema":"agent-command-orchestrator-v1","specialists":[{"name":"quinn","role":"Playtester","task":"t","status":"IDLE","workspace":null}]}"#,
        )
        .unwrap();
        let restored = &store.load_roster().unwrap()[0];
        assert_eq!(restored.permission, PermissionMode::ReadOnly);
        assert_eq!(restored.review, ReviewLedger::default());
    }

    #[test]
    fn worktree_auto_spawns_only_into_a_git_worktree() {
        let scratch = Scratch::new("worktree-auto");
        let repository = scratch.0.join("repo");
        git_repository(&repository);
        let store = Store::new(scratch.0.join("profiles"));
        let mut out = Vec::new();
        run_with(
            &store,
            &repository,
            &strings(&[
                "spawn",
                "bolt",
                "builder",
                "--mode",
                "worktree-auto",
                "Ship",
            ]),
            &mut out,
        )
        .unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("Spawned bolt (builder, worktree-auto) in git worktree"));
        let bolt = &store.load_roster().unwrap()[0];
        assert_eq!(bolt.permission, PermissionMode::WorktreeAuto);
        assert_eq!(bolt.mission, "Ship");
        let plain = scratch.0.join("plain");
        fs::create_dir(&plain).unwrap();
        let refusal = run_with(
            &store,
            &plain,
            &strings(&["spawn", "echo", "builder", "--mode=worktree-auto"]),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(refusal.to_string().contains("not an isolated git worktree"));
        assert_eq!(store.load_roster().unwrap().len(), 1);
        assert!(!scratch
            .0
            .join("profiles")
            .join(STATE_DIRECTORY)
            .join(DIRECTORY_FALLBACK)
            .join("echo")
            .exists());
    }

    #[test]
    fn parses_spawn_and_delegate_lines_from_a_reply() {
        let reply = "Sure.\nSPAWN_SPECIALIST: nova | reviewer | Review the patch\n  DELEGATE: atlas | Run the tests\nDELEGATE: broken\nSPAWN_SPECIALIST: echo | writer";
        assert_eq!(
            directives(reply),
            vec![
                Directive::Spawn {
                    name: "nova".into(),
                    role: "reviewer".into(),
                    task: Some("Review the patch".into())
                },
                Directive::Delegate {
                    name: "atlas".into(),
                    task: "Run the tests".into()
                },
                Directive::Spawn {
                    name: "echo".into(),
                    role: "writer".into(),
                    task: None
                },
            ]
        );
    }

    #[test]
    fn names_are_bounded_unique_and_never_commander() {
        let crew = vec![Member::commander(), Member::new("nova", "r", None, None)];
        assert!(validate_name("atlas-2", &crew).is_ok());
        for bad in [
            "",
            "9lives",
            "../x",
            "a b",
            "commander",
            "NOVA",
            &"a".repeat(25),
        ] {
            assert!(validate_name(bad, &crew).is_err(), "{bad}");
        }
    }

    #[test]
    fn summary_keeps_the_last_line_and_bounds_its_length() {
        assert_eq!(summary("one\ntwo\n\n"), "two");
        assert_eq!(summary(""), "No output");
        assert_eq!(summary(&"x".repeat(400)).chars().count(), SUMMARY_CHARS);
    }

    #[test]
    fn prompts_carry_roster_history_and_conversation() {
        let mut nova = Member::new("nova", "reviewer", Some("Review"), None);
        nova.remember(Entry::now("nova  Found two issues".into()));
        let crew = vec![Member::commander(), nova.clone()];
        let prompt = commander_prompt(
            ["YOU  hi", "SYSTEM  Auto → x", "Commander  hello"].into_iter(),
            &crew,
            "next",
        );
        assert!(prompt.contains("- nova (reviewer, IDLE): Review"));
        assert!(prompt.contains("YOU  hi\nCommander  hello"));
        assert!(!prompt.contains("SYSTEM"));
        assert!(prompt.ends_with("Operator: next"));
        let prompt = specialist_prompt(&nova, "Fix them");
        assert!(prompt.contains("role: reviewer"));
        assert!(prompt.contains("nova  Found two issues"));
        assert!(prompt.ends_with("Task: Fix them"));
    }

    #[test]
    fn roster_and_histories_persist_and_running_becomes_error() {
        let scratch = Scratch::new("roster");
        let store = Store::new(scratch.0.join("profiles"));
        assert!(store.load_roster().unwrap().is_empty());
        let mut nova = Member::new("nova", "reviewer", Some("Review"), None);
        nova.status = Status::Running;
        store.save_roster(&[Member::commander(), nova]).unwrap();
        store
            .append_history("nova", &Entry::now("nova  working".into()))
            .unwrap();
        store
            .append_commander_for_workspace(scratch.0.as_path(), &Entry::now("YOU  hello".into()))
            .unwrap();
        let crew = store.load_roster().unwrap();
        assert_eq!(crew.len(), 1);
        assert_eq!(crew[0].status, Status::Error);
        assert_eq!(crew[0].history[0].text, "nova  working");
        assert_eq!(
            store
                .load_commander_for_workspace(scratch.0.as_path())
                .unwrap()[0]
                .text,
            "YOU  hello"
        );
        let roster = scratch
            .0
            .join("profiles")
            .join(STATE_DIRECTORY)
            .join(ROSTER_FILE);
        let text = fs::read_to_string(&roster).unwrap();
        assert!(text.contains("\"schema\": \"agent-command-orchestrator-v1\""));
        fs::write(&roster, text.replace("orchestrator-v1", "orchestrator-v0")).unwrap();
        assert!(store.load_roster().is_err());
    }

    #[test]
    fn cli_spawns_into_a_git_worktree_and_removes_it() {
        let scratch = Scratch::new("cli-worktree");
        let repository = scratch.0.join("repo");
        git_repository(&repository);
        let store = Store::new(scratch.0.join("profiles"));
        let mut out = Vec::new();
        for args in [
            &["spawn", "nova", "reviewer", "Review", "it"][..],
            &["spawn", "atlas", "builder"],
            &["list"],
            &["status", "nova"],
        ] {
            run_with(&store, &repository, &strings(args), &mut out).unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("nova\treviewer\tIDLE\tgit worktree"));
        assert!(text.contains("task: Review it"));
        let crew = store.load_roster().unwrap();
        let nova = crew[0].workspace.clone().unwrap();
        let atlas = crew[1].workspace.clone().unwrap();
        assert_ne!(nova.path, atlas.path);
        assert!(nova.path.join("README").is_file());
        assert!(!nova.path.starts_with(&repository));
        let worktrees = git(&repository, &["worktree", "list"]).unwrap();
        assert!(worktrees.contains(nova.path.to_str().unwrap()));
        fs::write(nova.path.join("scratch"), "nova only").unwrap();
        assert!(!atlas.path.join("scratch").exists());
        assert!(!repository.join("scratch").exists());
        assert!(run_with(
            &store,
            &repository,
            &strings(&["spawn", "NOVA", "x"]),
            &mut Vec::new()
        )
        .is_err());
        run_with(
            &store,
            &repository,
            &strings(&["remove", "nova"]),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!nova.path.exists());
        assert!(!git(&repository, &["worktree", "list"])
            .unwrap()
            .contains(nova.path.to_str().unwrap()));
        assert_eq!(store.load_roster().unwrap().len(), 1);
    }

    #[test]
    fn outside_git_specialists_use_project_directory_and_remove_preserves_it() {
        let scratch = Scratch::new("cli-directory");
        let base = scratch.0.join("plain");
        fs::create_dir(&base).unwrap();
        let sentinel = base.join("project.txt");
        fs::write(&sentinel, "keep me").unwrap();
        let store = Store::new(scratch.0.join("profiles"));
        run_with(
            &store,
            &base,
            &strings(&["spawn", "echo", "writer"]),
            &mut Vec::new(),
        )
        .unwrap();
        let workspace = store.load_roster().unwrap()[0].workspace.clone().unwrap();
        assert_eq!(workspace.isolation, Isolation::Directory);
        assert_eq!(workspace.path, base.canonicalize().unwrap());
        assert!(!scratch
            .0
            .join("profiles")
            .join(STATE_DIRECTORY)
            .join(DIRECTORY_FALLBACK)
            .join("echo")
            .exists());
        run_with(
            &store,
            &base,
            &strings(&["remove", "echo"]),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(base.is_dir());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "keep me");
        assert!(run_with(&store, &base, &strings(&["bogus"]), &mut Vec::new()).is_err());
    }

    #[test]
    fn parses_update_disconnect_and_reconnect_commands() {
        assert_eq!(parse_command("/update").unwrap(), Command::Update);
        assert_eq!(parse_command("/update now").unwrap_err(), UPDATE_USAGE);
        assert_eq!(
            parse_command("/disconnect").unwrap(),
            Command::Disconnect { target: None }
        );
        assert_eq!(
            parse_command("/disconnect codex/main").unwrap(),
            Command::Disconnect {
                target: Some("codex/main".into())
            }
        );
        assert_eq!(
            parse_command("/disconnect a b").unwrap_err(),
            DISCONNECT_USAGE
        );
        assert_eq!(
            parse_command("/reconnect").unwrap(),
            Command::Reconnect { target: None }
        );
        assert_eq!(
            parse_command(" /reconnect  claude ").unwrap(),
            Command::Reconnect {
                target: Some("claude".into())
            }
        );
        assert_eq!(
            parse_command("/reconnect claude extra").unwrap_err(),
            RECONNECT_USAGE
        );
    }

    #[test]
    fn parses_diff_pr_and_settle_commands() {
        assert_eq!(
            parse_command("/diff").unwrap(),
            Command::Diff { name: None }
        );
        assert_eq!(
            parse_command("/diff nova").unwrap(),
            Command::Diff {
                name: Some("nova".into())
            }
        );
        assert_eq!(parse_command("/diff a b").unwrap_err(), DIFF_USAGE);
        assert_eq!(
            parse_command("/pr").unwrap(),
            Command::Pr {
                name: None,
                commit: false
            }
        );
        for input in ["/pr nova --commit", "/pr --commit nova"] {
            assert_eq!(
                parse_command(input).unwrap(),
                Command::Pr {
                    name: Some("nova".into()),
                    commit: true
                }
            );
        }
        for bad in ["/pr a b", "/pr --force", "/pr nova --force"] {
            assert_eq!(parse_command(bad).unwrap_err(), PR_USAGE, "{bad}");
        }
        assert_eq!(
            parse_command("/settle nova").unwrap(),
            Command::Settle {
                name: "nova".into(),
                settled: true
            }
        );
        assert_eq!(
            parse_command("/unsettle nova").unwrap(),
            Command::Settle {
                name: "nova".into(),
                settled: false
            }
        );
        assert_eq!(parse_command("/settle").unwrap_err(), SETTLE_USAGE);
        assert_eq!(parse_command("/settled").unwrap(), Command::SettledDrawer);
    }

    fn crew_with(statuses: &[(&str, Status, bool, u64)]) -> Vec<Member> {
        let mut crew = vec![Member::commander()];
        for (name, status, settled, touched) in statuses {
            let mut member = Member::new(name, "r", None, None);
            member.status = *status;
            member.settled = *settled;
            member.touched = *touched;
            crew.push(member);
        }
        crew
    }

    fn names(crew: &[Member], rows: &[RosterRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                RosterRow::Member(index) => crew[*index].name.clone(),
                RosterRow::Settled { count, open } => format!("settled:{count}:{open}"),
            })
            .collect()
    }

    #[test]
    fn inbox_puts_action_needed_first_and_settled_work_in_a_drawer() {
        let crew = crew_with(&[
            ("idle", Status::Idle, false, 9),
            ("done-old", Status::Done, false, 1),
            ("running", Status::Running, false, 2),
            ("snoozed", Status::Snoozed, false, 5),
            ("approval", Status::NeedsApproval, false, 3),
            ("done-new", Status::Done, false, 7),
            ("broken", Status::Error, false, 8),
            ("archived", Status::Done, true, 10),
            ("shelved", Status::Idle, true, 4),
        ]);
        assert_eq!(
            names(&crew, &inbox_rows(&crew, false)),
            [
                "Commander",
                "broken",
                "approval",
                "snoozed",
                "running",
                "done-new",
                "done-old",
                "idle",
                "settled:2:false"
            ]
        );
        assert_eq!(
            names(&crew, &inbox_rows(&crew, true))[8..],
            ["settled:2:true", "archived", "shelved"]
        );
        let plain = crew_with(&[("solo", Status::Idle, false, 0)]);
        assert_eq!(
            names(&plain, &inbox_rows(&plain, true)),
            ["Commander", "solo"]
        );
        assert!(Status::NeedsApproval.needs_attention() && Status::Error.needs_attention());
        assert!(!Status::Done.needs_attention());
    }

    #[test]
    fn settling_keeps_history_and_refuses_running_or_commander() {
        let mut crew = crew_with(&[("nova", Status::Done, false, 0)]);
        crew[1].remember(Entry::now("nova  kept".into()));
        assert!(settle(&mut crew[0], true).is_err());
        assert!(settle(&mut crew[1], true)
            .unwrap()
            .starts_with("Settled nova"));
        assert!(crew[1].settled && crew[1].history.len() == 1);
        assert_eq!(
            settle(&mut crew[1], true).unwrap_err(),
            "nova is already settled."
        );
        assert!(settle(&mut crew[1], false)
            .unwrap()
            .contains("active queue"));
        assert_eq!(
            settle(&mut crew[1], false).unwrap_err(),
            "nova is not settled."
        );
        crew[1].status = Status::Running;
        assert!(settle(&mut crew[1], true)
            .unwrap_err()
            .contains("is running"));
    }

    #[test]
    fn approval_requests_are_read_from_the_last_marker() {
        assert_eq!(approval_request("Done\nall good"), None);
        assert_eq!(
            approval_request("NEEDS_APPROVAL: old\nwork\n  NEEDS_APPROVAL: Delete λ.txt?  "),
            Some("Delete λ.txt?".into())
        );
        assert_eq!(
            approval_request("NEEDS_APPROVAL:"),
            Some("approval requested".into())
        );
        let prompt = specialist_prompt(&Member::new("nova", "r", None, None), "x");
        assert!(prompt.contains("NEEDS_APPROVAL: <question>"));
    }

    #[test]
    fn settled_flag_and_branch_persist_and_legacy_rosters_still_load() {
        let scratch = Scratch::new("roster-v03");
        let store = Store::new(scratch.0.join("profiles"));
        let mut nova = Member::new("nova", "reviewer", Some("Review"), None);
        nova.status = Status::NeedsApproval;
        nova.settled = true;
        nova.workspace = Some(Workspace {
            path: scratch.0.join("missing"),
            isolation: Isolation::Worktree {
                repository: scratch.0.clone(),
            },
            branch: Some(Branch {
                name: "ac/nova-review".into(),
                base: "main".into(),
                base_commit: "0".repeat(40),
            }),
        });
        store
            .save_roster(&[Member::commander(), nova.clone()])
            .unwrap();
        let loaded = store.load_roster().unwrap();
        assert_eq!(loaded[0].status, Status::NeedsApproval);
        assert!(loaded[0].settled);
        assert_eq!(loaded[0].workspace, nova.workspace);
        assert!(loaded[0].changes.is_none());
        let roster = scratch
            .0
            .join("profiles")
            .join(STATE_DIRECTORY)
            .join(ROSTER_FILE);
        assert!(fs::read_to_string(&roster)
            .unwrap()
            .contains("\"NEEDS_APPROVAL\""));
        let legacy = format!(
            r#"{{"schema":"{SCHEMA}","specialists":[{{"name":"old","role":"r","task":"t","status":"DONE","workspace":{{"path":"{}","isolation":{{"worktree":{{"repository":"{}"}}}}}}}}]}}"#,
            scratch.0.join("old").display(),
            scratch.0.display()
        );
        fs::write(&roster, legacy).unwrap();
        let loaded = store.load_roster().unwrap();
        assert!(!loaded[0].settled && loaded[0].branch().is_none());
        assert!(loaded[0].diff().unwrap_err().contains("detached"));
        assert!(publish(&loaded[0], true, &branches::Tools::default())
            .unwrap_err()
            .starts_with("No PR for old: its worktree is detached"));
    }

    #[test]
    fn cli_names_branches_reports_diffs_settles_and_keeps_unmerged_branches() {
        let scratch = Scratch::new("cli-branches");
        let repository = scratch.0.join("repo");
        git_repository(&repository);
        branches::tests::configure_identity(&repository);
        let store = Store::new(scratch.0.join("profiles"));
        let cli = |args: &[&str]| {
            let mut out = Vec::new();
            let result = run_with(&store, &repository, &strings(args), &mut out);
            (result, String::from_utf8(out).unwrap())
        };
        cli(&["spawn", "nova", "reviewer", "Review", "the", "λ", "docs"])
            .0
            .unwrap();
        cli(&["spawn", "atlas", "builder"]).0.unwrap();
        let crew = store.load_roster().unwrap();
        let nova = crew[0].workspace.clone().unwrap();
        assert_eq!(
            nova.branch.as_ref().unwrap().name,
            "ac/nova-review-the-docs"
        );
        assert_eq!(crew[1].branch().unwrap().name, "ac/atlas-builder");
        fs::write(nova.path.join("README"), "fixture\nλ docs\n").unwrap();
        fs::write(nova.path.join("ünïcode.md"), "one\n").unwrap();
        let (result, diff) = cli(&["diff", "nova"]);
        result.unwrap();
        assert!(
            diff.starts_with("nova · ac/nova-review-the-docs vs "),
            "{diff}"
        );
        assert!(diff.contains("+1\t-0\tREADME\n+1\t-0\tünïcode.md\n+2 −0 · 2 files\n"));
        let (_, listed) = cli(&["list"]);
        assert!(listed.contains("nova\treviewer\tIDLE\tgit worktree"));
        assert!(listed.contains("ac/nova-review-the-docs vs "), "{listed}");
        assert!(listed.contains("+2 −0\tactive"));
        let (refused, _) = cli(&["pr", "nova"]);
        assert!(refused.unwrap_err().to_string().contains("--commit"));
        let (missing, out) = cli(&["pr", "nova", "--commit"]);
        assert!(missing.unwrap_err().to_string().contains("No Git remote"));
        assert!(out.is_empty());
        cli(&["settle", "nova"]).0.unwrap();
        assert!(store.load_roster().unwrap()[0].settled);
        let (_, status) = cli(&["status", "nova"]);
        assert!(status.contains("queue: settled") && status.contains("branch: ac/nova"));
        assert!(cli(&["settle", "nova"]).0.is_err());
        cli(&["unsettle", "nova"]).0.unwrap();
        let (_, removed) = cli(&["remove", "nova"]);
        assert_eq!(
            removed,
            "Removed nova; kept unmerged branch ac/nova-review-the-docs.\n"
        );
        assert!(!git(
            &repository,
            &["branch", "--list", "ac/nova-review-the-docs"]
        )
        .unwrap()
        .is_empty());
        let (_, removed) = cli(&["remove", "atlas"]);
        assert_eq!(removed, "Removed atlas.\n");
        assert!(git(&repository, &["branch", "--list", "ac/atlas-builder"])
            .unwrap()
            .is_empty());
        cli(&["spawn", "nova", "reviewer", "Review", "the", "docs"])
            .0
            .unwrap();
        assert_eq!(
            store.load_roster().unwrap()[0].branch().unwrap().name,
            "ac/nova-review-the-docs-2"
        );
        let plain = scratch.0.join("plain");
        fs::create_dir(&plain).unwrap();
        let mut out = Vec::new();
        run_with(&store, &plain, &strings(&["spawn", "echo", "w"]), &mut out).unwrap();
        let error = run_with(&store, &plain, &strings(&["diff", "echo"]), &mut out).unwrap_err();
        assert!(error.to_string().contains("plain directory"), "{error}");
    }
}

#[cfg(test)]
mod cross_platform_tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ac-spec-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn help_command_and_typo_guard_never_send_unknown_slash_commands_to_llm() {
        assert_eq!(parse_command("/help").unwrap(), Command::Help);
        assert_eq!(parse_command("/?").unwrap(), Command::Help);
        assert_eq!(parse_command("/help extra").unwrap_err(), HELP_USAGE);
        let help = format_help();
        assert!(help.contains("/spawn"));
        assert!(help.contains("/goal"));
        assert!(help.contains("/help"));
        assert!(!help_entries().is_empty());

        assert_eq!(
            parse_command("/spwn nova builder").unwrap(),
            Command::Unknown {
                command: "/spwn".into(),
                suggestion: Some("/spawn".into()),
            }
        );
        assert_eq!(
            parse_command("/rostr").unwrap(),
            Command::Unknown {
                command: "/rostr".into(),
                suggestion: Some("/roster".into()),
            }
        );
        assert_eq!(
            parse_command("/completelyunknowncmd").unwrap(),
            Command::Unknown {
                command: "/completelyunknowncmd".into(),
                suggestion: None,
            }
        );
        assert!(unknown_command_message("/spwn", Some("/spawn")).contains("Did you mean /spawn?"));
        assert!(unknown_command_message("/foo", None).contains("Try /help"));

        for routing_msg in [
            "/hard Fix auth",
            "/standard Check tests",
            "/urgent Ship now",
            "/ordinary Tidy docs",
            "/any Explain this",
            "/split a ;; b",
            "/etc/hosts looks wrong",
        ] {
            assert_eq!(
                parse_command(routing_msg).unwrap(),
                Command::Message(routing_msg.into()),
                "{routing_msg}"
            );
        }
    }

    #[test]
    fn non_git_isolate_uses_actual_project_directory_and_release_never_deletes_it() {
        let dir = TempDir::new("non-git");
        let project = dir.0.join("my-project");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("README.md");
        fs::write(&file, "real user work").unwrap();

        let store = Store::new(dir.0.join("profiles"));
        let workspace = store.isolate(&project, "nova", "inspect").unwrap();
        assert_eq!(workspace.isolation, Isolation::Directory);
        assert_eq!(workspace.path, project.canonicalize().unwrap());
        assert!(workspace.branch.is_none());

        let member = Member::new("nova", "builder", Some("inspect"), Some(workspace));
        store.release(&member).unwrap();
        assert!(project.is_dir());
        assert_eq!(fs::read_to_string(&file).unwrap(), "real user work");
    }

    #[test]
    fn roster_save_merging_preserves_concurrent_peer_specialists() {
        let dir = TempDir::new("roster-merge");
        let store = Store::new(dir.0.join("profiles"));
        let tui_member = Member::new("alpha", "builder", Some("UI work"), None);
        store
            .save_roster_merging(&[Member::commander(), tui_member], &[])
            .unwrap();

        let daemon_member = Member::new("beta", "reviewer", Some("Check API"), None);
        store
            .save_roster_merging(&[Member::commander(), daemon_member], &[])
            .unwrap();

        let loaded = store.load_roster().unwrap();
        let names: Vec<&str> = loaded.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "{names:?}");
        assert!(names.contains(&"beta"), "{names:?}");

        store
            .save_roster_merging(&[Member::commander()], &["alpha"])
            .unwrap();
        let remaining = store.load_roster().unwrap();
        let remaining_names: Vec<&str> = remaining.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(remaining_names, ["beta"]);
    }

    #[test]
    fn goal_turn_formatting_separates_display_label_from_model_instruction() {
        let turn = format_goal_turn("finish   testing\nnow", 1, 10, "GOAL_MET");
        assert_eq!(turn.display_label, "Goal (1/10): finish testing now");
        assert!(!turn.display_label.contains("Work toward"));
        assert!(!turn.display_label.contains("GOAL_MET"));
        assert!(!turn.model_prompt.contains("testingWork"));
        assert!(turn
            .model_prompt
            .contains("finish testing now\n\nWork toward"));
        assert!(turn.model_prompt.contains("GOAL_MET"));

        let (head, body) = split_goal_prompt(&turn.model_prompt);
        assert_eq!(
            head,
            "Pinned goal (turn 1 of at most 10): finish testing now"
        );
        assert!(body.starts_with("Work toward"));
    }

    #[test]
    fn resolve_target_matches_whole_words_acronyms_versions_and_detects_ambiguity() {
        let candidates = vec![
            TargetCandidate {
                id: "w-core".into(),
                name: "core-refactor".into(),
                role: "builder".into(),
                topic: "Refactor core auth v1.3".into(),
                recency_rank: 2,
            },
            TargetCandidate {
                id: "w-score".into(),
                name: "score-engine".into(),
                role: "builder".into(),
                topic: "Update score ranking for KR market".into(),
                recency_rank: 0,
            },
            TargetCandidate {
                id: "w-billing-a".into(),
                name: "billing-a".into(),
                role: "builder".into(),
                topic: "Stripe billing webhook handler".into(),
                recency_rank: 0,
            },
            TargetCandidate {
                id: "w-billing-b".into(),
                name: "billing-b".into(),
                role: "builder".into(),
                topic: "Stripe billing retry queue".into(),
                recency_rank: 1,
            },
        ];

        assert_eq!(
            resolve_target("core", &candidates),
            TargetMatch::Chosen("w-core".into())
        );
        assert_eq!(
            resolve_target("KR", &candidates),
            TargetMatch::Chosen("w-score".into())
        );
        assert_eq!(
            resolve_target("v1.3", &candidates),
            TargetMatch::Chosen("w-core".into())
        );
        assert_eq!(
            resolve_target("billing", &candidates),
            TargetMatch::Ambiguous(vec!["w-billing-a".into(), "w-billing-b".into()])
        );
        assert_eq!(
            resolve_target("unrelated quantum", &candidates),
            TargetMatch::None
        );
    }
}
