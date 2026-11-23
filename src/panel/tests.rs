use super::*;
use crate::settings::LaunchConfig;
#[cfg(unix)]
use crate::Live;
use crossterm::event::KeyModifiers;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};
use std::{cell::RefCell, collections::BTreeSet, ffi::OsString, fs, path::PathBuf, rc::Rc};

const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];
const COMMAND_WIDTH: usize = PANEL_MAX_WIDTH as usize - 6;

#[derive(Default)]
struct Script {
    calls: Vec<String>,
    checks: Vec<String>,
    signed: BTreeSet<String>,
    installed: [bool; 2],
    refuse_sign_in: bool,
    adopted: [Option<PathBuf>; 2],
    absent_programs: Vec<&'static str>,
    home: Option<PathBuf>,
    installs: Option<Landing>,
    install_failure: Option<&'static str>,
}

#[derive(Clone, Copy)]
enum Landing {
    OnPath,
    DocumentedDirectory,
    Nowhere,
}

#[derive(Clone)]
struct FakeHost(Rc<RefCell<Script>>);

fn documented_in(home: &Path, provider: Provider) -> Option<PathBuf> {
    install::documented_executable(provider, Platform::current(), |name| {
        ["HOME", "USERPROFILE", "LOCALAPPDATA"]
            .contains(&name)
            .then(|| home.as_os_str().to_owned())
    })
}

impl Host for FakeHost {
    fn found(&self, provider: Provider) -> Option<Found> {
        let mut script = self.0.borrow_mut();
        script.checks.push(format!("found {}", provider.binary()));
        let slot = provider_slot(provider);
        if let Some(path) = script.adopted[slot].clone() {
            return Some(Found::At(path));
        }
        script.installed[slot].then_some(Found::OnPath)
    }

    fn documented_executable(&self, provider: Provider) -> Option<PathBuf> {
        documented_in(self.0.borrow().home.as_deref()?, provider)
    }

    fn adopt(&self, provider: Provider, executable: &Path) {
        let mut script = self.0.borrow_mut();
        script.calls.push(format!(
            "adopt {} {}",
            provider.binary(),
            executable.display()
        ));
        script.adopted[provider_slot(provider)] = Some(executable.into());
    }

    fn has_program(&self, name: &str) -> bool {
        !self.0.borrow().absent_programs.contains(&name)
    }

    fn install(&mut self, plan: &Plan) -> io::Result<()> {
        let mut script = self.0.borrow_mut();
        script.calls.push(format!("install {}", plan.command));
        if let Some(reason) = script.install_failure {
            return Err(io::Error::other(reason));
        }
        match script.installs {
            Some(Landing::OnPath) => script.installed[provider_slot(plan.provider)] = true,
            Some(Landing::DocumentedDirectory) => {
                let executable =
                    documented_in(script.home.as_deref().unwrap(), plan.provider).unwrap();
                fs::create_dir_all(executable.parent().unwrap())?;
                fs::write(executable, "")?;
            }
            Some(Landing::Nowhere) | None => {}
        }
        Ok(())
    }

    fn profiles(&self, root: &Path) -> io::Result<Vec<Profile>> {
        Ok(auth::profiles(root)?
            .into_iter()
            .map(Profile::without_provider)
            .collect())
    }

    fn signed_in(&self, profile: &Profile) -> bool {
        self.0.borrow().signed.contains(&profile.to_string())
    }

    fn suspend(&mut self) -> io::Result<()> {
        self.0.borrow_mut().calls.push("suspend".into());
        Ok(())
    }

    fn sign_in(&mut self, profile: &Profile) -> io::Result<()> {
        let mut script = self.0.borrow_mut();
        script.calls.push(format!("sign_in {profile}"));
        if script.refuse_sign_in {
            return Err(io::Error::other("claude exited with exit status: 1"));
        }
        profile.prepare()?;
        script.signed.insert(profile.to_string());
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        self.0.borrow_mut().calls.push("restore".into());
        Ok(())
    }

    fn sign_out(&mut self, profile: &Profile) -> io::Result<()> {
        let mut script = self.0.borrow_mut();
        script.calls.push(format!("sign_out {profile}"));
        script.signed.remove(&profile.to_string());
        Ok(())
    }
}

fn fake(installed: [bool; 2], signed: &[&str]) -> FakeHost {
    FakeHost(Rc::new(RefCell::new(Script {
        installed,
        signed: signed.iter().map(|label| (*label).to_owned()).collect(),
        ..Script::default()
    })))
}

