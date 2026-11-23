use crate::{
    auth,
    branches::{self, Branch, DiffStat},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

pub(crate) const STATE_DIRECTORY: &str = "agent-command-orchestrator-v1";
const SCHEMA: &str = "agent-command-orchestrator-v1";
const ROSTER_FILE: &str = "roster.json";
const COMMANDER_FILE: &str = "commander.jsonl";
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
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Running => "RUNNING",
            Self::NeedsApproval => "NEEDS_APPROVAL",
            Self::Done => "DONE",
            Self::Error => "ERROR",
        }
    }

    pub fn caption(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::NeedsApproval => "needs approval",
            Self::Done => "done",
            Self::Error => "error",
        }
    }

    pub fn needs_attention(self) -> bool {
        matches!(self, Self::NeedsApproval | Self::Error)
    }

    fn inbox_rank(self) -> u8 {
        match self {
            Self::NeedsApproval | Self::Error => 0,
            Self::Running => 1,
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
        }
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
        member.history = read_entries(&history_path(directory, &record.name))?.into();
        Ok(member)
    }

    pub(crate) fn save_roster(&self, crew: &[Member]) -> io::Result<()> {
        let roster = RosterFile {
            schema: SCHEMA.into(),
            specialists: crew
                .iter()
                .filter(|member| !member.is_commander())
                .map(|member| Record {
                    name: member.name.clone(),
                    role: member.role.clone(),
                    task: member.mission.clone(),
                    status: member.status,
                    settled: member.settled,
                    workspace: member.workspace.clone(),
                })
                .collect(),
        };
        let path = self.directory()?.join(ROSTER_FILE);
        let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
        let mut file = fs::File::create_new(&temporary)?;
        serde_json::to_writer_pretty(&mut file, &roster)?;
        file.sync_all()?;
        drop(file);
        fs::rename(temporary, path)
    }

    pub(crate) fn load_commander(&self) -> io::Result<Vec<Entry>> {
        match self.existing()? {
            Some(directory) => read_entries(&directory.join(COMMANDER_FILE)),
            None => Ok(Vec::new()),
        }
    }

    pub(crate) fn append_commander(&self, entry: &Entry) -> io::Result<()> {
        append_entry(&self.directory()?.join(COMMANDER_FILE), entry)
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
                let path = self.subdirectory(DIRECTORY_FALLBACK)?.join(&key);
                fs::create_dir(&path)?;
                Ok(Workspace {
                    path,
                    isolation: Isolation::Directory,
                    branch: None,
                })
            }
        }
    }

    pub(crate) fn release(&self, member: &Member) -> io::Result<Option<String>> {
        let mut retained = None;
        if let Some(workspace) = &member.workspace {
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
                    retained = workspace
                        .branch
                        .as_ref()
                        .filter(|branch| !branches::delete_if_merged(repository, &branch.name))
                        .map(|branch| branch.name.clone());
                }
                Isolation::Directory => {
                    remove_if_present(&workspace.path, |path| fs::remove_dir_all(path))?
                }
            }
        }
        if let Some(directory) = self.existing()? {
            remove_if_present(&history_path(&directory, &member.name), |path| {
                fs::remove_file(path)
            })?;
        }
        Ok(retained)
    }
}

