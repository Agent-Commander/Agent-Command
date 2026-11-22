use crate::{
    auth::{Profile, Provider},
    checkpoint::{Checkpoint, Claim, Progress},
    usage::{self, Usage},
};
use serde_json::Value;
use std::{
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    thread,
    time::Duration,
};

pub(crate) enum Event {
    Running { profile: usize, rotated: bool },
    Output(String),
    Continuing { from: usize, to: usize },
    Finished(io::Result<()>),
}

pub(crate) struct Mission {
    pub(crate) events: Receiver<Event>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Mission {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Mission {
    pub(crate) fn launch(
        profiles: Vec<Profile>,
        preferred: usize,
        workspace: PathBuf,
        prompt: String,
    ) -> Self {
        Self::launch_with_clock(profiles, preferred, workspace, prompt, usage::now)
    }

    fn launch_with_clock(
        profiles: Vec<Profile>,
        preferred: usize,
        workspace: PathBuf,
        prompt: String,
        clock: fn() -> usage::Tick,
    ) -> Self {
        Self::start(
            Execution {
                profiles,
                workspace,
                prompt,
                clock,
            },
            preferred,
            None,
        )
    }

    pub(crate) fn resume(
        profiles: Vec<Profile>,
        interrupted: usize,
        workspace: PathBuf,
        prompt: String,
        claim: Claim,
    ) -> Self {
        Self::start(
            Execution {
                profiles,
                workspace,
                prompt,
                clock: usage::now,
            },
            interrupted,
            Some(claim),
        )
    }

    fn start(execution: Execution, preferred: usize, claim: Option<Claim>) -> Self {
        let (sender, events) = mpsc::sync_channel(64);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            let result = match claim {
                Some(claim) => execution.resume(preferred, claim, &sender, &worker_cancel),
                None => execution.run(preferred, &sender, &worker_cancel),
            };
            let _ = sender.send(Event::Finished(result));
        });
        Self { events, cancel }
    }
}

struct Execution {
    profiles: Vec<Profile>,
    workspace: PathBuf,
    prompt: String,
    clock: fn() -> usage::Tick,
}

impl Execution {
    fn run(
        &self,
        preferred: usize,
        sender: &SyncSender<Event>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        let (index, command) = select_command(&self.profiles, preferred, (self.clock)())?;
        let profile = &self.profiles[index];
        let limit = run(
            (command, profile.clone(), index, index != preferred),
            &self.workspace,
            self.prompt.clone(),
            sender,
            cancel,
            self.clock,
            None,
        )?;
        let Some(progress) = limit else { return Ok(()) };
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let checkpoint = Checkpoint::save(profile, uuid::Uuid::new_v4(), &self.workspace, progress)
            .map_err(|error| {
                io::Error::other(format!(
                    "Usage limit reached; mission stopped without replay. {error}"
                ))
            })?;
        let (to, next) = self.alternate(index).ok_or_else(|| {
            io::Error::other(
            "Usage limit reached; mission stopped without replay. No eligible signed-in alternate."
        )
        })?;
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let claim = checkpoint.reserve(profile, &self.workspace, &self.profiles[to])?;
        self.continue_with((index, to, next), &claim, sender, cancel)
    }

    fn resume(
        &self,
        index: usize,
        claim: Claim,
        sender: &SyncSender<Event>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        let (to, profile) = self
            .profiles
            .iter()
            .enumerate()
            .find(|(to, profile)| {
                *to != index
                    && profile.provider == self.profiles[index].provider
                    && profile.to_string() == claim.alternate
            })
            .ok_or_else(|| io::Error::other("Claimed alternate profile is unavailable"))?;
        let next = profile.authenticated_command()?;
        if !Usage::read(profile)?.can_resume_claim((self.clock)()) {
            return Err(io::Error::other(
                "Claimed alternate needs a fresh reading or is latched",
            ));
        }
        self.continue_with((index, to, next), &claim, sender, cancel)
    }