struct Sandbox(PathBuf);

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ac-panel-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("work")).unwrap();
        Self(root)
    }

    #[cfg(unix)]
    fn profiles(&self) -> PathBuf {
        self.0.join("profiles")
    }

    fn launch(&self, no_update_check: Option<&str>) -> LaunchConfig {
        LaunchConfig {
            settings_path: Some(self.0.join("config/settings.toml")),
            theme_path: Some(self.0.join("config/theme.toml")),
            no_update_check: no_update_check.map(OsString::from),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn existing(sandbox: &Sandbox, provider: Provider, name: &str) {
    Profile::new(sandbox.profiles(), provider, name)
        .unwrap()
        .prepare()
        .unwrap();
}

#[cfg(unix)]
fn launched(sandbox: &Sandbox, host: &FakeHost) -> App {
    let profiles = host.profiles(&sandbox.profiles()).unwrap_or_default();
    let live = Live::new(sandbox.0.join("work"), profiles, Some(sandbox.profiles()));
    let mut app = App::from_live(live);
    app.attach(Box::new(host.clone()), sandbox.launch(None));
    app
}

fn preview(host: &FakeHost, launch: LaunchConfig) -> App {
    let mut app = App::default();
    app.attach(Box::new(host.clone()), launch);
    app
}

fn render(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
    terminal.backend().buffer().clone()
}

fn screen(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| row(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn row(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

fn press(app: &mut App, code: KeyCode) {
    app.key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn hit(app: &App, action: Action) -> Rect {
    app.panel_hits
        .iter()
        .find(|(candidate, _)| *candidate == action)
        .map(|(_, area)| *area)
        .unwrap_or_else(|| panic!("{action:?} is not on screen"))
}

fn click(app: &mut App, area: Rect) {
    app.click(area.x + area.width / 2, area.y);
}

fn assert_inside(app: &App, width: u16, height: u16) {
    for (action, area) in &app.panel_hits {
        assert!(
            area.width > 0 && area.right() <= width && area.bottom() <= height,
            "{action:?} outside {width}x{height}"
        );
    }
}

fn origin(app: &App) -> Option<Origin> {
    form_origin(&app.panel)
}

#[cfg(unix)]
#[test]
fn onboarding_opens_only_when_no_profile_is_signed_in() {
    for (width, height) in SIZES {
        let sandbox = Sandbox::new(&format!("onboarding-{width}"));
        existing(&sandbox, Provider::Claude, "work");
        let mut signed_out = launched(&sandbox, &fake([true, false], &[]));
        assert_eq!(origin(&signed_out), Some(Origin::Onboarding));
        let text = screen(&render(&mut signed_out, width, height));
        assert!(text.contains("Connect providers"), "{text}");
        assert!(text.contains("✓ claude on PATH"), "{text}");
        assert_inside(&signed_out, width, height);
        if let Some(Panel::Connect(form)) = &signed_out.panel {
            assert_eq!(form.name, DEFAULT_PROFILE_NAME);
            assert_eq!(form.chosen_providers(), [Provider::Claude]);
        }

        let mut ready = launched(&sandbox, &fake([true, false], &["claude/work"]));
        assert!(ready.panel.is_none());
        let text = screen(&render(&mut ready, width, height));
        assert!(!text.contains("Connect providers"), "{text}");
        assert!(text.contains("Commander"));
    }
}

#[cfg(unix)]
#[test]
fn skip_leaves_the_app_usable_with_a_one_line_hint() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let sandbox = Sandbox::new(&format!("skip-{width}"));
        let mut app = launched(&sandbox, &fake([true, true], &[]));
        render(&mut app, width, height);
        if index == 0 {
            press(&mut app, KeyCode::Esc);
        } else {
            let skip = hit(&app, Action::Leave);
            click(&mut app, skip);
        }
        assert!(app.panel.is_none());
        let buffer = render(&mut app, width, height);
        assert!(row(&buffer, height - 2).contains(NO_PROVIDER_HINT));
        app.notice.clear();
        let buffer = render(&mut app, width, height);
        assert!(row(&buffer, height - 2).contains(NO_PROVIDER_HINT));
        app.input = "Hello".into();
        app.submit();
        assert!(app.notice.contains(NO_PROVIDER_HINT), "{}", app.notice);
    }
}

#[cfg(unix)]
#[test]
fn connect_suspends_signs_in_restores_and_shows_the_profile_without_restart() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let sandbox = Sandbox::new(&format!("connect-{width}"));
        let host = fake([true, false], &[]);
        let mut app = launched(&sandbox, &host);
        render(&mut app, width, height);
        if index == 0 {
            press(&mut app, KeyCode::Tab);
            press(&mut app, KeyCode::Tab);
            press(&mut app, KeyCode::Tab);
            assert!(app.editing_name());
            press(&mut app, KeyCode::Backspace);
            press(&mut app, KeyCode::Char('x'));
            assert!(matches!(&app.panel, Some(Panel::Connect(form)) if form.name == "maix"));
            press(&mut app, KeyCode::Backspace);
            press(&mut app, KeyCode::Char('n'));
            press(&mut app, KeyCode::Enter);
        } else {
            let connect = hit(&app, Action::Connect);
            click(&mut app, connect);
        }
        assert_eq!(
            host.0.borrow().calls,
            ["suspend", "sign_in claude/main", "restore"]
        );
        assert!(app.full_redraw);
        assert!(app.panel.is_none());
        assert_eq!(app.notice, "claude/main signed in");
        let live = app.live.as_ref().unwrap();
        assert_eq!(live.profiles.len(), 1);
        assert_eq!(live.profiles[0].to_string(), "claude/main");
        assert_eq!(live.signed_in, [true]);
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("claude · main"), "{text}");
        assert!(!text.contains(NO_PROVIDER_HINT));
        press(&mut app, KeyCode::F(3));
        let text = screen(&render(&mut app, width, height));
        assert!(
            text.contains("claude/main") && text.contains("● signed in"),
            "{text}"
        );
    }
}

#[cfg(unix)]
#[test]
fn failed_sign_in_keeps_the_form_open_with_the_reason() {
    for (width, height) in SIZES {
        let sandbox = Sandbox::new(&format!("refused-{width}"));
        let host = fake([true, false], &[]);
        host.0.borrow_mut().refuse_sign_in = true;
        let mut app = launched(&sandbox, &host);
        render(&mut app, width, height);
        let connect = hit(&app, Action::Connect);
        click(&mut app, connect);
        assert_eq!(
            host.0.borrow().calls,
            ["suspend", "sign_in claude/main", "restore"]
        );
        assert_eq!(origin(&app), Some(Origin::Onboarding));
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("claude/main: sign-in failed"), "{text}");
    }
}

