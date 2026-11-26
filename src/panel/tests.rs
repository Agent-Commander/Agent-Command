use super::*;
use crate::settings::LaunchConfig;
#[cfg(unix)]
use crate::{tui::render::RailHit, Live};
use crossterm::event::KeyModifiers;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};
use std::{cell::RefCell, collections::BTreeSet, ffi::OsString, fs, path::PathBuf, rc::Rc};

const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];
#[cfg(unix)]
const RAIL_SIZES: [(u16, u16); 2] = [(100, 30), (120, 40)];
#[cfg(unix)]
const RAIL_WIDTH: u16 = 100;
const COMMAND_WIDTH: usize = PANEL_MAX_WIDTH as usize - 6;

#[derive(Default)]
struct Script {
    calls: Vec<String>,
    checks: Vec<String>,
    signed: BTreeSet<String>,
    installed: [bool; 3],
    refuse_sign_in: bool,
    accounts: [Option<&'static str>; 3],
    adopted: [Option<PathBuf>; 3],
    absent_programs: Vec<&'static str>,
    home: Option<PathBuf>,
    installs: Option<Landing>,
    install_failure: Option<&'static str>,
    update_failure: Option<&'static str>,
    disconnect_failure: Option<&'static str>,
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
        script
            .checks
            .push(format!("found {}", provider.executable()));
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
            provider.executable(),
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

    fn connect(
        &mut self,
        root: &Path,
        provider: Provider,
        name: Option<&str>,
    ) -> io::Result<Profile> {
        let mut script = self.0.borrow_mut();
        script.calls.push(format!(
            "connect {} {}",
            provider.binary(),
            name.unwrap_or("auto")
        ));
        if script.refuse_sign_in {
            return Err(io::Error::other("claude exited with exit code: 130"));
        }
        let account = script.accounts[provider_slot(provider)];
        let chosen = auth::chosen_name(root, provider, name, account).map_err(io::Error::other)?;
        let profile = Profile::new(root.into(), provider, &chosen).map_err(io::Error::other)?;
        profile.prepare()?;
        script.signed.insert(profile.to_string());
        Ok(profile)
    }

    fn restore(&mut self) -> io::Result<()> {
        self.0.borrow_mut().calls.push("restore".into());
        Ok(())
    }

    fn disconnect(&mut self, profile: &Profile) -> io::Result<()> {
        let mut script = self.0.borrow_mut();
        script.calls.push(format!("disconnect {profile}"));
        if let Some(reason) = script.disconnect_failure {
            return Err(io::Error::other(reason));
        }
        script.signed.remove(&profile.to_string());
        auth::disconnect(profile)
    }

    fn update(&mut self, latest: &str) -> io::Result<update::Applied> {
        let mut script = self.0.borrow_mut();
        script.calls.push(format!("update {latest}"));
        match script.update_failure {
            Some(reason) => Err(io::Error::other(reason)),
            None => Ok(update::Applied::Installed),
        }
    }
}

fn fake(installed: [bool; 2], signed: &[&str]) -> FakeHost {
    FakeHost(Rc::new(RefCell::new(Script {
        installed: [installed[0], installed[1], false],
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
            assert!(form.name.is_empty());
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
        assert_eq!(app.notice, NO_PROVIDER_HINT);
        let footer = row(&buffer, height - 1);
        assert!(footer.contains("No provider connected"), "{footer}");
        app.notice.clear();
        let buffer = render(&mut app, width, height);
        let top = row(&buffer, 0);
        assert!(top.contains("Commander [No provider]"), "{top}");
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
            for _ in 0..5 {
                press(&mut app, KeyCode::Tab);
            }
            assert!(app.editing_name());
            for c in "maix".chars() {
                press(&mut app, KeyCode::Char(c));
            }
            assert!(matches!(&app.panel, Some(Panel::Connect(form)) if form.name == "maix"));
            press(&mut app, KeyCode::Backspace);
            press(&mut app, KeyCode::Char('n'));
            press(&mut app, KeyCode::Enter);
        } else {
            let connect = hit(&app, Action::Connect);
            click(&mut app, connect);
        }
        let expected = if index == 0 {
            "connect claude main"
        } else {
            "connect claude auto"
        };
        assert_eq!(host.0.borrow().calls, ["suspend", expected, "restore"]);
        assert!(app.full_redraw);
        assert!(app.panel.is_none());
        assert_eq!(app.notice, "claude/main signed in");
        let live = app.live.as_ref().unwrap();
        assert_eq!(live.profiles.len(), 1);
        assert_eq!(live.profiles[0].to_string(), "claude/main");
        assert_eq!(live.signed_in, [true]);
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("Commander [Claude · main]"), "{text}");
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
            ["suspend", "connect claude auto", "restore"]
        );
        assert_eq!(origin(&app), Some(Origin::Onboarding));
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("claude/(auto): sign-in failed"), "{text}");
        assert!(host.profiles(&sandbox.profiles()).unwrap().is_empty());
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
        host.0.borrow_mut().installed = [true, false, false];
        if index == 0 {
            press(&mut app, KeyCode::Char('R'));
        } else {
            let recheck = hit(&app, Action::Recheck);
            click(&mut app, recheck);
        }
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("✓ claude on PATH"), "{text}");
        assert!(text.contains("✗ codex not found"), "{text}");
        host.0.borrow_mut().installed = [true, true, true];
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
            assert!(bar.contains("agent-command"), "{bar}");
            assert!(!bar.contains("PRIVATEER"), "{bar}");
            assert!(bar.contains('⚙'), "{bar}");
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
                "SUBSCRIPTIONS",
                "+ Connect subscription",
                "Commander profile",
                "ProdGate floor",
                "Check for updates",
            ] {
                assert!(text.contains(expected), "{opener} {expected}\n{text}");
            }
            if height >= 40 {
                for expected in [
                    &format!("Version   {}", update::CURRENT),
                    "Profiles",
                    "Theme",
                    "Close",
                ] {
                    assert!(text.contains(expected), "{opener} {expected}\n{text}");
                }
            }
            assert_inside(&app, width, height);
            press(&mut app, KeyCode::Esc);
            assert!(app.panel.is_none(), "{opener}");
            assert!(app.editing, "{opener}");
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
fn labels(app: &App) -> Vec<String> {
    app.profile_labels()
}

