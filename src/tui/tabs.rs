use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    bridge,
    ledger::{LedgerEntry, LedgerFilter, TaskKind, TaskStatus, TaskUpdate},
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
    LedgerRow(usize),
    LedgerFilter(LedgerFilter),
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
            InboxKind::Member(_) => &[
                (InboxAction::Open, "Open"),
                (InboxAction::Diff, "Diff"),
                (InboxAction::Pr, "PR"),
                (InboxAction::Dismiss, "Read"),
            ],
            InboxKind::Wake { .. } => &[
                (InboxAction::Open, "Open"),
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
        self.clamp_tab_selection();
        if tab == ViewTab::Chat {
            self.editing = true;
        }
    }

    fn clamp_tab_selection(&mut self) {
        let inbox_len = self.inbox_items().len();
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

    pub fn inbox_count(&self) -> usize {
        self.inbox_items().len()
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
        if !self.ledger.update_latest(&result.worker_id, &update, now()) {
            self.ledger
                .record(TaskKind::Spawn, &result.worker_id, &result.summary, now());
            self.ledger.update_latest(&result.worker_id, &update, now());
        }
        self.save_ledger();
    }

    pub fn inbox_act(&mut self, index: usize, action: InboxAction) {
        let Some(item) = self.inbox_items().into_iter().nth(index) else {
            return;
        };
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
                self.notice = "Type your next Commander message to resume the continuation.".into();
            }
            (InboxKind::AskUser { request_id }, InboxAction::Dismiss) => {
                if let Some(broker) = self.live.as_ref().and_then(|live| live.ask_user.as_ref()) {
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
    }

    pub fn ledger_open(&mut self, index: usize, diff: bool) {
        let Some(entry) = self.ledger_rows().into_iter().nth(index) else {
            return;
        };
        self.ledger_selected = index;
        if diff {
            self.open_diff_viewer(Some(&entry.owner));
        } else {
            self.jump_to_thread(&entry.owner);
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
        self.ledger.record(kind, owner, title, stamp);
        self.ledger
            .update_latest(owner, &TaskUpdate::status(status), stamp);
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
                self.ledger.record(TaskKind::Spawn, &name, &title, stamp);
                self.ledger.update_latest(&name, &update, stamp);
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
        let displayed = live.displayed_profile();
        live.profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                let allowance = live.allowances.get(index).copied().unwrap_or_default();
                let (mut primary, mut secondary) = (allowance.five, allowance.week);
                if let (true, Some((five_used, week_used))) = (index == displayed, live.live_quota)
                {
                    let left = |used: f32| (100.0 - f64::from(used)).clamp(0.0, 100.0);
                    primary = five_used.map(left).or(primary);
                    secondary = week_used.map(left).or(secondary);
                }
                let tier = profile
                    .is_system_default()
                    .then(|| {
                        self.auth_detections
                            .iter()
                            .find(|detection| detection.provider == profile.provider)
                            .and_then(|detection| detection.tier.as_deref())
                    })
                    .flatten();
                let card = AccountCard {
                    provider: provider_short_name(profile.provider).to_owned(),
                    profile: profile.name().to_owned(),
                    tier: normalize_tier(tier),
                    primary_remaining: primary,
                    secondary_remaining: secondary,
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

    pub(crate) fn handle_view_key(&mut self, key: KeyEvent) -> bool {
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
        let len = self.inbox_items().len();
        let index = self.inbox_selected;
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.inbox_selected = index.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.inbox_selected = (index + 1).min(len.saturating_sub(1))
            }
            KeyCode::Home => self.inbox_selected = 0,
            KeyCode::Enter => self.inbox_act(index, InboxAction::Open),
            KeyCode::Char('a') => self.inbox_act(index, InboxAction::Approve),
            KeyCode::Char('n') => self.inbox_act(index, InboxAction::Deny),
            KeyCode::Char('d') => self.inbox_act(index, InboxAction::Diff),
            KeyCode::Char('p') => self.inbox_act(index, InboxAction::Pr),
            KeyCode::Char('x') => self.inbox_act(index, InboxAction::Dismiss),
            KeyCode::Esc => self.set_view_tab(ViewTab::Chat),
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
            TabHit::LedgerRow(index) => {
                if self.ledger_selected == index {
                    self.ledger_open(index, false);
                } else {
                    self.ledger_selected = index;
                }
            }
            TabHit::LedgerFilter(filter) => self.set_ledger_filter(filter),
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
    use crate::{auth, usage::Allowance, Live};
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

    fn is_braille(symbol: &str) -> bool {
        symbol
            .chars()
            .next()
            .is_some_and(|ch| ('\u{2801}'..='\u{28FF}').contains(&ch))
    }

    #[test]
    fn left_panel_renders_concentric_and_unknown_rings_with_token_burn_colors() {
        let root = scratch("rings");
        let mut app = live_app(&root);
        let buffer = render(&mut app, 140, 40);
        let panel = text(&buffer, Rect::new(0, 0, 30, 40));
        assert!(panel.contains("Claude · default"), "{panel}");
        assert!(
            panel.contains("64% left") && panel.contains(" MAX "),
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
        let braille_cells = (1..40u16)
            .flat_map(|y| (0..30u16).map(move |x| (x, y)))
            .filter(|&(x, y)| is_braille(buffer[(x, y)].symbol()))
            .count();
        assert!(braille_cells >= 30, "{braille_cells}\n{panel}");
        let deck = app.deck_palette;
        let (x, y) = find(&buffer, "64% left").unwrap();
        assert_eq!(buffer[(x, y)].fg, deck.healthy);
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
        assert!(top.contains("◑") || top.contains("◕"), "{top}");
        let (x, y) = find(&buffer, "◕").or_else(|| find(&buffer, "◑")).unwrap();
        assert_eq!(buffer[(x, y)].fg, app.deck_palette.healthy);
        std::fs::remove_dir_all(root).unwrap();
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
}