    fn continue_with(
        &self,
        selection: (usize, usize, std::process::Command),
        claim: &Claim,
        sender: &SyncSender<Event>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        let (index, to, next) = selection;
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if run(
            (next, self.profiles[to].clone(), to, false),
            &self.workspace,
            self.prompt.clone(),
            sender,
            cancel,
            self.clock,
            Some(Continuation {
                claim,
                from: index,
                profile: &self.profiles[index],
            }),
        )?
        .is_some()
        {
            return Err(io::Error::other(
                "Continuation hit a usage limit; mission stopped without replay",
            ));
        }
        Ok(())
    }

    fn alternate(&self, index: usize) -> Option<(usize, std::process::Command)> {
        usage::alternates(&self.profiles, index, (self.clock)())
            .into_iter()
            .find_map(|candidate| {
                let profile = &self.profiles[candidate];
                let command = profile.authenticated_command().ok()?;
                Usage::read(profile)
                    .ok()?
                    .eligible_alternate((self.clock)())
                    .then_some((candidate, command))
            })
    }
}

fn select_command(
    profiles: &[Profile],
    preferred: usize,
    now: usage::Tick,
) -> io::Result<(usize, std::process::Command)> {
    for index in usage::choices(profiles, preferred, now)? {
        match profiles[index].authenticated_command() {
            Ok(command) if !Usage::read(&profiles[index])?.blocked(now) => {
                return Ok((index, command))
            }
            Ok(_) => continue,
            Err(error) if index == preferred => return Err(error),
            Err(_) => continue,
        }
    }
    Err(io::Error::other(
        "Profile is latched and no eligible signed-in alternative is available",
    ))
}

struct Continuation<'a> {
    claim: &'a Claim,
    from: usize,
    profile: &'a Profile,
}

fn run(
    selection: (std::process::Command, Profile, usize, bool),
    workspace: &std::path::Path,
    prompt: String,
    sender: &SyncSender<Event>,
    cancel: &AtomicBool,
    clock: fn() -> usage::Tick,
    continuation: Option<Continuation<'_>>,
) -> io::Result<Option<Progress>> {
    let (mut command, profile, index, rotated) = selection;
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    match profile.provider {
        Provider::Claude => command.args([
            "--print",
            "--verbose",
            "--output-format",
            "stream-json",
            "--setting-sources",
            "user",
            "--",
        ]),
        Provider::Codex => command.args([
            "-a",
            "never",
            "exec",
            "--json",
            "--skip-git-repo-check",
            "--sandbox",
            "workspace-write",
            "-",
        ]),
    };
    command
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let prompt = match &continuation {
        Some(next) => next.claim.start(next.profile, workspace, &prompt)?,
        None => prompt,
    };
    let mut child = command.spawn()?;
    if let Some(next) = continuation {
        let _ = sender.send(Event::Continuing {
            from: next.from,
            to: index,
        });
    }
    let stdout = child.stdout.take().expect("mission stdout is piped");
    let stdout_sender = sender.clone();
    let _ = sender.send(Event::Running {
        profile: index,
        rotated,
    });
    let limited = Arc::new(AtomicBool::new(false));
    let output_limit = Arc::clone(&limited);
    let reader =
        thread::spawn(move || read_output(&profile, stdout, &stdout_sender, &output_limit, clock));
    let mut stdin = child.stdin.take().expect("mission stdin is piped");
    let writer = thread::spawn(move || stdin.write_all(prompt.as_bytes()));
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            child.wait()?;
            return Ok(None);
        }
        if limited.load(Ordering::Acquire) {
            let _ = child.kill();
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let (progress, completed) = reader
        .join()
        .map_err(|_| io::Error::other("Mission output reader stopped"))??;
    if limited.load(Ordering::Acquire) {
        return Ok((!completed).then_some(progress));
    }
    if !status.success() {
        return Err(io::Error::other(format!("Provider exited with {status}")));
    }
    writer
        .join()
        .map_err(|_| io::Error::other("Mission input writer stopped"))??;
    Ok(None)
}