#[cfg(unix)]
fn send(app: &mut App, command: &str) {
    app.input = command.into();
    app.submit();
}

#[cfg(unix)]
fn confirming_index(app: &App) -> Option<usize> {
    match &app.panel {
        Some(Panel::Settings(view)) => view.confirm,
        _ => None,
    }
}

#[cfg(unix)]
#[test]
fn disconnect_needs_confirmation_then_removes_the_profile_slots_and_deck_card() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let sandbox = Sandbox::new(&format!("disconnect-{width}"));
        existing(&sandbox, Provider::Claude, "main");
        existing(&sandbox, Provider::Codex, "work");
        let settings_path = sandbox.0.join("config/settings.toml");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            "fallback_profile = \"codex/work\"\ntext_gen_profile = \"codex/work\"\n",
        )
        .unwrap();
        let host = fake([true, true], &["claude/main"]);
        let mut app = launched(&sandbox, &host);
        app.live.as_mut().unwrap().preferred = Some(1);
        app.live.as_ref().unwrap().save_override().unwrap();
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("Commander [Codex · work]"), "{text}");
        assert_eq!(labels(&app), ["claude/main", "codex/work"]);
        assert_eq!(
            connection_rows(&app),
            if width >= RAIL_WIDTH { 3 } else { 0 }
        );
        press(&mut app, KeyCode::F(3));
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("SUBSCRIPTIONS"), "{text}");
        assert!(text.contains("○ signed out"), "{text}");
        assert!(text.contains(" Disconnect "), "{text}");
        assert_inside(&app, width, height);
        let ask = |app: &mut App| {
            if index == 0 {
                for _ in 0..3 {
                    press(app, KeyCode::Down);
                }
                press(app, KeyCode::Enter);
            } else {
                let disconnect = hit(app, Action::Disconnect(1));
                click(app, disconnect);
            }
        };
        ask(&mut app);
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("Disconnect codex/work?"), "{text}");
        assert!(text.contains("Y Confirm disconnect"), "{text}");
        assert_inside(&app, width, height);
        press(&mut app, KeyCode::Esc);
        assert!(host.0.borrow().calls.is_empty());
        assert!(status(&app).contains("Disconnect cancelled"));
        assert!(sandbox.profiles().join("codex/work").is_dir());

        render(&mut app, width, height);
        ask(&mut app);
        render(&mut app, width, height);
        if index == 0 {
            press(&mut app, KeyCode::Char('y'));
        } else {
            let confirm = hit(&app, Action::ConfirmDisconnect);
            click(&mut app, confirm);
        }
        assert_eq!(host.0.borrow().calls, ["disconnect codex/work"]);
        assert!(!sandbox.profiles().join("codex/work").exists());
        assert_eq!(labels(&app), ["claude/main"]);
        let summary = status(&app);
        assert!(summary.contains("Disconnected codex/work"), "{summary}");
        assert!(
            summary.contains("Cleared fallback profile, text-gen profile, Commander profile."),
            "{summary}"
        );
        let saved = settings::Settings::load(&settings_path).unwrap();
        assert_eq!(saved.fallback_profile, None);
        assert_eq!(saved.text_gen_profile, None);
        assert_eq!(app.live.as_ref().unwrap().preferred, None);
        let text = screen(&render(&mut app, width, height));
        assert!(!text.contains("│ codex/work"), "{text}");
        assert!(!app.panel_actions().contains(&Action::Disconnect(1)));
        press(&mut app, KeyCode::Esc);
        let text = screen(&render(&mut app, width, height));
        assert!(
            !text.contains("codex/work") && !text.contains("Codex · work"),
            "{text}"
        );
        assert_eq!(labels(&app), ["claude/main"]);
        if width >= RAIL_WIDTH {
            let codex = rail_row(&app, Provider::Codex);
            click(&mut app, codex);
            assert!(matches!(app.panel, Some(Panel::Settings(_))));
            assert!(!app.panel_actions().contains(&Action::Disconnect(1)));
            press(&mut app, KeyCode::Esc);
        }
    }
}

