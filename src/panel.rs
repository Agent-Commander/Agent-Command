use crate::{
    auth::{self, Found, Profile, Provider},
    color,
    install::{self, Plan, Platform},
    settings, update, App,
};
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableMouseCapture, EnableMouseCapture, KeyCode, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Position, Rect},
    style::Style,
    text::Span,
    widgets::{Block, Clear, Paragraph},
    Frame,
};
use std::{
    env, io,
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) const NO_PROVIDER_HINT: &str = "No provider connected · press F3 or Settings to connect";
pub(crate) const DEFAULT_PROFILE_NAME: &str = "main";
const PANEL_MAX_WIDTH: u16 = 78;
const PROFILE_LABEL_WIDTH: usize = 22;
const STATUS_MAX_ROWS: usize = 3;
const PREVIEW_BLOCKER: &str = "Connecting needs the live terminal; this is a preview.";

pub(crate) trait Host {
    fn found(&self, provider: Provider) -> Option<Found>;
    fn documented_executable(&self, provider: Provider) -> Option<PathBuf>;
    fn adopt(&self, provider: Provider, executable: &Path);
    fn has_program(&self, name: &str) -> bool;
    fn profiles(&self, root: &Path) -> io::Result<Vec<Profile>>;
    fn signed_in(&self, profile: &Profile) -> bool;
    fn suspend(&mut self) -> io::Result<()>;
    fn sign_in(&mut self, profile: &Profile) -> io::Result<()>;
    fn install(&mut self, plan: &Plan) -> io::Result<()>;
    fn restore(&mut self) -> io::Result<()>;
    fn sign_out(&mut self, profile: &Profile) -> io::Result<()>;
}

pub(crate) struct TerminalHost;

impl Host for TerminalHost {
    fn found(&self, provider: Provider) -> Option<Found> {
        auth::found(provider)
    }

    fn documented_executable(&self, provider: Provider) -> Option<PathBuf> {
        install::documented_executable(provider, Platform::current(), |name| env::var_os(name))
    }

    fn adopt(&self, provider: Provider, executable: &Path) {
        auth::adopt(provider, executable)
    }

    fn has_program(&self, name: &str) -> bool {
        auth::program_on_path(name)
    }

    fn profiles(&self, root: &Path) -> io::Result<Vec<Profile>> {
        auth::profiles(root)
    }

    fn signed_in(&self, profile: &Profile) -> bool {
        profile.signed_in()
    }

    fn suspend(&mut self) -> io::Result<()> {
        execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        )?;
        disable_raw_mode()
    }

    fn sign_in(&mut self, profile: &Profile) -> io::Result<()> {
        println!("Signing in {profile}. Follow the provider's prompts.");
        auth::succeeded(profile, auth::sign_in(profile)?)
    }

    fn install(&mut self, plan: &Plan) -> io::Result<()> {
        println!(
            "Installing {}: {}",
            provider_title(plan.provider),
            plan.command
        );
        let status = Command::new(plan.program).args(&plan.arguments).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("installer exited with {status}")))
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)
    }

    fn sign_out(&mut self, profile: &Profile) -> io::Result<()> {
        auth::succeeded(profile, auth::sign_out_quietly(profile)?)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Origin {
    Onboarding,
    Settings,
    Direct,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    Reconnect(usize),
    SignOut(usize),
    ConfirmSignOut,
    CancelSignOut,
    ConnectNew,
    CommanderProfile,
    UpdateCheck,
    AutoResume,
    SnoozeMissions,
    FallbackProfile,
    TextGenProfile,
    Close,
    Toggle(Provider),
    Name,
    Connect,
    Recheck,
    Leave,
    Install(Provider),
    ConfirmInstall,
    CancelInstall,
}

#[derive(Debug, Default)]
struct Clis([Option<Found>; 2]);

impl Clis {
    fn located(host: Option<&dyn Host>) -> Self {
        Self(Provider::ALL.map(|provider| host.and_then(|host| locate(host, provider))))
    }

    fn found(&self, provider: Provider) -> Option<&Found> {
        self.0[provider_slot(provider)].as_ref()
    }

    fn missing(&self) -> Vec<Provider> {
        Provider::ALL
            .into_iter()
            .filter(|provider| self.found(*provider).is_none())
            .collect()
    }
}

fn locate(host: &dyn Host, provider: Provider) -> Option<Found> {
    if let Some(found) = host.found(provider) {
        return Some(found);
    }
    let executable = host
        .documented_executable(provider)
        .filter(|path| path.is_file())?;
    host.adopt(provider, &executable);
    Some(Found::At(executable))
}

#[derive(Debug)]
pub(crate) struct SettingsView {
    focus: usize,
    confirm: Option<usize>,
    clis: Clis,
    installing: Option<Provider>,
    status: String,
}

#[derive(Debug)]
pub(crate) struct ConnectForm {
    pub(crate) origin: Origin,
    clis: Clis,
    installing: Option<Provider>,
    chosen: [bool; 2],
    pub(crate) name: String,
    focus: usize,
    pub(crate) status: String,
}

#[derive(Debug)]
pub(crate) enum Panel {
    Settings(SettingsView),
    Connect(ConnectForm),
}

fn provider_slot(provider: Provider) -> usize {
    match provider {
        Provider::Claude => 0,
        Provider::Codex => 1,
    }
}

fn provider_title(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude Code",
        Provider::Codex => "Codex CLI",
    }
}

