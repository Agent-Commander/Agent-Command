use std::{collections::BTreeMap, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    bridge,
    evolution::OutcomeSignal,
    inbox::{
        DecisionCard, DecisionReadiness, DecisionSubmitRequest, DraftUpdateRequest, InboxError,
        InboxFilter,
    },
    ledger::{LedgerDocument, LedgerEntry, LedgerFilter, TaskKind, TaskStatus, TaskUpdate},
    specialists::{self, Status, COMMANDER},
    token_burn::{normalize_tier, AccountCard},
    tui::provider_short_name,
    App,
};

pub const LEFT_PANEL_AUTO_MIN_WIDTH: u16 = 120;
pub const LEFT_PANEL_MIN_WIDTH: u16 = 100;
pub const RAIL_MIN_WIDTH: u16 = 100;
pub const WIDE_LAYOUT_MIN_WIDTH: u16 = 132;
pub const MIN_CANVAS_WIDTH: u16 = 48;
const WAKE_NOTE_LIMIT: usize = 20;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ViewTab {
    #[default]
    Chat,
    Inbox,
    Ledger,
}

impl ViewTab {
    pub const ALL: [Self; 3] = [Self::Chat, Self::Inbox, Self::Ledger];

    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Inbox => "Inbox",
            Self::Ledger => "Ledger",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Chat => Self::Inbox,
            Self::Inbox => Self::Ledger,
            Self::Ledger => Self::Chat,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Chat => Self::Ledger,
            Self::Inbox => Self::Chat,
            Self::Ledger => Self::Inbox,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DecisionFilter {
    #[default]
    All,
    Unanswered,
    Drafts,
    Preparation,
    Past,
}

impl DecisionFilter {
    pub const ALL: [Self; 5] = [
        Self::All,
        Self::Unanswered,
        Self::Drafts,
        Self::Preparation,
        Self::Past,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Unanswered => "Unanswered",
            Self::Drafts => "Drafts",
            Self::Preparation => "Preparation",
            Self::Past => "Past",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Unanswered,
            Self::Unanswered => Self::Drafts,
            Self::Drafts => Self::Preparation,
            Self::Preparation => Self::Past,
            Self::Past => Self::All,
        }
    }