#[cfg(unix)]
#[test]
fn a_failed_disconnect_keeps_the_profile_and_says_why() {
    let sandbox = Sandbox::new("disconnect-failure");
    existing(&sandbox, Provider::Codex, "main");
    let host = fake([true, true], &["codex/main"]);
    host.0.borrow_mut().disconnect_failure = Some("profile folder is busy");
    let mut app = launched(&sandbox, &host);
    send(&mut app, "/disconnect codex");
    assert_eq!(confirming_index(&app), Some(0));
    press(&mut app, KeyCode::Char('y'));
    assert!(status(&app).contains("codex/main: disconnect failed: profile folder is busy"));
    assert_eq!(labels(&app), ["codex/main"]);
    assert!(sandbox.profiles().join("codex/main").is_dir());
}

#[cfg(unix)]
#[test]
fn reconnect_restores_and_reloads_after_a_refused_sign_in_then_succeeds() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let sandbox = Sandbox::new(&format!("reconnect-{width}"));
        existing(&sandbox, Provider::Codex, "main");
        let host = fake([true, true], &["claude/other"]);
        host.0.borrow_mut().refuse_sign_in = true;
        let mut app = launched(&sandbox, &host);
        app.panel = None;
        press(&mut app, KeyCode::F(3));
        render(&mut app, width, height);
        let reconnect = |app: &mut App| {
            if index == 0 {
                press(app, KeyCode::Enter);
            } else {
                let button = hit(app, Action::Reconnect(0));
                click(app, button);
            }
        };
        reconnect(&mut app);
        assert_eq!(
            host.0.borrow().calls,
            ["suspend", "sign_in codex/main", "restore"]
        );
        let failure = status(&app);
        assert!(failure.contains("sign-in failed"), "{failure}");
        assert!(
            failure.contains("Disconnect removes a broken subscription"),
            "{failure}"
        );
        assert!(app.full_redraw);
        assert_eq!(app.live.as_ref().unwrap().signed_in, [false]);

        host.0.borrow_mut().refuse_sign_in = false;
        render(&mut app, width, height);
        reconnect(&mut app);
        assert!(
            status(&app).contains("codex/main signed in"),
            "{}",
            status(&app)
        );
        assert_eq!(app.live.as_ref().unwrap().signed_in, [true]);
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("● signed in"), "{text}");
    }
}

