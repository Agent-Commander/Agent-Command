use crate::{
    auth::{Profile, Provider},
    catalog::Catalog,
    checkpoint::{Checkpoint, Claim, Progress},
    routing::{self, Candidate, Configuration, Request},
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
    Running {
        profile: usize,
        reason: Option<String>,
    },
    Output(String),
    Continuing {
        from: usize,
        to: usize,
    },
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

pub(crate) struct Route {
    pub(crate) request: Request,
    pub(crate) preferred: Option<String>,
    pub(crate) catalog: Catalog,
}

impl Route {
    #[cfg(test)]
    pub(crate) fn single(prompt: &str, preferred: Option<String>) -> Self {
        Self {
            request: Request {
                units: vec![prompt.into()],
                ..Request::default()
            },
            preferred,
            catalog: Catalog::default(),
        }
    }
}

impl Mission {
    pub(crate) fn launch(profiles: Vec<Profile>, route: Route, workspace: PathBuf) -> Self {
        Self::launch_with_clock(profiles, route, workspace, usage::now)
    }

    fn launch_with_clock(
        profiles: Vec<Profile>,
        route: Route,
        workspace: PathBuf,
        clock: fn() -> usage::Tick,
    ) -> Self {
        Self::start(
            Execution {
                profiles,
                workspace,
                clock,
            },
            Work::Routed(route),
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
                clock: usage::now,
            },
            Work::Resume {
                interrupted,
                prompt,
                claim,
            },
        )
    }

    fn start(execution: Execution, work: Work) -> Self {
        let (sender, events) = mpsc::sync_channel(64);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            let result = match work {
                Work::Routed(route) => execution.route(&route, &sender, &worker_cancel),
                Work::Resume {
                    interrupted,
                    prompt,
                    claim,
                } => execution.resume(interrupted, &prompt, claim, &sender, &worker_cancel),
            };
            let _ = sender.send(Event::Finished(result));
        });
        Self { events, cancel }
    }
}

enum Work {
    Routed(Route),
    Resume {
        interrupted: usize,
        prompt: String,
        claim: Claim,
    },
}

struct Execution {
    profiles: Vec<Profile>,
    workspace: PathBuf,
    clock: fn() -> usage::Tick,
}

fn default_configuration(provider: Provider) -> &'static Configuration {
    routing::supported(provider, "default", "default").expect("provider default configuration")
}

impl Execution {
    fn route(
        &self,
        route: &Route,
        sender: &SyncSender<Event>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        let now = (self.clock)();
        let (candidates, exclusions) = routing::candidates(&self.profiles, &route.catalog, now);
        let ranked = routing::rank(candidates, route.request.policy, route.request.difficulty);
        if route.request.split {
            return self.run_split(&route.request.units, ranked, &exclusions, sender, cancel);
        }
        let rotation = self.rotation(route.preferred.as_deref(), now);
        let choice = routing::choose(ranked, &exclusions, route.preferred.as_deref(), &rotation)
            .map_err(io::Error::other)?;
        self.dispatch(
            &choice.candidate,
            &route.request.units[0],
            Some(choice.reason),
            sender,
            cancel,
        )
    }

    fn rotation(&self, preferred: Option<&str>, now: usage::Tick) -> Vec<String> {
        let Some(index) = preferred.and_then(|label| {
            self.profiles
                .iter()
                .position(|profile| profile.to_string() == label)
        }) else {
            return Vec::new();
        };
        usage::choices(&self.profiles, index, now)
            .unwrap_or_default()
            .into_iter()
            .map(|index| self.profiles[index].to_string())
            .collect()
    }