    pub fn to_inbox_filter(self) -> InboxFilter {
        match self {
            Self::All => InboxFilter::All,
            Self::Unanswered => InboxFilter::Unanswered,
            Self::Drafts => InboxFilter::Drafts,
            Self::Preparation => InboxFilter::Preparation,
            Self::Past => InboxFilter::Past,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionModalState {
    pub card_id: String,
    pub selected_option: Option<usize>,
    pub write_in: String,
    pub explain_mode: bool,
    pub explain_note: String,
    pub conflict_banner: Option<String>,
    pub scroll: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerDocViewer {
    pub entry_id: String,
    pub title: String,
    pub summary: String,
    pub markdown: String,
    pub scroll: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VaultModalMode {
    List,
    PromptSecret,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultModalState {
    pub mode: VaultModalMode,
    pub entries: Vec<crate::vault::VaultMeta>,
    pub selected: usize,
    pub request_id: Option<String>,
    pub name: String,
    pub namespace: String,
    pub description: String,
    pub secret_input: String,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LeftPanelMode {
    #[default]
    Auto,
    Shown,
    Hidden,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PanelWidths {
    pub left: u16,
    pub rail: u16,
}

pub fn left_panel_visible(width: u16, mode: LeftPanelMode) -> bool {
    match mode {
        LeftPanelMode::Auto => width >= LEFT_PANEL_AUTO_MIN_WIDTH,
        LeftPanelMode::Shown => width >= LEFT_PANEL_MIN_WIDTH,
        LeftPanelMode::Hidden => false,
    }
}

pub fn panel_widths(width: u16, mode: LeftPanelMode, rail_open: bool) -> PanelWidths {
    let left = left_panel_visible(width, mode);
    let rail = rail_open && width >= RAIL_MIN_WIDTH;
    let (left_w, rail_w) = match (left, width) {
        (false, _) => (0, 34),
        (true, w) if w >= WIDE_LAYOUT_MIN_WIDTH => (30, 34),
        (true, w) if w >= LEFT_PANEL_AUTO_MIN_WIDTH => (26, 30),
        (true, _) => (24, 28),
    };
    PanelWidths {
        left: if left { left_w } else { 0 },
        rail: if rail { rail_w } else { 0 },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboxAction {
    Open,
    Approve,
    Deny,
    Diff,
    Pr,
    Dismiss,
}

impl InboxAction {
    pub fn key_hint(self) -> &'static str {
        match self {
            Self::Open => "⏎",
            Self::Approve => "a",
            Self::Deny => "n",
            Self::Diff => "d",
            Self::Pr => "p",
            Self::Dismiss => "x",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabHit {
    SelectTab(ViewTab),
    ToggleLeftPanel,
    UsageTile(usize),
    JumpThread(usize),
    InboxRow(usize),
    InboxAction(usize, InboxAction),
    DecisionFilter(DecisionFilter),
    InboxSearch,
    DecisionRow(usize),
    DecisionModalOption(usize),
    DecisionModalSubmit,
    DecisionModalSaveDraft,
    DecisionModalExplain,
    DecisionModalRetire,
    DecisionModalReconcile,
    DecisionModalClose,
    LedgerRow(usize),
    LedgerFilter(LedgerFilter),
    LedgerDocJump,
    LedgerDocDiff,
    LedgerDocClose,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WakeNote {
    pub serial: u64,
    pub worker: String,
    pub summary: String,
    pub branch: Option<String>,
    pub is_error: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InboxKind {
    ProdGate { req_id: String },
    AskUser { request_id: String },
    Continuation,
    Member(Status),
    Wake { is_error: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboxItem {
    pub key: String,
    pub kind: InboxKind,
    pub title: String,
    pub detail: String,
    pub thread: String,
}

impl InboxItem {
    pub fn actions(&self) -> &'static [(InboxAction, &'static str)] {
        match self.kind {
            InboxKind::ProdGate { .. } => &[
                (InboxAction::Approve, "Approve"),
                (InboxAction::Deny, "Deny"),
                (InboxAction::Open, "Jump"),
            ],
            InboxKind::AskUser { .. } => &[
                (InboxAction::Open, "Jump"),
                (InboxAction::Dismiss, "Dismiss"),
            ],
            InboxKind::Continuation => &[
                (InboxAction::Open, "Resume"),
                (InboxAction::Deny, "Abandon"),
            ],
            InboxKind::Member(Status::Done | Status::NeedsApproval) => &[
                (InboxAction::Open, "Open"),
                (InboxAction::Approve, "Approve"),
                (InboxAction::Deny, "Disapprove"),
                (InboxAction::Diff, "Diff"),
                (InboxAction::Pr, "PR"),
                (InboxAction::Dismiss, "Read"),
            ],
            InboxKind::Member(_) => &[
                (InboxAction::Open, "Open"),
                (InboxAction::Diff, "Diff"),
                (InboxAction::Pr, "PR"),
                (InboxAction::Dismiss, "Read"),
            ],
            InboxKind::Wake { .. } => &[
                (InboxAction::Open, "Open"),
                (InboxAction::Approve, "Approve"),
                (InboxAction::Deny, "Disapprove"),
                (InboxAction::Diff, "Diff"),
                (InboxAction::Dismiss, "Read"),
            ],
        }
    }

    pub fn needs_action(&self) -> bool {
        matches!(
            self.kind,
            InboxKind::ProdGate { .. }
                | InboxKind::AskUser { .. }
                | InboxKind::Continuation
                | InboxKind::Member(Status::NeedsApproval | Status::Error)
        )
    }

    fn supports(&self, action: InboxAction) -> bool {
        self.actions()
            .iter()
            .any(|(candidate, _)| *candidate == action)
    }
}

fn member_inbox_rank(status: Status) -> Option<u8> {
    match status {
        Status::NeedsApproval => Some(0),
        Status::Error => Some(1),
        Status::Done => Some(2),
        Status::Snoozed => Some(3),
        Status::Idle | Status::Running => None,
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn matches_decision_query(card: &DecisionCard, query: &str) -> bool {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return true;
    }
    card.id.to_ascii_lowercase().contains(&needle)
        || card.title.to_ascii_lowercase().contains(&needle)
        || card.question.to_ascii_lowercase().contains(&needle)
        || card.project.to_ascii_lowercase().contains(&needle)
        || card.group.to_ascii_lowercase().contains(&needle)
        || card.owner_name.to_ascii_lowercase().contains(&needle)
}

fn sort_decision_cards(cards: &mut [DecisionCard]) {
    cards.sort_by(|left, right| {
        let tier = |card: &DecisionCard| match card.readiness {
            DecisionReadiness::Ready => 0u8,
            DecisionReadiness::NotReady => 1u8,
            DecisionReadiness::Answered => 2u8,
            DecisionReadiness::ResolvedStale => 3u8,
        };
        tier(left)
            .cmp(&tier(right))
            .then_with(|| right.is_current_blocker.cmp(&left.is_current_blocker))
            .then_with(|| left.project.cmp(&right.project))
            .then_with(|| left.group.cmp(&right.group))
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn matches_live_query(item: &InboxItem, query: &str) -> bool {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return true;
    }
    item.title.to_ascii_lowercase().contains(&needle)
        || item.detail.to_ascii_lowercase().contains(&needle)
        || item.thread.to_ascii_lowercase().contains(&needle)
}

impl App {
    pub fn left_panel_visible(&self) -> bool {
        left_panel_visible(self.last_frame_width, self.left_panel)
    }

    pub fn toggle_left_panel(&mut self) {
        let visible = self.left_panel_visible();
        self.left_panel = if visible {
            LeftPanelMode::Hidden
        } else {
            LeftPanelMode::Shown
        };
        if visible {
            self.left_focused = false;
        }
        self.notice = if visible {
            "Left panel hidden · F8 or Ctrl+\\ to show".into()
        } else if self.last_frame_width > 0 && self.last_frame_width < LEFT_PANEL_MIN_WIDTH {
            format!("Left panel needs {LEFT_PANEL_MIN_WIDTH}+ columns · tabs stay in the top bar")
        } else {
            "Left panel shown · F8 or Ctrl+\\ to hide".into()
        };
    }

    pub fn set_view_tab(&mut self, tab: ViewTab) {
        self.view_tab = tab;
        self.inbox_searching = false;
        self.clamp_tab_selection();
        if tab == ViewTab::Chat {
            self.editing = true;
        }
    }

    pub fn set_inbox_filter(&mut self, filter: DecisionFilter) {
        self.inbox_filter = filter;
        self.inbox_selected = 0;
        self.clamp_tab_selection();
    }

    fn clamp_tab_selection(&mut self) {
        let inbox_len = self.visible_inbox_total();
        self.inbox_selected = self.inbox_selected.min(inbox_len.saturating_sub(1));
        let ledger_len = self.ledger_rows().len();
        self.ledger_selected = self.ledger_selected.min(ledger_len.saturating_sub(1));
    }

    pub fn inbox_items(&self) -> Vec<InboxItem> {
        let mut items = Vec::new();
        self.push_prod_gate_items(&mut items);
        self.push_ask_user_items(&mut items);
        self.push_continuation_item(&mut items);
        self.push_member_items(&mut items);
        self.push_wake_items(&mut items);
        items.retain(|item| !self.inbox_dismissed.contains(&item.key));
        items
    }

    pub fn filtered_live_inbox_items(&self) -> Vec<InboxItem> {
        if !matches!(
            self.inbox_filter,
            DecisionFilter::All | DecisionFilter::Unanswered
        ) {
            return Vec::new();
        }
        self.inbox_items()
            .into_iter()
            .filter(|item| matches_live_query(item, &self.inbox_search))
            .collect()
    }

    pub fn filtered_decision_cards(&self) -> Vec<DecisionCard> {
        let filter = self.inbox_filter.to_inbox_filter();
        let mut cards: Vec<DecisionCard> = self
            .decision_cards
            .iter()
            .filter(|card| card.record_error.is_none())
            .filter(|card| match filter {
                InboxFilter::All => card.is_active_open(),
                InboxFilter::Unanswered => {
                    card.is_active_open()
                        && card.readiness == DecisionReadiness::Ready
                        && !card.has_draft()
                }
                InboxFilter::Drafts => card.is_active_open() && card.has_draft(),
                InboxFilter::Answered => {
                    !card.retired && card.readiness == DecisionReadiness::Answered
                }
                InboxFilter::Preparation => {
                    card.is_active_open() && card.readiness == DecisionReadiness::NotReady
                }
                InboxFilter::Past => card.readiness.is_past() || card.retired,
            })
            .filter(|card| matches_decision_query(card, &self.inbox_search))
            .cloned()
            .collect();
        sort_decision_cards(&mut cards);
        cards
    }

    pub fn past_decision_cards(&self) -> Vec<DecisionCard> {
        let mut cards: Vec<DecisionCard> = self
            .decision_cards
            .iter()
            .filter(|card| card.record_error.is_none())
            .filter(|card| card.readiness.is_past() || card.retired)
            .filter(|card| matches_decision_query(card, &self.inbox_search))
            .cloned()
            .collect();
        sort_decision_cards(&mut cards);
        cards
    }

    pub fn decision_filter_count(&self, filter: DecisionFilter) -> usize {
        self.decision_cards
            .iter()
            .filter(|card| card.record_error.is_none())
            .filter(|card| match filter {
                DecisionFilter::All => card.is_active_open(),
                DecisionFilter::Unanswered => {
                    card.is_active_open()
                        && card.readiness == DecisionReadiness::Ready
                        && !card.has_draft()
                }
                DecisionFilter::Drafts => card.is_active_open() && card.has_draft(),
                DecisionFilter::Preparation => {
                    card.is_active_open() && card.readiness == DecisionReadiness::NotReady
                }
                DecisionFilter::Past => card.readiness.is_past() || card.retired,
            })
            .count()
    }

    fn visible_inbox_total(&self) -> usize {
        self.filtered_live_inbox_items().len() + self.filtered_decision_cards().len()
    }

    pub fn active_decision_card_count(&self) -> usize {
        self.decision_cards
            .iter()
            .filter(|card| card.is_active_open())
            .count()
    }

    pub fn inbox_count(&self) -> usize {
        self.inbox_items().len() + self.active_decision_card_count()
    }

    fn inbox_store_path(&self) -> Option<PathBuf> {
        let ledger = self.ledger_path.as_ref()?;
        let dir = ledger.parent()?;
        let stem = ledger
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("ledger");
        let suffix = stem.strip_prefix("ledger-").unwrap_or("default");
        Some(dir.join(format!("inbox-{suffix}.json")))
    }

    fn inbox_outbox_path(&self) -> Option<PathBuf> {
        let ledger = self.ledger_path.as_ref()?;
        let dir = ledger.parent()?;
        let stem = ledger
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("ledger");
        let suffix = stem.strip_prefix("ledger-").unwrap_or("default");
        Some(dir.join(format!("inbox-outbox-{suffix}.jsonl")))
    }

    fn persist_inbox_store(&self) {
        let Some(path) = self.inbox_store_path() else {
            return;
        };
        if let Some(store) = self
            .live
            .as_ref()
            .and_then(|live| live.inbox_store.as_ref())
        {
            let _ = store.save(&path);
        }
    }

    fn sync_decision_cards_from_store(&mut self) {
        if let Some(cards) = self
            .live
            .as_ref()
            .and_then(|live| live.inbox_store.as_ref())
            .map(|store| store.cards().to_vec())
        {
            self.decision_cards = cards;
        }
    }

    pub fn open_inbox_modal(&mut self, card_id: &str) -> bool {
        let Some(card) = self
            .decision_cards
            .iter()
            .find(|card| card.id == card_id)
            .cloned()
        else {
            return false;
        };
        let selected_option = card.draft.as_ref().and_then(|draft| {
            draft.selected_option_id.as_deref().and_then(|opt_id| {
                card.alternatives
                    .iter()
                    .position(|option| option.id == opt_id)
            })
        });
        let write_in = card
            .draft
            .as_ref()
            .map(|draft| draft.write_in.clone())
            .unwrap_or_default();
        self.inbox_searching = false;
        self.editing = false;
        self.inbox_modal = Some(DecisionModalState {
            card_id: card.id,
            selected_option,
            write_in,
            explain_mode: false,
            explain_note: String::new(),
            conflict_banner: None,
            scroll: 0,
        });
        true
    }

    pub fn save_inbox_draft(&mut self) -> bool {
        let Some(modal) = self.inbox_modal.clone() else {
            return false;
        };
        let Some(card) = self
            .decision_cards
            .iter()
            .find(|card| card.id == modal.card_id)
            .cloned()
        else {
            return false;
        };
        let selected_option_id = modal
            .selected_option
            .and_then(|idx| card.alternatives.get(idx))
            .map(|opt| opt.id.clone());
        let stamp = now();
        let req = DraftUpdateRequest {
            expected_version: card.record_version,
            expected_fingerprint: card.content_fingerprint.clone(),
            selected_option_id: selected_option_id.clone(),
            write_in: modal.write_in.clone(),
            direction_choices: BTreeMap::new(),
            force_reconcile: false,
        };
        if let Some(store) = self
            .live
            .as_mut()
            .and_then(|live| live.inbox_store.as_mut())
        {
            match store.save_draft(&modal.card_id, req, stamp) {
                Ok(_) => {
                    self.persist_inbox_store();
                    self.sync_decision_cards_from_store();
                    if let Some(active) = self.inbox_modal.as_mut() {
                        active.conflict_banner = None;
                    }
                    self.notice = format!("Draft saved for {}.", modal.card_id);
                    return true;
                }
                Err(InboxError::Conflict { message, .. }) => {
                    if let Some(active) = self.inbox_modal.as_mut() {
                        active.conflict_banner = Some(message);
                    }
                    return false;
                }
                Err(error) => {
                    self.notice = format!("Draft save failed: {error}");
                    return false;
                }
            }
        }
        if let Some(target) = self
            .decision_cards
            .iter_mut()
            .find(|entry| entry.id == modal.card_id)
        {
            target.draft = Some(crate::inbox::DecisionDraft {
                selected_option_id,
                write_in: modal.write_in,
                direction_choices: BTreeMap::new(),
                content_fingerprint: target.content_fingerprint.clone(),
                saved_at: stamp,
            });
            target.record_version += 1;
            target.updated_at = stamp;
            self.notice = format!("Draft saved for {}.", modal.card_id);
            return true;
        }
        false
    }

    pub fn submit_inbox_modal(&mut self, force_reconcile: bool) -> bool {
        let Some(modal) = self.inbox_modal.clone() else {
            return false;
        };
        let Some(card) = self
            .decision_cards
            .iter()
            .find(|card| card.id == modal.card_id)
            .cloned()
        else {
            return false;
        };
        let selected_option_id = modal
            .selected_option
            .and_then(|idx| card.alternatives.get(idx))
            .map(|opt| opt.id.clone());
        let stamp = now();
        let operation_id = format!("op-{}", uuid::Uuid::new_v4().simple());
        let outbox = self.inbox_outbox_path();
        let req = DecisionSubmitRequest {
            operation_id,
            expected_version: card.record_version,
            expected_fingerprint: card.content_fingerprint.clone(),
            selected_option_id: selected_option_id.clone(),
            write_in: modal.write_in.clone(),
            direction_choices: BTreeMap::new(),
            force_reconcile,
        };
        let live_count = self.filtered_live_inbox_items().len();
        let current_cards = self.filtered_decision_cards();
        let fallback_neighbor = current_cards
            .iter()
            .position(|entry| entry.id == modal.card_id)
            .and_then(|idx| {
                current_cards
                    .get(idx + 1)
                    .or_else(|| idx.checked_sub(1).and_then(|prev| current_cards.get(prev)))
                    .map(|neighbor| neighbor.id.clone())
            });

        let outcome = if let Some(store) = self
            .live
            .as_mut()
            .and_then(|live| live.inbox_store.as_mut())
        {
            match store.submit_answer(&modal.card_id, req, stamp, outbox.as_deref()) {
                Ok(result) => {
                    self.persist_inbox_store();
                    self.sync_decision_cards_from_store();
                    Ok((
                        result.answer.rendered_text(&card),
                        result.next_neighbor_id.or(fallback_neighbor),
                    ))
                }
                Err(err) => Err(err),
            }
        } else if selected_option_id.is_none() && modal.write_in.trim().is_empty() {
            Err(InboxError::Validation(
                "Select an option or enter a write-in answer.".into(),
            ))
        } else {
            let mut rendered_parts = Vec::new();
            if let Some(opt_id) = &selected_option_id {
                if let Some(opt) = card.alternatives.iter().find(|o| &o.id == opt_id) {
                    rendered_parts.push(format!("{} ({})", opt.label, opt.id));
                } else {
                    rendered_parts.push(opt_id.clone());
                }
            }
            if !modal.write_in.trim().is_empty() {
                rendered_parts.push(modal.write_in.trim().to_owned());
            }
            if let Some(target) = self
                .decision_cards
                .iter_mut()
                .find(|entry| entry.id == modal.card_id)
            {
                let delivery_id = format!("del-{}", uuid::Uuid::new_v4().simple());
                let answer = crate::inbox::SubmittedAnswer {
                    operation_id: req.operation_id,
                    selected_option_id,
                    write_in: modal.write_in,
                    direction_choices: BTreeMap::new(),
                    content_fingerprint: target.content_fingerprint.clone(),
                    content_revision: target.content_revision,
                    submitted_at: stamp,
                    delivery_id,
                };
                target.answer = Some(answer.clone());
                target.history.push(answer);
                target.draft = None;
                target.readiness = DecisionReadiness::Answered;
                target.answer_enabled = false;
                target.record_version += 1;
                target.updated_at = stamp;
                target.readiness_since_utc = stamp;
            }
            Ok((rendered_parts.join(" — "), fallback_neighbor))
        };

        match outcome {
            Ok((answer_text, next_neighbor)) => {
                let state_dir = self.ledger_state_dir();
                if self
                    .ledger
                    .record_decision_answer(
                        state_dir.as_deref(),
                        &card.source_row,
                        &card.id,
                        &answer_text,
                        stamp,
                    )
                    .unwrap_or(false)
                {
                    self.save_ledger();
                }
                self.inbox_modal = None;
                self.editing = false;
                if let Some(neighbor_id) = next_neighbor {
                    let after_cards = self.filtered_decision_cards();
                    if let Some(pos) = after_cards.iter().position(|c| c.id == neighbor_id) {
                        self.inbox_selected = live_count + pos;
                    } else {
                        self.clamp_tab_selection();
                    }
                } else {
                    self.clamp_tab_selection();
                }
                self.notice = format!("Submitted answer for {}.", card.id);
                true
            }
            Err(InboxError::Conflict { message, .. }) => {
                if let Some(active) = self.inbox_modal.as_mut() {
                    active.conflict_banner = Some(message);
                }
                false
            }
            Err(error) => {
                self.notice = format!("Cannot submit {}: {error}", card.id);
                false
            }
        }
    }

    pub fn request_explain_more(&mut self, card_id: &str, note: &str) -> bool {
        if note.trim().is_empty() {
            self.notice = "Enter a short note explaining what is unclear.".into();
            return false;
        }
        let stamp = now();
        let outbox = self.inbox_outbox_path();
        if let Some(store) = self
            .live
            .as_mut()
            .and_then(|live| live.inbox_store.as_mut())
        {
            match store.explain_more(card_id, note, stamp, outbox.as_deref()) {
                Ok(_) => {
                    self.persist_inbox_store();
                    self.sync_decision_cards_from_store();
                }
                Err(error) => {
                    self.notice = format!("Explain more failed: {error}");
                    return false;
                }
            }
        } else if let Some(card) = self
            .decision_cards
            .iter_mut()
            .find(|entry| entry.id == card_id)
        {
            card.explain_more.push(crate::inbox::ExplainMoreRequest {
                id: format!("exp-{}", uuid::Uuid::new_v4().simple()),
                note: note.trim().to_owned(),
                requested_at: stamp,
                digest_at_request: card.item_digest.clone(),
            });
            card.being_rewritten = true;
            card.rewritten_since_explain = false;
            card.record_version += 1;
            card.updated_at = stamp;
        } else {
            return false;
        }
        if self
            .inbox_modal
            .as_ref()
            .is_some_and(|modal| modal.card_id == card_id)
        {
            self.inbox_modal = None;
        }
        self.clamp_tab_selection();
        self.notice = format!("Requested rewrite for {card_id}; hidden until updated.");
        true
    }

    pub fn retire_inbox_card(&mut self, card_id: &str, reason: &str) -> bool {
        let stamp = now();
        let outbox = self.inbox_outbox_path();
        if let Some(store) = self
            .live
            .as_mut()
            .and_then(|live| live.inbox_store.as_mut())
        {
            match store.retire(card_id, reason, stamp, outbox.as_deref()) {
                Ok(()) => {
                    self.persist_inbox_store();
                    self.sync_decision_cards_from_store();
                }
                Err(error) => {
                    self.notice = format!("Retire failed: {error}");
                    return false;
                }
            }
        } else if let Some(card) = self
            .decision_cards
            .iter_mut()
            .find(|entry| entry.id == card_id)
        {
            card.retired = true;
            card.answer_enabled = false;
            card.record_version += 1;
            card.updated_at = stamp;
        } else {
            return false;
        }
        if self
            .inbox_modal
            .as_ref()
            .is_some_and(|modal| modal.card_id == card_id)
        {
            self.inbox_modal = None;
        }
        self.clamp_tab_selection();
        self.notice = format!("Retired card {card_id} (underlying task unchanged).");
        true
    }

    pub fn link_previous_decision(&mut self, card_id: &str, target_id: &str, note: &str) -> bool {
        let stamp = now();
        let outbox = self.inbox_outbox_path();
        if let Some(store) = self
            .live
            .as_mut()
            .and_then(|live| live.inbox_store.as_mut())
        {
            match store.link_previous(card_id, target_id, note, stamp, outbox.as_deref()) {
                Ok(_) => {
                    self.persist_inbox_store();
                    self.sync_decision_cards_from_store();
                }
                Err(error) => {
                    self.notice = format!("Link previous failed: {error}");
                    return false;
                }
            }
        } else {
            let Some(target) = self
                .decision_cards
                .iter()
                .find(|entry| entry.id == target_id)
                .cloned()
            else {
                return false;
            };
            let Some(card) = self
                .decision_cards
                .iter_mut()
                .find(|entry| entry.id == card_id)
            else {
                return false;
            };
            card.previous_decision = Some(crate::inbox::PreviousDecisionLink {
                target_id: target.id,
                source_row: target.source_row,
                content_fingerprint: target.content_fingerprint,
                note: note.trim().to_owned(),
                linked_at: stamp,
            });
            card.readiness = DecisionReadiness::ResolvedStale;
            card.answer_enabled = false;
            card.record_version += 1;
            card.updated_at = stamp;
        }
        if self
            .inbox_modal
            .as_ref()
            .is_some_and(|modal| modal.card_id == card_id)
        {
            self.inbox_modal = None;
        }
        self.clamp_tab_selection();
        self.notice = format!("Linked {card_id} to previous decision {target_id}.");
        true
    }

    fn record_bot_feedback(&mut self, bot_id: &str, approved: bool, detail: &str) {
        let signal = if approved {
            OutcomeSignal::UserApproved
        } else {
            OutcomeSignal::UserDisapproved
        };
        let report = self
            .live
            .as_ref()
            .and_then(|live| live.evolution_store.as_ref())
            .and_then(|store| {
                store
                    .record_feedback(bot_id, signal, None, None, Some(detail), now())
                    .ok()
            });
        if let Some(report) = report {
            self.bot_badges
                .insert(report.bot_id.clone(), report.lifecycle.badge().to_owned());
            if report.lifecycle == crate::evolution::LifecycleState::Decommissioned {
                self.decommissioned_bots.insert(report.bot_id);
            }
        }
    }

    fn push_prod_gate_items(&self, items: &mut Vec<InboxItem>) {
        let Some(live) = &self.live else { return };
        let mut gates: Vec<(&String, &String)> = live.pending_prod_gates.iter().collect();
        gates.sort();
        for (req_id, thread) in gates {
            items.push(InboxItem {
                key: format!("gate:{req_id}"),
                kind: InboxKind::ProdGate {
                    req_id: req_id.clone(),
                },
                title: format!("ProdGate approval · {thread}"),
                detail: "A gated command is waiting for Approve or Deny".into(),
                thread: thread.clone(),
            });
        }
    }

    fn push_ask_user_items(&self, items: &mut Vec<InboxItem>) {
        let Some(broker) = self.live.as_ref().and_then(|live| live.ask_user.as_ref()) else {
            return;
        };
        for request in broker.pending() {
            items.push(InboxItem {
                key: format!("ask:{}", request.request_id),
                kind: InboxKind::AskUser {
                    request_id: request.request_id.clone(),
                },
                title: format!("Question · {}", request.question),
                detail: request
                    .context
                    .clone()
                    .unwrap_or_else(|| request.choices.join(" / ")),
                thread: COMMANDER.into(),
            });
        }
    }

    fn push_continuation_item(&self, items: &mut Vec<InboxItem>) {
        let Some(live) = &self.live else { return };
        if live.pending.is_none() || live.running(COMMANDER) {
            return;
        }
        items.push(InboxItem {
            key: "continuation".into(),
            kind: InboxKind::Continuation,
            title: "Interrupted continuation pending".into(),
            detail: "Resume with your next Commander message, or abandon it".into(),
            thread: COMMANDER.into(),
        });
    }

    fn push_member_items(&self, items: &mut Vec<InboxItem>) {
        let mut ranked: Vec<(u8, usize)> = self
            .crew
            .iter()
            .enumerate()
            .filter_map(|(index, member)| {
                member_inbox_rank(member.status).map(|rank| (rank, index))
            })
            .collect();
        ranked.sort();
        for (_, index) in ranked {
            let member = &self.crew[index];
            let changes = member
                .changes
                .as_ref()
                .map(|changes| format!(" · {}", changes.totals()))
                .unwrap_or_default();
            items.push(InboxItem {
                key: format!(
                    "member:{}:{}:{}",
                    member.name,
                    member.status.label(),
                    member.history.len()
                ),
                kind: InboxKind::Member(member.status),
                title: format!("{} · {}", member.name, member.status.caption()),
                detail: format!("{}{changes}", member.mission),
                thread: member.name.clone(),
            });
        }
    }

    fn push_wake_items(&self, items: &mut Vec<InboxItem>) {
        for wake in self.inbox_wakes.iter().rev() {
            let outcome = if wake.is_error { "failed" } else { "finished" };
            items.push(InboxItem {
                key: format!("wake:{}", wake.serial),
                kind: InboxKind::Wake {
                    is_error: wake.is_error,
                },
                title: format!("Worker {} {outcome}", wake.worker),
                detail: wake.summary.clone(),
                thread: wake.worker.clone(),
            });
        }
    }

    pub(crate) fn note_worker_wake(&mut self, result: &bridge::BackgroundResult) {
        let serial = self.inbox_wakes.last().map_or(1, |wake| wake.serial + 1);
        self.inbox_wakes.push(WakeNote {
            serial,
            worker: result.worker_id.clone(),
            summary: result.summary.clone(),
            branch: result.branch.clone(),
            is_error: result.is_error,
        });
        let excess = self.inbox_wakes.len().saturating_sub(WAKE_NOTE_LIMIT);
        self.inbox_wakes.drain(..excess);
        let status = if result.is_error {
            TaskStatus::Error
        } else {
            TaskStatus::Done
        };
        let update = TaskUpdate {
            status,
            branch: result.branch.clone(),
            changed_files: None,
        };
        let stamp = now();
        if !self.ledger.update_latest(&result.worker_id, &update, stamp) {
            let id = self
                .ledger
                .record(TaskKind::Spawn, &result.worker_id, &result.summary, stamp);
            self.ledger.update_latest(&result.worker_id, &update, stamp);
            self.write_ledger_doc_for(&id, &result.summary);
        } else if let Some(entry_id) = self
            .ledger
            .latest_for(&result.worker_id)
            .map(|entry| entry.id.clone())
        {
            if let Some(dir) = self.ledger_state_dir() {
                let _ = self.ledger.append_document(
                    &dir,
                    &entry_id,
                    if result.is_error {
                        "Worker Error"
                    } else {
                        "Worker Finished"
                    },
                    &result.summary,
                    stamp,
                );
            }
        }
        self.save_ledger();
    }

    pub fn inbox_act(&mut self, index: usize, action: InboxAction) {
        let live_items = self.filtered_live_inbox_items();
        if index < live_items.len() {
            let item = live_items[index].clone();
            if !item.supports(action) {
                return;
            }
            self.inbox_selected = index;
            match (&item.kind, action) {
                (InboxKind::ProdGate { req_id }, InboxAction::Approve) => {
                    self.resolve_prod_gate(req_id, &item.thread, true)
                }
                (InboxKind::ProdGate { req_id }, InboxAction::Deny) => {
                    self.resolve_prod_gate(req_id, &item.thread, false)
                }
                (InboxKind::Continuation, InboxAction::Deny) => self.abandon_continuation(),
                (InboxKind::Continuation, InboxAction::Open) => {
                    self.jump_to_thread(COMMANDER);
                    self.notice =
                        "Type your next Commander message to resume the continuation.".into();
                }
                (InboxKind::AskUser { request_id }, InboxAction::Dismiss) => {
                    if let Some(broker) = self.live.as_ref().and_then(|live| live.ask_user.as_ref())
                    {
                        broker.answer(
                            request_id,
                            bridge::AskUserAnswer {
                                selected_index: None,
                                answer: String::new(),
                            },
                        );
                    }
                    self.inbox_dismissed.insert(item.key.clone());
                }
                (InboxKind::Member(_) | InboxKind::Wake { .. }, InboxAction::Approve) => {
                    self.record_bot_feedback(&item.thread, true, &item.detail);
                    self.inbox_dismissed.insert(item.key.clone());
                    self.notice = format!("Approved work from {}.", item.thread);
                }
                (InboxKind::Member(_) | InboxKind::Wake { .. }, InboxAction::Deny) => {
                    self.record_bot_feedback(&item.thread, false, &item.detail);
                    self.inbox_dismissed.insert(item.key.clone());
                    self.notice = format!("Disapproved work from {}.", item.thread);
                }
                (_, InboxAction::Open) => {
                    self.mark_inbox_read(&item);
                    self.jump_to_thread(&item.thread);
                }
                (_, InboxAction::Diff) => {
                    self.mark_inbox_read(&item);
                    self.open_diff_viewer(Some(&item.thread));
                }
                (_, InboxAction::Pr) => {
                    self.mark_inbox_read(&item);
                    let thread = item.thread.clone();
                    self.open_pr(Some(&thread), false);
                }
                (_, InboxAction::Dismiss) => {
                    self.inbox_dismissed.insert(item.key.clone());
                    self.notice = format!("Marked read: {}", item.title);
                }
                _ => {}
            }
            self.clamp_tab_selection();
            return;
        }
        let card_idx = index.saturating_sub(live_items.len());
        let cards = self.filtered_decision_cards();
        let Some(card) = cards.get(card_idx).cloned() else {
            return;
        };
        self.inbox_selected = index;
        match action {
            InboxAction::Open => {
                self.open_inbox_modal(&card.id);
            }
            InboxAction::Dismiss => {
                self.retire_inbox_card(&card.id, "Retired from Inbox feed");
            }
            _ => {}
        }
        self.clamp_tab_selection();
    }

    fn mark_inbox_read(&mut self, item: &InboxItem) {
        if matches!(
            item.kind,
            InboxKind::Member(Status::Done | Status::Snoozed) | InboxKind::Wake { .. }
        ) {
            self.inbox_dismissed.insert(item.key.clone());
        }
    }

    pub fn jump_to_thread(&mut self, name: &str) {
        if let Some(index) = specialists::find(&self.crew, name) {
            self.select_member_thread(index);
        }
        self.left_focused = false;
        self.rail_focused = false;
        self.set_view_tab(ViewTab::Chat);
    }

    pub(crate) fn resolve_prod_gate(&mut self, req_id: &str, thread: &str, allow: bool) {
        let run_name = self
            .live
            .as_mut()
            .and_then(|live| live.pending_prod_gates.remove(req_id))
            .unwrap_or_else(|| thread.to_owned());
        if let Some(live) = &self.live {
            if let Some(pos) = live.run_position(&run_name) {
                let reason = (!allow).then_some("User denied destructive action via ProdGate.");
                let _ = live.runs[pos]
                    .mission
                    .respond_permission(req_id, allow, reason);
            }
        }
        self.transcript_for_mut(&run_name).resolve_prod_gate(req_id);
        self.notice = if allow {
            "Allowed ProdGate action.".into()
        } else {
            "Denied ProdGate action.".into()
        };
    }

    pub fn ledger_rows(&self) -> Vec<LedgerEntry> {
        self.ledger
            .filtered(self.ledger_filter)
            .into_iter()
            .cloned()
            .collect()
    }

    pub fn ledger_entries(&self) -> &[LedgerEntry] {
        self.ledger.entries()
    }

    pub fn set_ledger_filter(&mut self, filter: LedgerFilter) {
        self.ledger_filter = filter;
        self.ledger_selected = 0;
        self.ledger_viewer = None;
    }

    fn ledger_state_dir(&self) -> Option<PathBuf> {
        self.ledger_path
            .as_deref()
            .and_then(|path| path.parent().map(PathBuf::from))
    }

    fn write_ledger_doc_for(&self, id: &str, brief: &str) {
        let Some(dir) = self.ledger_state_dir() else {
            return;
        };
        let Some(entry) = self.ledger.get(id) else {
            return;
        };
        let doc = LedgerDocument::from_entry(entry, brief);
        let _ = self.ledger.write_document(&dir, id, &doc);
    }

    pub fn open_ledger_document(&mut self, index: usize) -> bool {
        let Some(entry) = self.ledger_rows().into_iter().nth(index) else {
            return false;
        };
        self.ledger_selected = index;
        let markdown = match self.ledger_state_dir() {
            Some(dir) => match self.ledger.read_document(&dir, &entry.id) {
                Ok(text) if !text.trim().is_empty() => text,
                _ => {
                    let doc = LedgerDocument::from_entry(&entry, &entry.title);
                    let _ = self.ledger.write_document(&dir, &entry.id, &doc);
                    doc.render_markdown()
                }
            },
            None => LedgerDocument::from_entry(&entry, &entry.title).render_markdown(),
        };
        self.ledger_viewer = Some(LedgerDocViewer {
            entry_id: entry.id.clone(),
            title: entry.title.clone(),
            summary: entry.display_summary().to_owned(),
            markdown,
            scroll: 0,
        });
        true
    }

    pub fn close_ledger_document(&mut self) {
        self.ledger_viewer = None;
    }

    pub fn scroll_ledger_document(&mut self, delta: i32) {
        let Some(viewer) = self.ledger_viewer.as_mut() else {
            return;
        };
        if delta >= 0 {
            viewer.scroll = viewer.scroll.saturating_add(delta as u16);
        } else {
            viewer.scroll = viewer.scroll.saturating_sub(delta.unsigned_abs() as u16);
        }
    }

    pub fn ledger_open(&mut self, index: usize, diff: bool) {
        let Some(entry) = self.ledger_rows().into_iter().nth(index) else {
            return;
        };
        self.ledger_selected = index;
        if diff {
            self.open_diff_viewer(Some(&entry.owner));
        } else {
            self.open_ledger_document(index);
        }
    }

    pub(crate) fn goal_pinned_on(&self, member: &str) -> bool {
        self.goal
            .as_ref()
            .is_some_and(|goal| goal.member.eq_ignore_ascii_case(member))
    }

    pub(crate) fn ledger_record(&mut self, kind: TaskKind, owner: &str, title: &str) {
        self.ledger_record_with(kind, owner, title, TaskStatus::Running);
    }

    pub(crate) fn ledger_record_with(
        &mut self,
        kind: TaskKind,
        owner: &str,
        title: &str,
        status: TaskStatus,
    ) {
        let stamp = now();
        let id = self.ledger.record(kind, owner, title, stamp);
        self.ledger
            .update_latest(owner, &TaskUpdate::status(status), stamp);
        self.write_ledger_doc_for(&id, title);
        self.save_ledger();
    }

    pub(crate) fn record_commander_mission(&mut self, display: &str) {
        if !self.goal_pinned_on(COMMANDER) {
            self.ledger_record(TaskKind::Mission, COMMANDER, display);
        }
    }

    pub(crate) fn ledger_finish_goal(&mut self, member: &str, status: TaskStatus) {
        if self.ledger.update_latest_of_kind(
            member,
            TaskKind::Goal,
            &TaskUpdate::status(status),
            now(),
        ) {
            self.save_ledger();
        }
    }

    pub(crate) fn sync_ledger(&mut self) {
        let stamp = now();
        let mut changed = false;
        for index in 0..self.crew.len() {
            changed |= self.sync_member_ledger(index, stamp);
        }
        if let Some(goal) = self.goal.clone() {
            let open = self
                .ledger
                .latest_of_kind(&goal.member, TaskKind::Goal)
                .is_some_and(|entry| !entry.status.is_finished());
            if open {
                changed |= self.ledger.update_latest_of_kind(
                    &goal.member,
                    TaskKind::Goal,
                    &TaskUpdate::status(TaskStatus::Running),
                    stamp,
                );
            }
        }
        if changed {
            self.save_ledger();
        }
    }

    fn sync_member_ledger(&mut self, index: usize, stamp: i64) -> bool {
        let member = &self.crew[index];
        let Some(status) = TaskStatus::from_member(member.status) else {
            return false;
        };
        if self.goal_pinned_on(&member.name) {
            return false;
        }
        let update = TaskUpdate {
            status,
            branch: member.branch().map(|branch| branch.name.clone()),
            changed_files: member.changes.as_ref().map(|changes| changes.files.len()),
        };
        let latest = self
            .ledger
            .entries()
            .iter()
            .rev()
            .find(|entry| {
                entry.kind != TaskKind::Goal && entry.owner.eq_ignore_ascii_case(&member.name)
            })
            .map(|entry| entry.status);
        match latest {
            Some(current) if current.is_finished() => false,
            Some(_) => self.ledger.update_latest_of_kind(
                &member.name,
                self.latest_non_goal_kind(&member.name),
                &update,
                stamp,
            ),
            None if member.is_commander() => false,
            None => {
                let name = member.name.clone();
                let title = member.mission.clone();
                let id = self.ledger.record(TaskKind::Spawn, &name, &title, stamp);
                self.ledger.update_latest(&name, &update, stamp);
                self.write_ledger_doc_for(&id, &title);
                true
            }
        }
    }

    fn latest_non_goal_kind(&self, owner: &str) -> TaskKind {
        self.ledger
            .entries()
            .iter()
            .rev()
            .find(|entry| entry.kind != TaskKind::Goal && entry.owner.eq_ignore_ascii_case(owner))
            .map_or(TaskKind::Spawn, |entry| entry.kind)
    }

    fn save_ledger(&self) {
        if let Some(path) = &self.ledger_path {
            let _ = self.ledger.save(path);
        }
    }

    pub fn usage_cards(&self) -> Vec<(usize, AccountCard)> {
        let Some(live) = &self.live else {
            return Vec::new();
        };
        live.profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                let allowance = live.allowances.get(index).copied().unwrap_or_default();
                let label = profile.to_string();
                let tier = if profile.is_system_default() {
                    self.auth_detections
                        .iter()
                        .find(|detection| detection.provider == profile.provider)
                        .and_then(|detection| detection.tier.as_deref())
                        .or_else(|| {
                            live.profile_detections
                                .get(&label)
                                .and_then(|detection| detection.tier.as_deref())
                        })
                } else {
                    live.profile_detections
                        .get(&label)
                        .and_then(|detection| detection.tier.as_deref())
                };
                let card = AccountCard {
                    provider: provider_short_name(profile.provider).to_owned(),
                    profile: profile.name().to_owned(),
                    tier: normalize_tier(tier),
                    primary_remaining: allowance.five,
                    secondary_remaining: allowance.week,
                    seconds_until_reset: allowance.reset_in,
                    route_model: None,
                    route_metric: None,
                };
                (index, card)
            })
            .collect()
    }

    pub fn headline_usage(&self) -> Option<f64> {
        let live = self.live.as_ref()?;
        let displayed = live.displayed_profile();
        self.usage_cards()
            .into_iter()
            .find(|(index, _)| *index == displayed)
            .and_then(|(_, card)| card.headline_remaining())
    }

    pub fn handle_inbox_modal_key(&mut self, key: KeyEvent) -> bool {
        let Some(modal) = self.inbox_modal.clone() else {
            return false;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if modal.explain_mode {
            match key.code {
                KeyCode::Esc => {
                    if let Some(active) = self.inbox_modal.as_mut() {
                        active.explain_mode = false;
                    }
                }
                KeyCode::Enter => {
                    let note = modal.explain_note;
                    let card_id = modal.card_id;
                    self.request_explain_more(&card_id, &note);
                }
                KeyCode::Backspace => {
                    if let Some(active) = self.inbox_modal.as_mut() {
                        active.explain_note.pop();
                    }
                }
                KeyCode::Char(ch) if !ctrl && !alt => {
                    if let Some(active) = self.inbox_modal.as_mut() {
                        active.explain_note.push(ch);
                    }
                }
                _ => {}
            }
            return true;
        }
        if ctrl {
            match key.code {
                KeyCode::Char('s') => {
                    self.save_inbox_draft();
                    return true;
                }
                KeyCode::Char('e') => {
                    if let Some(active) = self.inbox_modal.as_mut() {
                        active.explain_mode = true;
                        if active.explain_note.is_empty() {
                            active.explain_note = active.write_in.clone();
                        }
                    }
                    return true;
                }
                KeyCode::Char('x') => {
                    self.retire_inbox_card(&modal.card_id, "Retired from modal");
                    return true;
                }
                KeyCode::Char('r') => {
                    self.submit_inbox_modal(true);
                    return true;
                }
                KeyCode::Char('j') => {
                    let cards = self.filtered_decision_cards();
                    if let Some(pos) = cards.iter().position(|c| c.id == modal.card_id) {
                        if let Some(next) = cards.get(pos + 1).or_else(|| cards.first()) {
                            let next_id = next.id.clone();
                            self.open_inbox_modal(&next_id);
                        }
                    }
                    return true;
                }
                KeyCode::Enter => {
                    self.submit_inbox_modal(false);
                    return true;
                }
                _ => return true,
            }
        }
        match key.code {
            KeyCode::Esc => {
                if !modal.write_in.trim().is_empty() || modal.selected_option.is_some() {
                    let _ = self.save_inbox_draft();
                }
                self.inbox_modal = None;
            }
            KeyCode::Enter => {
                self.submit_inbox_modal(false);
            }
            KeyCode::Up => {
                if let Some(active) = self.inbox_modal.as_mut() {
                    active.scroll = active.scroll.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(active) = self.inbox_modal.as_mut() {
                    active.scroll = active.scroll.saturating_add(1);
                }
            }
            KeyCode::PageUp => {
                if let Some(active) = self.inbox_modal.as_mut() {
                    active.scroll = active.scroll.saturating_sub(6);
                }
            }
            KeyCode::PageDown => {
                if let Some(active) = self.inbox_modal.as_mut() {
                    active.scroll = active.scroll.saturating_add(6);
                }
            }
            KeyCode::Backspace => {
                if let Some(active) = self.inbox_modal.as_mut() {
                    active.write_in.pop();
                }
            }
            KeyCode::Char(ch @ '1'..='4') if modal.write_in.is_empty() || alt => {
                let idx = (ch as u8 - b'1') as usize;
                let max_opts = self
                    .decision_cards
                    .iter()
                    .find(|card| card.id == modal.card_id)
                    .map_or(0, |card| card.alternatives.len());
                if idx < max_opts {
                    if let Some(active) = self.inbox_modal.as_mut() {
                        active.selected_option = Some(idx);
                    }
                }
            }
            KeyCode::Char(ch) if !alt => {
                if let Some(active) = self.inbox_modal.as_mut() {
                    active.write_in.push(ch);
                }
            }
            _ => {}
        }
        true
    }

    pub fn handle_ledger_viewer_key(&mut self, key: KeyEvent) -> bool {
        let Some(viewer) = self.ledger_viewer.clone() else {
            return false;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
        {
            return false;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                self.close_ledger_document();
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll_ledger_document(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_ledger_document(1),
            KeyCode::PageUp => self.scroll_ledger_document(-10),
            KeyCode::PageDown => self.scroll_ledger_document(10),
            KeyCode::Home => {
                if let Some(active) = self.ledger_viewer.as_mut() {
                    active.scroll = 0;
                }
            }
            KeyCode::Char('g') => {
                if let Some(owner) = self
                    .ledger
                    .get(&viewer.entry_id)
                    .map(|entry| entry.owner.clone())
                {
                    self.close_ledger_document();
                    self.jump_to_thread(&owner);
                }
            }
            KeyCode::Char('d') => {
                if let Some(owner) = self
                    .ledger
                    .get(&viewer.entry_id)
                    .map(|entry| entry.owner.clone())
                {
                    self.open_diff_viewer(Some(&owner));
                }
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn handle_view_key(&mut self, key: KeyEvent) -> bool {
        if self.inbox_modal.is_some() && self.handle_inbox_modal_key(key) {
            return true;
        }
        if self.inbox_searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.inbox_searching = false;
                    self.clamp_tab_selection();
                }
                KeyCode::Backspace => {
                    self.inbox_search.pop();
                    self.clamp_tab_selection();
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.inbox_search.push(ch);
                    self.clamp_tab_selection();
                }
                _ => {}
            }
            return true;
        }
        if self.view_tab == ViewTab::Ledger
            && self.ledger_viewer.is_some()
            && self.handle_ledger_viewer_key(key)
        {
            return true;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if alt && !ctrl {
            let tab = match key.code {
                KeyCode::Char('1') => Some(ViewTab::Chat),
                KeyCode::Char('2') => Some(ViewTab::Inbox),
                KeyCode::Char('3') => Some(ViewTab::Ledger),
                _ => None,
            };
            if let Some(tab) = tab {
                self.set_view_tab(tab);
                return true;
            }
        }
        let toggle_left = key.code == KeyCode::F(8)
            || (ctrl && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4')));
        if toggle_left {
            self.toggle_left_panel();
            return true;
        }
        if key.code == KeyCode::BackTab && self.left_panel_visible() {
            self.left_focused = !self.left_focused;
            self.rail_focused = false;
            self.editing = !self.left_focused;
            return true;
        }
        if self.left_focused && self.handle_left_focus_key(key) {
            return true;
        }
        if self.view_tab == ViewTab::Chat
            || ctrl
            || alt
            || !self.composer.is_empty()
            || self.autocomplete.is_open()
        {
            return false;
        }
        match self.view_tab {
            ViewTab::Inbox => self.handle_inbox_key(key.code),
            ViewTab::Ledger => self.handle_ledger_key(key.code),
            ViewTab::Chat => false,
        }
    }

    fn handle_left_focus_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.set_view_tab(self.view_tab.previous()),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.set_view_tab(self.view_tab.next())
            }
            KeyCode::Enter | KeyCode::Esc => {
                self.left_focused = false;
                self.editing = true;
            }
            _ => {
                self.left_focused = false;
                self.editing = true;
                return false;
            }
        }
        true
    }

    fn handle_inbox_key(&mut self, code: KeyCode) -> bool {
        let len = self.visible_inbox_total();
        let index = self.inbox_selected;
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.inbox_selected = index.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.inbox_selected = (index + 1).min(len.saturating_sub(1))
            }
            KeyCode::Home => self.inbox_selected = 0,
            KeyCode::Char('f') => self.set_inbox_filter(self.inbox_filter.next()),
            KeyCode::Char('/') => {
                self.inbox_searching = true;
                self.editing = false;
            }
            KeyCode::Enter => self.inbox_act(index, InboxAction::Open),
            KeyCode::Char('a') => self.inbox_act(index, InboxAction::Approve),
            KeyCode::Char('n') => self.inbox_act(index, InboxAction::Deny),
            KeyCode::Char('d') => self.inbox_act(index, InboxAction::Diff),
            KeyCode::Char('p') => self.inbox_act(index, InboxAction::Pr),
            KeyCode::Char('x') => self.inbox_act(index, InboxAction::Dismiss),
            KeyCode::Esc => {
                if !self.inbox_search.is_empty() {
                    self.inbox_search.clear();
                    self.clamp_tab_selection();
                } else {
                    self.set_view_tab(ViewTab::Chat);
                }
            }
            _ => return false,
        }
        true
    }

    fn handle_ledger_key(&mut self, code: KeyCode) -> bool {
        let len = self.ledger_rows().len();
        let index = self.ledger_selected;
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.ledger_selected = index.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.ledger_selected = (index + 1).min(len.saturating_sub(1))
            }
            KeyCode::Home => self.ledger_selected = 0,
            KeyCode::Char('f') => self.set_ledger_filter(self.ledger_filter.next()),
            KeyCode::Enter => self.ledger_open(index, false),
            KeyCode::Char('g') => {
                if let Some(entry) = self.ledger_rows().into_iter().nth(index) {
                    self.jump_to_thread(&entry.owner);
                }
            }
            KeyCode::Char('d') => self.ledger_open(index, true),
            KeyCode::Esc => self.set_view_tab(ViewTab::Chat),
            _ => return false,
        }
        true
    }

    pub(crate) fn handle_tab_hit(&mut self, hit: TabHit) {
        match hit {
            TabHit::SelectTab(tab) => self.set_view_tab(tab),
            TabHit::ToggleLeftPanel => self.toggle_left_panel(),
            TabHit::UsageTile(index) => self.open_subscription(index, false),
            TabHit::JumpThread(index) => {
                if let Some(name) = self.crew.get(index).map(|member| member.name.clone()) {
                    self.jump_to_thread(&name);
                }
            }
            TabHit::InboxRow(index) => self.inbox_selected = index,
            TabHit::InboxAction(index, action) => self.inbox_act(index, action),
            TabHit::DecisionFilter(filter) => self.set_inbox_filter(filter),
            TabHit::InboxSearch => {
                self.inbox_searching = true;
                self.editing = false;
            }
            TabHit::DecisionRow(index) => {
                let live_count = self.filtered_live_inbox_items().len();
                self.inbox_selected = live_count + index;
                if let Some(card) = self.filtered_decision_cards().get(index).cloned() {
                    self.open_inbox_modal(&card.id);
                }
            }
            TabHit::DecisionModalOption(opt_idx) => {
                if let Some(modal) = self.inbox_modal.as_mut() {
                    modal.selected_option = Some(opt_idx);
                }
            }
            TabHit::DecisionModalSubmit => {
                self.submit_inbox_modal(false);
            }
            TabHit::DecisionModalSaveDraft => {
                self.save_inbox_draft();
            }
            TabHit::DecisionModalExplain => {
                if let Some(modal) = self.inbox_modal.as_mut() {
                    modal.explain_mode = true;
                    if modal.explain_note.is_empty() {
                        modal.explain_note = modal.write_in.clone();
                    }
                }
            }
            TabHit::DecisionModalRetire => {
                if let Some(card_id) = self.inbox_modal.as_ref().map(|m| m.card_id.clone()) {
                    self.retire_inbox_card(&card_id, "Retired from modal");
                }
            }
            TabHit::DecisionModalReconcile => {
                self.submit_inbox_modal(true);
            }
            TabHit::DecisionModalClose => {
                if let Some(modal) = self.inbox_modal.as_ref() {
                    if !modal.write_in.trim().is_empty() || modal.selected_option.is_some() {
                        let _ = self.save_inbox_draft();
                    }
                }
                self.inbox_modal = None;
            }
            TabHit::LedgerRow(index) => {
                if self.ledger_selected == index {
                    self.open_ledger_document(index);
                } else {
                    self.ledger_selected = index;
                    if self.ledger_viewer.is_some() {
                        self.open_ledger_document(index);
                    }
                }
            }
            TabHit::LedgerFilter(filter) => self.set_ledger_filter(filter),
            TabHit::LedgerDocJump => {
                if let Some(entry_id) = self.ledger_viewer.as_ref().map(|v| v.entry_id.clone()) {
                    if let Some(owner) = self.ledger.get(&entry_id).map(|e| e.owner.clone()) {
                        self.close_ledger_document();
                        self.jump_to_thread(&owner);
                    }
                }
            }
            TabHit::LedgerDocDiff => {
                if let Some(entry_id) = self.ledger_viewer.as_ref().map(|v| v.entry_id.clone()) {
                    if let Some(owner) = self.ledger.get(&entry_id).map(|e| e.owner.clone()) {
                        self.open_diff_viewer(Some(&owner));
                    }
                }
            }
            TabHit::LedgerDocClose => self.close_ledger_document(),
        }
    }

    pub fn ledger_badge(&self) -> String {
        format!("{}/{}", self.ledger.active_count(), self.ledger.total())
    }
}

pub fn ledger_filter_count(entries: &[LedgerEntry], filter: LedgerFilter) -> usize {
    entries
        .iter()
        .filter(|entry| filter.matches(entry.status))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth,
        inbox::{DecisionInput, DecisionKind, DecisionOption, InboxStore},
        usage::Allowance,
        Live,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, Terminal};
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agent-command-tabs-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn render(app: &mut App, w: u16, h: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(buffer: &Buffer, area: Rect) -> String {
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn find(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
        let width = needle.chars().count();
        (0..buffer.area.height).find_map(|y| {
            let symbols: Vec<&str> = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            (0..symbols.len().saturating_sub(width - 1))
                .find_map(|x| (symbols[x..x + width].concat() == needle).then_some((x as u16, y)))
        })
    }

    fn find_left_of(buffer: &Buffer, needle: &str, max_x: u16) -> Option<(u16, u16)> {
        let width = needle.chars().count() as u16;
        (0..buffer.area.height).find_map(|y| {
            (0..max_x.saturating_sub(width)).find_map(|x| {
                let run: String = (x..x + width).map(|cx| buffer[(cx, y)].symbol()).collect();
                (run == needle).then_some((x, y))
            })
        })
    }

    fn live_app(root: &std::path::Path) -> App {
        let profiles = vec![
            auth::Profile::system_default(root.join("state"), auth::Provider::Claude),
            auth::Profile::new(root.join("state"), auth::Provider::Codex, "backup").unwrap(),
        ];
        let mut live = Live::new(root.join("work"), profiles, None);
        live.allowances = vec![
            Allowance {
                five: Some(64.0),
                week: Some(12.0),
                reset_in: Some(2 * 3600 + 26 * 60),
                latched: false,
                stale: false,
            },
            Allowance::default(),
        ];
        live.signed_in = vec![true, true];
        let mut app = App {
            live: Some(live),
            ..App::default()
        };
        app.auth_detections = vec![auth::AuthDetection {
            provider: auth::Provider::Claude,
            logged_in: true,
            auth_method: Some("claude.ai".into()),
            account: None,
            tier: Some("max".into()),
            api_provider: None,
            config_dir: None,
            warning: None,
        }];
        app
    }

    fn sample_decision(id: &str, question: &str) -> DecisionCard {
        let mut store = InboxStore::default();
        store
            .upsert_card(
                DecisionInput {
                    id: id.into(),
                    title: format!("Decision {id}"),
                    question: question.into(),
                    current_state: "Ready for review.".into(),
                    why_it_matters: "Unblocks the next phase.".into(),
                    recommendation: "approve — lowest risk".into(),
                    source_row: "M-001".into(),
                    owner_agent_id: "builder".into(),
                    owner_name: "Builder".into(),
                    project: "Agent-Command".into(),
                    group: "v0.3".into(),
                    readiness: DecisionReadiness::Ready,
                    alternatives: vec![
                        DecisionOption {
                            id: "approve".into(),
                            label: "Approve rollout".into(),
                        },
                        DecisionOption {
                            id: "defer".into(),
                            label: "Defer to later".into(),
                        },
                    ],
                    kind: DecisionKind::Standard,
                    constraints: Some("No breaking changes.".into()),
                    example: None,
                    detail: None,
                    preparation: None,
                    canonical_sources: vec!["ledger-docs/active/M-001.md".into()],
                    is_current_blocker: true,
                    self_serve: None,
                    stuck: None,
                },
                1_700_000_000,
            )
            .unwrap()
            .clone()
    }

    #[test]
    fn left_panel_renders_concentric_and_unknown_rings_with_token_burn_colors() {
        let root = scratch("rings");
        let mut app = live_app(&root);
        let buffer = render(&mut app, 140, 40);
        let panel = text(&buffer, Rect::new(0, 0, 30, 40));
        assert!(panel.contains("Claude · default"), "{panel}");
        assert!(
            panel.contains("12% left") && panel.contains(" MAX "),
            "{panel}"
        );
        assert!(
            panel.contains("7d 12%") && panel.contains("↻ 2h 26m"),
            "{panel}"
        );
        assert!(
            panel.contains("Codex · backup") && panel.contains("-- left"),
            "{panel}"
        );
        let ring_cells = (1..40u16)
            .flat_map(|y| (0..30u16).map(move |x| (x, y)))
            .filter(|&(x, y)| crate::token_burn::is_ring_cell_symbol(buffer[(x, y)].symbol()))
            .count();
        assert!(ring_cells >= 20, "{ring_cells}\n{panel}");
        let deck = app.deck_palette;
        let (x, y) = find(&buffer, "12% left").unwrap();
        assert_eq!(buffer[(x, y)].fg, deck.depleted);
        let (x, y) = find_left_of(&buffer, "7d 12%", 30).unwrap();
        assert_eq!(buffer[(x, y)].fg, deck.depleted);
        let (x, y) = find(&buffer, "-- left").unwrap();
        assert_eq!(buffer[(x, y)].fg, deck.unknown);
        let (x, y) = find(&buffer, "↻ 2h 26m").unwrap();
        assert_eq!(buffer[(x, y)].fg, deck.reset_six_hours);
        let codex_row = find(&buffer, "Codex · backup").unwrap().1;
        assert!(text(&buffer, Rect::new(0, codex_row + 2, 10, 1)).contains("--"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compact_ring_glyph_appears_in_top_bar_when_panel_is_collapsed() {
        let root = scratch("compact");
        let mut app = live_app(&root);
        let buffer = render(&mut app, 100, 30);
        let top = text(&buffer, Rect::new(0, 0, 100, 1));
        assert!(top.contains("○"), "{top}");
        let (x, y) = find(&buffer, "○").unwrap();
        assert_eq!(buffer[(x, y)].fg, app.deck_palette.depleted);

        app.live.as_mut().unwrap().allowances[0].week = Some(80.0);
        let buffer = render(&mut app, 100, 30);
        let top = text(&buffer, Rect::new(0, 0, 100, 1));
        assert!(top.contains("◑") || top.contains("◕"), "{top}");
        let (x, y) = find(&buffer, "◕").or_else(|| find(&buffer, "◑")).unwrap();
        assert_eq!(buffer[(x, y)].fg, app.deck_palette.healthy);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn isolated_profiles_show_plan_tier_badges_and_gemini_and_stale_render_cleanly() {
        let root = scratch("isolated-tiers");
        let state_root = root.join("state");
        let claude_main =
            auth::Profile::new(state_root.clone(), auth::Provider::Claude, "main").unwrap();
        let codex_main =
            auth::Profile::new(state_root.clone(), auth::Provider::Codex, "main").unwrap();
        let gemini_default =
            auth::Profile::system_default(state_root.clone(), auth::Provider::Gemini);

        let claude_dir = state_root.join("claude").join("main");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"tok","subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}}"#,
        )
        .unwrap();

        let codex_dir = state_root.join("codex").join("main");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let payload = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "promax",
                "chatgpt_account_id": "acct-1"
            }
        });
        let encoded_payload = base64_url_no_pad(&payload.to_string());
        let fake_jwt = format!("eyJhbGciOiJub25lIn0.{encoded_payload}.sig");
        std::fs::write(
            codex_dir.join("auth.json"),
            serde_json::json!({
                "tokens": {
                    "access_token": "tok",
                    "id_token": fake_jwt
                }
            })
            .to_string(),
        )
        .unwrap();

        let mut live = Live::new(root.join("work"), Vec::new(), None);
        live.reset_profiles(vec![claude_main, codex_main, gemini_default]);
        live.allowances = vec![
            Allowance {
                five: None,
                week: Some(23.9),
                reset_in: Some(3600),
                latched: false,
                stale: true,
            },
            Allowance {
                five: Some(0.0),
                week: Some(0.0),
                reset_in: Some(15 * 3600),
                latched: true,
                stale: false,
            },
            Allowance::default(),
        ];
        live.signed_in = vec![true, true, true];
        let mut app = App {
            live: Some(live),
            ..App::default()
        };
        app.auth_detections = vec![auth::AuthDetection {
            provider: auth::Provider::Gemini,
            logged_in: true,
            auth_method: Some("oauth-personal".into()),
            account: None,
            tier: Some("google".into()),
            api_provider: None,
            config_dir: None,
            warning: None,
        }];

        let cards = app.usage_cards();
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].1.tier, "max");
        assert_eq!(cards[1].1.tier, "max");
        assert_eq!(cards[2].1.tier, "google");

        let buffer = render(&mut app, 140, 40);
        let panel = text(&buffer, Rect::new(0, 0, 30, 40));
        assert!(panel.contains("Claude · main"), "{panel}");
        assert!(panel.contains("Codex · main"), "{panel}");
        assert!(panel.contains("Gemini · default"), "{panel}");
        assert_eq!(panel.matches(" MAX ").count(), 2, "{panel}");
        assert!(panel.contains(" GOOGLE "), "{panel}");
        assert!(panel.contains("7d · stale"), "{panel}");
        assert!(panel.contains("no CLI quota"), "{panel}");

        let _ = render(&mut app, 80, 24);
        let _ = render(&mut app, 120, 40);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reset_profiles_reconciles_peer_accounts_and_latched_gemini_labels_cleanly() {
        let base = std::env::temp_dir().join(format!(
            "agent-command-tabs-reconcile-{}",
            uuid::Uuid::new_v4()
        ));
        let state_root = base.join("profiles");
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();

        let claude_main =
            auth::Profile::new(state_root.clone(), auth::Provider::Claude, "main").unwrap();
        let claude_peer =
            auth::Profile::new(state_root.clone(), auth::Provider::Claude, "peer").unwrap();
        let gemini_main =
            auth::Profile::new(state_root.clone(), auth::Provider::Gemini, "main").unwrap();
        let gemini_unknown =
            auth::Profile::new(state_root, auth::Provider::Gemini, "unknown").unwrap();
        claude_main.prepare().unwrap();
        claude_peer.prepare().unwrap();
        gemini_main.prepare().unwrap();
        gemini_unknown.prepare().unwrap();

        let shared_claude = serde_json::json!({
            "oauthAccount": {
                "accountUuid": "acct-shared",
                "organizationUuid": "org-shared",
                "organizationRateLimitTier": "default_claude_max_20x"
            }
        })
        .to_string();
        for name in ["main", "peer"] {
            let dir = base.join("profiles").join("claude").join(name);
            std::fs::write(dir.join(".claude.json"), &shared_claude).unwrap();
        }

        let now = crate::usage::now();
        let stale_tick = crate::usage::tick(now.wall - 16 * 3600, 0);
        crate::usage::Usage::observe_claude(
            &claude_peer,
            &serde_json::json!({
                "type": "rate_limit_event",
                "rate_limit_info": {
                    "status": "allowed",
                    "rateLimitType": "seven_day",
                    "utilization": 0.77,
                    "resetsAt": now.wall + 24 * 3600
                }
            }),
            stale_tick,
        )
        .unwrap();
        crate::usage::Usage::observe_claude(
            &claude_main,
            &serde_json::json!({
                "type": "rate_limit_event",
                "rate_limit_info": {
                    "status": "rejected",
                    "rateLimitType": "seven_day",
                    "resetsAt": now.wall + 8 * 3600
                }
            }),
            now,
        )
        .unwrap();
        crate::usage::Usage::record_gemini_limit(&gemini_main, Some(now.wall + 3600), now).unwrap();
        crate::usage::Usage::record_gemini_limit(&gemini_unknown, None, now).unwrap();

        let mut live = Live::new(work, Vec::new(), None);
        live.reset_profiles(vec![claude_main, claude_peer, gemini_main, gemini_unknown]);
        live.signed_in = vec![true, true, true, true];
        assert!(live.allowances[0].latched);
        assert!(live.allowances[1].latched);
        assert_eq!(live.allowances[1].week, Some(0.0));

        let mut app = App {
            live: Some(live),
            ..App::default()
        };
        app.live.as_mut().unwrap().preferred = Some(2);
        assert_eq!(app.top_bar_quota_badge().as_deref(), Some("latched"));
        app.live.as_mut().unwrap().preferred = Some(3);
        assert_eq!(app.top_bar_quota_badge().as_deref(), Some("reset unknown"));

        let buffer = render(&mut app, 140, 40);
        let panel = text(&buffer, Rect::new(0, 0, 30, 40));
        assert!(panel.contains("Gemini · main"), "{panel}");
        assert!(panel.contains("latched"), "{panel}");
        assert!(panel.contains("reset unknown"), "{panel}");
        assert!(!panel.contains("5h window"), "{panel}");

        let live = app.live.as_mut().unwrap();
        live.allowances[0] = Allowance {
            five: Some(0.0),
            week: Some(40.0),
            reset_in: None,
            latched: true,
            stale: false,
        };
        let buffer = render(&mut app, 140, 40);
        let panel = text(&buffer, Rect::new(0, 0, 30, 40));
        assert!(panel.contains("↻ - · reset"), "{panel}");

        std::fs::remove_dir_all(base).unwrap();
    }

    fn base64_url_no_pad(input: &str) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let bytes = input.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = chunk.get(1).copied().map(u32::from).unwrap_or(0);
            let b2 = chunk.get(2).copied().map(u32::from).unwrap_or(0);
            let triple = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(TABLE[(triple & 0x3f) as usize] as char);
            }
        }
        out
    }

    #[test]
    fn clicking_a_usage_tile_opens_settings_on_that_subscription() {
        let root = scratch("tile");
        let mut app = live_app(&root);
        let buffer = render(&mut app, 140, 40);
        let (x, y) = find(&buffer, "Codex · backup").unwrap();
        app.click(x, y + 1);
        assert!(app.panel.is_some());
        let settings = render(&mut app, 140, 40);
        let screen = text(&settings, settings.area);
        assert!(screen.contains("codex/backup: Reconnect"), "{screen}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inbox_puts_prod_gates_and_questions_first_and_resolves_them() {
        let root = scratch("gates");
        let mut app = live_app(&root);
        let broker = bridge::AskUserBroker::new(false);
        let _ = broker.ask(
            bridge::AskUserRequest {
                request_id: "ask-1".into(),
                question: "Ship it?".into(),
                choices: vec!["yes".into(), "no".into()],
                context: None,
                allow_freeform: false,
            },
            std::time::Duration::ZERO,
        );
        let live = app.live.as_mut().unwrap();
        live.ask_user = Some(broker.clone());
        live.pending_prod_gates
            .insert("req-a".into(), COMMANDER.into());
        live.pending_prod_gates
            .insert("req-b".into(), COMMANDER.into());
        app.crew
            .push(specialists::Member::new("nova", "reviewer", None, None));
        app.crew[1].status = Status::Done;
        let items = app.inbox_items();
        assert!(matches!(items[0].kind, InboxKind::ProdGate { .. }));
        assert!(matches!(items[1].kind, InboxKind::ProdGate { .. }));
        assert!(matches!(items[2].kind, InboxKind::AskUser { .. }));
        assert_eq!(items[3].kind, InboxKind::Member(Status::Done));
        assert_eq!(app.inbox_count(), 4);

        app.set_view_tab(ViewTab::Inbox);
        let buffer = render(&mut app, 140, 40);
        let screen = text(&buffer, buffer.area);
        assert!(screen.contains("ProdGate approval · Commander"), "{screen}");
        assert!(
            screen.contains(" a Approve ") && screen.contains(" n Deny "),
            "{screen}"
        );
        assert!(screen.contains("Question · Ship it?"), "{screen}");

        app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.notice, "Allowed ProdGate action.");
        let buffer = render(&mut app, 140, 40);
        let (x, y) = find(&buffer, " n Deny ").unwrap();
        app.click(x + 1, y);
        assert_eq!(app.notice, "Denied ProdGate action.");
        assert!(app.live.as_ref().unwrap().pending_prod_gates.is_empty());

        assert!(matches!(
            app.inbox_items()[0].kind,
            InboxKind::AskUser { .. }
        ));
        app.inbox_selected = 0;
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(broker.pending().is_empty());
        assert_eq!(app.inbox_count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn decision_inbox_modal_preserves_no_auto_open_and_renders_at_80x24_and_120x40() {
        let mut app = App {
            decision_cards: vec![
                sample_decision("AC-DEC-1", "Enable local compaction?"),
                sample_decision("AC-DEC-2", "Keep archive weekly?"),
            ],
            ..App::default()
        };
        app.set_view_tab(ViewTab::Inbox);
        assert_eq!(app.inbox_count(), 2);
        let feed = render(&mut app, 120, 40);
        let feed_text = text(&feed, feed.area);
        assert!(feed_text.contains("AC-DEC-1"), "{feed_text}");
        assert!(feed_text.contains("AC-DEC-2"), "{feed_text}");

        assert!(app.open_inbox_modal("AC-DEC-1"));
        let modal_wide = render(&mut app, 120, 40);
        let wide_text = text(&modal_wide, modal_wide.area);
        assert!(wide_text.contains("Where things stand"), "{wide_text}");
        assert!(wide_text.contains("Owner recommendation"), "{wide_text}");
        let modal_narrow = render(&mut app, 80, 24);
        assert!(text(&modal_narrow, modal_narrow.area).contains("AC-DEC-1"));

        app.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert!(app.save_inbox_draft());
        assert_eq!(app.decision_filter_count(DecisionFilter::Drafts), 1);

        assert!(app.submit_inbox_modal(false));
        assert!(app.inbox_modal.is_none());
        assert_eq!(app.filtered_decision_cards().len(), 1);
        assert_eq!(app.filtered_decision_cards()[0].id, "AC-DEC-2");
        assert_eq!(app.past_decision_cards().len(), 1);

        assert!(app.request_explain_more("AC-DEC-2", "Clarify weekly trigger"));
        assert!(app.filtered_decision_cards().is_empty());
    }

    #[test]
    fn ledger_document_viewer_renders_markdown_in_split_and_narrow_layouts() {
        let root = scratch("ledger-doc-viewer");
        let workspace = root.join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut app = App::from_live(Live::new(workspace, Vec::new(), Some(root.join("state"))));
        app.ledger_record(
            TaskKind::Mission,
            COMMANDER,
            "Ship v0.3 platform. Verify all tests pass.",
        );
        app.set_view_tab(ViewTab::Ledger);
        let list_buf = render(&mut app, 120, 40);
        let list_text = text(&list_buf, list_buf.area);
        assert!(list_text.contains("M-001"), "{list_text}");
        assert!(list_text.contains("Ship v0.3 platform."), "{list_text}");

        assert!(app.open_ledger_document(0));
        assert!(app.ledger_viewer.is_some());
        let split_buf = render(&mut app, 120, 40);
        let split_text = text(&split_buf, split_buf.area);
        assert!(split_text.contains("Task Brief"), "{split_text}");
        assert!(split_text.contains("Jump"), "{split_text}");

        let narrow_buf = render(&mut app, 80, 24);
        let narrow_text = text(&narrow_buf, narrow_buf.area);
        assert!(narrow_text.contains("M-001"), "{narrow_text}");

        app.scroll_ledger_document(3);
        assert_eq!(app.ledger_viewer.as_ref().unwrap().scroll, 3);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.ledger_viewer.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_wakes_land_in_inbox_and_ledger_and_open_their_thread() {
        let mut app = App::default();
        app.crew
            .push(specialists::Member::new("w1", "builder", None, None));
        app.apply_background_event(crate::tui::events::AppEvent::WorkerWake(
            bridge::BackgroundResult {
                worker_id: "w1".into(),
                role: "builder".into(),
                status: "completed".into(),
                branch: Some("agent/w1".into()),
                workspace: "/tmp/w1".into(),
                summary: "Built the thing".into(),
                is_error: false,
                finished_at: "now".into(),
            },
        ));
        let items = app.inbox_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Worker w1 finished");
        let entry = app.ledger.latest_for("w1").unwrap();
        assert_eq!(entry.status, TaskStatus::Done);
        assert_eq!(entry.branch.as_deref(), Some("agent/w1"));
        app.set_view_tab(ViewTab::Inbox);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.view_tab, ViewTab::Chat);
        assert_eq!(app.crew[app.selected].name, "w1");
        assert_eq!(app.inbox_count(), 0);
    }

    #[test]
    fn ledger_persists_per_workspace_and_reloads_after_restart() {
        let root = scratch("persist");
        let workspace = root.join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut app = App::from_live(Live::new(
            workspace.clone(),
            Vec::new(),
            Some(root.join("state")),
        ));
        let path = app.ledger_path.clone().expect("ledger path");
        app.ledger_record(TaskKind::Mission, COMMANDER, "Ship release notes");
        app.ledger_record(TaskKind::Review, "nova", "Review round 1/3");
        assert!(path.exists());
        let reopened = App::from_live(Live::new(workspace, Vec::new(), Some(root.join("state"))));
        let titles: Vec<&str> = reopened
            .ledger_entries()
            .iter()
            .map(|entry| entry.title.as_str())
            .collect();
        assert_eq!(titles, ["Ship release notes", "Review round 1/3"]);
        std::fs::write(&path, b"{ broken").unwrap();
        let tolerant = App::from_live(Live::new(
            root.join("work"),
            Vec::new(),
            Some(root.join("state")),
        ));
        assert!(tolerant.ledger_entries().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn goals_are_tracked_and_closed_in_the_ledger() {
        let mut app = App {
            input: "/goal ship the release".into(),
            ..App::default()
        };
        app.submit();
        let goal = app
            .ledger
            .latest_of_kind(COMMANDER, TaskKind::Goal)
            .unwrap();
        assert_eq!(goal.title, "ship the release");
        assert_eq!(goal.status, TaskStatus::Error);
        assert!(app.goal.is_none());
        assert_eq!(app.ledger.total(), 1);
    }

    #[test]
    fn gemini_live_quota_and_untouched_full_buckets_render_in_top_bar_and_left_panel() {
        let root = scratch("gemini-live-quota");
        let state_root = root.join("state");
        let gemini_default =
            auth::Profile::system_default(state_root.clone(), auth::Provider::Gemini);
        let gemini_full = auth::Profile::new(state_root, auth::Provider::Gemini, "fresh").unwrap();

        let mut live = Live::new(root.join("work"), vec![gemini_default, gemini_full], None);
        live.allowances = vec![
            Allowance {
                five: Some(81.87),
                week: Some(86.99),
                reset_in: Some(75 * 60),
                latched: false,
                stale: false,
            },
            Allowance {
                five: Some(100.0),
                week: Some(100.0),
                reset_in: None,
                latched: false,
                stale: false,
            },
        ];
        live.signed_in = vec![true, true];
        live.preferred = Some(0);
        let mut app = App {
            live: Some(live),
            ..App::default()
        };
        app.auth_detections = vec![auth::AuthDetection {
            provider: auth::Provider::Gemini,
            logged_in: true,
            auth_method: Some("oauth-personal".into()),
            account: None,
            tier: Some("google".into()),
            api_provider: None,
            config_dir: None,
            warning: None,
        }];

        let cards = app.usage_cards();
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].1.primary_remaining, Some(81.87));
        assert_eq!(cards[0].1.secondary_remaining, Some(86.99));
        assert_eq!(cards[0].1.seconds_until_reset, Some(75 * 60));
        assert_eq!(cards[1].1.primary_remaining, Some(100.0));
        assert_eq!(cards[1].1.secondary_remaining, Some(100.0));
        assert_eq!(cards[1].1.seconds_until_reset, None);

        assert_eq!(app.top_bar_quota_badge().as_deref(), Some("5h 81% 7d 86%"));
        app.live.as_mut().unwrap().preferred = Some(1);
        assert_eq!(
            app.top_bar_quota_badge().as_deref(),
            Some("5h 100% 7d 100%")
        );
        app.live.as_mut().unwrap().preferred = Some(0);

        let buffer = render(&mut app, 140, 40);
        let panel = text(&buffer, Rect::new(0, 0, 30, 40));
        assert!(panel.contains("Gemini · default"), "{panel}");
        assert!(panel.contains("81% left"), "{panel}");
        assert!(panel.contains("7d 86%"), "{panel}");
        assert!(panel.contains("↻ 1h 15m"), "{panel}");
        assert!(panel.contains("Gemini · fresh"), "{panel}");
        assert!(panel.contains("100% left"), "{panel}");
        assert!(panel.contains("7d 100%"), "{panel}");
        assert!(panel.contains("↻ -"), "{panel}");
        assert!(!panel.contains("no CLI quota"), "{panel}");

        let top_compact = render(&mut app, 100, 30);
        let top_line = text(&top_compact, Rect::new(0, 0, 100, 1));
        assert!(top_line.contains("5h 81% 7d 86%"), "{top_line}");
        let _ = render(&mut app, 120, 40);
        let _ = render(&mut app, 80, 24);

        app.live.as_mut().unwrap().allowances[0].stale = true;
        assert_eq!(
            app.top_bar_quota_badge().as_deref(),
            Some("5h 81% 7d 86% stale")
        );
        let stale_buf = render(&mut app, 140, 40);
        let stale_panel = text(&stale_buf, Rect::new(0, 0, 30, 40));
        assert!(stale_panel.contains("stale"), "{stale_panel}");

        app.live.as_mut().unwrap().allowances[0] = Allowance {
            five: Some(0.0),
            week: Some(86.99),
            reset_in: Some(3600),
            latched: true,
            stale: false,
        };
        assert_eq!(app.top_bar_quota_badge().as_deref(), Some("latched"));
        let latched_buf = render(&mut app, 140, 40);
        let latched_panel = text(&latched_buf, Rect::new(0, 0, 30, 40));
        assert!(latched_panel.contains("0% left"), "{latched_panel}");
        assert!(latched_panel.contains("latched"), "{latched_panel}");

        std::fs::remove_dir_all(root).unwrap();
    }
}
