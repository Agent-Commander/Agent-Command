use crate::auth::{Profile, Provider};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const POLL_SECONDS: i64 = 300;
const STALE_SECONDS: i64 = 900;

pub(crate) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(crate) struct Window {
    used: f64,
    reset: i64,
    observed: i64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub(crate) struct Usage {
    five: Option<Window>,
    week: Option<Window>,
    latch: Option<i64>,
    next_poll: Option<i64>,
    unavailable: bool,
}

impl Usage {
    pub(crate) fn read(profile: &Profile) -> io::Result<Self> {
        read_state(&profile.usage_directory()?)
    }

    pub(crate) fn blocked(self, now: i64) -> bool {
        self.latch.is_some_and(|reset| reset > now)
    }

    pub(crate) fn eligible_alternate(self, now: i64) -> bool {
        !self.blocked(now)
            && !self.unavailable
            && [self.five, self.week].into_iter().all(|window| {
                window.is_some_and(|window| {
                    window.used < 100.0
                        && window.reset > now
                        && now.saturating_sub(window.observed) < STALE_SECONDS
                })
            })
    }

    pub(crate) fn rows(self, now: i64) -> [String; 2] {
        let row = |name, window: Option<Window>| match window {
            Some(window) => {
                let age = now.saturating_sub(window.observed);
                let stale = self.unavailable || age >= STALE_SECONDS || now >= window.reset;
                format!(
                    "{name} {:.0}% used / {:.0}% left, reset {} · {}{}",
                    window.used,
                    100.0 - window.used,
                    duration(window.reset.saturating_sub(now)),
                    if stale { "stale " } else { "age " },
                    duration(age)
                )
            }
            None => format!("{name} unavailable"),
        };
        [row("5h", self.five), row("Week", self.week)]
    }

    pub(crate) fn latch_label(self, now: i64) -> String {
        match self.latch.filter(|reset| *reset > now) {
            Some(i64::MAX) => "Latched · reset unknown".into(),
            Some(reset) => format!("Latched {}", duration(reset - now)),
            None => "No latch".into(),
        }
    }

    pub(crate) fn codex_due(self, now: i64) -> bool {
        !self.blocked(now) && self.next_poll.is_none_or(|next| next <= now)
    }

    pub(crate) fn observe_claude(profile: &Profile, event: &Value, now: i64) -> io::Result<bool> {
        if event["type"] != "rate_limit_event" {
            return Ok(false);
        }
        let info = &event["rate_limit_info"];
        let limited = info["status"] == "rejected";
        update(profile, |state| {
            let windows = &info["unifiedWindows"];
            let mut five = claude_window(&windows["five_hour"], now);
            let mut week = claude_window(&windows["seven_day"], now);
            match info["rateLimitType"].as_str() {
                Some("five_hour") => five = five.or_else(|| claude_window(info, now)),
                Some("seven_day") => week = week.or_else(|| claude_window(info, now)),
                _ => {}
            }
            state.five = five.or(state.five);
            state.week = week.or(state.week);
            state.unavailable = five.is_none() && week.is_none();
            if limited {
                state.record_limit(info["resetsAt"].as_i64(), now);
            }
            Ok(())
        })?;
        Ok(limited)
    }

    pub(crate) fn record_codex_limit(profile: &Profile, now: i64) -> io::Result<()> {
        update(profile, |state| {
            let reset = [state.five, state.week]
                .into_iter()
                .flatten()
                .filter(|window| window.used >= 100.0 && window.reset > now)
                .map(|window| window.reset)
                .max();
            state.record_limit(reset, now);
            Ok(())
        })
    }

    fn record_limit(&mut self, reset: Option<i64>, now: i64) {
        let reset = reset.filter(|reset| *reset > now).unwrap_or(i64::MAX);
        self.latch = Some(self.latch.unwrap_or_default().max(reset));
    }
}

fn duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, seconds / 60 % 60),
    }
}

fn window(used: f64, reset: i64, observed: i64) -> Option<Window> {
    ((0.0..=100.0).contains(&used) && (observed..i64::MAX).contains(&reset)).then_some(Window {
        used,
        reset,
        observed,
    })
}

fn claude_window(raw: &Value, now: i64) -> Option<Window> {
    window(
        raw["utilization"].as_f64()? * 100.0,
        raw["resetsAt"].as_i64()?,
        now,
    )
}

fn read_state(directory: &Path) -> io::Result<Usage> {
    match fs::read(directory.join("state.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Usage::default()),
        Err(error) => Err(error),
    }
}

fn update<T>(profile: &Profile, edit: impl FnOnce(&mut Usage) -> io::Result<T>) -> io::Result<T> {
    let directory = profile.usage_directory()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("lock"))?;
    lock.lock()?;
    let mut state = read_state(&directory)?;
    let outcome = edit(&mut state)?;
    let temporary = directory.join("state.next");
    let mut file = File::create(&temporary)?;
    serde_json::to_writer(&mut file, &state)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, directory.join("state.json"))?;
    Ok(outcome)
}