#[cfg(unix)]
#[test]
fn disconnect_and_reconnect_commands_target_one_subscription() {
    let sandbox = Sandbox::new("subscription-commands");
    existing(&sandbox, Provider::Claude, "main");
    existing(&sandbox, Provider::Codex, "a");
    existing(&sandbox, Provider::Codex, "b");
    let host = fake([true, true], &["claude/main", "codex/a"]);
    let mut app = launched(&sandbox, &host);

    send(&mut app, "/disconnect");
    assert!(matches!(app.panel, Some(Panel::Settings(_))));
    assert_eq!(app.focused_action(), Some(Action::Reconnect(0)));
    assert_eq!(confirming_index(&app), None);
    press(&mut app, KeyCode::Esc);

    send(&mut app, "/disconnect codex");
    assert!(app.panel.is_none());
    assert!(
        app.notice
            .contains("codex has 2 subscriptions: codex/a, codex/b"),
        "{}",
        app.notice
    );
    send(&mut app, "/disconnect ghost");
    assert!(app.notice.contains("No subscription named ghost"));
    send(&mut app, "/reconnect codex");
    assert!(app.panel.is_none() && app.notice.contains("Name one as PROVIDER/PROFILE"));
    assert!(host.0.borrow().calls.is_empty());

    send(&mut app, "/disconnect codex/b");
    assert_eq!(confirming_index(&app), Some(2));
    let text = screen(&render(&mut app, 80, 24));
    assert!(text.contains("Disconnect codex/b?"), "{text}");
    press(&mut app, KeyCode::Char('n'));
    assert!(host.0.borrow().calls.is_empty());
    press(&mut app, KeyCode::Esc);

    send(&mut app, "/disconnect claude");
    assert_eq!(confirming_index(&app), Some(0));
    press(&mut app, KeyCode::Char('y'));
    assert_eq!(host.0.borrow().calls, ["disconnect claude/main"]);
    assert_eq!(labels(&app), ["codex/a", "codex/b"]);
    press(&mut app, KeyCode::Esc);

    send(&mut app, "/disconnect claude");
    assert!(app.notice.contains("No claude subscription is connected"));
    send(&mut app, "/reconnect claude");
    assert_eq!(origin(&app), Some(Origin::Direct));
    assert_eq!(chosen(&app), [Provider::Claude]);
    press(&mut app, KeyCode::Esc);
    assert!(app.panel.is_none());

    host.0.borrow_mut().calls.clear();
    send(&mut app, "/reconnect codex/b");
    assert_eq!(
        host.0.borrow().calls,
        ["suspend", "sign_in codex/b", "restore"]
    );
    assert!(matches!(app.panel, Some(Panel::Settings(_))));
    assert_eq!(app.focused_action(), Some(Action::Reconnect(1)));
    assert!(
        status(&app).contains("codex/b signed in"),
        "{}",
        status(&app)
    );
    press(&mut app, KeyCode::Esc);

    send(&mut app, "/reconnect");
    assert!(matches!(app.panel, Some(Panel::Settings(_))));
}

#[cfg(unix)]
#[test]
fn clicking_a_deck_card_opens_settings_on_that_subscription() {
    for (width, height) in RAIL_SIZES {
        let sandbox = Sandbox::new(&format!("deck-click-{width}"));
        existing(&sandbox, Provider::Claude, "main");
        existing(&sandbox, Provider::Codex, "work");
        let host = fake([true, true], &["claude/main", "codex/work"]);
        let mut app = launched(&sandbox, &host);
        render(&mut app, width, height);
        let card = rail_row(&app, Provider::Codex);
        assert!(card.width > 0 && card.right() <= width && card.bottom() <= height);
        app.click(card.x + card.width / 2, card.y + card.height / 2);
        assert!(matches!(app.panel, Some(Panel::Settings(_))));
        assert_eq!(app.focused_action(), Some(Action::Reconnect(1)));
        assert!(status(&app).contains("codex/work: Reconnect signs in again"));
        let text = screen(&render(&mut app, width, height));
        assert!(
            text.contains("Reconnect") && text.contains("Disconnect"),
            "{text}"
        );
        press(&mut app, KeyCode::Down);
        assert_eq!(app.focused_action(), Some(Action::Disconnect(1)));
        press(&mut app, KeyCode::Enter);
        assert_eq!(confirming_index(&app), Some(1));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);

        render(&mut app, width, height);
        let card = rail_row(&app, Provider::Claude);
        app.click(card.x, card.y);
        assert_eq!(app.focused_action(), Some(Action::Reconnect(0)));
        assert!(host.0.borrow().calls.is_empty());
    }
}