fn truncated(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.into();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn wrapped(text: &str, width: usize, rows: usize) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    characters
        .chunks(width.max(1))
        .take(rows)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[derive(Clone, Copy)]
enum Tone {
    Plain,
    Muted,
    Heading,
    Good,
    Warn,
}

struct Segment {
    text: String,
    tone: Tone,
    action: Option<Action>,
}

fn text(text: impl Into<String>, tone: Tone) -> Segment {
    Segment {
        text: text.into(),
        tone,
        action: None,
    }
}

fn button(label: impl AsRef<str>, action: Action) -> Segment {
    Segment {
        text: format!(" {} ", label.as_ref()),
        tone: Tone::Plain,
        action: Some(action),
    }
}

type Row = Vec<Segment>;

impl ConnectForm {
    fn new(origin: Origin, host: Option<&dyn Host>) -> Self {
        let mut form = Self {
            origin,
            clis: Clis::default(),
            installing: None,
            chosen: [false; 2],
            name: DEFAULT_PROFILE_NAME.into(),
            focus: 0,
            status: String::new(),
        };
        form.recheck(host);
        if let Some(first) = Provider::ALL
            .into_iter()
            .find(|provider| form.clis.found(*provider).is_some())
        {
            form.chosen[provider_slot(first)] = true;
        }
        form
    }

    fn recheck(&mut self, host: Option<&dyn Host>) {
        self.clis = Clis::located(host);
        for provider in Provider::ALL {
            self.chosen[provider_slot(provider)] &= self.clis.found(provider).is_some();
        }
    }

    fn missing(&self) -> bool {
        !self.clis.missing().is_empty()
    }

    fn chosen_providers(&self) -> Vec<Provider> {
        Provider::ALL
            .into_iter()
            .filter(|provider| self.chosen[provider_slot(*provider)])
            .collect()
    }

    fn actions(&self) -> Vec<Action> {
        if self.installing.is_some() {
            return vec![Action::ConfirmInstall, Action::CancelInstall];
        }
        let mut actions = Vec::new();
        for provider in Provider::ALL {
            actions.push(Action::Toggle(provider));
            if self.clis.found(provider).is_none() {
                actions.push(Action::Install(provider));
            }
        }
        actions.extend([Action::Name, Action::Connect]);
        if self.missing() {
            actions.push(Action::Recheck);
        }
        actions.push(Action::Leave);
        actions
    }

    fn leave_label(&self) -> &'static str {
        match self.origin {
            Origin::Onboarding => "Skip",
            Origin::Settings => "Back",
            Origin::Direct => "Cancel",
        }
    }

    fn rows(&self, editing_name: bool, width: usize) -> Vec<Row> {
        let mut rows: Vec<Row> = vec![
            vec![text(
                "Pick Claude, Codex or both. One provider is enough to continue.",
                Tone::Muted,
            )],
            vec![],
        ];
        for provider in Provider::ALL {
            let slot = provider_slot(provider);
            let mark = if self.chosen[slot] { "[x]" } else { "[ ]" };
            let mut row = vec![button(
                format!("{mark} {:<11}", provider_title(provider)),
                Action::Toggle(provider),
            )];
            row.extend(availability(
                provider,
                self.clis.found(provider),
                width.saturating_sub(19),
            ));
            rows.push(row);
        }
        let cursor = if editing_name { "▏" } else { " " };
        rows.push(vec![]);
        rows.push(vec![
            text("Profile name  ", Tone::Plain),
            button(format!("{:<16}{cursor}", self.name), Action::Name),
        ]);
        rows.push(vec![]);
        if let Some(provider) = self.installing {
            rows.extend(install_prompt(provider, width));
            return rows;
        }
        let mut controls = vec![button("Connect", Action::Connect), text("  ", Tone::Plain)];
        if self.missing() {
            controls.push(button("R Recheck", Action::Recheck));
            controls.push(text("  ", Tone::Plain));
        }
        controls.push(button(self.leave_label(), Action::Leave));
        rows.push(controls);
        rows.push(vec![]);
        rows.push(vec![text(
            "Connect hands this terminal to the provider's sign-in, then returns here.",
            Tone::Muted,
        )]);
        rows
    }
}

fn availability(provider: Provider, found: Option<&Found>, width: usize) -> Vec<Segment> {
    let binary = provider.binary();
    match found {
        Some(Found::OnPath) => vec![text(format!("  ✓ {binary} on PATH"), Tone::Good)],
        Some(Found::At(path)) => vec![text(
            truncated(&format!("  ✓ {binary} at {}", path.display()), width),
            Tone::Good,
        )],
        None => vec![
            text(format!("  ✗ {binary} not found  "), Tone::Warn),
            button("Install", Action::Install(provider)),
        ],
    }
}