pub(crate) fn choices(profiles: &[Profile], preferred: usize, now: i64) -> io::Result<Vec<usize>> {
    let selected = &profiles[preferred];
    if !Usage::read(selected)?.blocked(now) {
        return Ok(vec![preferred]);
    }
    let mut alternates: Vec<_> = profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| profile.provider == selected.provider)
        .filter(|(_, profile)| {
            Usage::read(profile).is_ok_and(|usage| usage.eligible_alternate(now))
        })
        .map(|(index, profile)| (profile.to_string(), index))
        .collect();
    alternates.sort_unstable();
    Ok(alternates.into_iter().map(|(_, index)| index).collect())
}

fn codex_windows(
    result: &Value,
    now: i64,
) -> (Option<Window>, Option<Window>, Option<Option<i64>>) {
    let limits = result["rateLimitsByLimitId"]
        .get("codex")
        .unwrap_or(&result["rateLimits"]);
    let buckets = [&limits["primary"], &limits["secondary"]];
    let by_duration = |minutes| {
        let raw = buckets
            .into_iter()
            .find(|raw| raw["windowDurationMins"].as_i64() == Some(minutes))?;
        window(raw["usedPercent"].as_f64()?, raw["resetsAt"].as_i64()?, now)
    };
    let exhausted = buckets
        .into_iter()
        .filter(|raw| {
            raw["usedPercent"]
                .as_f64()
                .is_some_and(|used| used >= 100.0)
        })
        .collect::<Vec<_>>();
    let reset = exhausted
        .iter()
        .filter_map(|raw| raw["resetsAt"].as_i64())
        .max();
    let limited = !exhausted.is_empty() || !limits["rateLimitReachedType"].is_null();
    (
        by_duration(300),
        by_duration(10080),
        limited.then_some(reset),
    )
}

pub(crate) fn refresh_codex(profile: &Profile, now: i64) -> io::Result<()> {
    if profile.provider != Provider::Codex || !Usage::read(profile)?.codex_due(now) {
        return Ok(());
    }
    let reserved = update(profile, |state| {
        if !state.codex_due(now) {
            return Ok(false);
        }
        state.next_poll = Some(now.saturating_add(POLL_SECONDS));
        Ok(true)
    })?;
    if !reserved {
        return Ok(());
    }
    let result = codex_readback(profile);
    update(profile, |state| {
        if let Ok(result) = result {
            let (five, week, limit) = codex_windows(&result, now);
            state.five = five.or(state.five);
            state.week = week.or(state.week);
            state.unavailable = five.is_none() || week.is_none();
            if let Some(reset) = limit {
                state.record_limit(reset, now);
            }
        } else {
            state.unavailable = true;
        }
        Ok(())
    })
}

fn codex_readback(profile: &Profile) -> io::Result<Value> {
    let (mut command, directory) = profile.isolated_command()?;
    command
        .current_dir(directory)
        .args(["app-server", "--listen", "stdio://"]);
    exchange(&mut command, Duration::from_secs(15))
}