fn output_text(provider: Provider, event: &Value) -> Vec<&str> {
    match (provider, event["type"].as_str()) {
        (Provider::Claude, Some("assistant")) => event["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "text")
            .filter_map(|item| item["text"].as_str())
            .collect(),
        (Provider::Codex, Some("item.completed")) if event["item"]["type"] == "agent_message" => {
            event["item"]["text"].as_str().into_iter().collect()
        }
        _ => Vec::new(),
    }
}

fn read_output(
    profile: &Profile,
    stdout: impl io::Read,
    sender: &SyncSender<Event>,
    limited: &AtomicBool,
    clock: fn() -> usage::Tick,
) -> io::Result<(Progress, bool)> {
    let mut progress = Progress::default();
    let mut completed = false;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let observed = clock();
        record_progress(profile.provider, &event, &mut progress);
        completed |= match profile.provider {
            Provider::Claude => {
                event["type"] == "result"
                    && event["subtype"] == "success"
                    && event["is_error"] == false
            }
            Provider::Codex => event["type"] == "turn.completed",
        };
        let limit = match profile.provider {
            Provider::Claude => Usage::observe_claude(profile, &event, observed)?,
            Provider::Codex if codex_limit(&event) => {
                Usage::record_codex_limit(profile, observed)?;
                true
            }
            Provider::Codex => false,
        };
        if limit {
            limited.store(true, Ordering::Release);
        }
        for text in output_text(profile.provider, &event) {
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let safe = line
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(500)
                    .collect();
                if sender.send(Event::Output(safe)).is_err() {
                    return Ok((progress, completed));
                }
            }
        }
    }
    Ok((progress, completed))
}

fn record_progress(provider: Provider, event: &Value, progress: &mut Progress) {
    match (provider, event["type"].as_str()) {
        (Provider::Claude, Some("assistant")) => progress.assistant_messages += 1,
        (Provider::Claude, Some("user")) => {
            let tools = event["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "tool_result" && item["is_error"] != true);
            progress.completed_tool_events += tools.count() as u64;
        }
        (Provider::Codex, Some("item.completed")) => match event["item"]["type"].as_str() {
            Some("agent_message") => progress.assistant_messages += 1,
            Some("command_execution" | "file_change" | "mcp_tool_call")
                if event["item"]["status"] == "completed" =>
            {
                progress.completed_tool_events += 1
            }
            _ => {}
        },
        _ => {}
    }
}

fn codex_limit(event: &Value) -> bool {
    if event["type"] == "error" || event["type"] == "turn.failed" {
        let error = if event["type"] == "error" {
            event
        } else {
            &event["error"]
        };
        return error["code"] == "usage_limit_reached"
            || error["message"].as_str().is_some_and(|message| {
                message.starts_with("You've hit your usage limit")
                    || message.starts_with("You’ve hit your usage limit")
            });
    }
    false
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::usage::tests::Fixture;
    use serde_json::json;

    const FAKE_NOW: i64 = 1_000;

    fn git_workspace(fixture: &Fixture) -> PathBuf {
        let workspace = fixture.root.join("work");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("completed"), "").unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "completed"],
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-qm",
                "Fixture",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .current_dir(&workspace)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        workspace
    }

    pub(crate) fn limit_fixture(name: &str, now: usage::Tick) -> (Fixture, PathBuf, Vec<Profile>) {
        let fixture = Fixture::new(
            name,
            r#"
profile=${CLAUDE_CONFIG_DIR##*/}
work="${CLAUDE_CONFIG_DIR%/profiles/claude/*}/work"
if test "$1" = auth; then
    if test "$profile" = standby && test -f "$work/.git/change-after-checkpoint"; then
        printf 'changed\n' >> "$work/completed"
    fi
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
prompt=$(cat)
if test "$profile" = first; then
    test "$prompt" = "Keep the finished step" || exit 7
    printf 'done\n' >> completed
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Finished the first step"}]}}'
    printf '%s\n' '{"type":"user","message":{"content":[{"type":"tool_result","content":"fixture-credential-never-persist"}]}}'
    if test -f .git/completed-before-limit; then
        printf '%s\n' '{"type":"result","subtype":"success","is_error":false}'
    fi
    printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":4102444800}}'
    exit 1
fi
test "$(cat completed)" = done || exit 8
if test -f untracked; then
    case "$prompt" in
      *'"kind":"untracked","path":"untracked","sha256":"373ca25fde04545677f483dcec193a407a9e61ba46dec587ba51f7275332b19d"'*)
        case "$prompt" in *"synthetic file contents"*|*"fixture-credential-never-persist"*) exit 11;; esac
        ;;
      *) exit 10 ;;
    esac