#[cfg(unix)]
fn connection_rows(app: &App) -> usize {
    app.rail_hits
        .iter()
        .filter(|(_, hit)| matches!(hit, RailHit::OpenConnect(_)))
        .count()
}

#[cfg(unix)]
fn rail_row(app: &App, provider: Provider) -> Rect {
    app.rail_hits
        .iter()
        .find(|(_, hit)| *hit == RailHit::OpenConnect(provider))
        .map(|(rect, _)| *rect)
        .unwrap_or_else(|| panic!("no rail row for {provider:?}"))
}

fn last_row(buffer: &Buffer) -> String {
    row(buffer, buffer.area.height - 1)
}

fn detected(app: &mut App, latest: &str) -> std::sync::mpsc::Sender<String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    app.watch_updates(Some(receiver));
    app.poll_update();
    sender.send(latest.into()).unwrap();
    app.poll_update();
    sender
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum Trigger {
    Click,
    Key,
    Command,
}

#[cfg(unix)]
fn trigger_update(app: &mut App, trigger: Trigger) {
    match trigger {
        Trigger::Click => {
            let button = app.update_area;
            click(app, button);
        }
        Trigger::Key => press(app, KeyCode::F(6)),
        Trigger::Command => send(app, "/update"),
    }
}

#[cfg(unix)]
#[test]
fn update_button_runs_the_host_updater_and_stays_for_a_retry_after_failure() {
    for (width, height) in SIZES {
        for trigger in [Trigger::Click, Trigger::Key, Trigger::Command] {
            let sandbox = Sandbox::new(&format!("update-{width}"));
            existing(&sandbox, Provider::Claude, "main");
            let host = fake([true, true], &["claude/main"]);
            let mut app = launched(&sandbox, &host);
            let text = last_row(&render(&mut app, width, height));
            assert!(!text.contains("Update"), "{text}");
            assert_eq!(app.update_area, ratatui::layout::Rect::default());
            send(&mut app, "/update");
            assert_eq!(app.update_state, update::UpdateState::Checking);
            assert!(app.notice.contains("Checking"), "{}", app.notice);
            press(&mut app, KeyCode::F(6));
            assert!(host.0.borrow().calls.is_empty());

            let _sender = detected(&mut app, "9.9.9");
            render(&mut app, width, height);
            host.0.borrow_mut().update_failure = Some("checksum mismatch");
            trigger_update(&mut app, trigger);
            assert_eq!(
                host.0.borrow().calls,
                ["suspend", "update 9.9.9", "restore"]
            );
            assert!(!app.quit);
            assert!(
                app.notice.contains(
                    "Update to 9.9.9 failed: checksum mismatch. Click Update or press F6 to retry."
                ),
                "{}",
                app.notice
            );
            assert!(app.full_redraw);
            let text = last_row(&render(&mut app, width, height));
            assert!(text.contains("Update"), "{text}");

            host.0.borrow_mut().update_failure = None;
            host.0.borrow_mut().calls.clear();
            trigger_update(&mut app, trigger);
            assert_eq!(host.0.borrow().calls, ["suspend", "update 9.9.9"]);
            assert!(app.quit);
            let message = app.take_exit_message().unwrap();
            assert!(message.contains("updated to 9.9.9"), "{message}");
        }
    }
}

#[cfg(unix)]
#[test]
fn update_is_refused_while_a_mission_runs() {
    let sandbox = Sandbox::new("update-busy");
    existing(&sandbox, Provider::Claude, "main");
    let host = fake([true, true], &["claude/main"]);
    let mut app = launched(&sandbox, &host);
    let _sender = detected(&mut app, "9.9.9");
    let (mission, _events) = crate::mission::Mission::held();
    app.live
        .as_mut()
        .unwrap()
        .runs
        .push(crate::Run::new(crate::COMMANDER, mission, false));
    render(&mut app, 120, 40);
    for trigger in [Trigger::Click, Trigger::Key, Trigger::Command] {
        app.notice.clear();
        trigger_update(&mut app, trigger);
        assert_eq!(app.notice, UPDATE_MISSION_BLOCKER);
    }
    assert!(host.0.borrow().calls.is_empty());
    assert!(!app.quit);
    assert!(app.update_available.is_some());
}