fn install_prompt(provider: Provider, width: usize) -> Vec<Row> {
    let plan = install::plan(provider, Platform::current());
    let mut rows = vec![vec![text(
        format!(
            "Install {} with its official installer:",
            provider_title(provider)
        ),
        Tone::Heading,
    )]];
    rows.extend(
        wrapped(&plan.command, width.saturating_sub(2), 3)
            .into_iter()
            .map(|line| vec![text(format!("  {line}"), Tone::Plain)]),
    );
    rows.push(vec![text(
        truncated(&format!("Docs  {}", plan.docs), width),
        Tone::Muted,
    )]);
    rows.push(vec![text(
        "Runs here for your user; no sudo or admin. Nothing runs without Y.",
        Tone::Muted,
    )]);
    rows.push(vec![
        button("Y Install", Action::ConfirmInstall),
        text("  ", Tone::Plain),
        button("N Cancel", Action::CancelInstall),
    ]);
    rows
}

impl SettingsView {
    fn new(host: Option<&dyn Host>) -> Self {
        Self {
            focus: 0,
            confirm: None,
            clis: Clis::located(host),
            installing: None,
            status: String::new(),
        }
    }
}

impl App {
    pub(crate) fn open_settings(&mut self) {
        self.panel = Some(Panel::Settings(SettingsView::new(self.host.as_deref())));
    }

    pub(crate) fn open_connect(&mut self, origin: Origin) {
        let form = ConnectForm::new(origin, self.host.as_deref());
        self.panel = Some(Panel::Connect(form));
    }

    pub(crate) fn refresh_sign_in(&mut self) {
        let Some(live) = &mut self.live else { return };
        live.signed_in = live
            .profiles
            .iter()
            .map(|profile| {
                self.host
                    .as_ref()
                    .is_some_and(|host| host.signed_in(profile))
            })
            .collect();
    }

    pub(crate) fn no_provider_connected(&self) -> bool {
        self.host.is_some()
            && self
                .live
                .as_ref()
                .is_some_and(|live| !live.signed_in.contains(&true))
    }

    pub(crate) fn reload_profiles(&mut self) {
        let (Some(live), Some(host)) = (&mut self.live, &self.host) else {
            return;
        };
        let Some(root) = live.profile_root.clone() else {
            return;
        };
        match host.profiles(&root) {
            Ok(profiles) => live.reset_profiles(profiles),
            Err(error) => self.notice = format!("Cannot load profiles: {error}"),
        }
        self.refresh_sign_in();
    }

    fn panel_actions(&self) -> Vec<Action> {
        match &self.panel {
            Some(Panel::Settings(view)) if view.confirm.is_some() => {
                vec![Action::ConfirmSignOut, Action::CancelSignOut]
            }
            Some(Panel::Settings(view)) if view.installing.is_some() => {
                vec![Action::ConfirmInstall, Action::CancelInstall]
            }
            Some(Panel::Settings(view)) => {
                let count = self.live.as_ref().map_or(0, |live| live.profiles.len());
                let mut actions: Vec<Action> = (0..count)
                    .flat_map(|index| [Action::Reconnect(index), Action::SignOut(index)])
                    .collect();
                actions.extend(view.clis.missing().into_iter().map(Action::Install));
                actions.extend([
                    Action::ConnectNew,
                    Action::CommanderProfile,
                    Action::UpdateCheck,
                    Action::AutoResume,
                    Action::SnoozeMissions,
                    Action::FallbackProfile,
                    Action::TextGenProfile,
                    Action::Close,
                ]);
                actions
            }
            Some(Panel::Connect(form)) => form.actions(),
            None => Vec::new(),
        }
    }

    fn focus_index(&mut self) -> Option<&mut usize> {
        match &mut self.panel {
            Some(Panel::Settings(view)) => Some(&mut view.focus),
            Some(Panel::Connect(form)) => Some(&mut form.focus),
            None => None,
        }
    }

    fn focused_action(&self) -> Option<Action> {
        let focus = match &self.panel {
            Some(Panel::Settings(view)) => view.focus,
            Some(Panel::Connect(form)) => form.focus,
            None => return None,
        };
        let actions = self.panel_actions();
        actions
            .get(focus.min(actions.len().saturating_sub(1)))
            .copied()
    }

    fn move_focus(&mut self, forward: bool) {
        let count = self.panel_actions().len().max(1);
        if let Some(focus) = self.focus_index() {
            *focus = if forward {
                (*focus + 1) % count
            } else {
                (*focus + count - 1) % count
            };
        }
    }

    fn set_panel_status(&mut self, status: String) {
        match &mut self.panel {
            Some(Panel::Settings(view)) => view.status = status,
            Some(Panel::Connect(form)) => form.status = status,
            None => self.notice = status,
        }
    }