fn exchange(command: &mut Command, timeout: Duration) -> io::Result<Value> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut input = child.stdin.take().expect("piped input");
    let output = child.stdout.take().expect("piped output");
    let (sender, receiver) = mpsc::sync_channel(8);
    let reader = thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if sender.send(value).is_err() {
                    break;
                }
            }
        }
    });
    let result = (|| {
        let deadline = Instant::now() + timeout;
        writeln!(
            input,
            "{}",
            serde_json::json!({
                "id": 1,
                "method": "initialize",
                "params": {"clientInfo": {"name": "agent-command", "version": env!("CARGO_PKG_VERSION")}}
            })
        )?;
        wait_reply(&receiver, deadline, 1)?;
        writeln!(input, r#"{{"method":"initialized"}}"#)?;
        let account = request_account(&mut input, &receiver, deadline)?;
        if account["account"]["type"] != "chatgpt" {
            return Err(io::Error::other(
                "Codex profile is not signed in with ChatGPT",
            ));
        }
        writeln!(input, r#"{{"id":3,"method":"account/rateLimits/read"}}"#)?;
        wait_reply(&receiver, deadline, 3)
    })();
    drop(input);
    drop(receiver);
    let _ = child.kill();
    child.wait()?;
    let _ = reader.join();
    result
}

fn request_account(
    input: &mut impl Write,
    receiver: &mpsc::Receiver<Value>,
    deadline: Instant,
) -> io::Result<Value> {
    writeln!(
        input,
        r#"{{"id":2,"method":"account/read","params":{{"refreshToken":false}}}}"#
    )?;
    wait_reply(receiver, deadline, 2)
}

fn wait_reply(receiver: &mpsc::Receiver<Value>, deadline: Instant, id: i64) -> io::Result<Value> {
    loop {
        let value = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| io::Error::other("Codex usage readback unavailable"))?;
        if value["id"] == id {
            return value
                .get("result")
                .cloned()
                .ok_or_else(|| io::Error::other("Codex rejected the usage request"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("Codex usage readback timed out"));
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::{os::unix::fs::PermissionsExt, path::PathBuf};

    pub(crate) struct Fixture {
        pub(crate) root: PathBuf,
        pub(crate) executable: PathBuf,
    }

    impl Fixture {
        pub(crate) fn new(name: &str, script: &str) -> Self {
            let root = std::env::temp_dir().join(format!("ac-{name}-{}", std::process::id()));
            fs::create_dir(&root).unwrap();
            let executable = root.join("provider");
            fs::write(&executable, format!("#!/bin/sh\n{script}")).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            Self { root, executable }
        }

        pub(crate) fn profile(&self, provider: Provider, name: &str) -> Profile {
            Profile::named_fixture(
                &self.root.join("profiles"),
                provider,
                name,
                self.executable.clone(),
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const CLOCK: i64 = 1_000;

    fn claude_status(status: &str) -> Value {
        json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": status, "resetsAt": 2000,
                "unifiedWindows": {
                    "five_hour": {"utilization": 0.25, "resetsAt": 2000},
                    "seven_day": {"utilization": 0.6, "resetsAt": 3000}
                }
            }
        })
    }

    #[test]
    fn forced_latch_survives_reload_and_never_shortens() {
        let fixture = Fixture::new("latch", "");
        let profile = fixture.profile(Provider::Claude, "selected");
        assert!(Usage::observe_claude(&profile, &claude_status("rejected"), CLOCK).unwrap());
        let mut early_reset = claude_status("rejected");
        early_reset["rate_limit_info"]["resetsAt"] = json!(1500);
        Usage::observe_claude(&profile, &early_reset, CLOCK).unwrap();
        Usage::observe_claude(&profile, &claude_status("allowed"), CLOCK).unwrap();
        let reopened = fixture.profile(Provider::Claude, "selected");
        let state = Usage::read(&reopened).unwrap();
        assert!(state.blocked(1999));
        assert!(!state.blocked(2000));
        assert_eq!(state.latch_label(1999), "Latched 1s");
        assert!(state.rows(1950)[0].contains("stale 15m50s"));
    }

    #[test]
    fn rotation_order_uses_fresh_same_provider_allowances() {
        let fixture = Fixture::new("rotation", "");
        let profiles = [
            fixture.profile(Provider::Claude, "selected"),
            fixture.profile(Provider::Claude, "zebra"),
            fixture.profile(Provider::Claude, "alpha"),
            fixture.profile(Provider::Codex, "first"),
            fixture.profile(Provider::Claude, "unobserved"),
        ];
        assert_eq!(choices(&profiles, 0, CLOCK).unwrap(), [0]);
        Usage::observe_claude(&profiles[0], &claude_status("rejected"), CLOCK).unwrap();
        for profile in &profiles[1..3] {
            Usage::observe_claude(profile, &claude_status("allowed"), CLOCK).unwrap();
        }
        assert_eq!(choices(&profiles, 0, CLOCK).unwrap(), [2, 1]);
        assert!(choices(&profiles, 0, CLOCK + STALE_SECONDS)
            .unwrap()
            .is_empty());
        assert_eq!(choices(&profiles, 0, 2000).unwrap(), [0]);
    }

    #[test]
    fn codex_profile_probe_is_paced_across_reopen_and_maps_window_lengths() {
        let fixture = Fixture::new(
            "pace",
            r#"
test "$5" = app-server || exit 9
while IFS= read -r request; do
  case "$request" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}' ;;
    *'"method":"account/rateLimits/read"'*)
      printf 'probe\n' >> calls
      printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":33,"resetsAt":5000,"windowDurationMins":10080},"secondary":{"usedPercent":42,"resetsAt":4000,"windowDurationMins":300}}}}'
      ;;
  esac
done
"#,
        );
        let profile = fixture.profile(Provider::Codex, "selected");
        refresh_codex(&profile, CLOCK).unwrap();
        let reopened = fixture.profile(Provider::Codex, "selected");
        refresh_codex(&reopened, CLOCK + POLL_SECONDS - 1).unwrap();
        let (_, directory) = profile.isolated_command().unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("calls")).unwrap(),
            "probe\n"
        );
        let rows = Usage::read(&profile).unwrap().rows(CLOCK);
        assert!(rows[0].contains("42% used / 58% left"));
        assert!(rows[1].contains("33% used / 67% left"));
        refresh_codex(&reopened, CLOCK + POLL_SECONDS).unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("calls")).unwrap(),
            "probe\nprobe\n"
        );
    }
}