#[test]
fn invalid_profile_name_is_refused_before_any_sign_in() {
    let host = fake([true, true], &[]);
    let mut app = preview(&host, LaunchConfig::default());
    app.open_connect(Origin::Direct);
    if let Some(Panel::Connect(form)) = &mut app.panel {
        form.name = "Bad Name".into();
    }
    app.activate(Action::Connect);
    assert!(host.0.borrow().calls.is_empty());
    assert!(matches!(&app.panel, Some(Panel::Connect(form)) if form.status == auth::NAME_RULE));
}

#[test]
fn missing_cli_shows_the_recheck_state_until_it_is_found() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let host = fake([false, false], &[]);
        let mut app = preview(&host, LaunchConfig::default());
        app.open_connect(Origin::Direct);
        let text = screen(&render(&mut app, width, height));
        for expected in ["✗ claude not found", "✗ codex not found", "R Recheck"] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
        assert_inside(&app, width, height);
        press(&mut app, KeyCode::Enter);
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("claude is not installed"), "{text}");
        host.0.borrow_mut().installed = [true, false];
        if index == 0 {
            press(&mut app, KeyCode::Char('R'));
        } else {
            let recheck = hit(&app, Action::Recheck);
            click(&mut app, recheck);
        }
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("✓ claude on PATH"), "{text}");
        assert!(text.contains("✗ codex not found"), "{text}");
        host.0.borrow_mut().installed = [true, true];
        press(&mut app, KeyCode::Char('r'));
        let text = screen(&render(&mut app, width, height));
        assert!(!text.contains("R Recheck"), "{text}");
    }
}

