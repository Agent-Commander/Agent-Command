use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

pub const LEDGER_FILE: &str = "ledger.json";
pub const MAX_ENTRIES: usize = 200;
const SCHEMA: u32 = 1;

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
        matches!(self, Self::NeedsApproval | Self::Error | Self::Snoozed)
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
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
    pub fn label(self) -> &'static str {
        match self {
            Self::Mission => "mission",
            Self::Spawn => "spawn",
            Self::Goal => "goal",
            Self::Review => "review",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub key: String,
    pub id: String,
    pub kind: TaskKind,
    pub title: String,
    pub owner: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<usize>,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub key: String,
    pub kind: TaskKind,
    pub title: String,
    pub owner: String,
    pub status: TaskStatus,
    pub branch: Option<String>,
    pub changed_files: Option<usize>,
}

#[derive(Default, Deserialize, Serialize)]
struct LedgerFile {
    #[serde(default)]
    schema: u32,
    #[serde(default)]
    next_id: u32,
    #[serde(default)]
    entries: Vec<LedgerEntry>,
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
            Self::Done => status.is_finished(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Ledger {
    entries: Vec<LedgerEntry>,
    next_id: u32,
    dirty: bool,
}

impl Ledger {
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        let Ok(file) = serde_json::from_slice::<LedgerFile>(&bytes) else {
            return Self::default();
        };
        if file.schema > SCHEMA {
            return Self::default();
        }
        let highest = file
            .entries
            .iter()
            .filter_map(|entry| entry.id.trim_start_matches('T').parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        Self {
            next_id: file.next_id.max(highest),
            entries: file.entries,
            dirty: false,
        }
    }

    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        let file = LedgerFile {
            schema: SCHEMA,
            next_id: self.next_id,
            entries: self.entries.clone(),
        };
        let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
        let mut handle = fs::File::create_new(&temporary)?;
        serde_json::to_writer_pretty(&mut handle, &file)?;
        handle.sync_all()?;
        drop(handle);
        fs::rename(&temporary, path).inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })?;
        self.dirty = false;
        Ok(())
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn entries(&self) -> &[LedgerEntry] {
        &self.entries
    }