fi
case "$prompt" in
  *"do not redo completed work"*"Retained workspace manifest:"*'"kind":"tracked","path":"completed","sha256":"d117fa006ba9208500b2930ce69cbde436c647afa917cb7396a9bc9111a46dd2"'*"Keep the finished step"*) ;;
  *) exit 9 ;;
esac
printf 'continued\n' >> completed
"#,
        );
        let workspace = git_workspace(&fixture);
        let profiles = vec![
            fixture.profile(Provider::Claude, "first"),
            fixture.profile(Provider::Claude, "standby"),
        ];
        Usage::observe_claude(
            &profiles[1],
            &json!({
                "type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{
                    "five_hour":{"utilization":0.25,"resetsAt":now.wall + 3000},
                    "seven_day":{"utilization":0.25,"resetsAt":now.wall + 8000}
                }}
            }),
            now,
        )
        .unwrap();
        (fixture, workspace, profiles)
    }

    fn drain(mission: &Mission) -> (Vec<(usize, usize)>, io::Result<()>) {
        let mut links = Vec::new();
        loop {
            match mission.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Continuing { from, to } => links.push((from, to)),
                Event::Finished(result) => return (links, result),
                _ => {}
            }
        }
    }

    #[test]
    fn interrupted_mission_checkpoints_once_and_continues_on_the_next_profile() {
        let (_fixture, workspace, profiles) = limit_fixture("continue", usage::tick(FAKE_NOW, 0));
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            0,
            workspace.clone(),
            "Keep the finished step".into(),
            || usage::tick(FAKE_NOW, 0),
        );
        let (links, result) = drain(&mission);
        result.unwrap();
        assert_eq!(links, [(0, 1)]);
        assert_eq!(
            std::fs::read_to_string(workspace.join("completed")).unwrap(),
            "done\ncontinued\n"
        );
        assert!(Usage::read(&profiles[0])
            .unwrap()
            .blocked(usage::tick(FAKE_NOW, 0)));
        let directory = profiles[0].checkpoint_directory().unwrap();
        let paths: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(paths.len(), 3);
        let checkpoint_path = paths
            .iter()
            .find(|path| path.extension().unwrap() == "json")
            .unwrap();
        let bytes = std::fs::read_to_string(checkpoint_path).unwrap();
        for excluded in [
            "fixture-credential-never-persist",
            "Finished the first step",
            "Keep the finished step",
        ] {
            assert!(!bytes.contains(excluded));
        }
        let checkpoint: Checkpoint = serde_json::from_str(&bytes).unwrap();
        for _ in 0..2 {
            let error = checkpoint
                .reserve(&profiles[0], &workspace, &profiles[1])
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("already claimed for this checkpoint by claude/standby"));
        }
        assert_eq!(
            std::fs::read_to_string(workspace.join("completed")).unwrap(),
            "done\ncontinued\n"
        );
    }

    #[test]
    fn separate_process_shows_and_resumes_pending_claim_once_or_abandons_it() {
        for abandon in [false, true] {
            let tag = if abandon {
                "restart-abandon"
            } else {
                "restart-resume"
            };
            let (fixture, workspace, profiles) = limit_fixture(tag, usage::now());
            std::fs::write(workspace.join("completed"), "done\n").unwrap();
            std::fs::write(workspace.join("untracked"), "synthetic file contents").unwrap();
            let checkpoint = Checkpoint::save(
                &profiles[0],
                uuid::Uuid::new_v4(),
                &workspace,
                Progress::default(),
            )
            .unwrap();
            let claim = checkpoint
                .reserve(&profiles[0], &workspace, &profiles[1])
                .unwrap();
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "mission::tests::recovery_child", "--nocapture"])
                .current_dir(&workspace)
                .env("SUB_AUTH_DIR", fixture.root.join("profiles"))
                .env("AC_RECOVERY_TEST_PROVIDER", &fixture.executable)
                .env(
                    "AC_RECOVERY_TEST_ACTION",
                    if abandon { "abandon" } else { "resume" },
                )
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "{}",
                String::from_utf8_lossy(&child.stdout)
            );
            assert!(Claim::pending(&profiles, &workspace).unwrap().is_none());
            assert!(claim
                .start(&profiles[0], &workspace, "Keep the finished step")
                .is_err());
            let expected = if abandon {
                "done\n"
            } else {
                "done\ncontinued\n"
            };
            assert_eq!(
                std::fs::read_to_string(workspace.join("completed")).unwrap(),
                expected
            );
            for entry in std::fs::read_dir(profiles[0].checkpoint_directory().unwrap()).unwrap() {
                let path = entry.unwrap().path();
                let content = std::fs::read_to_string(path).unwrap();
                for excluded in [
                    "Keep the finished step",
                    "fixture-credential-never-persist",
                    "synthetic file contents",
                ] {
                    assert!(!content.contains(excluded));
                }
            }
        }
    }

    #[test]
    fn recovery_child() {
        let Some(executable) = std::env::var_os("AC_RECOVERY_TEST_PROVIDER") else {
            return;
        };
        let root = crate::auth::root().unwrap();
        let workspace = std::env::current_dir().unwrap();
        let profiles = ["first", "standby"].map(|name| {
            Profile::named_fixture(&root, Provider::Claude, name, PathBuf::from(&executable))
        });
        let mut app = crate::App::live(workspace);
        app.live.as_mut().unwrap().profiles = profiles.into();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let notice: String = (0..80).map(|x| buffer[(x, 22)].symbol()).collect();
        assert!(notice.contains("Interrupted continuation pending"));
        if std::env::var("AC_RECOVERY_TEST_ACTION").unwrap() == "abandon" {
            let button = app.live.as_ref().unwrap().abandon_area;
            app.click(button.x, button.y);
            assert!(app.notice.contains("abandoned"));
            return;
        }
        app.input = "Keep the finished step".into();
        app.click(app.send_area.x, app.send_area.y);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.mission_running() {
            assert!(std::time::Instant::now() < deadline);
            app.poll_mission();
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.status(0), "DONE", "{}", app.notice);
    }

    #[test]
    fn a_completed_mission_never_creates_a_continuation() {
        let (_fixture, workspace, profiles) = limit_fixture("completed", usage::tick(FAKE_NOW, 0));
        std::fs::write(workspace.join(".git/completed-before-limit"), "").unwrap();
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            0,
            workspace.clone(),
            "Keep the finished step".into(),
            || usage::tick(FAKE_NOW, 0),
        );
        let (links, result) = drain(&mission);
        result.unwrap();
        assert!(links.is_empty());
        assert_eq!(
            std::fs::read_to_string(workspace.join("completed")).unwrap(),
            "done\n"
        );
        assert_eq!(
            std::fs::read_dir(profiles[0].checkpoint_directory().unwrap())
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn changed_workspace_refuses_the_cross_profile_continuation() {
        let (_fixture, workspace, profiles) =
            limit_fixture("changed-workspace", usage::tick(FAKE_NOW, 0));
        std::fs::write(workspace.join(".git/change-after-checkpoint"), "").unwrap();
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            0,
            workspace.clone(),
            "Keep the finished step".into(),
            || usage::tick(FAKE_NOW, 0),
        );
        let (links, result) = drain(&mission);
        assert!(links.is_empty());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Workspace changed since the interruption"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("completed")).unwrap(),
            "done\nchanged\n"
        );
        assert_eq!(
            std::fs::read_dir(profiles[0].checkpoint_directory().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn a_new_mission_rotates_once_to_the_first_authorized_alternate() {
        let fixture = Fixture::new(
            "launch-alternate",
            r#"
profile=${CLAUDE_CONFIG_DIR##*/}
if test "$1" = auth; then
    test "$profile" != alpha || exit 1
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
test "$(cat)" = "New work" || exit 9
printf '%s\n' "$profile" >> starts
"#,
        );
        let profiles = ["first", "zebra", "bravo", "alpha"]
            .map(|name| fixture.profile(Provider::Claude, name));
        let now = usage::now();
        let mut event = json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "rejected", "resetsAt": now.wall + 3600,
                "unifiedWindows": {
                    "five_hour": {"utilization": 0.5, "resetsAt": now.wall + 3600},
                    "seven_day": {"utilization": 0.5, "resetsAt": now.wall + 3600}
                }
            }
        });
        Usage::observe_claude(&profiles[0], &event, now).unwrap();
        event["rate_limit_info"]["status"] = json!("allowed");
        for profile in &profiles[1..] {
            Usage::observe_claude(profile, &event, now).unwrap();
        }
        let mission = Mission::launch(profiles.into(), 0, fixture.root.clone(), "New work".into());
        assert!(matches!(
            mission.events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Running {
                profile: 2,
                rotated: true
            }
        ));
        assert!(matches!(
            mission.events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Finished(Ok(()))
        ));
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("starts")).unwrap(),
            "bravo\n"
        );
    }

    #[test]
    fn provider_limit_stops_without_replaying_mission() {
        let fixture = Fixture::new(
            "interrupted",
            r#"
case "$1" in
    auth) printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'; exit ;;
    -c)
        shift 4
        if test "$1" = login; then printf '%s\n' 'Logged in using ChatGPT' >&2; exit; fi
        ;;