#[test]
fn settings_opens_from_the_top_bar_f3_and_slash_command() {
    for (width, height) in SIZES {
        let host = fake([true, true], &[]);
        for opener in ["click", "f3", "slash"] {
            let mut app = preview(&host, LaunchConfig::default());
            let buffer = render(&mut app, width, height);
            let bar = row(&buffer, 0);
            assert!(bar.contains("Radio    |    PRIVATEER"), "{bar}");
            assert!(bar.trim_end().ends_with("Settings"), "{bar}");
            assert!(app.settings_area.right() <= width && app.settings_area.y == 0);
            match opener {
                "click" => {
                    let item = app.settings_area;
                    click(&mut app, item);
                }
                "f3" => press(&mut app, KeyCode::F(3)),
                _ => {
                    app.input = "/settings".into();
                    app.submit();
                }
            }
            assert!(matches!(app.panel, Some(Panel::Settings(_))), "{opener}");
            let text = screen(&render(&mut app, width, height));
            for expected in [
                "Settings",
                "PROVIDERS",
                "+ Connect new profile",
                "Commander profile",
                "Check for updates",
                &format!("Version   {}", update::CURRENT),
                "Profiles",
                "Theme",
            ] {
                assert!(text.contains(expected), "{opener} {expected}\n{text}");
            }
            assert_inside(&app, width, height);
            press(&mut app, KeyCode::Esc);
            assert!(app.panel.is_none(), "{opener}");
        }
    }
}

#[test]
fn connect_command_opens_the_connect_flow() {
    for (width, height) in SIZES {
        let host = fake([true, true], &[]);
        let mut app = preview(&host, LaunchConfig::default());
        app.input = "/connect".into();
        app.submit();
        assert_eq!(origin(&app), Some(Origin::Direct));
        assert!(app.input.is_empty());
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("Connect providers"), "{text}");
        assert!(text.contains(" Cancel "), "{text}");
        press(&mut app, KeyCode::Esc);
        assert!(app.panel.is_none());

        app.open_settings();
        render(&mut app, width, height);
        let connect_new = hit(&app, Action::ConnectNew);
        click(&mut app, connect_new);
        assert_eq!(origin(&app), Some(Origin::Settings));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.panel, Some(Panel::Settings(_))));
    }
}

#[cfg(unix)]
#[test]
fn sign_out_needs_confirmation_and_keeps_the_profile_folder() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let sandbox = Sandbox::new(&format!("sign-out-{width}"));
        existing(&sandbox, Provider::Codex, "work");
        let host = fake([true, true], &["codex/work"]);
        let mut app = launched(&sandbox, &host);
        press(&mut app, KeyCode::F(3));
        render(&mut app, width, height);
        let ask = |app: &mut App| {
            if index == 0 {
                press(app, KeyCode::Down);
                press(app, KeyCode::Enter);
            } else {
                let sign_out = hit(app, Action::SignOut(0));
                click(app, sign_out);
            }
        };
        ask(&mut app);
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("Sign out codex/work?"), "{text}");
        assert!(host.0.borrow().calls.is_empty());
        press(&mut app, KeyCode::Esc);
        assert!(host.0.borrow().calls.is_empty());
        assert!(matches!(app.panel, Some(Panel::Settings(_))));

        render(&mut app, width, height);
        ask(&mut app);
        render(&mut app, width, height);
        if index == 0 {
            press(&mut app, KeyCode::Char('y'));
        } else {
            let confirm = hit(&app, Action::ConfirmSignOut);
            click(&mut app, confirm);
        }
        assert_eq!(host.0.borrow().calls, ["sign_out codex/work"]);
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("○ signed out"), "{text}");
        assert!(text.contains("Signed out codex/work"), "{text}");
        assert!(sandbox.profiles().join("codex/work").is_dir());
    }
}

#[test]
fn update_toggle_persists_and_the_environment_variable_overrides_it() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let sandbox = Sandbox::new(&format!("updates-{width}"));
        let host = fake([true, true], &[]);
        let mut app = preview(&host, sandbox.launch(None));
        assert!(app.launch_update_check());
        app.open_settings();
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("On on launch"), "{text}");
        if index == 0 {
            while app.focused_action() != Some(Action::UpdateCheck) {
                press(&mut app, KeyCode::Down);
            }
            press(&mut app, KeyCode::Enter);
        } else {
            let toggle = hit(&app, Action::UpdateCheck);
            click(&mut app, toggle);
        }
        let path = sandbox.0.join("config/settings.toml");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "check_for_updates = false\n"
        );
        assert!(!app.launch_update_check());
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("Off on launch"), "{text}");

        let reopened = preview(&host, sandbox.launch(None));
        assert!(!reopened.settings.check_for_updates);
        let mut forced = preview(&host, sandbox.launch(Some("1")));
        forced.settings.check_for_updates = true;
        assert!(!forced.launch_update_check());
        forced.open_settings();
        let text = screen(&render(&mut forced, width, height));
        assert!(text.contains("AGENT_COMMAND_NO_UPDATE_CHECK=1"), "{text}");
    }
}