    pub fn get(&self, key: &str) -> Option<&LedgerEntry> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    pub fn observe(&mut self, observation: Observation, now: i64) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.key == observation.key) {
            let changed = entry.title != observation.title
                || entry.owner != observation.owner
                || entry.status != observation.status
                || entry.branch != observation.branch
                || entry.changed_files != observation.changed_files;
            if changed {
                entry.title = observation.title;
                entry.owner = observation.owner;
                entry.status = observation.status;
                entry.branch = observation.branch;
                entry.changed_files = observation.changed_files;
                entry.updated_at = now;
                self.dirty = true;
            }
            return;
        }
        self.next_id += 1;
        self.entries.push(LedgerEntry {
            key: observation.key,
            id: format!("T{:03}", self.next_id),
            kind: observation.kind,
            title: observation.title,
            owner: observation.owner,
            status: observation.status,
            branch: observation.branch,
            changed_files: observation.changed_files,
            updated_at: now,
        });
        self.trim();
        self.dirty = true;
    }

    pub fn set_status(&mut self, key: &str, status: TaskStatus, now: i64) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.key == key) {
            if entry.status != status {
                entry.status = status;
                entry.updated_at = now;
                self.dirty = true;
            }
        }
    }

    pub fn merge(&mut self, stored: Ledger) {
        for entry in stored.entries.into_iter().rev() {
            if self.get(&entry.key).is_none() {
                self.entries.insert(0, entry);
            }
        }
        self.next_id = self.next_id.max(stored.next_id);
        self.trim();
    }

    pub fn view(&self, filter: LedgerFilter) -> Vec<&LedgerEntry> {
        let mut rows: Vec<&LedgerEntry> = self
            .entries
            .iter()
            .filter(|entry| filter.matches(entry.status))
            .collect();
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        rows
    }

    pub fn active_count(&self) -> usize {
        self.entries.iter().filter(|e| e.status.is_active()).count()
    }

    fn trim(&mut self) {
        while self.entries.len() > MAX_ENTRIES {
            let oldest_finished = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.status.is_active())
                .min_by_key(|(_, entry)| entry.updated_at)
                .map(|(index, _)| index);
            self.entries.remove(oldest_finished.unwrap_or(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(key: &str, status: TaskStatus) -> Observation {
        Observation {
            key: key.into(),
            kind: TaskKind::Spawn,
            title: format!("task {key}"),
            owner: "alice".into(),
            status,
            branch: Some("agent/alice".into()),
            changed_files: Some(2),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-command-ledger-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn observe_inserts_once_and_only_marks_dirty_on_change() {
        let mut ledger = Ledger::default();
        ledger.observe(observation("a", TaskStatus::Running), 10);
        assert_eq!(ledger.entries().len(), 1);
        assert_eq!(ledger.entries()[0].id, "T001");
        assert!(ledger.is_dirty());
        ledger.dirty = false;
        ledger.observe(observation("a", TaskStatus::Running), 20);
        assert!(!ledger.is_dirty());
        assert_eq!(ledger.entries()[0].updated_at, 10);
        ledger.observe(observation("a", TaskStatus::Done), 30);
        assert!(ledger.is_dirty());
        assert_eq!(ledger.entries()[0].status, TaskStatus::Done);
        assert_eq!(ledger.entries()[0].updated_at, 30);
        ledger.observe(observation("b", TaskStatus::Queued), 40);
        assert_eq!(ledger.entries()[1].id, "T002");
    }

    #[test]
    fn persistence_round_trips_atomically_and_tolerates_bad_files() {
        let dir = scratch("roundtrip");
        let path = dir.join(LEDGER_FILE);
        assert!(Ledger::load(&path).entries().is_empty());
        let mut ledger = Ledger::default();
        ledger.observe(observation("a", TaskStatus::Running), 10);
        ledger.observe(observation("b", TaskStatus::Error), 11);
        ledger.save(&path).unwrap();
        assert!(!ledger.is_dirty());
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != LEDGER_FILE)
            .collect();
        assert!(leftovers.is_empty());
        let loaded = Ledger::load(&path);
        assert_eq!(loaded.entries(), ledger.entries());
        let mut resumed = loaded.clone();
        resumed.observe(observation("c", TaskStatus::Queued), 12);
        assert_eq!(resumed.get("c").unwrap().id, "T003");

        fs::write(&path, b"{not json").unwrap();
        assert!(Ledger::load(&path).entries().is_empty());
        fs::write(&path, br#"{"entries":[{"key":"k","id":"T009","kind":"goal","title":"t","owner":"o","status":"done"}]}"#).unwrap();
        let legacy = Ledger::load(&path);
        assert_eq!(legacy.entries().len(), 1);
        assert_eq!(legacy.next_id, 9);
        fs::write(&path, br#"{"schema":99,"entries":[]}"#).unwrap();
        assert!(Ledger::load(&path).entries().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn filters_partition_statuses_and_view_sorts_newest_first() {
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
            ledger.observe(observation(status.label(), status), index as i64);
        }
        let labels = |filter| -> Vec<&str> {
            ledger
                .view(filter)
                .iter()
                .map(|e| e.status.label())
                .collect()
        };
        assert_eq!(labels(LedgerFilter::All).len(), 7);
        assert_eq!(labels(LedgerFilter::All)[0], "cancelled");
        assert_eq!(
            labels(LedgerFilter::Active),
            ["snoozed", "needs_approval", "running", "queued"]
        );
        assert_eq!(
            labels(LedgerFilter::Attention),
            ["snoozed", "error", "needs_approval"]
        );
        assert_eq!(labels(LedgerFilter::Done), ["cancelled", "done"]);
        assert_eq!(ledger.active_count(), 4);
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
    fn merge_keeps_live_entries_and_trim_drops_oldest_finished() {
        let mut stored = Ledger::default();
        stored.observe(observation("old", TaskStatus::Done), 1);
        stored.observe(observation("shared", TaskStatus::Done), 2);
        let mut live = Ledger::default();
        live.observe(observation("shared", TaskStatus::Running), 5);
        live.merge(stored);
        assert_eq!(live.entries().len(), 2);
        assert_eq!(live.get("shared").unwrap().status, TaskStatus::Running);
        assert!(live.next_id >= 2);

        let mut full = Ledger::default();
        full.observe(observation("keep-running", TaskStatus::Running), 0);
        for index in 0..MAX_ENTRIES {
            full.observe(
                observation(&format!("d{index}"), TaskStatus::Done),
                index as i64 + 1,
            );
        }
        assert_eq!(full.entries().len(), MAX_ENTRIES);
        assert!(full.get("keep-running").is_some());
        assert!(full.get("d0").is_none());
    }
}