#[test]
fn update_is_refused_in_preview() {
    let host = fake([true, true], &[]);
    let mut app = preview(&host, LaunchConfig::default());
    app.panel = None;
    let _sender = detected(&mut app, "9.9.9");
    let text = last_row(&render(&mut app, 80, 24));
    assert!(text.ends_with(" ⬆ Update v9.9.9 "), "{text}");
    let button = app.update_area;
    click(&mut app, button);
    assert_eq!(app.notice, PREVIEW_BLOCKER);
    assert!(host.0.borrow().calls.is_empty());
}

#[test]
fn update_button_sits_bottom_right_and_left_of_abandon_at_both_sizes() {
    for (width, height) in SIZES {
        let mut app = App::from_live(crate::Live::new(std::env::temp_dir(), Vec::new(), None));
        let text = last_row(&render(&mut app, width, height));
        assert!(!text.contains("Update"), "{text}");
        assert_eq!(app.update_area, ratatui::layout::Rect::default());

        let _sender = detected(&mut app, "9.9.9");
        let buffer = render(&mut app, width, height);
        let text = last_row(&buffer);
        assert!(text.contains("Update v9.9.9"), "{text}");
        assert_eq!(app.update_area.y, height - 1);
        assert_eq!(app.update_area.right(), width);
        assert!(app.update_area.width > 0);
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
        assert!(fs::read_to_string(&path)
            .unwrap()
            .starts_with("check_for_updates = false\n"));
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

#[cfg(unix)]
#[test]
fn limit_toggles_and_profile_slots_persist_from_keyboard_and_mouse() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let sandbox = Sandbox::new(&format!("limit-slots-{width}"));
        existing(&sandbox, Provider::Claude, "work");
        existing(&sandbox, Provider::Codex, "backup");
        let host = fake([true, true], &["claude/work", "codex/backup"]);
        let mut app = launched(&sandbox, &host);
        press(&mut app, KeyCode::F(3));
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("Resume after limits"), "{text}");
        assert!(text.contains("Fallback profile"), "{text}");
        assert_inside(&app, width, height);
        let activate = |app: &mut App, action: Action| {
            if index == 0 {
                while app.focused_action() != Some(action) {
                    press(app, KeyCode::Down);
                }
                press(app, KeyCode::Enter);
            } else {
                render(app, width, height);
                click(app, hit(app, action));
            }
        };
        activate(&mut app, Action::AutoResume);
        activate(&mut app, Action::SnoozeMissions);
        activate(&mut app, Action::FallbackProfile);
        activate(&mut app, Action::FallbackProfile);
        activate(&mut app, Action::TextGenProfile);
        assert!(status(&app).contains("Text generation profile: claude/work"));
        let saved = settings::Settings::load(&sandbox.0.join("config/settings.toml")).unwrap();
        assert!(saved.auto_resume_on_reset);
        assert!(saved.snooze_limited_missions);
        assert_eq!(saved.fallback_profile.as_deref(), Some("codex/backup"));
        assert_eq!(saved.text_gen_profile.as_deref(), Some("claude/work"));
        assert_eq!(
            app.limit_policy(),
            crate::mission::LimitPolicy {
                auto_resume_on_reset: true,
                fallback_profile: Some("codex/backup".into()),
            }
        );
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("codex/backup"), "{text}");
        activate(&mut app, Action::FallbackProfile);
        assert!(status(&app).contains("Cross-provider fallback: Off"));
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
            ready.0.borrow_mut().installed[2] = true;
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
fn gemini_install_uses_agy_executable_and_official_installer() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let sandbox = Sandbox::new(&format!("gemini-agy-{width}"));
        let home = sandbox.0.join("home");
        let host = fake([true, true], &[]);
        host.0.borrow_mut().home = Some(home.clone());
        host.0.borrow_mut().installs = Some(if index == 0 {
            Landing::OnPath
        } else {
            Landing::DocumentedDirectory
        });
        let mut app = preview(&host, LaunchConfig::default());
        open(&mut app, false);
        let before = screen(&render(&mut app, width, height));
        assert!(before.contains("Antigravity CLI"), "{before}");
        assert!(before.contains("✗ agy not found"), "{before}");
        assert_inside(&app, width, height);

        choose_install(&mut app, Provider::Gemini, index == 0, width, height);
        let prompt = screen(&render(&mut app, width, height));
        assert!(
            prompt.contains("Install Antigravity CLI with its official installer:"),
            "{prompt}"
        );
        assert!(prompt.contains(plan_for(Provider::Gemini).docs), "{prompt}");
        assert_inside(&app, width, height);

        host.0.borrow_mut().checks.clear();
        confirm_install(&mut app, index == 0, width, height);
        assert!(host.0.borrow().checks.contains(&"found agy".to_owned()));
        let after = screen(&render(&mut app, width, height));
        if index == 0 {
            assert_eq!(status(&app), "Antigravity CLI installed: agy is on PATH.");
            assert!(after.contains("✓ agy on PATH"), "{after}");
        } else {
            let executable = documented_in(&home, Provider::Gemini).unwrap();
            assert_eq!(
                status(&app),
                format!(
                    "Antigravity CLI installed. Using {} for agy in this session.",
                    executable.display()
                )
            );
            assert!(after.contains("✓ agy at "), "{after}");
        }
        assert!(!has_hit(&app, Action::Install(Provider::Gemini)), "{after}");
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