    fn editing_name(&self) -> bool {
        matches!(self.panel, Some(Panel::Connect(_))) && self.focused_action() == Some(Action::Name)
    }

    pub(crate) fn panel_key(&mut self, key: KeyEvent) {
        if self.editing_name() {
            match key.code {
                KeyCode::Char(c) if !c.is_whitespace() => return self.edit_name(Some(c)),
                KeyCode::Backspace => return self.edit_name(None),
                KeyCode::Enter => return self.activate(Action::Connect),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => self.escape_panel(),
            KeyCode::Up | KeyCode::Left | KeyCode::BackTab => self.move_focus(false),
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => self.move_focus(true),
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(action) = self.focused_action() {
                    self.activate(action);
                }
            }
            KeyCode::Char('y' | 'Y') if self.pending_install().is_some() => {
                self.activate(Action::ConfirmInstall)
            }
            KeyCode::Char('n' | 'N') if self.pending_install().is_some() => {
                self.activate(Action::CancelInstall)
            }
            KeyCode::Char('r' | 'R') if matches!(self.panel, Some(Panel::Connect(_))) => {
                self.activate(Action::Recheck)
            }
            KeyCode::Char('y' | 'Y') if self.confirming() => self.activate(Action::ConfirmSignOut),
            KeyCode::Char('n' | 'N') if self.confirming() => self.activate(Action::CancelSignOut),
            _ => {}
        }
    }

    fn confirming(&self) -> bool {
        matches!(&self.panel, Some(Panel::Settings(view)) if view.confirm.is_some())
    }

    pub(crate) fn panel_click(&mut self, x: u16, y: u16) {
        let point = Position::new(x, y);
        let allowed = self.panel_actions();
        let Some(action) = self
            .panel_hits
            .iter()
            .find(|(action, area)| area.contains(point) && allowed.contains(action))
            .map(|(action, _)| *action)
        else {
            return;
        };
        if let Some(index) = allowed.iter().position(|candidate| *candidate == action) {
            if let Some(focus) = self.focus_index() {
                *focus = index;
            }
        }
        if action != Action::Name {
            self.activate(action);
        }
    }

    fn edit_name(&mut self, character: Option<char>) {
        if let Some(Panel::Connect(form)) = &mut self.panel {
            match character {
                Some(c) => form.name.push(c),
                None => {
                    form.name.pop();
                }
            }
        }
    }

    fn escape_panel(&mut self) {
        if self.confirming() {
            return self.activate(Action::CancelSignOut);
        }
        if self.pending_install().is_some() {
            return self.activate(Action::CancelInstall);
        }
        match &self.panel {
            Some(Panel::Connect(_)) => self.activate(Action::Leave),
            _ => self.panel = None,
        }
    }

    fn activate(&mut self, action: Action) {
        match action {
            Action::Reconnect(index) => self.reconnect(index),
            Action::SignOut(index) => {
                if let Some(Panel::Settings(view)) = &mut self.panel {
                    view.confirm = Some(index);
                    view.focus = 0;
                    view.status.clear();
                }
            }
            Action::ConfirmSignOut => self.confirm_sign_out(),
            Action::CancelSignOut => {
                if let Some(Panel::Settings(view)) = &mut self.panel {
                    view.confirm = None;
                    view.focus = 0;
                    view.status = "Sign out cancelled.".into();
                }
            }
            Action::ConnectNew => self.open_connect(Origin::Settings),
            Action::CommanderProfile => {
                self.select_profile();
                let notice = std::mem::take(&mut self.notice);
                self.set_panel_status(notice);
            }
            Action::UpdateCheck => self.toggle_update_check(),
            Action::AutoResume => {
                self.settings.auto_resume_on_reset = !self.settings.auto_resume_on_reset;
                let state = on_off(self.settings.auto_resume_on_reset);
                self.save_setting(format!("Resume after usage limits: {state}"));
            }
            Action::SnoozeMissions => {
                self.settings.snooze_limited_missions = !self.settings.snooze_limited_missions;
                let state = on_off(self.settings.snooze_limited_missions);
                self.save_setting(format!("Snooze limited missions: {state}"));
            }
            Action::FallbackProfile => {
                self.settings.fallback_profile = settings::next_profile_slot(
                    self.settings.fallback_profile.as_deref(),
                    &self.profile_labels(),
                );
                let label = self.slot_label(self.settings.fallback_profile.as_deref());
                self.save_setting(format!("Cross-provider fallback: {label}"));
            }
            Action::TextGenProfile => {
                self.settings.text_gen_profile = settings::next_profile_slot(
                    self.settings.text_gen_profile.as_deref(),
                    &self.profile_labels(),
                );
                let label = self.slot_label(self.settings.text_gen_profile.as_deref());
                self.save_setting(format!("Text generation profile: {label}"));
            }
            Action::Close => self.panel = None,
            Action::Toggle(provider) => self.toggle_provider(provider),
            Action::Name => {}
            Action::Connect => self.connect_chosen(),
            Action::Recheck => self.recheck(),
            Action::Leave => self.leave_connect(),
            Action::Install(provider) => self.offer_install(provider),
            Action::ConfirmInstall => {
                if let Some(provider) = self.pending_install() {
                    self.run_install(provider);
                }
            }
            Action::CancelInstall => {
                self.set_pending_install(None);
                self.set_panel_status("Install cancelled. Nothing ran.".into());
            }
        }
    }