    fn run_split(
        &self,
        units: &[String],
        ranked: Result<Vec<Candidate>, String>,
        exclusions: &[routing::Exclusion],
        sender: &SyncSender<Event>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        let ranked = routing::require_ranked(ranked, exclusions).map_err(io::Error::other)?;
        let plan = routing::allocate(units.len(), &ranked);
        if plan.len() != units.len() {
            return Err(io::Error::other(
                "No eligible profile has headroom for every split unit.",
            ));
        }
        for (position, (unit, candidate)) in units.iter().zip(&plan).enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let usage = Usage::read(&self.profiles[candidate.profile]);
            if let Some(reason) = routing::allowance_refusal(&usage, (self.clock)()) {
                return Err(io::Error::other(format!(
                    "Split unit {} of {} stopped: {} {reason}.",
                    position + 1,
                    units.len(),
                    candidate.label
                )));
            }
            let reason = format!(
                "Unit {}/{} → {}",
                position + 1,
                units.len(),
                routing::describe(candidate)
            );
            self.dispatch(candidate, unit, Some(reason), sender, cancel)?;
        }
        Ok(())
    }

    fn dispatch(
        &self,
        candidate: &Candidate,
        prompt: &str,
        reason: Option<String>,
        sender: &SyncSender<Event>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        let index = candidate.profile;
        let profile = &self.profiles[index];
        let command = profile.authenticated_command().map_err(|error| {
            io::Error::other(format!("{profile} sign-in refused at dispatch: {error}"))
        })?;
        let limit = run(
            Launch {
                command,
                profile: profile.clone(),
                index,
                reason,
                configuration: candidate.configuration,
            },
            &self.workspace,
            prompt.to_owned(),
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
        self.continue_with(
            (index, to, next),
            candidate.configuration,
            prompt,
            &claim,
            sender,
            cancel,
        )
    }

    fn resume(
        &self,
        index: usize,
        prompt: &str,
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
        if !Usage::read(profile)?.eligible_alternate((self.clock)()) {
            return Err(io::Error::other(
                "Claimed alternate is latched or has an exhausted usage window",
            ));
        }
        let next = profile.authenticated_command()?;
        self.continue_with(
            (index, to, next),
            default_configuration(profile.provider),
            prompt,
            &claim,
            sender,
            cancel,
        )
    }

    fn continue_with(
        &self,
        selection: (usize, usize, std::process::Command),
        configuration: &'static Configuration,
        prompt: &str,
        claim: &Claim,
        sender: &SyncSender<Event>,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        let (index, to, command) = selection;
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if run(
            Launch {
                command,
                profile: self.profiles[to].clone(),
                index: to,
                reason: None,
                configuration,
            },
            &self.workspace,
            prompt.to_owned(),
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

struct Launch {
    command: std::process::Command,
    profile: Profile,
    index: usize,
    reason: Option<String>,
    configuration: &'static Configuration,
}

struct Continuation<'a> {
    claim: &'a Claim,
    from: usize,
    profile: &'a Profile,
}

fn run(
    launch: Launch,
    workspace: &std::path::Path,
    prompt: String,
    sender: &SyncSender<Event>,
    cancel: &AtomicBool,
    clock: fn() -> usage::Tick,
    continuation: Option<Continuation<'_>>,
) -> io::Result<Option<Progress>> {
    let Launch {
        mut command,
        profile,
        index,
        reason,
        configuration,
    } = launch;
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    command.args(configuration.arguments());
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
        reason,
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

    fn prefer(profiles: &[Profile], prompt: &str) -> Route {
        Route::single(prompt, Some(profiles[0].to_string()))
    }

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
    if test "$profile" = standby && test -f "$work/.git/change-after-checkpoint" \
        && ls "${CLAUDE_CONFIG_DIR%/claude/*}"/agent-command-checkpoints-v1/*.json >/dev/null 2>&1; then
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
      *'"kind":"untracked","path":"untracked","index_sha256":null,"worktree_sha256":"373ca25fde04545677f483dcec193a407a9e61ba46dec587ba51f7275332b19d"'*)
        case "$prompt" in *"synthetic file contents"*|*"fixture-credential-never-persist"*) exit 11;; esac
        ;;
      *) exit 10 ;;
    esac
fi
case "$prompt" in
  *"do not redo completed work"*"Retained workspace manifest:"*'"kind":"tracked","path":"completed","index_sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","worktree_sha256":"d117fa006ba9208500b2930ce69cbde436c647afa917cb7396a9bc9111a46dd2"'*"Keep the finished step"*) ;;
  *) exit 9 ;;
esac
if test -f .git/reject-continuation; then
    printf 'rejected\n' >> completed
    printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"seven_day","utilization":1,"resetsAt":4102444800}}'
    exit 1
fi
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
                "type":"rate_limit_event","rate_limit_info":{
                    "status":"allowed_warning","rateLimitType":"seven_day",
                    "utilization":0.97,"resetsAt":now.wall + 8000
                }
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
    fn stale_weekly_only_alternate_continues_once() {
        let (_fixture, workspace, profiles) = limit_fixture("continue", usage::tick(FAKE_NOW, 0));
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            prefer(&profiles, "Keep the finished step"),
            workspace.clone(),
            || usage::tick(FAKE_NOW + 901, 901),
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
    fn exhausted_latched_or_corrupt_alternates_never_start() {
        for (case, status, utilization) in [
            ("latched", "rejected", 0.5),
            ("exhausted", "allowed_warning", 1.0),
            ("over-limit", "allowed_warning", 1.02),
            ("corrupt", "allowed", 0.5),
        ] {
            let now = usage::tick(FAKE_NOW, 0);
            let (_fixture, workspace, profiles) = limit_fixture(case, now);
            Usage::observe_claude(
                &profiles[1],
                &json!({"type":"rate_limit_event","rate_limit_info":{
                    "status":status,"rateLimitType":"seven_day",
                    "utilization":utilization,"resetsAt":FAKE_NOW + 3600
                }}),
                now,
            )
            .unwrap();
            if case == "corrupt" {
                std::fs::write(
                    profiles[1].usage_directory().unwrap().join("state.json"),
                    b"{",
                )
                .unwrap();
            }
            let route = prefer(&profiles, "Keep the finished step");
            let mission = Mission::launch_with_clock(profiles, route, workspace.clone(), || {
                usage::tick(FAKE_NOW, 0)
            });
            let (links, result) = drain(&mission);
            assert!(links.is_empty(), "{case}");
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("No eligible signed-in alternate"),
                "{case}"
            );
            assert_eq!(
                std::fs::read_to_string(workspace.join("completed")).unwrap(),
                "done\n"
            );
        }
    }

    #[test]
    fn rejected_unobserved_continuation_latches_and_does_not_try_third_profile() {
        let (fixture, workspace, mut profiles) =
            limit_fixture("rejected-continuation", usage::tick(FAKE_NOW, 0));
        std::fs::remove_file(profiles[1].usage_directory().unwrap().join("state.json")).unwrap();
        profiles.push(fixture.profile(Provider::Claude, "third"));
        std::fs::write(workspace.join(".git/reject-continuation"), "").unwrap();
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            prefer(&profiles, "Keep the finished step"),
            workspace.clone(),
            || usage::tick(FAKE_NOW, 0),
        );
        let (links, result) = drain(&mission);
        assert_eq!(links, [(0, 1)]);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Continuation hit a usage limit"));
        assert!(Usage::read(&profiles[1])
            .unwrap()
            .blocked(usage::tick(FAKE_NOW, 0)));
        assert_eq!(
            std::fs::read_to_string(workspace.join("completed")).unwrap(),
            "done\nrejected\n"
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
            std::fs::remove_file(profiles[1].usage_directory().unwrap().join("state.json"))
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
            prefer(&profiles, "Keep the finished step"),
            workspace.clone(),
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
            prefer(&profiles, "Keep the finished step"),
            workspace.clone(),
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
        let route = prefer(&profiles, "New work");
        let mission = Mission::launch(profiles.into(), route, fixture.root.clone());
        assert!(matches!(
            mission.events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Running {
                profile: 2,
                reason: Some(reason)
            } if reason.starts_with("Override claude/first skipped: latched · rotated → claude/bravo")
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
            let route = prefer(std::slice::from_ref(&selected), "Do work");
            let mission = Mission::launch(vec![selected.clone()], route, workspace.clone());
            assert!(matches!(
                mission.events.recv_timeout(Duration::from_secs(3)).unwrap(),
                Event::Running {
                    profile: 0,
                    reason: Some(_)
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

    const ROUTED_PROVIDER: &str = r#"
case "$1" in
    auth) printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'; exit ;;
    -c) if test "$5" = login; then printf '%s\n' 'Logged in using ChatGPT' >&2; exit; fi ;;
esac
mkdir running.lock || exit 5
prompt=$(cat)
profile=${CLAUDE_CONFIG_DIR##*/}
test -n "$CLAUDE_CONFIG_DIR" || profile=${CODEX_HOME##*/}
printf '%s|%s|%s\n' "$profile" "$*" "$prompt" >> starts
sleep 0.05
rmdir running.lock
"#;

    fn synthetic_catalog(rows: Vec<crate::catalog::Row>) -> Catalog {
        use crate::catalog::{Endpoint, Origin, Snapshot, Source};
        Catalog {
            sources: vec![Source {
                endpoint: Endpoint::AgentsCoding,
                origin: Origin::Cache,
                snapshot: Snapshot::sealed(
                    Endpoint::AgentsCoding,
                    "synthetic-1".into(),
                    "synthetic test fixture".into(),
                    rows,
                ),
            }],
        }
    }

    fn agent_row(provider: &str, model: &str, effort: &str, index: f64) -> crate::catalog::Row {
        crate::catalog::Row {
            provider: provider.into(),
            model: model.into(),
            effort: effort.into(),
            coding_index: Some(index),
            speed: None,
        }
    }

    fn finish(mission: &Mission) -> (Vec<String>, io::Result<()>) {
        let mut reasons = Vec::new();
        loop {
            match mission.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Running {
                    reason: Some(reason),
                    ..
                } => reasons.push(reason),
                Event::Finished(result) => return (reasons, result),
                _ => {}
            }
        }
    }

    #[test]
    fn routed_dispatch_passes_selected_model_and_effort_to_native_cli() {
        let fixture = Fixture::new("routed-arguments", ROUTED_PROVIDER);
        let profiles = vec![
            fixture.profile(Provider::Claude, "alpha"),
            fixture.profile(Provider::Codex, "beta"),
        ];
        let rows = || {
            vec![
                agent_row("claude", "opus", "default", 60.0),
                agent_row("codex", "gpt-5-codex", "high", 70.0),
            ]
        };
        let route = |difficulty, preferred: Option<&str>, rows| Route {
            request: Request {
                difficulty,
                units: vec!["Routed work".into()],
                ..Request::default()
            },
            preferred: preferred.map(str::to_owned),
            catalog: synthetic_catalog(rows),
        };
        for (difficulty, preferred) in [
            (routing::Difficulty::Hard, None),
            (routing::Difficulty::Hard, Some("claude/alpha")),
        ] {
            let mission = Mission::launch(
                profiles.clone(),
                route(difficulty, preferred, rows()),
                fixture.root.clone(),
            );
            let (reasons, result) = finish(&mission);
            result.unwrap();
            assert_eq!(reasons.len(), 1);
        }
        let starts = std::fs::read_to_string(fixture.root.join("starts")).unwrap();
        assert_eq!(
            starts,
            "beta|-c cli_auth_credentials_store=\"file\" -c forced_login_method=\"chatgpt\" -c model=\"gpt-5-codex\" -c model_reasoning_effort=\"high\" -a never exec --json --skip-git-repo-check --sandbox workspace-write -|Routed work\n\
alpha|--model opus --print --verbose --output-format stream-json --setting-sources user --|Routed work\n"
        );
        let below = vec![agent_row("claude", "opus", "default", 50.0)];
        let mission = Mission::launch(
            profiles,
            route(routing::Difficulty::Hard, None, below),
            fixture.root.clone(),
        );
        let (reasons, result) = finish(&mission);
        assert!(reasons.is_empty());
        assert_eq!(
            result.unwrap_err().to_string(),
            "No eligible model meets the hard floor 55; best published index is 50.0."
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("starts")).unwrap(),
            starts
        );
    }

    #[test]
    fn split_mission_sends_each_unit_once_in_sequence_to_allocated_profiles() {
        let fixture = Fixture::new("split-mission", ROUTED_PROVIDER);
        let profiles = vec![
            fixture.profile(Provider::Claude, "big"),
            fixture.profile(Provider::Claude, "latched"),
            fixture.profile(Provider::Claude, "small"),
        ];
        let now = usage::now();
        for (profile, status, used) in [
            (&profiles[0], "allowed", 0.4),
            (&profiles[1], "rejected", 0.1),
            (&profiles[2], "allowed", 0.7),
        ] {
            Usage::observe_claude(
                profile,
                &json!({"type":"rate_limit_event","rate_limit_info":{
                    "status":status,"resetsAt":now.wall + 3600,
                    "unifiedWindows":{"five_hour":{"utilization":used,"resetsAt":now.wall + 3600}}
                }}),
                now,
            )
            .unwrap();
        }
        let request = routing::parse("/split unit a ;; unit b ;; unit c").unwrap();
        let mission = Mission::launch(
            profiles,
            Route {
                request,
                preferred: None,
                catalog: Catalog::default(),
            },
            fixture.root.clone(),
        );
        let (reasons, result) = finish(&mission);
        result.unwrap();
        assert_eq!(reasons.len(), 3);
        assert!(reasons[0].starts_with("Unit 1/3 → claude/big provider default"));
        assert!(reasons[2].starts_with("Unit 3/3 → claude/small provider default"));
        let starts = std::fs::read_to_string(fixture.root.join("starts")).unwrap();
        let sent: Vec<_> = starts
            .lines()
            .map(|line| {
                let mut fields = line.split('|');
                (fields.next().unwrap(), fields.nth(1).unwrap())
            })
            .collect();
        assert_eq!(
            sent,
            [("big", "unit a"), ("big", "unit b"), ("small", "unit c")]
        );
        let none = Mission::launch(
            vec![fixture.profile(Provider::Claude, "latched")],
            Route {
                request: routing::parse("/split x ;; y").unwrap(),
                preferred: None,
                catalog: Catalog::default(),
            },
            fixture.root.clone(),
        );
        assert_eq!(
            finish(&none).1.unwrap_err().to_string(),
            "No eligible signed-in subscription profile. Skipped: claude/latched latched."
        );
    }
}