fn plan_for(provider: Provider) -> Plan {
    install::plan(provider, Platform::current())
}

fn status(app: &App) -> String {
    match &app.panel {
        Some(Panel::Settings(view)) => view.status.clone(),
        Some(Panel::Connect(form)) => form.status.clone(),
        None => app.notice.clone(),
    }
}

fn has_hit(app: &App, action: Action) -> bool {
    app.panel_hits
        .iter()
        .any(|(candidate, _)| *candidate == action)
}

fn open(app: &mut App, settings: bool) {
    if settings {
        app.open_settings();
    } else {
        app.open_connect(Origin::Direct);
    }
}

fn choose_install(app: &mut App, provider: Provider, by_keyboard: bool, width: u16, height: u16) {
    render(app, width, height);
    if by_keyboard {
        while app.focused_action() != Some(Action::Install(provider)) {
            press(app, KeyCode::Tab);
        }
        press(app, KeyCode::Enter);
    } else {
        let install = hit(app, Action::Install(provider));
        click(app, install);
    }
}

fn confirm_install(app: &mut App, by_keyboard: bool, width: u16, height: u16) {
    render(app, width, height);
    if by_keyboard {
        press(app, KeyCode::Char('y'));
    } else {
        let confirm = hit(app, Action::ConfirmInstall);
        click(app, confirm);
    }
}

#[test]
fn install_appears_only_for_a_missing_cli_in_connect_and_settings() {
    for (width, height) in SIZES {
        for settings in [false, true] {
            let host = fake([true, false], &[]);
            let mut app = preview(&host, LaunchConfig::default());
            open(&mut app, settings);
            let text = screen(&render(&mut app, width, height));
            assert!(has_hit(&app, Action::Install(Provider::Codex)), "{text}");
            assert!(!has_hit(&app, Action::Install(Provider::Claude)), "{text}");
            assert!(text.contains(" Install "), "{text}");
            assert_inside(&app, width, height);

            let ready = fake([true, true], &[]);
            let mut app = preview(&ready, LaunchConfig::default());
            open(&mut app, settings);
            let text = screen(&render(&mut app, width, height));
            assert!(!text.contains(" Install "), "{text}");
            assert!(!app
                .panel_hits
                .iter()
                .any(|(action, _)| matches!(action, Action::Install(_))));
        }
    }
}

#[test]
fn install_shows_the_platform_command_and_n_or_esc_runs_nothing() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        for settings in [false, true] {
            let host = fake([false, false], &[]);
            let mut app = preview(&host, LaunchConfig::default());
            open(&mut app, settings);
            choose_install(&mut app, Provider::Claude, index == 0, width, height);
            let plan = plan_for(Provider::Claude);
            let text = screen(&render(&mut app, width, height));
            for expected in [
                "Install Claude Code with its official installer:",
                plan.docs,
                "Y Install",
                "N Cancel",
            ] {
                assert!(text.contains(expected), "{expected}\n{text}");
            }
            for line in wrapped(&plan.command, COMMAND_WIDTH, 3) {
                assert!(text.contains(&format!("  {line}")), "{line}\n{text}");
            }
            assert_inside(&app, width, height);
            press(&mut app, KeyCode::Char('n'));
            assert!(host.0.borrow().calls.is_empty());
            assert_eq!(status(&app), "Install cancelled. Nothing ran.");

            choose_install(&mut app, Provider::Codex, index == 1, width, height);
            assert_eq!(app.pending_install(), Some(Provider::Codex));
            press(&mut app, KeyCode::Esc);
            assert!(host.0.borrow().calls.is_empty());
            assert!(app.panel.is_some());
            assert_eq!(app.pending_install(), None);
        }
    }
}

#[test]
fn yes_suspends_runs_the_platform_command_restores_and_rechecks() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        for settings in [false, true] {
            let host = fake([false, true], &[]);
            host.0.borrow_mut().installs = Some(Landing::OnPath);
            let mut app = preview(&host, LaunchConfig::default());
            open(&mut app, settings);
            choose_install(&mut app, Provider::Claude, index == 0, width, height);
            host.0.borrow_mut().checks.clear();
            confirm_install(&mut app, index == 0, width, height);
            let install = format!("install {}", plan_for(Provider::Claude).command);
            assert_eq!(
                host.0.borrow().calls,
                ["suspend", install.as_str(), "restore"]
            );
            assert!(host.0.borrow().checks.contains(&"found claude".to_owned()));
            assert!(app.full_redraw);
            assert_eq!(status(&app), "Claude Code installed: claude is on PATH.");
            let text = screen(&render(&mut app, width, height));
            assert!(!has_hit(&app, Action::Install(Provider::Claude)), "{text}");
            if !settings {
                assert!(text.contains("✓ claude on PATH"), "{text}");
            }
        }
    }
}

