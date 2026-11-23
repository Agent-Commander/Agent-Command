use crate::auth;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command as Process, Stdio},
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
    Done,
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Running => "RUNNING",
            Self::Done => "DONE",
            Self::Error => "ERROR",
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
    pub workspace: Option<Workspace>,
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
            workspace,
            history: VecDeque::new(),
        }
    }

    pub fn is_commander(&self) -> bool {
        self.name == COMMANDER
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

    pub(crate) fn isolate(&self, base: &Path, name: &str) -> io::Result<Workspace> {
        let key = name.to_ascii_lowercase();
        match repository_root(base) {
            Some(repository) => {
                let path = self.subdirectory(WORKTREE_DIRECTORY)?.join(&key);
                git(
                    &repository,
                    &["worktree", "add", "--detach", path_text(&path)?, "HEAD"],
                )?;
                Ok(Workspace {
                    path,
                    isolation: Isolation::Worktree { repository },
                })
            }
            None => {
                let path = self.subdirectory(DIRECTORY_FALLBACK)?.join(&key);
                fs::create_dir(&path)?;
                Ok(Workspace {
                    path,
                    isolation: Isolation::Directory,
                })
            }
        }
    }

    pub(crate) fn release(&self, member: &Member) -> io::Result<()> {
        if let Some(workspace) = &member.workspace {
            match &workspace.isolation {
                Isolation::Worktree { repository } => {
                    git(
                        repository,
                        &["worktree", "remove", "--force", path_text(&workspace.path)?],
                    )?;
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
        Ok(())
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

fn path_text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::other("Workspace path must be valid UTF-8"))
}

fn git(directory: &Path, args: &[&str]) -> io::Result<String> {
    let output = Process::new("git")
        .current_dir(directory)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn repository_root(base: &Path) -> Option<PathBuf> {
    git(base, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(PathBuf::from)
}

pub(crate) fn find(crew: &[Member], name: &str) -> Option<usize> {
    crew.iter()
        .position(|member| member.name.eq_ignore_ascii_case(name))
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
    Settings,
    Connect,
    Message(String),
}

const SPAWN_USAGE: &str = "Usage: /spawn NAME ROLE [TASK]";
const ASK_USAGE: &str = "Usage: /ask NAME TASK";
const REMOVE_USAGE: &str = "Usage: /remove NAME";
const ENTRY_USAGE: &str = "Usage: /entry PROVIDER/PROFILE or /entry auto";

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
        "You are {}, an Agent Command specialist with the role: {}. Work only inside the current directory, your isolated workspace. Finish with a one-line result summary.\n\
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

const CLI_USAGE: &str =
    "Usage: agent-command specialist {list|spawn NAME ROLE [TASK]|status NAME|remove NAME}";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(crate) fn run(args: &[String]) -> io::Result<()> {
    let store = Store::new(auth::root()?);
    run_with(&store, &std::env::current_dir()?, args, &mut io::stdout())
}

fn run_with(store: &Store, base: &Path, args: &[String], out: &mut impl Write) -> io::Result<()> {
    let mut crew = store.load_roster()?;
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words[..] {
        ["list"] => {
            if crew.is_empty() {
                writeln!(out, "No specialists. {CLI_USAGE}")?;
            }
            for member in &crew {
                writeln!(
                    out,
                    "{}\t{}\t{}\t{}",
                    member.name,
                    member.role,
                    member.status.label(),
                    workspace_label(member)
                )?;
            }
            Ok(())
        }
        ["spawn", name, role, ref task @ ..] => {
            validate_name(name, &crew).map_err(invalid)?;
            let task = task.join(" ");
            let workspace = store.isolate(base, name)?;
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
            let member = &crew[find(&crew, name).ok_or_else(|| unknown(name))?];
            writeln!(
                out,
                "{}\nrole: {}\nstatus: {}\ntask: {}\nworkspace: {}\nhistory: {} entries",
                member.name,
                member.role,
                member.status.label(),
                member.mission,
                workspace_label(member),
                member.history.len()
            )?;
            for entry in member.history.iter().rev().take(3).rev() {
                writeln!(out, "  {}  {}", entry.at, entry.text)?;
            }
            Ok(())
        }
        ["remove", name] => {
            let member = crew.remove(find(&crew, name).ok_or_else(|| unknown(name))?);
            store.release(&member)?;
            store.save_roster(&crew)?;
            writeln!(out, "Removed {}", member.name)
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
        }) => format!("git worktree {}", path.display()),
        Some(Workspace {
            path,
            isolation: Isolation::Directory,
        }) => format!("directory {}", path.display()),
        None => "preview only".into(),
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

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
}
