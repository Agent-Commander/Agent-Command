use std::{
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use chrono::Datelike;
use serde::{Deserialize, Serialize};

use crate::specialists::Status;

pub const LEDGER_SCHEMA: &str = "agent-command-ledger-v2";
pub const LEDGER_SCHEMA_V1: &str = "agent-command-ledger-v1";
pub const LEDGER_LIMIT: usize = 200;
const DOCS_ROOT_DIR: &str = "ledger-docs";
const ACTIVE_DOCS_DIR: &str = "active";
const ARCHIVE_DOCS_DIR: &str = "archive";
const MAX_DOC_BYTES: u64 = 2 * 1024 * 1024;
const SUMMARY_MAX_CHARS: usize = 220;

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
    pub id: String,
    pub kind: TaskKind,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub doc_rel_path: String,
    pub owner: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    pub updated_at: i64,
}

impl LedgerEntry {
    pub fn display_summary(&self) -> &str {
        if self.summary.trim().is_empty() {
            &self.title
        } else {
            &self.summary
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LedgerDecisionRecord {
    pub decision_id: String,
    pub question: String,
    pub answer: String,
    pub answered_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LedgerActivity {
    pub at: i64,
    pub state: String,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LedgerDocument {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub kind: TaskKind,
    pub owner: String,
    pub status: TaskStatus,
    pub branch: Option<String>,
    pub changed_files: Option<usize>,
    pub created_at: i64,
    pub updated_at: i64,
    pub task_brief: String,
    pub decisions: Vec<LedgerDecisionRecord>,
    pub checkpoints: Vec<String>,
    pub activity: Vec<LedgerActivity>,
}

impl LedgerDocument {
    pub fn from_entry(entry: &LedgerEntry, task_brief: &str) -> Self {
        Self {
            id: entry.id.clone(),
            title: entry.title.clone(),
            summary: entry.display_summary().to_owned(),
            kind: entry.kind,
            owner: entry.owner.clone(),
            status: entry.status,
            branch: entry.branch.clone(),
            changed_files: entry.changed_files,
            created_at: entry.updated_at,
            updated_at: entry.updated_at,
            task_brief: task_brief.trim().to_owned(),
            decisions: Vec::new(),
            checkpoints: Vec::new(),
            activity: vec![LedgerActivity {
                at: entry.updated_at,
                state: entry.status.label().into(),
                detail: format!("Recorded {} task for {}", entry.kind.label(), entry.owner),
            }],
        }
    }

    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# {} · {}\n\n", self.id, self.title));
        out.push_str(&format!("{}\n\n", single_sentence(&self.summary)));
        out.push_str(&format!("- **ID**: `{}`\n", self.id));
        out.push_str(&format!("- **Kind**: {}\n", self.kind.label()));
        out.push_str(&format!("- **Owner**: {}\n", self.owner));
        out.push_str(&format!("- **Status**: {}\n", self.status.label()));
        if let Some(branch) = &self.branch {
            out.push_str(&format!("- **Branch**: `{branch}`\n"));
        }
        if let Some(files) = self.changed_files {
            out.push_str(&format!("- **Changed files**: {files}\n"));
        }
        out.push_str(&format!(
            "- **Updated**: {}\n\n",
            format_iso_utc(self.updated_at)
        ));
        out.push_str("## Task Brief\n\n");
        if self.task_brief.trim().is_empty() {
            out.push_str(&format!("{}\n\n", self.title));
        } else {
            out.push_str(self.task_brief.trim());
            out.push_str("\n\n");
        }
        out.push_str("## Decisions & Direction\n\n");
        if self.decisions.is_empty() {
            out.push_str("_No recorded decisions yet._\n\n");
        } else {
            for decision in &self.decisions {
                out.push_str(&format!(
                    "- **{}** ({}): {}\n  - Answer: {}\n",
                    decision.decision_id,
                    format_iso_utc(decision.answered_at),
                    single_line(&decision.question),
                    single_line(&decision.answer)
                ));
            }
            out.push('\n');
        }
        out.push_str("## Execution & Stage Checkpoints\n\n");
        if self.checkpoints.is_empty() {
            out.push_str("_No stage checkpoints recorded yet._\n\n");
        } else {
            for checkpoint in &self.checkpoints {
                out.push_str(&format!("- {}\n", single_line(checkpoint)));
            }
            out.push('\n');
        }
        out.push_str("## Activity Log\n\n");
        for item in &self.activity {
            out.push_str(&format!(
                "- `{}` [{}] {}\n",
                format_iso_utc(item.at),
                item.state,
                single_line(&item.detail)
            ));
        }
        out
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchiveCompaction {
    pub week_label: String,
    pub archived_ids: Vec<String>,
    pub index_rel_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchivedSummary {
    pub id: String,
    pub owner: String,
    pub status: TaskStatus,
    pub summary: String,
    pub doc_rel_path: String,
    pub archived_at: i64,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    archived_overflow: Vec<ArchivedSummary>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Ledger {
    entries: Vec<LedgerEntry>,
    next_serial: u64,
    archived_overflow: Vec<ArchivedSummary>,
}

impl Ledger {
    pub fn entries(&self) -> &[LedgerEntry] {
        &self.entries
    }

    pub fn archived_overflow(&self) -> &[ArchivedSummary] {
        &self.archived_overflow
    }

    pub fn get(&self, id: &str) -> Option<&LedgerEntry> {
        self.entries
            .iter()
            .find(|entry| entry.id.eq_ignore_ascii_case(id))
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut LedgerEntry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.id.eq_ignore_ascii_case(id))
    }

    pub fn record(&mut self, kind: TaskKind, owner: &str, title: &str, now: i64) -> String {
        let summary = single_sentence(title);
        self.record_with_summary(kind, owner, title, &summary, now)
    }

    pub fn record_with_summary(
        &mut self,
        kind: TaskKind,
        owner: &str,
        title: &str,
        summary: &str,
        now: i64,
    ) -> String {
        self.next_serial += 1;
        let id = format!("{}-{:03}", kind.prefix(), self.next_serial);
        let clean_title = single_line(title);
        let clean_summary = if summary.trim().is_empty() {
            single_sentence(&clean_title)
        } else {
            single_sentence(summary)
        };
        let doc_rel_path = active_doc_rel_path(&id);
        self.entries.push(LedgerEntry {
            id: id.clone(),
            kind,
            title: clean_title,
            summary: clean_summary,
            doc_rel_path,
            owner: owner.to_owned(),
            status: TaskStatus::Running,
            branch: None,
            changed_files: None,
            decision_ids: Vec::new(),
            waiting_for: None,
            archived_at: None,
            updated_at: now,
        });
        self.trim(now);
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

    pub fn set_waiting_for(&mut self, id: &str, dependency: Option<&str>, now: i64) -> bool {
        let Some(entry) = self.get_mut(id) else {
            return false;
        };
        entry.waiting_for = dependency.map(single_line).filter(|text| !text.is_empty());
        entry.updated_at = now;
        true
    }

    pub fn link_decision(&mut self, entry_id: &str, decision_id: &str, now: i64) -> bool {
        let Some(entry) = self.get_mut(entry_id) else {
            return false;
        };
        if !entry
            .decision_ids
            .iter()
            .any(|existing| existing == decision_id)
        {
            entry.decision_ids.push(decision_id.to_owned());
        }
        entry.status = TaskStatus::NeedsApproval;
        entry.updated_at = now;
        true
    }

    pub fn record_decision_answer(
        &mut self,
        state_dir: Option<&Path>,
        entry_id: &str,
        decision_id: &str,
        answer: &str,
        now: i64,
    ) -> io::Result<bool> {
        let Some(entry) = self.get_mut(entry_id) else {
            return Ok(false);
        };
        if !entry
            .decision_ids
            .iter()
            .any(|existing| existing == decision_id)
        {
            entry.decision_ids.push(decision_id.to_owned());
        }
        if entry.status == TaskStatus::NeedsApproval {
            entry.status = TaskStatus::Running;
        }
        entry.updated_at = now;
        if let Some(dir) = state_dir {
            let line = format!("Decision `{decision_id}` answered: {}", single_line(answer));
            let _ = self.append_document(dir, entry_id, "Decision Answer", &line, now)?;
        }
        Ok(true)
    }

    pub fn write_document(
        &self,
        state_dir: &Path,
        entry_id: &str,
        doc: &LedgerDocument,
    ) -> io::Result<PathBuf> {
        let rel = self
            .get(entry_id)
            .map(|entry| entry.doc_rel_path.as_str())
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| active_doc_rel_path(entry_id));
        let target = resolve_safe_doc_path(state_dir, &rel, true)?;
        let markdown = doc.render_markdown();
        write_atomic(&target, markdown.as_bytes())?;
        Ok(target)
    }

    pub fn append_document(
        &self,
        state_dir: &Path,
        entry_id: &str,
        heading: &str,
        body: &str,
        now: i64,
    ) -> io::Result<PathBuf> {
        let rel = self
            .get(entry_id)
            .map(|entry| entry.doc_rel_path.as_str())
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| active_doc_rel_path(entry_id));
        let target = resolve_safe_doc_path(state_dir, &rel, true)?;
        let mut content = match read_bounded_safe(&target) {
            Ok(existing) => existing,
            Err(error) if error.kind() == io::ErrorKind::NotFound => self
                .get(entry_id)
                .map(|entry| LedgerDocument::from_entry(entry, &entry.title).render_markdown())
                .unwrap_or_else(|| format!("# {entry_id}\n\n")),
            Err(error) => return Err(error),
        };
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&format!(
            "\n### {} ({})\n\n{}\n",
            single_line(heading),
            format_iso_utc(now),
            body.trim()
        ));
        write_atomic(&target, content.as_bytes())?;
        Ok(target)
    }

    pub fn read_document(&self, state_dir: &Path, entry_id: &str) -> io::Result<String> {
        let rel = self
            .get(entry_id)
            .map(|entry| entry.doc_rel_path.as_str())
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| active_doc_rel_path(entry_id));
        let target = resolve_safe_doc_path(state_dir, &rel, false)?;
        read_bounded_safe(&target)
    }

    pub fn compact_weekly_archive(
        &mut self,
        state_dir: &Path,
        now: i64,
        max_age_secs: i64,
    ) -> io::Result<ArchiveCompaction> {
        let week_label = iso_week_label(now);
        let mut archived_ids = Vec::new();
        let mut rows_for_index = Vec::new();
        for entry in &mut self.entries {
            if !entry.status.is_finished() || entry.archived_at.is_some() {
                continue;
            }
            if now.saturating_sub(entry.updated_at) < max_age_secs {
                continue;
            }
            let current_rel = if entry.doc_rel_path.is_empty() {
                active_doc_rel_path(&entry.id)
            } else {
                entry.doc_rel_path.clone()
            };
            let new_rel = archive_doc_rel_path(&week_label, &entry.id);
            let old_path = resolve_safe_doc_path(state_dir, &current_rel, false);
            let new_path = resolve_safe_doc_path(state_dir, &new_rel, true)?;
            let doc_body = match old_path.and_then(|path| read_bounded_safe(&path)) {
                Ok(existing) => existing,
                Err(_) => LedgerDocument::from_entry(entry, &entry.title).render_markdown(),
            };
            write_atomic(&new_path, doc_body.as_bytes())?;
            if let Ok(old_file) = resolve_safe_doc_path(state_dir, &current_rel, false) {
                if old_file != new_path {
                    let _ = fs::remove_file(old_file);
                }
            }
            entry.doc_rel_path = new_rel.clone();
            entry.archived_at = Some(now);
            archived_ids.push(entry.id.clone());
            rows_for_index.push((
                entry.id.clone(),
                entry.status.label().to_owned(),
                entry.owner.clone(),
                entry.display_summary().to_owned(),
                format!("{}.md", sanitize_id_filename(&entry.id)),
            ));
        }
        let index_rel_path = format!("{DOCS_ROOT_DIR}/{ARCHIVE_DOCS_DIR}/{week_label}/INDEX.md");
        if !rows_for_index.is_empty() {
            let index_path = resolve_safe_doc_path(state_dir, &index_rel_path, true)?;
            let mut markdown = match read_bounded_safe(&index_path) {
                Ok(existing) => existing,
                Err(_) => format!("# Ledger Archive {week_label}\n\n"),
            };
            for (id, status, owner, summary, filename) in rows_for_index {
                let bullet = format!("- [{id}]({filename}) · `{status}` · {owner} — {summary}\n");
                if !markdown.contains(&format!("- [{id}](")) {
                    markdown.push_str(&bullet);
                }
            }
            write_atomic(&index_path, markdown.as_bytes())?;
        }
        Ok(ArchiveCompaction {
            week_label,
            archived_ids,
            index_rel_path,
        })
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

    fn trim(&mut self, now: i64) {
        let excess = self.entries.len().saturating_sub(LEDGER_LIMIT);
        if excess > 0 {
            for drained in self.entries.drain(..excess) {
                self.archived_overflow.push(ArchivedSummary {
                    id: drained.id,
                    owner: drained.owner,
                    status: drained.status,
                    summary: if drained.summary.is_empty() {
                        drained.title
                    } else {
                        drained.summary
                    },
                    doc_rel_path: drained.doc_rel_path,
                    archived_at: drained.archived_at.unwrap_or(now),
                });
            }
        }
    }

    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        match serde_json::from_slice::<LedgerFile>(&bytes) {
            Ok(mut file) if file.schema == LEDGER_SCHEMA || file.schema == LEDGER_SCHEMA_V1 => {
                for entry in &mut file.entries {
                    if entry.summary.trim().is_empty() {
                        entry.summary = single_sentence(&entry.title);
                    }
                    if entry.doc_rel_path.trim().is_empty() {
                        entry.doc_rel_path = active_doc_rel_path(&entry.id);
                    }
                }
                let next_serial = file.next_serial.max(file.entries.len() as u64);
                let mut ledger = Self {
                    entries: file.entries,
                    next_serial,
                    archived_overflow: file.archived_overflow,
                };
                ledger.trim(0);
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
            archived_overflow: self.archived_overflow.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        write_atomic(path, &bytes)
    }
}

pub fn active_doc_rel_path(id: &str) -> String {
    format!(
        "{DOCS_ROOT_DIR}/{ACTIVE_DOCS_DIR}/{}.md",
        sanitize_id_filename(id)
    )
}

pub fn archive_doc_rel_path(week_label: &str, id: &str) -> String {
    format!(
        "{DOCS_ROOT_DIR}/{ARCHIVE_DOCS_DIR}/{}/{}.md",
        sanitize_id_filename(week_label),
        sanitize_id_filename(id)
    )
}

fn sanitize_id_filename(id: &str) -> String {
    let clean: String = id
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if clean.is_empty() {
        "entry".into()
    } else {
        clean
    }
}

fn resolve_safe_doc_path(state_dir: &Path, rel: &str, create_parents: bool) -> io::Result<PathBuf> {
    if rel.trim().is_empty() || rel.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Ledger document path must not be empty or contain NUL.",
        ));
    }
    let rel_path = Path::new(rel);
    let mut joined = PathBuf::new();
    for component in rel_path.components() {
        match component {
            Component::Normal(segment) => joined.push(segment),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Unsafe ledger document path '{rel}'."),
                ));
            }
        }
    }
    let docs_root = state_dir.join(DOCS_ROOT_DIR);
    if create_parents {
        fs::create_dir_all(&docs_root)?;
    }
    let target = state_dir.join(&joined);
    if let Some(parent) = target.parent() {
        if create_parents {
            fs::create_dir_all(parent)?;
        }
    }
    if let Ok(canonical_root) = docs_root.canonicalize() {
        if let Some(parent) = target.parent() {
            if let Ok(canonical_parent) = parent.canonicalize() {
                if !canonical_parent.starts_with(&canonical_root) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "Ledger document path escapes {}: {}",
                            docs_root.display(),
                            target.display()
                        ),
                    ));
                }
            }
        }
    }
    if let Ok(meta) = fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("Refusing symlink ledger document at {}", target.display()),
            ));
        }
    }
    Ok(target)
}

