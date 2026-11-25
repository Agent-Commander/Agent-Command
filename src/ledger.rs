use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::specialists::Status;

pub const LEDGER_SCHEMA: &str = "agent-command-ledger-v1";
pub const LEDGER_LIMIT: usize = 200;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Queued,
    Running,
    NeedsApproval,
    Done,
    Error,
    Snoozed,
    Cancelled,
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::NeedsApproval => "needs_approval",
            Self::Done => "done",
            Self::Error => "error",
            Self::Snoozed => "snoozed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Running | Self::NeedsApproval | Self::Snoozed
        )
    }

    pub fn needs_attention(self) -> bool {
        matches!(self, Self::NeedsApproval | Self::Error)
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Error | Self::Cancelled)
    }

    pub fn from_member(status: Status) -> Option<Self> {
        match status {
            Status::Idle => None,
            Status::Running => Some(Self::Running),
            Status::NeedsApproval => Some(Self::NeedsApproval),
            Status::Done => Some(Self::Done),
            Status::Error => Some(Self::Error),
            Status::Snoozed => Some(Self::Snoozed),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Mission,
    Spawn,
    Goal,
    Review,
}

impl TaskKind {
    fn prefix(self) -> char {
        match self {
            Self::Mission => 'M',
            Self::Spawn => 'S',
            Self::Goal => 'G',
            Self::Review => 'R',
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub id: String,
    pub kind: TaskKind,
    pub title: String,
    pub owner: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<usize>,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LedgerFilter {
    #[default]
    All,
    Active,
    Attention,
    Done,
}

impl LedgerFilter {
    pub const ALL: [Self; 4] = [Self::All, Self::Active, Self::Attention, Self::Done];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Active => "Active",
            Self::Attention => "Attention",
            Self::Done => "Done",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Active,
            Self::Active => Self::Attention,
            Self::Attention => Self::Done,
            Self::Done => Self::All,
        }
    }

    pub fn matches(self, status: TaskStatus) -> bool {
        match self {
            Self::All => true,
            Self::Active => status.is_active(),
            Self::Attention => status.needs_attention(),
            Self::Done => status.is_finished() && !status.needs_attention(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskUpdate {
    pub status: TaskStatus,
    pub branch: Option<String>,
    pub changed_files: Option<usize>,
}

impl TaskUpdate {
    pub fn status(status: TaskStatus) -> Self {
        Self {
            status,
            branch: None,
            changed_files: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct LedgerFile {
    schema: String,
    next_serial: u64,
    entries: Vec<LedgerEntry>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Ledger {
    entries: Vec<LedgerEntry>,
    next_serial: u64,
}

impl Ledger {
    pub fn entries(&self) -> &[LedgerEntry] {
        &self.entries
    }

    pub fn record(&mut self, kind: TaskKind, owner: &str, title: &str, now: i64) -> String {
        self.next_serial += 1;
        let id = format!("{}-{:03}", kind.prefix(), self.next_serial);
        self.entries.push(LedgerEntry {
            id: id.clone(),
            kind,
            title: single_line(title),
            owner: owner.to_owned(),
            status: TaskStatus::Running,
            branch: None,
            changed_files: None,
            updated_at: now,
        });
        self.trim();
        id
    }

    pub fn latest_for(&self, owner: &str) -> Option<&LedgerEntry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.owner.eq_ignore_ascii_case(owner))
    }

    pub fn latest_of_kind(&self, owner: &str, kind: TaskKind) -> Option<&LedgerEntry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.kind == kind && entry.owner.eq_ignore_ascii_case(owner))
    }

    pub fn update_latest(&mut self, owner: &str, update: &TaskUpdate, now: i64) -> bool {
        let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| entry.owner.eq_ignore_ascii_case(owner))
        else {
            return false;
        };
        apply_update(entry, update, now)
    }

    pub fn update_latest_of_kind(
        &mut self,
        owner: &str,
        kind: TaskKind,
        update: &TaskUpdate,
        now: i64,
    ) -> bool {
        let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| entry.kind == kind && entry.owner.eq_ignore_ascii_case(owner))
        else {
            return false;
        };
        apply_update(entry, update, now)
    }

    pub fn filtered(&self, filter: LedgerFilter) -> Vec<&LedgerEntry> {
        let mut rows: Vec<&LedgerEntry> = self
            .entries
            .iter()
            .filter(|entry| filter.matches(entry.status))
            .collect();
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        rows
    }

    pub fn active_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.status.is_active())
            .count()
    }

    pub fn total(&self) -> usize {
        self.entries.len()
    }

    fn trim(&mut self) {
        let excess = self.entries.len().saturating_sub(LEDGER_LIMIT);
        if excess > 0 {
            self.entries.drain(..excess);
        }
    }

    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        match serde_json::from_slice::<LedgerFile>(&bytes) {
            Ok(file) if file.schema == LEDGER_SCHEMA => {
                let next_serial = file.next_serial.max(file.entries.len() as u64);
                let mut ledger = Self {
                    entries: file.entries,
                    next_serial,
                };
                ledger.trim();
                ledger
            }
            _ => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let file = LedgerFile {
            schema: LEDGER_SCHEMA.into(),
            next_serial: self.next_serial,
            entries: self.entries.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        write_atomic(path, &bytes)
    }
}

fn apply_update(entry: &mut LedgerEntry, update: &TaskUpdate, now: i64) -> bool {
    let branch = update.branch.clone().or_else(|| entry.branch.clone());
    let changed_files = update.changed_files.or(entry.changed_files);
    if entry.status == update.status
        && entry.branch == branch
        && entry.changed_files == changed_files
    {
        return false;
    }
    entry.status = update.status;
    entry.branch = branch;
    entry.changed_files = changed_files;
    entry.updated_at = now;
    true
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let mut file = fs::File::create_new(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if fs::rename(&temporary, path).is_err() {
        fs::write(path, bytes)?;
        let _ = fs::remove_file(&temporary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "agent-command-ledger-{name}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn records_get_short_ids_and_updates_only_touch_latest_owner_entry() {
        let mut ledger = Ledger::default();
        assert_eq!(
            ledger.record(TaskKind::Mission, "Commander", "Fix   the\nbuild", 10),
            "M-001"
        );
        assert_eq!(
            ledger.record(TaskKind::Spawn, "nova", "Review README", 11),
            "S-002"
        );
        assert_eq!(ledger.entries()[0].title, "Fix the build");
        assert!(ledger.update_latest(
            "NOVA",
            &TaskUpdate {
                status: TaskStatus::Done,
                branch: Some("agent/nova".into()),
                changed_files: Some(3),
            },
            20,
        ));
        assert!(!ledger.update_latest("nova", &TaskUpdate::status(TaskStatus::Done), 30));
        let nova = ledger.latest_for("nova").unwrap();
        assert_eq!(nova.status, TaskStatus::Done);
        assert_eq!(nova.branch.as_deref(), Some("agent/nova"));
        assert_eq!(nova.changed_files, Some(3));
        assert_eq!(nova.updated_at, 20);
        assert_eq!(
            ledger.latest_for("Commander").unwrap().status,
            TaskStatus::Running
        );
        assert!(!ledger.update_latest("ghost", &TaskUpdate::status(TaskStatus::Error), 1));
        assert_eq!(ledger.active_count(), 1);
        assert_eq!(ledger.total(), 2);
    }

    #[test]
    fn filters_select_active_attention_and_done_newest_first() {
        let mut ledger = Ledger::default();
        for (index, status) in [
            TaskStatus::Queued,
            TaskStatus::Running,
            TaskStatus::NeedsApproval,
            TaskStatus::Done,
            TaskStatus::Error,
            TaskStatus::Snoozed,
            TaskStatus::Cancelled,
        ]
        .into_iter()
        .enumerate()
        {
            let owner = format!("w{index}");
            ledger.record(TaskKind::Spawn, &owner, "task", index as i64);
            ledger.update_latest(&owner, &TaskUpdate::status(status), index as i64);
        }
        let owners = |filter| {
            ledger
                .filtered(filter)
                .iter()
                .map(|entry| entry.owner.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(owners(LedgerFilter::All).len(), 7);
        assert_eq!(owners(LedgerFilter::All)[0], "w6");
        assert_eq!(owners(LedgerFilter::Active), ["w5", "w2", "w1", "w0"]);
        assert_eq!(owners(LedgerFilter::Attention), ["w4", "w2"]);
        assert_eq!(owners(LedgerFilter::Done), ["w6", "w3"]);
        let mut filter = LedgerFilter::All;
        for expected in [
            LedgerFilter::Active,
            LedgerFilter::Attention,
            LedgerFilter::Done,
            LedgerFilter::All,
        ] {
            filter = filter.next();
            assert_eq!(filter, expected);
        }
    }

    #[test]
    fn persistence_round_trips_and_tolerates_missing_or_old_files() {
        let directory = scratch("roundtrip");
        let path = directory.join("nested").join("ledger.json");
        assert_eq!(Ledger::load(&path), Ledger::default());
        let mut ledger = Ledger::default();
        ledger.record(TaskKind::Goal, "Commander", "ship it", 5);
        ledger.record(TaskKind::Review, "nova", "round 1", 6);
        ledger.save(&path).unwrap();
        let restored = Ledger::load(&path);
        assert_eq!(restored, ledger);
        let mut continued = restored;
        assert_eq!(continued.record(TaskKind::Spawn, "x", "y", 7), "S-003");
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".next"))
            .collect();
        assert!(leftovers.is_empty());
        fs::write(
            &path,
            b"{\"schema\":\"agent-command-ledger-v0\",\"entries\":[]}",
        )
        .unwrap();
        assert_eq!(Ledger::load(&path), Ledger::default());
        fs::write(&path, b"not json").unwrap();
        assert_eq!(Ledger::load(&path), Ledger::default());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ledger_keeps_the_newest_entries_within_its_limit() {
        let mut ledger = Ledger::default();
        for index in 0..LEDGER_LIMIT + 5 {
            ledger.record(TaskKind::Mission, "Commander", "turn", index as i64);
        }
        assert_eq!(ledger.total(), LEDGER_LIMIT);
        assert_eq!(ledger.entries()[0].id, "M-006");
    }

    #[test]
    fn member_status_maps_to_ledger_status() {
        assert_eq!(TaskStatus::from_member(Status::Idle), None);
        assert_eq!(
            TaskStatus::from_member(Status::NeedsApproval),
            Some(TaskStatus::NeedsApproval)
        );
        assert_eq!(TaskStatus::NeedsApproval.label(), "needs_approval");
    }
}