esac
test "$(cat)" = "Do work" || exit 9
if test "$1" = --print; then
    printf '%s\n' 'claude' >> starts
    printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":4102444800}}'
else
    printf '%s\n' 'codex' >> starts
    printf '%s\n' '{"type":"turn.failed","error":{"message":"You have a problem","code":"usage_limit_reached"}}'
fi
exit 1
"#,
        );
        let workspace = git_workspace(&fixture);
        for provider in [Provider::Claude, Provider::Codex] {
            let selected = fixture.profile(provider, "original");
            let standby = fixture.profile(provider, "standby");
            let mission = Mission::launch(
                vec![selected.clone(), standby],
                0,
                workspace.clone(),
                "Do work".into(),
            );
            assert!(matches!(
                mission.events.recv_timeout(Duration::from_secs(3)).unwrap(),
                Event::Running {
                    profile: 0,
                    rotated: false
                }
            ));
            let Event::Finished(result) =
                mission.events.recv_timeout(Duration::from_secs(3)).unwrap()
            else {
                panic!("expected interrupted outcome")
            };
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("No eligible signed-in alternate"));
            assert!(Usage::read(&selected).unwrap().blocked(usage::now()));
        }
        assert_eq!(
            std::fs::read_to_string(workspace.join("starts")).unwrap(),
            "claude\ncodex\n"
        );
    }
}