#[cfg(unix)]
fn chosen(app: &App) -> Vec<Provider> {
    match &app.panel {
        Some(Panel::Connect(form)) => form.chosen_providers(),
        _ => panic!("Connect is not open"),
    }
}

#[cfg(unix)]
#[test]
fn onboarding_pre_checks_every_installed_cli_and_shows_the_auto_name() {
    for (width, height) in SIZES {
        let sandbox = Sandbox::new(&format!("onboarding-both-{width}"));
        let mut app = launched(&sandbox, &fake([true, true], &[]));
        assert_eq!(origin(&app), Some(Origin::Onboarding));
        assert_eq!(chosen(&app), [Provider::Claude, Provider::Codex]);
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("[x] Claude Code"), "{text}");
        assert!(text.contains("[x] Codex CLI"), "{text}");
        assert!(text.contains(AUTO_NAME_PLACEHOLDER), "{text}");
        assert!(!text.contains("● signed in"), "{text}");
        assert_inside(&app, width, height);
    }
}

#[cfg(unix)]
#[test]
fn connect_pre_checks_only_installed_providers_that_are_not_signed_in() {
    for (width, height) in SIZES {
        let sandbox = Sandbox::new(&format!("connect-unconnected-{width}"));
        existing(&sandbox, Provider::Claude, "work");
        let host = fake([true, true], &["claude/work"]);
        let mut app = launched(&sandbox, &host);
        assert!(app.panel.is_none());
        app.input = "/connect".into();
        app.submit();
        assert_eq!(chosen(&app), [Provider::Codex]);
        let text = screen(&render(&mut app, width, height));
        let claude_row = text
            .lines()
            .find(|line| line.contains("Claude Code"))
            .unwrap();
        assert!(claude_row.contains("[ ] Claude Code"), "{claude_row}");
        assert!(claude_row.contains("● signed in"), "{claude_row}");
        let codex_row = text
            .lines()
            .find(|line| line.contains("Codex CLI"))
            .unwrap();
        assert!(codex_row.contains("[x] Codex CLI"), "{codex_row}");
        assert!(!codex_row.contains("● signed in"), "{codex_row}");
        assert_inside(&app, width, height);
        press(&mut app, KeyCode::Esc);

        app.open_settings();
        render(&mut app, width, height);
        let area = hit(&app, Action::ConnectNew);
        click(&mut app, area);
        assert_eq!(origin(&app), Some(Origin::Settings));
        assert_eq!(chosen(&app), [Provider::Codex]);
        render(&mut app, width, height);
        let area = hit(&app, Action::Toggle(Provider::Claude));
        click(&mut app, area);
        assert_eq!(chosen(&app), [Provider::Claude, Provider::Codex]);
    }
}

#[cfg(unix)]
#[test]
fn connect_leaves_every_box_unchecked_when_all_installed_providers_are_signed_in() {
    let sandbox = Sandbox::new("connect-all-signed");
    existing(&sandbox, Provider::Claude, "work");
    existing(&sandbox, Provider::Codex, "work");
    let host = fake([true, true], &["claude/work", "codex/work"]);
    let mut app = launched(&sandbox, &host);
    app.open_connect(Origin::Direct);
    assert!(chosen(&app).is_empty());
    app.activate(Action::Connect);
    assert_eq!(status(&app), "Pick Claude, Codex or both.");
    assert!(host.0.borrow().calls.is_empty());
}

