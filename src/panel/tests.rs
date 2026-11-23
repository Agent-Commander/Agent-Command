use super::*;
use crate::settings::LaunchConfig;
#[cfg(unix)]
use crate::Live;
use crossterm::event::KeyModifiers;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};
use std::{cell::RefCell, collections::BTreeSet, ffi::OsString, fs, path::PathBuf, rc::Rc};

const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];

#[derive(Default)]
struct Script {
    calls: Vec<String>,
    signed: BTreeSet<String>,
    installed: [bool; 2],
    refuse_sign_in: bool,
}

#[derive(Clone)]
struct FakeHost(Rc<RefCell<Script>>);

impl Host for FakeHost {
    fn installed(&self, provider: Provider) -> bool {
        self.0.borrow().installed[provider_slot(provider)]
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
        for expected in [
            "✗ claude not on PATH · install it, then press R",
            "✗ codex not on PATH · install it, then press R",
            "R Recheck",
        ] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
        assert_inside(&app, width, height);
        press(&mut app, KeyCode::Enter);
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("claude is not on PATH"), "{text}");
        host.0.borrow_mut().installed = [true, false];
        if index == 0 {
            press(&mut app, KeyCode::Char('R'));
        } else {
            let recheck = hit(&app, Action::Recheck);
            click(&mut app, recheck);
        }
        let text = screen(&render(&mut app, width, height));
        assert!(text.contains("✓ claude on PATH"), "{text}");
        assert!(text.contains("✗ codex not on PATH"), "{text}");
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