    fn pending_install(&self) -> Option<Provider> {
        match &self.panel {
            Some(Panel::Settings(view)) => view.installing,
            Some(Panel::Connect(form)) => form.installing,
            None => None,
        }
    }

    fn set_pending_install(&mut self, provider: Option<Provider>) {
        match &mut self.panel {
            Some(Panel::Settings(view)) => {
                view.installing = provider;
                view.focus = 0;
            }
            Some(Panel::Connect(form)) => {
                form.installing = provider;
                form.focus = 0;
            }
            None => {}
        }
    }

    fn offer_install(&mut self, provider: Provider) {
        let Some(host) = self.host.as_deref() else {
            return self.set_panel_status(PREVIEW_BLOCKER.into());
        };
        let plan = install::plan(provider, Platform::current());
        let missing = install::missing_prerequisites(&plan, |name| host.has_program(name));
        if !missing.is_empty() {
            return self.set_panel_status(prerequisite_guidance(&plan, &missing));
        }
        self.set_pending_install(Some(provider));
        self.set_panel_status(String::new());
    }

    fn install_blocker(&self) -> Option<&'static str> {
        if self.host.is_none() {
            return Some(PREVIEW_BLOCKER);
        }
        self.live
            .as_ref()
            .filter(|live| live.mission.is_some())
            .map(|_| "Finish the running mission before installing.")
    }

    fn run_install(&mut self, provider: Provider) {
        self.set_pending_install(None);
        if let Some(blocker) = self.install_blocker() {
            return self.set_panel_status(blocker.into());
        }
        let plan = install::plan(provider, Platform::current());
        let host = self.host.as_mut().expect("checked by install_blocker");
        let outcome = match host.suspend() {
            Ok(()) => host.install(&plan),
            Err(error) => Err(error),
        };
        let restored = host.restore();
        self.full_redraw = true;
        let mut summary = match outcome {
            Ok(()) => self.located_after_install(provider),
            Err(error) => format!(
                "{} install failed: {error}. Nothing else changed.",
                provider_title(provider)
            ),
        };
        self.recheck_clis();
        if let Err(error) = restored {
            summary.push_str(&format!(" · Terminal restore failed: {error}"));
        }
        self.set_panel_status(summary);
    }

    fn located_after_install(&self, provider: Provider) -> String {
        let title = provider_title(provider);
        let binary = provider.binary();
        let host = self.host.as_deref().expect("checked by install_blocker");
        match locate(host, provider) {
            Some(Found::OnPath) => format!("{title} installed: {binary} is on PATH."),
            Some(Found::At(path)) => format!(
                "{title} installed. Using {} for {binary} in this session.",
                path.display()
            ),
            None => format!(
                "{title} installer finished, but {binary} was not found. Open a new terminal, then press R."
            ),
        }
    }

    fn recheck_clis(&mut self) {
        let host = self.host.as_deref();
        match &mut self.panel {
            Some(Panel::Settings(view)) => view.clis = Clis::located(host),
            Some(Panel::Connect(form)) => form.recheck(host),
            None => {}
        }
    }

    fn toggle_provider(&mut self, provider: Provider) {
        let Some(Panel::Connect(form)) = &mut self.panel else {
            return;
        };
        let slot = provider_slot(provider);
        if form.clis.found(provider).is_some() {
            form.chosen[slot] = !form.chosen[slot];
        } else {
            form.status = format!(
                "{} is not installed. Choose Install, or install it yourself and press R.",
                provider.binary()
            );
        }
    }

    fn recheck(&mut self) {
        let host = self.host.as_deref();
        if let Some(Panel::Connect(form)) = &mut self.panel {
            form.recheck(host);
            form.status = if form.missing() {
                "Rechecked PATH.".into()
            } else {
                "Rechecked PATH: Claude and Codex found.".into()
            };
        }
    }

    fn leave_connect(&mut self) {
        let Some(Panel::Connect(form)) = &self.panel else {
            return;
        };
        match form.origin {
            Origin::Onboarding => {
                self.panel = None;
                self.notice = NO_PROVIDER_HINT.into();
            }
            Origin::Settings => self.open_settings(),
            Origin::Direct => self.panel = None,
        }
    }

    fn connect_blocker(&self) -> Option<&'static str> {
        if self.host.is_none() {
            return Some(PREVIEW_BLOCKER);
        }
        match &self.live {
            None => Some(PREVIEW_BLOCKER),
            Some(live) if live.profile_root.is_none() => {
                Some("No profile directory. Set an absolute SUB_AUTH_DIR.")
            }
            Some(live) if live.mission.is_some() => {
                Some("Finish the running mission before connecting.")
            }
            Some(_) => None,
        }
    }

    fn connect_chosen(&mut self) {
        let Some(Panel::Connect(form)) = &self.panel else {
            return;
        };
        let name = form.name.trim().to_owned();
        let chosen = form.chosen_providers();
        if let Err(rule) = auth::validate_name(&name) {
            return self.set_panel_status(rule.into());
        }
        if chosen.is_empty() {
            return self.set_panel_status("Pick Claude, Codex or both.".into());
        }
        if let Some(blocker) = self.connect_blocker() {
            return self.set_panel_status(blocker.into());
        }
        let root = self
            .live
            .as_ref()
            .and_then(|live| live.profile_root.clone())
            .expect("checked by connect_blocker");
        let profiles: Vec<Profile> = chosen
            .into_iter()
            .filter_map(|provider| Profile::new(root.clone(), provider, &name).ok())
            .collect();
        let (connected, summary) = self.sign_in_all(&profiles);
        let origin = form_origin(&self.panel);
        if connected && origin == Some(Origin::Onboarding) {
            self.panel = None;
            self.notice = summary;
        } else if connected && origin == Some(Origin::Settings) {
            self.open_settings();
            self.set_panel_status(summary);
        } else if connected {
            self.panel = None;
            self.notice = summary;
        } else {
            self.set_panel_status(summary);
        }
    }

    fn reconnect(&mut self, index: usize) {
        if let Some(blocker) = self.connect_blocker() {
            return self.set_panel_status(blocker.into());
        }
        let Some(profile) = self
            .live
            .as_ref()
            .and_then(|live| live.profiles.get(index))
            .cloned()
        else {
            return;
        };
        let (_, summary) = self.sign_in_all(&[profile]);
        self.set_panel_status(summary);
    }

    fn sign_in_all(&mut self, profiles: &[Profile]) -> (bool, String) {
        let host = self.host.as_mut().expect("checked by connect_blocker");
        let outcomes: Vec<io::Result<()>> = match host.suspend() {
            Ok(()) => profiles
                .iter()
                .map(|profile| host.sign_in(profile))
                .collect(),
            Err(error) => profiles
                .iter()
                .map(|_| Err(io::Error::new(error.kind(), error.to_string())))
                .collect(),
        };
        let restored = host.restore();
        self.full_redraw = true;
        self.reload_profiles();
        let mut connected = false;
        let mut parts: Vec<String> = profiles
            .iter()
            .zip(outcomes)
            .map(|(profile, outcome)| {
                let label = profile.to_string();
                match outcome {
                    Err(error) => format!("{label}: sign-in failed: {error}"),
                    Ok(()) if self.is_signed_in(&label) => {
                        connected = true;
                        format!("{label} signed in")
                    }
                    Ok(()) => format!("{label}: sign-in did not complete"),
                }
            })
            .collect();
        if let Err(error) = restored {
            parts.push(format!("Terminal restore failed: {error}"));
        }
        (connected, parts.join(" · "))
    }

    fn is_signed_in(&self, label: &str) -> bool {
        self.live.as_ref().is_some_and(|live| {
            live.profiles
                .iter()
                .zip(&live.signed_in)
                .any(|(profile, signed_in)| *signed_in && profile.to_string() == label)
        })
    }

    fn confirm_sign_out(&mut self) {
        let Some(Panel::Settings(view)) = &mut self.panel else {
            return;
        };
        let Some(index) = view.confirm.take() else {
            return;
        };
        view.focus = 0;
        if let Some(blocker) = self.connect_blocker() {
            return self.set_panel_status(blocker.into());
        }
        let Some(profile) = self
            .live
            .as_ref()
            .and_then(|live| live.profiles.get(index))
            .cloned()
        else {
            return;
        };
        let outcome = self
            .host
            .as_mut()
            .expect("checked by connect_blocker")
            .sign_out(&profile);
        self.reload_profiles();
        self.set_panel_status(match outcome {
            Ok(()) => format!("Signed out {profile}. The profile folder is kept."),
            Err(error) => format!("{profile}: sign-out failed: {error}"),
        });
    }

    fn toggle_update_check(&mut self) {
        self.settings.check_for_updates = !self.settings.check_for_updates;
        let state = on_off(self.settings.check_for_updates);
        self.save_setting(format!("Check for updates on launch: {state}"));
    }

    fn save_setting(&mut self, change: String) {
        let status = match &self.launch.settings_path {
            Some(path) => match self.settings.save(path) {
                Ok(()) => format!("{change} (saved)."),
                Err(error) => format!("Setting not saved: {error}"),
            },
            None => format!("{change} (no config directory; not saved)."),
        };
        self.set_panel_status(status);
    }

    fn profile_labels(&self) -> Vec<String> {
        self.live.as_ref().map_or_else(Vec::new, |live| {
            live.profiles.iter().map(ToString::to_string).collect()
        })
    }

    fn slot_label(&self, slot: Option<&str>) -> String {
        settings::slot_label(slot, &self.profile_labels())
    }

    fn limit_rows(&self) -> Vec<Row> {
        vec![
            vec![
                text("Resume after limits  ", Tone::Plain),
                button(
                    on_off(self.settings.auto_resume_on_reset),
                    Action::AutoResume,
                ),
            ],
            vec![
                text("Snooze limited       ", Tone::Plain),
                button(
                    on_off(self.settings.snooze_limited_missions),
                    Action::SnoozeMissions,
                ),
            ],
            vec![
                text("Fallback profile     ", Tone::Plain),
                button(
                    self.slot_label(self.settings.fallback_profile.as_deref()),
                    Action::FallbackProfile,
                ),
            ],
            vec![
                text("Text-gen profile     ", Tone::Plain),
                button(
                    self.slot_label(self.settings.text_gen_profile.as_deref()),
                    Action::TextGenProfile,
                ),
            ],
        ]
    }

    fn settings_rows(&self, view: &SettingsView, width: usize) -> Vec<Row> {
        let mut rows: Vec<Row> = vec![vec![text("PROVIDERS", Tone::Heading)]];
        if let Some(provider) = view.installing {
            rows.extend(install_prompt(provider, width));
            return rows;
        }
        let profiles = self
            .live
            .as_ref()
            .map(|live| (&live.profiles, &live.signed_in));
        match profiles {
            Some((profiles, _)) if profiles.is_empty() => {
                rows.push(vec![text("No profiles yet.", Tone::Muted)])
            }
            None => rows.push(vec![text("Preview: profiles are not loaded.", Tone::Muted)]),
            Some((profiles, signed_in)) => {
                for (index, profile) in profiles.iter().enumerate() {
                    let active = signed_in.get(index).copied().unwrap_or(false);
                    rows.push(vec![
                        text(
                            format!(
                                "{:<width$}",
                                truncated(&profile.to_string(), PROFILE_LABEL_WIDTH - 1),
                                width = PROFILE_LABEL_WIDTH
                            ),
                            Tone::Plain,
                        ),
                        if active {
                            text("● signed in   ", Tone::Good)
                        } else {
                            text("○ signed out  ", Tone::Warn)
                        },
                        button("Reconnect", Action::Reconnect(index)),
                        text(" ", Tone::Plain),
                        button("Sign out", Action::SignOut(index)),
                    ]);
                }
            }
        }
        for provider in view.clis.missing() {
            rows.push(vec![
                text(
                    format!(
                        "{:<width$}",
                        format!("{} CLI", provider.binary()),
                        width = PROFILE_LABEL_WIDTH
                    ),
                    Tone::Plain,
                ),
                text("✗ not found   ", Tone::Warn),
                button("Install", Action::Install(provider)),
            ]);
        }
        if let Some(index) = view.confirm {
            let label = self
                .live
                .as_ref()
                .and_then(|live| live.profiles.get(index))
                .map_or_else(String::new, ToString::to_string);
            rows.push(vec![text(
                format!("Sign out {label}? The profile folder is kept."),
                Tone::Warn,
            )]);
            rows.push(vec![
                button("Y Confirm sign out", Action::ConfirmSignOut),
                text("  ", Tone::Plain),
                button("N Cancel", Action::CancelSignOut),
            ]);
        }
        rows.push(vec![button("+ Connect new profile", Action::ConnectNew)]);
        rows.push(vec![]);
        rows.push(vec![
            text("Commander profile    ", Tone::Plain),
            button(self.commander_profile_label(), Action::CommanderProfile),
        ]);
        let mut updates = vec![
            text("Check for updates    ", Tone::Plain),
            button(
                format!("{} on launch", on_off(self.settings.check_for_updates)),
                Action::UpdateCheck,
            ),
        ];
        if settings::forced_off(self.launch.no_update_check.as_ref()) {
            updates.push(text(
                format!("  off: {}=1", settings::NO_UPDATE_CHECK),
                Tone::Warn,
            ));
        }
        rows.push(updates);
        rows.extend(self.limit_rows());
        rows.push(vec![]);
        rows.push(vec![text(
            format!("Version   {}", update::CURRENT),
            Tone::Muted,
        )]);
        rows.push(vec![text(
            format!("Profiles  {}", self.profile_root_label()),
            Tone::Muted,
        )]);
        rows.push(vec![text(
            format!(
                "Theme     {}",
                path_label(self.launch.theme_path.as_deref())
            ),
            Tone::Muted,
        )]);
        rows.push(vec![text(
            format!(
                "Settings  {}",
                path_label(self.launch.settings_path.as_deref())
            ),
            Tone::Muted,
        )]);
        rows
    }

    fn commander_profile_label(&self) -> String {
        match &self.live {
            None => "Preview".into(),
            Some(live) => live
                .preferred
                .and_then(|index| live.profiles.get(index))
                .map_or_else(|| "Auto".into(), ToString::to_string),
        }
    }

    fn profile_root_label(&self) -> String {
        self.live
            .as_ref()
            .and_then(|live| live.profile_root.as_deref())
            .map_or_else(|| "unavailable".into(), |root| root.display().to_string())
    }

    pub(crate) fn draw_panel(&mut self, f: &mut Frame, area: Rect) {
        self.panel_hits.clear();
        let Some(panel) = &self.panel else { return };
        let width = area.width.saturating_sub(2).min(PANEL_MAX_WIDTH);
        let inner_width = usize::from(width.saturating_sub(4));
        let (title, rows, status, footer) = match panel {
            Panel::Settings(view) => (
                " Settings ",
                self.settings_rows(view, inner_width),
                view.status.clone(),
                "↑↓ Tab move · Enter select · Esc close",
            ),
            Panel::Connect(form) => (
                " Connect providers ",
                form.rows(self.editing_name(), inner_width),
                form.status.clone(),
                "↑↓ Tab move · Enter select · R recheck · Esc leave",
            ),
        };
        let footer = if self.pending_install().is_some() {
            "Y install · N or Esc cancel"
        } else {
            footer
        };
        let status_rows = wrapped(&status, inner_width, STATUS_MAX_ROWS);
        let content_rows = rows.len() + status_rows.len() + 2;
        let height = (content_rows as u16 + 2).min(area.height.saturating_sub(1));
        let modal = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + 1 + (area.height.saturating_sub(1) - height) / 2,
            width,
            height,
        );
        let theme = self.theme.clone();
        f.render_widget(Clear, modal);
        f.render_widget(
            Block::bordered()
                .title(title)
                .style(Style::default().bg(color(&theme.panel)))
                .border_style(Style::default().fg(color(&theme.accent))),
            modal,
        );
        let inner = Rect::new(
            modal.x + 2,
            modal.y + 1,
            modal.width.saturating_sub(4),
            modal.height.saturating_sub(2),
        );
        let mut all_rows = rows;
        all_rows.push(vec![]);
        all_rows.extend(
            status_rows
                .into_iter()
                .map(|line| vec![text(line, Tone::Warn)]),
        );
        let body = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        );
        let allowed = self.panel_actions();
        let painter = Painter {
            focused: self.focused_action(),
            allowed: &allowed,
            theme: &theme,
        };
        let mut hits = Vec::new();
        for (offset, row) in all_rows.iter().enumerate() {
            let y = body.y + offset as u16;
            if y >= body.bottom() {
                break;
            }
            hits.extend(painter.row(f, row, Rect::new(body.x, y, body.width, 1)));
        }
        let footer_area = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
        painter.row(f, &vec![text(footer, Tone::Muted)], footer_area);
        self.panel_hits = hits;
    }
}

