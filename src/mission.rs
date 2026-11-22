use crate::{
    auth::{Profile, Provider},
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
        let (sender, events) = mpsc::sync_channel(64);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            let result = (|| {
                let (index, command) = select_command(&profiles, preferred, usage::now())?;
                run(
                    (command, profiles[index].clone(), index, index != preferred),
                    workspace,
                    prompt,
                    &sender,
                    &worker_cancel,
                )
            })();
            let _ = sender.send(Event::Finished(result));
        });
        Self { events, cancel }
    }
}

fn select_command(
    profiles: &[Profile],
    preferred: usize,
    now: i64,
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

fn run(
    selection: (std::process::Command, Profile, usize, bool),
    workspace: PathBuf,
    prompt: String,
    sender: &SyncSender<Event>,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let (mut command, profile, index, rotated) = selection;
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
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
    let mut child = command
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("mission stdout is piped");
    let stdout_sender = sender.clone();
    let _ = sender.send(Event::Running {
        profile: index,
        rotated,
    });
    let limited = Arc::new(AtomicBool::new(false));
    let output_limit = Arc::clone(&limited);
    let reader =
        thread::spawn(move || read_output(&profile, stdout, &stdout_sender, &output_limit));
    let mut stdin = child.stdin.take().expect("mission stdin is piped");
    let writer = thread::spawn(move || stdin.write_all(prompt.as_bytes()));
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            child.wait()?;
            return Ok(());
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
    reader
        .join()
        .map_err(|_| io::Error::other("Mission output reader stopped"))??;
    if limited.load(Ordering::Acquire) {
        return Err(io::Error::other(
            "Provider usage limit reached; mission stopped without replay",
        ));
    }
    if !status.success() {
        return Err(io::Error::other(format!("Provider exited with {status}")));
    }
    writer
        .join()
        .map_err(|_| io::Error::other("Mission input writer stopped"))?
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
) -> io::Result<()> {
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let observed = usage::now();
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
                    return Ok(());
                }
            }
        }
    }
    Ok(())
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
mod tests {
    use super::*;
    use crate::usage::tests::Fixture;
    use serde_json::json;

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
                "status": "rejected", "resetsAt": now + 3600,
                "unifiedWindows": {
                    "five_hour": {"utilization": 0.5, "resetsAt": now + 3600},
                    "seven_day": {"utilization": 0.5, "resetsAt": now + 3600}
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
        for provider in [Provider::Claude, Provider::Codex] {
            let selected = fixture.profile(provider, "original");
            let standby = fixture.profile(provider, "standby");
            if provider == Provider::Claude {
                Usage::observe_claude(&standby, &json!({
                    "type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{
                        "five_hour":{"utilization":0.25,"resetsAt":4102444800_i64},
                        "seven_day":{"utilization":0.25,"resetsAt":4102444800_i64}
                    }}
                }), usage::now()).unwrap();
            }
            let mission = Mission::launch(
                vec![selected.clone(), standby],
                0,
                fixture.root.clone(),
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
            assert!(result.unwrap_err().to_string().contains("without replay"));
            assert!(Usage::read(&selected).unwrap().blocked(usage::now()));
        }
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("starts")).unwrap(),
            "claude\ncodex\n"
        );
    }
}