#[cfg(unix)]
#[test]
fn connect_names_profiles_from_the_signed_in_account() {
    for (width, height) in SIZES {
        let sandbox = Sandbox::new(&format!("connect-auto-{width}"));
        let host = fake([true, true], &[]);
        host.0.borrow_mut().accounts = [Some("Pilot@Example.test"), Some("Pilot Example"), None];
        let mut app = launched(&sandbox, &host);
        render(&mut app, width, height);
        let area = hit(&app, Action::Connect);
        click(&mut app, area);
        assert_eq!(
            host.0.borrow().calls,
            [
                "suspend",
                "connect claude auto",
                "connect codex auto",
                "restore"
            ]
        );
        assert!(app.panel.is_none());
        assert_eq!(
            app.notice,
            "claude/pilot@example.test signed in · codex/pilot-example signed in"
        );
        let labels = app.profile_labels();
        assert_eq!(labels, ["claude/pilot@example.test", "codex/pilot-example"]);
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("pilot"), "{text}");
        press(&mut app, KeyCode::F(3));
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("claude/pilot@example.test"), "{text}");
        assert_inside(&app, width, height);
        app.panel = None;
        app.input = "/entry claude/pilot@example.test".into();
        app.submit();
        assert_eq!(
            app.notice,
            "Entry model: claude/pilot@example.test (override)"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_typed_profile_name_overrides_the_account_name() {
    let sandbox = Sandbox::new("connect-custom");
    let host = fake([true, false], &[]);
    host.0.borrow_mut().accounts = [Some("pilot@example.test"), None, None];
    let mut app = launched(&sandbox, &host);
    if let Some(Panel::Connect(form)) = &mut app.panel {
        form.name = "work".into();
    }
    app.activate(Action::Connect);
    assert_eq!(app.notice, "claude/work signed in");
}

#[test]
fn commander_mode_cycles_from_settings_and_persists() {
    for (index, (width, height)) in SIZES.into_iter().enumerate() {
        let sandbox = Sandbox::new(&format!("commander-mode-{width}"));
        let host = fake([true, true], &[]);
        let mut app = preview(&host, sandbox.launch(None));
        assert_eq!(
            app.settings.commander_permission,
            crate::permissions::PermissionMode::WorkspaceAuto
        );
        app.open_settings();
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("Commander mode"), "{text}");
        assert!(text.contains("workspace-auto"), "{text}");
        if index == 0 {
            while app.focused_action() != Some(Action::CommanderMode) {
                press(&mut app, KeyCode::Down);
            }
            press(&mut app, KeyCode::Enter);
        } else {
            let area = hit(&app, Action::CommanderMode);
            click(&mut app, area);
        }
        assert!(
            status(&app).contains("Commander permissions: read-only (saved)."),
            "{}",
            status(&app)
        );
        let saved = settings::Settings::load(&sandbox.0.join("config/settings.toml")).unwrap();
        assert_eq!(
            saved.commander_permission,
            crate::permissions::PermissionMode::ReadOnly
        );
        let reopened = preview(&host, sandbox.launch(None));
        assert_eq!(
            reopened.settings.commander_permission,
            crate::permissions::PermissionMode::ReadOnly
        );
        assert_eq!(
            reopened.crew[0].permission,
            crate::permissions::PermissionMode::ReadOnly
        );
    }
}

#[test]
fn mode_command_sets_and_persists_the_commander_permission() {
    let sandbox = Sandbox::new("mode-command");
    let host = fake([true, true], &[]);
    let mut app = preview(&host, sandbox.launch(None));
    app.panel = None;
    app.input = "/mode supervised".into();
    app.submit();
    assert_eq!(app.notice, "Commander permissions: supervised (saved).");
    assert!(app.input.is_empty());
    let saved = settings::Settings::load(&sandbox.0.join("config/settings.toml")).unwrap();
    assert_eq!(
        saved.commander_permission,
        crate::permissions::PermissionMode::Supervised
    );
    app.input = "/mode worktree-auto".into();
    app.submit();
    assert!(app
        .notice
        .contains(crate::permissions::COMMANDER_MODE_USAGE));
    assert_eq!(
        app.settings.commander_permission,
        crate::permissions::PermissionMode::Supervised
    );
    app.input = "/mode".into();
    app.submit();
    assert_eq!(app.notice, crate::permissions::COMMANDER_MODE_USAGE);
    let text = screen(&render(&mut app, 120, 40));
    assert!(text.contains("supervised · ProdGate on"), "{text}");
}
