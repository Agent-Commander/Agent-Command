use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::{
    bridge::BackgroundResult,
    ledger::{Ledger, LedgerEntry, LedgerFilter, Observation, TaskKind, TaskStatus},
    markdown::TranscriptHitAction,
    specialists::{self, Member, Status},
    token_burn::{self, AccountCard},
    tui::provider_short_name,
    App,
};

pub const LEFT_PANEL_WIDTH: u16 = 30;
pub const LEFT_PANEL_TIGHT_WIDTH: u16 = 26;
pub const RAIL_WIDTH: u16 = 34;
pub const RAIL_TIGHT_WIDTH: u16 = 28;
pub const RAIL_MIN_TERMINAL: u16 = 100;
pub const LEFT_PANEL_MIN_TERMINAL: u16 = 100;
pub const LEFT_PANEL_AUTO_WIDTH: u16 = 120;
pub const CENTER_MIN_WIDTH: u16 = 48;
pub const MAX_WAKES: usize = 20;
pub const COMMANDER_OWNER: &str = "Commander";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ActiveTab {
    #[default]
    Chat,
    Inbox,
    Ledger,
}

impl ActiveTab {
    pub const ALL: [Self; 3] = [Self::Chat, Self::Inbox, Self::Ledger];

    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Inbox => "Inbox",
            Self::Ledger => "Ledger",
        }
    }

    pub fn step(self, forward: bool) -> Self {
        let index = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        let len = Self::ALL.len();
        let next = if forward {
            (index + 1) % len
        } else {
            (index + len - 1) % len
        };
        Self::ALL[next]
    }

    fn from_digit(digit: char) -> Option<Self> {
        match digit {
            '1' => Some(Self::Chat),
            '2' => Some(Self::Inbox),
            '3' => Some(Self::Ledger),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboxAction {
    Approve,
    Deny,
    Jump,
    Resume,
    Abandon,
    Diff,
    Pr,
    Dismiss,
}

impl InboxAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approve => "Approve",
            Self::Deny => "Deny",
            Self::Jump => "Jump",
            Self::Resume => "Resume",
            Self::Abandon => "Abandon",
            Self::Diff => "d Diff",
            Self::Pr => "p PR",
            Self::Dismiss => "x Dismiss",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InboxKind {
    Approval { req_id: String },
    Continuation,
    Specialist(Status),
    Wake { is_error: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboxItem {
    pub key: String,
    pub kind: InboxKind,
    pub thread: String,
    pub title: String,
    pub detail: String,
}

impl InboxItem {
    pub fn priority(&self) -> u8 {
        match &self.kind {
            InboxKind::Approval { .. } => 0,
            InboxKind::Continuation => 1,
            InboxKind::Specialist(Status::NeedsApproval) => 2,
            InboxKind::Specialist(Status::Error) => 3,
            InboxKind::Specialist(Status::Done) => 4,
            InboxKind::Specialist(_) => 5,
            InboxKind::Wake { .. } => 6,
        }
    }

    pub fn actions(&self) -> &'static [InboxAction] {
        match &self.kind {
            InboxKind::Approval { .. } => {
                &[InboxAction::Approve, InboxAction::Deny, InboxAction::Jump]
            }
            InboxKind::Continuation => &[InboxAction::Resume, InboxAction::Abandon],
            InboxKind::Specialist(_) => &[
                InboxAction::Jump,
                InboxAction::Diff,
                InboxAction::Pr,
                InboxAction::Dismiss,
            ],
            InboxKind::Wake { .. } => &[InboxAction::Jump, InboxAction::Dismiss],
        }
    }

    pub fn dismissable(&self) -> bool {
        matches!(self.kind, InboxKind::Specialist(_) | InboxKind::Wake { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabHit {
    Tab(ActiveTab),
    ToggleLeftPanel,
    UsageTile(usize),
    Thread(usize),
    InboxRow(usize),
    InboxAction(usize, InboxAction),
    LedgerRow(usize),
    LedgerFilter(LedgerFilter),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ColumnPlan {
    pub left: u16,
    pub rail: u16,
}

pub fn plan_columns(
    width: u16,
    left_wanted: bool,
    rail_wanted: bool,
    prefer_rail: bool,
) -> ColumnPlan {
    let left_ok = left_wanted && width >= LEFT_PANEL_MIN_TERMINAL;
    let rail_ok = rail_wanted && width >= RAIL_MIN_TERMINAL;
    let fits = |left: u16, rail: u16| width.saturating_sub(left + rail) >= CENTER_MIN_WIDTH;
    match (left_ok, rail_ok) {
        (false, false) => ColumnPlan::default(),
        (true, false) => ColumnPlan {
            left: LEFT_PANEL_WIDTH,
            rail: 0,
        },
        (false, true) => ColumnPlan {
            left: 0,
            rail: RAIL_WIDTH,
        },
        (true, true) if fits(LEFT_PANEL_WIDTH, RAIL_WIDTH) => ColumnPlan {
            left: LEFT_PANEL_WIDTH,
            rail: RAIL_WIDTH,
        },
        (true, true) if fits(LEFT_PANEL_TIGHT_WIDTH, RAIL_TIGHT_WIDTH) => ColumnPlan {
            left: LEFT_PANEL_TIGHT_WIDTH,
            rail: RAIL_TIGHT_WIDTH,
        },
        (true, true) if prefer_rail => ColumnPlan {
            left: 0,
            rail: RAIL_WIDTH,
        },
        (true, true) => ColumnPlan {
            left: LEFT_PANEL_WIDTH,
            rail: 0,
        },
    }
}

#[derive(Debug, Default)]
pub struct ViewState {
    pub active_tab: ActiveTab,
    pub left_panel_user_set: bool,
    pub left_panel_focused: bool,
    pub left_panel_visible: bool,
    pub view_focused: bool,
    pub last_width: u16,
    pub inbox_selected: usize,
    pub ledger_selected: usize,
    pub ledger_filter: LedgerFilter,
    pub inbox_dismissed: HashSet<String>,
    pub wakes: Vec<BackgroundResult>,
    pub ledger: Ledger,
    pub ledger_path: Option<PathBuf>,
    pub ledger_loaded: bool,
    pub commander_mission_key: Option<String>,
    pub goal_key: Option<String>,
    pub goal_cancelled: bool,
    pub hits: Vec<(Rect, TabHit)>,
    pub profile_tiers: HashMap<String, String>,
}

pub fn member_task_status(member: &Member, running: bool) -> TaskStatus {
    if running {
        return TaskStatus::Running;
    }
    match member.status {
        Status::Running => TaskStatus::Running,
        Status::NeedsApproval => TaskStatus::NeedsApproval,
        Status::Done => TaskStatus::Done,
        Status::Error => TaskStatus::Error,
        Status::Snoozed => TaskStatus::Snoozed,
        Status::Idle if member.settled => TaskStatus::Done,
        Status::Idle => TaskStatus::Queued,
    }
}

fn specialist_observations(member: &Member, running: bool) -> Vec<Observation> {
    let status = member_task_status(member, running);
    let branch = member.branch().map(|branch| branch.name.clone());
    let changed_files = member.changes.as_ref().map(|changes| changes.files.len());
    let mut observations = vec![Observation {
        key: format!("spawn:{}", member.name.to_ascii_lowercase()),
        kind: TaskKind::Spawn,
        title: member.mission.clone(),
        owner: member.name.clone(),
        status,
        branch: branch.clone(),
        changed_files,
    }];
    let rounds = member.review.rounds;
    if rounds > 0 {
        observations.push(Observation {
            key: format!("review:{}:{rounds}", member.name.to_ascii_lowercase()),
            kind: TaskKind::Review,
            title: format!("Review round {rounds} · {}", member.mission),
            owner: member.name.clone(),
            status,
            branch,
            changed_files,
        });
    }
    observations
}

fn unique_key(prefix: &str) -> String {
    format!("{prefix}:{}", uuid::Uuid::new_v4().simple())
}

impl App {
    pub fn active_tab(&self) -> ActiveTab {
        self.views.active_tab
    }

    pub fn left_panel_visible(&self) -> bool {
        self.views.left_panel_visible
    }

    pub fn left_panel_focused(&self) -> bool {
        self.views.left_panel_focused
    }

    pub fn view_focused(&self) -> bool {
        self.views.view_focused
    }

    pub fn ledger(&self) -> &Ledger {
        &self.views.ledger
    }

    pub fn ledger_filter(&self) -> LedgerFilter {
        self.views.ledger_filter
    }

    pub fn tab_hits(&self) -> &[(Rect, TabHit)] {
        &self.views.hits
    }

    pub fn deck_hits(&self) -> Vec<(usize, Rect)> {
        self.views
            .hits
            .iter()
            .filter_map(|(rect, hit)| match hit {
                TabHit::UsageTile(profile) if *profile != usize::MAX => Some((*profile, *rect)),
                _ => None,
            })
            .collect()
    }

    pub fn left_panel_wanted(&self, width: u16) -> bool {
        self.left_panel_open && (width >= LEFT_PANEL_AUTO_WIDTH || self.views.left_panel_user_set)
    }

    pub fn column_plan(&self, width: u16) -> ColumnPlan {
        plan_columns(
            width,
            self.left_panel_wanted(width),
            self.rail_open,
            self.rail_focused,
        )
    }

    pub fn toggle_left_panel(&mut self) {
        let width = self.views.last_width;
        let now_visible = self.views.left_panel_visible;
        self.left_panel_open = !now_visible;
        self.views.left_panel_user_set = true;
        if now_visible {
            self.views.left_panel_focused = false;
            self.notice = "Left panel hidden · F8 or Ctrl+\\ to show".into();
        } else if width > 0 && width < LEFT_PANEL_MIN_TERMINAL {
            self.left_panel_open = true;
            self.notice = format!(
                "Left panel needs {LEFT_PANEL_MIN_TERMINAL}+ columns · use the Chat/Inbox/Ledger pills"
            );
        } else {
            self.notice = "Left panel shown · F8 or Ctrl+\\ to hide".into();
        }
    }

    pub fn switch_tab(&mut self, tab: ActiveTab) {
        self.views.active_tab = tab;
        self.views.left_panel_focused = false;
        self.rail_focused = false;
        self.views.view_focused = tab != ActiveTab::Chat;
        self.editing = tab == ActiveTab::Chat;
    }

    pub fn inbox_items(&self) -> Vec<InboxItem> {
        let mut items = self.approval_items();
        items.extend(self.continuation_item());
        items.extend(self.specialist_items());
        items.extend(self.wake_items());
        items.retain(|item| !self.views.inbox_dismissed.contains(&item.key));
        items.sort_by_key(InboxItem::priority);
        items
    }

    pub fn inbox_count(&self) -> usize {
        self.inbox_items().len()
    }

    fn approval_items(&self) -> Vec<InboxItem> {
        let Some(live) = &self.live else {
            return Vec::new();
        };
        let mut pending: Vec<(&String, &String)> = live.pending_prod_gates.iter().collect();
        pending.sort();
        pending
            .into_iter()
            .map(|(req_id, run)| InboxItem {
                key: format!("approval:{req_id}"),
                kind: InboxKind::Approval {
                    req_id: req_id.clone(),
                },
                thread: run.clone(),
                title: format!("ProdGate approval · {run}"),
                detail: "Destructive command is waiting for Approve or Deny".into(),
            })
            .collect()
    }

    fn continuation_item(&self) -> Option<InboxItem> {
        let live = self.live.as_ref()?;
        if live.pending.is_none() || live.running(specialists::COMMANDER) {
            return None;
        }
        Some(InboxItem {
            key: "continuation".into(),
            kind: InboxKind::Continuation,
            thread: specialists::COMMANDER.into(),
            title: "Interrupted continuation pending".into(),
            detail: "Resume with your next message or abandon it (F4)".into(),
        })
    }

    fn specialist_items(&self) -> Vec<InboxItem> {
        self.crew
            .iter()
            .skip(1)
            .filter(|member| {
                matches!(
                    member.status,
                    Status::NeedsApproval | Status::Error | Status::Done | Status::Snoozed
                )
            })
            .map(|member| InboxItem {
                key: format!(
                    "member:{}:{}:{}",
                    member.name.to_ascii_lowercase(),
                    member.status.label(),
                    member.touched
                ),
                kind: InboxKind::Specialist(member.status),
                thread: member.name.clone(),
                title: format!("{} · {}", member.name, member.status.caption()),
                detail: member.mission.clone(),
            })
            .collect()
    }

    fn wake_items(&self) -> Vec<InboxItem> {
        self.views
            .wakes
            .iter()
            .rev()
            .map(|wake| InboxItem {
                key: format!("wake:{}:{}", wake.worker_id, wake.finished_at),
                kind: InboxKind::Wake {
                    is_error: wake.is_error,
                },
                thread: wake.worker_id.clone(),
                title: format!("{} finished · {}", wake.worker_id, wake.status),
                detail: wake.summary.clone(),
            })
            .collect()
    }

    pub(crate) fn record_wake(&mut self, result: BackgroundResult) {
        self.views.wakes.push(result);
        let excess = self.views.wakes.len().saturating_sub(MAX_WAKES);
        self.views.wakes.drain(..excess);
    }

    fn jump_to_thread(&mut self, thread: &str) {
        if let Some(index) = specialists::find(&self.crew, thread) {
            self.select_member_thread(index);
        }
        self.switch_tab(ActiveTab::Chat);
    }

    fn dismiss_inbox_item(&mut self, item: &InboxItem) {
        if item.dismissable() {
            self.views.inbox_dismissed.insert(item.key.clone());
        }
    }

    pub fn run_inbox_action(&mut self, index: usize, action: InboxAction) {
        let Some(item) = self.inbox_items().into_iter().nth(index) else {
            return;
        };
        self.views.inbox_selected = index;
        match (action, &item.kind) {
            (InboxAction::Approve, InboxKind::Approval { req_id }) => self.handle_transcript_hit(
                &item.thread,
                TranscriptHitAction::AllowProdGate {
                    req_id: req_id.clone(),
                },
            ),
            (InboxAction::Deny, InboxKind::Approval { req_id }) => self.handle_transcript_hit(
                &item.thread,
                TranscriptHitAction::DenyProdGate {
                    req_id: req_id.clone(),
                },
            ),
            (InboxAction::Resume, _) => {
                self.jump_to_thread(specialists::COMMANDER);
                self.notice =
                    "Type your next message to resume the interrupted continuation.".into();
            }
            (InboxAction::Abandon, _) => self.abandon_continuation(),
            (InboxAction::Jump, _) => {
                self.dismiss_inbox_item(&item);
                self.jump_to_thread(&item.thread);
            }
            (InboxAction::Diff, _) => self.open_diff_viewer(Some(&item.thread)),
            (InboxAction::Pr, _) => self.open_pr(Some(&item.thread), false),
            (InboxAction::Dismiss, _) => {
                self.dismiss_inbox_item(&item);
                if !item.dismissable() {
                    self.notice = "Approvals and continuations stay until resolved.".into();
                }
            }
            _ => {}
        }
        self.clamp_view_selection();
    }

    fn open_selected_inbox_item(&mut self) {
        let index = self.views.inbox_selected;
        let Some(item) = self.inbox_items().into_iter().nth(index) else {
            return;
        };
        let primary = match item.kind {
            InboxKind::Continuation => InboxAction::Resume,
            _ => InboxAction::Jump,
        };
        self.run_inbox_action(index, primary);
    }

    pub fn ledger_rows(&self) -> Vec<LedgerEntry> {
        self.views
            .ledger
            .view(self.views.ledger_filter)
            .into_iter()
            .cloned()
            .collect()
    }

    pub fn cycle_ledger_filter(&mut self) {
        self.set_ledger_filter(self.views.ledger_filter.next());
    }

    pub fn set_ledger_filter(&mut self, filter: LedgerFilter) {
        self.views.ledger_filter = filter;
        self.views.ledger_selected = 0;
    }

    pub fn open_ledger_row(&mut self, index: usize, diff: bool) {
        let Some(entry) = self.ledger_rows().into_iter().nth(index) else {
            return;
        };
        self.views.ledger_selected = index;
        let thread = if entry.owner == COMMANDER_OWNER {
            specialists::COMMANDER.to_owned()
        } else {
            entry.owner.clone()
        };
        if diff {
            self.open_diff_viewer(Some(&thread));
        } else {
            self.jump_to_thread(&thread);
        }
    }

    fn view_len(&self) -> usize {
        match self.views.active_tab {
            ActiveTab::Inbox => self.inbox_count(),
            ActiveTab::Ledger => self.ledger_rows().len(),
            ActiveTab::Chat => 0,
        }
    }

    fn clamp_view_selection(&mut self) {
        let inbox_max = self.inbox_count().saturating_sub(1);
        let ledger_max = self.ledger_rows().len().saturating_sub(1);
        self.views.inbox_selected = self.views.inbox_selected.min(inbox_max);
        self.views.ledger_selected = self.views.ledger_selected.min(ledger_max);
    }

    pub fn view_selection(&self) -> usize {
        match self.views.active_tab {
            ActiveTab::Ledger => self.views.ledger_selected,
            _ => self.views.inbox_selected,
        }
    }

    fn move_view_selection(&mut self, forward: bool) {
        let len = self.view_len();
        let slot = match self.views.active_tab {
            ActiveTab::Inbox => &mut self.views.inbox_selected,
            ActiveTab::Ledger => &mut self.views.ledger_selected,
            ActiveTab::Chat => return,
        };
        if len == 0 {
            *slot = 0;
        } else if forward {
            *slot = (*slot + 1).min(len - 1);
        } else {
            *slot = slot.saturating_sub(1);
        }
    }

    pub(crate) fn handle_tab_shortcut(&mut self, key: KeyEvent) -> bool {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::F(8) => {
                self.toggle_left_panel();
                true
            }
            KeyCode::Char('\\') | KeyCode::Char('4') if ctrl => {
                self.toggle_left_panel();
                true
            }
            KeyCode::Char(digit) if alt && !ctrl => match ActiveTab::from_digit(digit) {
                Some(tab) => {
                    self.switch_tab(tab);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    pub(crate) fn handle_left_panel_key(&mut self, key: KeyEvent) -> bool {
        if !self.views.left_panel_focused {
            return false;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Left => {
                let tab = self.views.active_tab.step(false);
                self.switch_tab(tab);
                self.focus_left_panel();
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Right => {
                let tab = self.views.active_tab.step(true);
                self.switch_tab(tab);
                self.focus_left_panel();
            }
            KeyCode::Enter => self.switch_tab(self.views.active_tab),
            KeyCode::Esc => {
                self.views.left_panel_focused = false;
                self.editing = true;
            }
            _ => {
                self.views.left_panel_focused = false;
                self.editing = true;
                return false;
            }
        }
        true
    }

    pub(crate) fn focus_left_panel(&mut self) {
        self.views.left_panel_focused = true;
        self.views.view_focused = false;
        self.rail_focused = false;
        self.editing = false;
    }

    pub(crate) fn cycle_focus(&mut self) -> bool {
        let left = self.views.left_panel_visible;
        let rail = self.rail_open && self.crew_area.width > 0;
        if self.views.left_panel_focused {
            self.views.left_panel_focused = false;
            self.editing = true;
            return true;
        }
        if self.rail_focused {
            self.rail_focused = false;
            if left {
                self.focus_left_panel();
            } else {
                self.editing = true;
            }
            return true;
        }
        if rail {
            self.rail_focused = true;
            self.views.view_focused = false;
            self.editing = false;
            return true;
        }
        if left {
            self.focus_left_panel();
            return true;
        }
        false
    }

    pub(crate) fn handle_view_key(&mut self, key: KeyEvent) -> bool {
        if !self.views.view_focused || self.views.active_tab == ActiveTab::Chat {
            return false;
        }
        let tab = self.views.active_tab;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_view_selection(false),
            KeyCode::Down | KeyCode::Char('j') => self.move_view_selection(true),
            KeyCode::Home => match tab {
                ActiveTab::Ledger => self.views.ledger_selected = 0,
                _ => self.views.inbox_selected = 0,
            },
            KeyCode::Enter if tab == ActiveTab::Inbox => self.open_selected_inbox_item(),
            KeyCode::Enter => self.open_ledger_row(self.views.ledger_selected, false),
            KeyCode::Char('d') if tab == ActiveTab::Inbox => {
                self.run_inbox_action(self.views.inbox_selected, InboxAction::Diff)
            }
            KeyCode::Char('d') => self.open_ledger_row(self.views.ledger_selected, true),
            KeyCode::Char('p') if tab == ActiveTab::Inbox => {
                self.run_inbox_action(self.views.inbox_selected, InboxAction::Pr)
            }
            KeyCode::Char('x') if tab == ActiveTab::Inbox => {
                self.run_inbox_action(self.views.inbox_selected, InboxAction::Dismiss)
            }
            KeyCode::Char('a') if tab == ActiveTab::Inbox => {
                self.run_inbox_action(self.views.inbox_selected, InboxAction::Approve)
            }
            KeyCode::Char('f') if tab == ActiveTab::Ledger => self.cycle_ledger_filter(),
            KeyCode::Esc => self.switch_tab(ActiveTab::Chat),
            _ => {
                self.views.view_focused = false;
                self.editing = true;
                return false;
            }
        }
        true
    }

    pub(crate) fn handle_tab_hit(&mut self, hit: TabHit) {
        match hit {
            TabHit::Tab(tab) => self.switch_tab(tab),
            TabHit::ToggleLeftPanel => self.toggle_left_panel(),
            TabHit::UsageTile(profile) => self.open_settings_for_profile(profile),
            TabHit::Thread(index) => {
                self.select_member_thread(index);
                self.switch_tab(ActiveTab::Chat);
            }
            TabHit::InboxRow(index) => {
                self.views.inbox_selected = index;
                self.views.view_focused = true;
                self.editing = false;
            }
            TabHit::InboxAction(index, action) => self.run_inbox_action(index, action),
            TabHit::LedgerRow(index) => {
                if self.views.ledger_selected == index && self.views.view_focused {
                    self.open_ledger_row(index, false);
                } else {
                    self.views.ledger_selected = index;
                    self.views.view_focused = true;
                    self.editing = false;
                }
            }
            TabHit::LedgerFilter(filter) => {
                self.set_ledger_filter(filter);
                self.views.view_focused = true;
                self.editing = false;
            }
        }
    }

    pub fn usage_cards(&self) -> Vec<(usize, AccountCard)> {
        let Some(live) = &self.live else {
            return Vec::new();
        };
        let displayed = live.displayed_profile();
        live.profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                let allowance = live.allowances.get(index).copied().unwrap_or_default();
                let (mut five, mut week) = (allowance.five, allowance.week);
                if index == displayed {
                    if let Some((five_used, week_used)) = live.live_quota {
                        let left = |used: f32| (100.0 - f64::from(used)).clamp(0.0, 100.0);
                        five = five_used.map(left).or(five);
                        week = week_used.map(left).or(week);
                    }
                }
                let tier = self
                    .views
                    .profile_tiers
                    .get(&profile.to_string())
                    .cloned()
                    .unwrap_or_default();
                let card = AccountCard {
                    provider: provider_short_name(profile.provider).into(),
                    profile: profile.name().into(),
                    tier,
                    primary_remaining: five,
                    secondary_remaining: week,
                    seconds_until_reset: allowance.reset_in,
                    route_model: None,
                    route_metric: None,
                };
                (index, card)
            })
            .collect()
    }

    pub fn headline_allowance(&self) -> Option<(String, Option<f64>)> {
        let live = self.live.as_ref()?;
        let cards = self.usage_cards();
        let displayed = live.displayed_profile();
        let (_, card) = cards.into_iter().find(|(index, _)| *index == displayed)?;
        Some((card.provider.clone(), card.ring_layout().headline()))
    }

    pub(crate) fn sync_views(&mut self) {
        self.cache_profile_tiers();
        self.sync_ledger();
    }

    fn cache_profile_tiers(&mut self) {
        let Some(live) = &self.live else {
            return;
        };
        for profile in &live.profiles {
            let key = profile.to_string();
            self.views.profile_tiers.entry(key).or_insert_with(|| {
                token_burn::normalize_tier(profile.detect_passive().tier.as_deref())
            });
        }
    }

    pub(crate) fn sync_ledger(&mut self) {
        self.load_stored_ledger();
        let now = chrono::Utc::now().timestamp();
        let observations: Vec<Observation> = self
            .crew
            .iter()
            .skip(1)
            .flat_map(|member| {
                specialist_observations(member, self.is_member_running(&member.name))
            })
            .collect();
        for observation in observations {
            self.views.ledger.observe(observation, now);
        }
        self.sync_commander_mission(now);
        self.sync_goal(now);
        self.persist_ledger();
    }

    fn is_member_running(&self, name: &str) -> bool {
        self.live.as_ref().is_some_and(|live| live.running(name))
    }

    fn sync_commander_mission(&mut self, now: i64) {
        let running = self.is_member_running(specialists::COMMANDER);
        match (self.views.commander_mission_key.clone(), running) {
            (None, true) => {
                let key = unique_key("mission");
                self.views.ledger.observe(
                    Observation {
                        key: key.clone(),
                        kind: TaskKind::Mission,
                        title: self.crew[0].mission.clone(),
                        owner: COMMANDER_OWNER.into(),
                        status: TaskStatus::Running,
                        branch: self.git_snapshot.branch.clone(),
                        changed_files: None,
                    },
                    now,
                );
                self.views.commander_mission_key = Some(key);
            }
            (Some(key), false) => {
                let status = if self.crew[0].status == Status::Error {
                    TaskStatus::Error
                } else {
                    TaskStatus::Done
                };
                self.views.ledger.set_status(&key, status, now);
                self.views.commander_mission_key = None;
            }
            _ => {}
        }
    }

    fn sync_goal(&mut self, now: i64) {
        match (self.goal.clone(), self.views.goal_key.clone()) {
            (Some(goal), key) => {
                let key = key.unwrap_or_else(|| unique_key("goal"));
                let owner = if goal.member == specialists::COMMANDER {
                    COMMANDER_OWNER.to_owned()
                } else {
                    goal.member.clone()
                };
                self.views.ledger.observe(
                    Observation {
                        key: key.clone(),
                        kind: TaskKind::Goal,
                        title: format!("Goal: {} ({}/{})", goal.criteria, goal.turns, goal.cap),
                        owner,
                        status: TaskStatus::Running,
                        branch: None,
                        changed_files: None,
                    },
                    now,
                );
                self.views.goal_key = Some(key);
                self.views.goal_cancelled = false;
            }
            (None, Some(key)) => {
                let owner = self.views.ledger.get(&key).map(|entry| entry.owner.clone());
                let errored = owner
                    .and_then(|owner| specialists::find(&self.crew, &owner))
                    .is_some_and(|index| self.crew[index].status == Status::Error);
                let status = if self.views.goal_cancelled {
                    TaskStatus::Cancelled
                } else if errored {
                    TaskStatus::Error
                } else {
                    TaskStatus::Done
                };
                self.views.ledger.set_status(&key, status, now);
                self.views.goal_key = None;
                self.views.goal_cancelled = false;
            }
            (None, None) => {}
        }
    }

    fn load_stored_ledger(&mut self) {
        if self.views.ledger_loaded {
            return;
        }
        let Some(live) = &self.live else {
            return;
        };
        self.views.ledger_loaded = true;
        let Some(store) = &live.store else {
            return;
        };
        let Ok(path) = store.ledger_path(&live.workspace) else {
            return;
        };
        let stored = Ledger::load(&path);
        self.views.ledger.merge(stored);
        self.views.ledger_path = Some(path);
    }

    fn persist_ledger(&mut self) {
        if !self.views.ledger.is_dirty() {
            return;
        }
        if let Some(path) = self.views.ledger_path.clone() {
            if let Err(error) = self.views.ledger.save(&path) {
                self.views.ledger_path = None;
                self.notice = format!("Ledger not saved: {error}");
            }
        }
    }
}