fn prerequisite_guidance(plan: &Plan, missing: &[&str]) -> String {
    format!(
        "Installing {} needs {} on PATH, which is missing. Nothing ran. Install it, or follow {}",
        provider_title(plan.provider),
        missing.join(" and "),
        plan.docs
    )
}

fn form_origin(panel: &Option<Panel>) -> Option<Origin> {
    match panel {
        Some(Panel::Connect(form)) => Some(form.origin),
        _ => None,
    }
}

fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "On"
    } else {
        "Off"
    }
}

fn path_label(path: Option<&Path>) -> String {
    path.map_or_else(
        || "built-in (no config directory)".into(),
        |path| path.display().to_string(),
    )
}

struct Painter<'a> {
    focused: Option<Action>,
    allowed: &'a [Action],
    theme: &'a crate::Theme,
}

impl Painter<'_> {
    fn row(&self, f: &mut Frame, row: &Row, line: Rect) -> Vec<(Action, Rect)> {
        let mut hits = Vec::new();
        let mut x = line.x;
        for segment in row {
            if x >= line.right() {
                break;
            }
            let width = (Span::raw(segment.text.as_str()).width() as u16).min(line.right() - x);
            let area = Rect::new(x, line.y, width, 1);
            f.render_widget(
                Paragraph::new(segment.text.as_str()).style(self.style(segment)),
                area,
            );
            if let Some(action) = segment.action {
                hits.push((action, area));
            }
            x += width;
        }
        hits
    }

    fn style(&self, segment: &Segment) -> Style {
        let theme = self.theme;
        if let Some(action) = segment.action {
            let base = Style::default()
                .fg(color(&theme.foreground))
                .bg(color("28323d"));
            return if Some(action) == self.focused {
                base.fg(color("242b32")).bg(color(&theme.ivory)).bold()
            } else if self.allowed.contains(&action) {
                base
            } else {
                base.fg(color(&theme.muted))
            };
        }
        match segment.tone {
            Tone::Plain => Style::default().fg(color(&theme.foreground)),
            Tone::Muted => Style::default().fg(color(&theme.muted)),
            Tone::Heading => Style::default().fg(color(&theme.ivory)).bold(),
            Tone::Good => Style::default().fg(color(&theme.accent)),
            Tone::Warn => Style::default().fg(color(&theme.amber)),
        }
    }
}

#[cfg(test)]
mod tests;