fn read_bounded_safe(path: &Path) -> io::Result<String> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Refusing symlink ledger document at {}", path.display()),
        ));
    }
    if meta.len() > MAX_DOC_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Ledger document {} exceeds {MAX_DOC_BYTES} bytes.",
                path.display()
            ),
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_DOC_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOC_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Ledger document {} exceeds {MAX_DOC_BYTES} bytes.",
                path.display()
            ),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
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

pub fn single_sentence(text: &str) -> String {
    let clean = single_line(text);
    if clean.is_empty() {
        return String::new();
    }
    let truncated: String = clean.chars().take(SUMMARY_MAX_CHARS).collect();
    if let Some(pos) = truncated.find(". ") {
        return truncated[..=pos].to_owned();
    }
    truncated
}

fn iso_week_label(epoch: i64) -> String {
    let dt = chrono::DateTime::from_timestamp(epoch, 0).unwrap_or_else(chrono::Utc::now);
    let week = dt.iso_week();
    format!("{:04}-W{:02}", week.year(), week.week())
}

fn format_iso_utc(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
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
        assert_eq!(ledger.entries()[0].summary, "Fix the build");
        assert_eq!(
            ledger.entries()[0].doc_rel_path,
            "ledger-docs/active/M-001.md"
        );
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
    fn v1_ledger_auto_migrates_to_v2_with_summary_and_doc_path() {
        let directory = scratch("v1-migrate");
        let path = directory.join("ledger.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            &path,
            r#"{
                "schema": "agent-command-ledger-v1",
                "next_serial": 4,
                "entries": [
                    {
                        "id": "M-004",
                        "kind": "mission",
                        "title": "Port TaskFlow scheduler. Also add tests.",
                        "owner": "Commander",
                        "status": "running",
                        "updated_at": 1700000000
                    }
                ]
            }"#,
        )
        .unwrap();
        let migrated = Ledger::load(&path);
        assert_eq!(migrated.entries().len(), 1);
        assert_eq!(migrated.entries()[0].summary, "Port TaskFlow scheduler.");
        assert_eq!(
            migrated.entries()[0].doc_rel_path,
            "ledger-docs/active/M-004.md"
        );
        migrated.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(LEDGER_SCHEMA));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn documents_decisions_and_weekly_archive_compaction_preserve_all_rows() {
        let directory = scratch("docs-archive");
        let mut ledger = Ledger::default();
        let id = ledger.record_with_summary(
            TaskKind::Mission,
            "builder",
            "Implement Decision Inbox",
            "Port Decision Inbox schema and triage into Rust.",
            1_700_000_000,
        );
        let entry = ledger.get(&id).unwrap().clone();
        let doc = LedgerDocument::from_entry(&entry, "Build src/inbox.rs with CAS persistence.");
        let written = ledger.write_document(&directory, &id, &doc).unwrap();
        assert!(written.is_file());

        assert!(ledger.link_decision(&id, "AC-INBOX-1", 1_700_000_050));
        assert_eq!(ledger.get(&id).unwrap().status, TaskStatus::NeedsApproval);
        assert!(ledger
            .record_decision_answer(
                Some(&directory),
                &id,
                "AC-INBOX-1",
                "Use pure Rust atomic JSON.",
                1_700_000_100,
            )
            .unwrap());
        assert_eq!(ledger.get(&id).unwrap().status, TaskStatus::Running);
        let content = ledger.read_document(&directory, &id).unwrap();
        assert!(content.contains("Implement Decision Inbox"));
        assert!(content.contains("Use pure Rust atomic JSON."));

        assert!(ledger.update_latest(
            "builder",
            &TaskUpdate::status(TaskStatus::Done),
            1_700_000_200,
        ));
        let compacted = ledger
            .compact_weekly_archive(&directory, 1_700_700_000, 7 * 86_400)
            .unwrap();
        assert_eq!(compacted.archived_ids, vec![id.clone()]);
        let archived_entry = ledger.get(&id).unwrap();
        assert!(archived_entry.archived_at.is_some());
        assert!(archived_entry
            .doc_rel_path
            .starts_with("ledger-docs/archive/"));
        let archived_doc = ledger.read_document(&directory, &id).unwrap();
        assert!(archived_doc.contains("Implement Decision Inbox"));
        let index_path = directory.join(&compacted.index_rel_path);
        let index_text = fs::read_to_string(&index_path).unwrap();
        assert!(index_text.contains(&format!("- [{id}](")));
        assert!(resolve_safe_doc_path(&directory, "../escape.md", false).is_err());
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
        assert_eq!(ledger.archived_overflow().len(), 5);
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