pub(crate) fn removal_note(name: &str, retained: Option<&str>) -> String {
    match retained {
        Some(branch) => format!("Removed {name}; kept unmerged branch {branch}."),
        None => format!("Removed {name}."),
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
        task: Option<String>,
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
    Message(String),
}

const SPAWN_USAGE: &str = "Usage: /spawn NAME ROLE [TASK]";
const ASK_USAGE: &str = "Usage: /ask NAME TASK";
const REMOVE_USAGE: &str = "Usage: /remove NAME";
const ENTRY_USAGE: &str = "Usage: /entry PROVIDER/PROFILE or /entry auto";
const DIFF_USAGE: &str = "Usage: /diff [NAME]";
const PR_USAGE: &str = "Usage: /pr [NAME] [--commit]";
const SETTLE_USAGE: &str = "Usage: /settle NAME or /unsettle NAME";
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

pub(crate) fn parse_command(input: &str) -> Result<Command, String> {
    let input = input.trim();
    let Some((head, rest)) = next_word(input) else {
        return Ok(Command::Message(String::new()));
    };
    match head {
        "/spawn" => {
            let (name, rest) = next_word(rest).ok_or(SPAWN_USAGE)?;
            let (role, task) = next_word(rest).ok_or(SPAWN_USAGE)?;
            Ok(Command::Spawn {
                name: name.into(),
                role: role.into(),
                task: optional(task),
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

pub(crate) const CLI_USAGE: &str = "Usage: agent-command specialist {list|spawn NAME ROLE [TASK]|status NAME|diff NAME|pr NAME [--commit]|settle NAME|unsettle NAME|remove NAME}";

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
            "{}\t{}\t{}\t{}\t{}\t{}",
            member.name,
            member.role,
            member.status.label(),
            workspace_label(member),
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
            store.save_roster(&crew)?;
            writeln!(out, "{note}")
        }
        ["spawn", name, role, ref task @ ..] => {
            validate_name(name, &crew).map_err(invalid)?;
            let task = task.join(" ");
            let topic = if task.is_empty() { role } else { task.as_str() };
            let workspace = store.isolate(base, name, topic)?;
            let member = Member::new(name, role, optional(&task).as_deref(), Some(workspace));
            writeln!(
                out,
                "Spawned {name} ({role}) in {}",
                workspace_label(&member)
            )?;
            crew.push(member);
            store.save_roster(&crew)
        }
        ["status", name] => {
            let member = &crew[position(&crew, name)?];
            writeln!(
                out,
                "{}\nrole: {}\nstatus: {}\ntask: {}\nworkspace: {}\nbranch: {}\nqueue: {}\nhistory: {} entries",
                member.name,
                member.role,
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
        ["remove", name] => {
            let member = crew.remove(position(&crew, name)?);
            let retained = store.release(&member)?;
            store.save_roster(&crew)?;
            writeln!(out, "{}", removal_note(&member.name, retained.as_deref()))
        }
        _ => Err(invalid(CLI_USAGE)),
    }
}

fn unknown(name: &str) -> io::Error {
    invalid(format!("No specialist named {name}"))
}

pub(crate) fn workspace_label(member: &Member) -> String {
    match &member.workspace {
        Some(Workspace {
            path,
            isolation: Isolation::Worktree { .. },
            ..
        }) => format!("git worktree {}", path.display()),
        Some(Workspace {
            path,
            isolation: Isolation::Directory,
            ..
        }) => format!("directory {}", path.display()),
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
                task: Some("Review the patch".into())
            }
        );
        assert_eq!(
            parse_command("/spawn nova reviewer").unwrap(),
            Command::Spawn {
                name: "nova".into(),
                role: "reviewer".into(),
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
            .append_commander(&Entry::now("YOU  hello".into()))
            .unwrap();
        let crew = store.load_roster().unwrap();
        assert_eq!(crew.len(), 1);
        assert_eq!(crew[0].status, Status::Error);
        assert_eq!(crew[0].history[0].text, "nova  working");
        assert_eq!(store.load_commander().unwrap()[0].text, "YOU  hello");
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
    fn outside_git_specialists_get_isolated_directories() {
        let scratch = Scratch::new("cli-directory");
        let base = scratch.0.join("plain");
        fs::create_dir(&base).unwrap();
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
        assert!(workspace.path.is_dir() && !workspace.path.starts_with(&base));
        run_with(
            &store,
            &base,
            &strings(&["remove", "echo"]),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!workspace.path.exists());
        assert!(run_with(&store, &base, &strings(&["bogus"]), &mut Vec::new()).is_err());
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
                "running",
                "done-new",
                "done-old",
                "idle",
                "settled:2:false"
            ]
        );
        assert_eq!(
            names(&crew, &inbox_rows(&crew, true))[7..],
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