#[test]
fn a_cli_found_only_in_the_documented_directory_is_used_for_the_session() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let sandbox = Sandbox::new(&format!("documented-{width}"));
        let home = sandbox.0.join("home");
        let host = fake([false, false], &[]);
        host.0.borrow_mut().home = Some(home.clone());
        host.0.borrow_mut().installs = Some(Landing::DocumentedDirectory);
        let mut app = preview(&host, LaunchConfig::default());
        open(&mut app, false);
        choose_install(&mut app, Provider::Codex, index == 0, width, height);
        confirm_install(&mut app, index == 0, width, height);
        let executable = documented_in(&home, Provider::Codex).unwrap();
        let install = format!("install {}", plan_for(Provider::Codex).command);
        let adopt = format!("adopt codex {}", executable.display());
        assert_eq!(
            host.0.borrow().calls,
            ["suspend", install.as_str(), "restore", adopt.as_str()]
        );
        assert_eq!(
            status(&app),
            format!(
                "Codex CLI installed. Using {} for codex in this session.",
                executable.display()
            )
        );
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("✓ codex at "), "{text}");
        assert!(!has_hit(&app, Action::Install(Provider::Codex)), "{text}");
        assert!(has_hit(&app, Action::Install(Provider::Claude)), "{text}");
    }
}

#[test]
fn an_installed_cli_that_is_still_missing_asks_for_a_new_terminal() {
    for (width, height) in SIZES {
        let host = fake([false, false], &[]);
        host.0.borrow_mut().installs = Some(Landing::Nowhere);
        let mut app = preview(&host, LaunchConfig::default());
        open(&mut app, false);
        choose_install(&mut app, Provider::Claude, false, width, height);
        confirm_install(&mut app, false, width, height);
        assert_eq!(
            status(&app),
            "Claude Code installer finished, but claude was not found. Open a new terminal, then press R."
        );
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("✗ claude not found"), "{text}");
    }
}

#[test]
fn a_missing_installer_prerequisite_shows_guidance_without_running_anything() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        for provider in Provider::ALL {
            let plan = plan_for(provider);
            let host = fake([false, false], &[]);
            host.0.borrow_mut().absent_programs = vec![plan.needs[0]];
            let mut app = preview(&host, LaunchConfig::default());
            open(&mut app, index == 1);
            choose_install(&mut app, provider, index == 0, width, height);
            assert!(host.0.borrow().calls.is_empty());
            assert_eq!(app.pending_install(), None);
            let guidance = status(&app);
            assert!(
                guidance.contains(&format!("needs {} on PATH", plan.needs[0])),
                "{guidance}"
            );
            assert!(guidance.contains("Nothing ran"), "{guidance}");
            assert!(guidance.ends_with(plan.docs), "{guidance}");
            render(&mut app, width, height);
            assert!(!has_hit(&app, Action::ConfirmInstall));
            press(&mut app, KeyCode::Char('y'));
            assert!(host.0.borrow().calls.is_empty());
        }
    }
}

#[test]
fn a_failed_install_shows_the_reason_and_leaves_the_app_usable() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let host = fake([false, false], &[]);
        host.0.borrow_mut().install_failure = Some("installer exited with exit status: 1");
        let mut app = preview(&host, LaunchConfig::default());
        open(&mut app, index == 1);
        choose_install(&mut app, Provider::Codex, true, width, height);
        confirm_install(&mut app, true, width, height);
        let install = format!("install {}", plan_for(Provider::Codex).command);
        assert_eq!(
            host.0.borrow().calls,
            ["suspend", install.as_str(), "restore"]
        );
        assert_eq!(
            status(&app),
            "Codex CLI install failed: installer exited with exit status: 1. Nothing else changed."
        );
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("Codex CLI install failed"), "{text}");
        assert!(has_hit(&app, Action::Install(Provider::Codex)), "{text}");
        press(&mut app, KeyCode::Esc);
        assert!(app.panel.is_none());
        app.input = "/settings".into();
        app.submit();
        assert!(matches!(app.panel, Some(Panel::Settings(_))));
    }
}
