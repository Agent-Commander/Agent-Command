use crate::auth::{Profile, Provider};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

const SECOND: u64 = 1_000_000_000;
const POLL_SECONDS: u64 = 300;
const STALE_SECONDS: u64 = 900;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tick {
    pub(crate) wall: i64,
    pub(crate) elapsed: u64,
    epoch: uuid::Uuid,
}

pub(crate) fn now() -> Tick {
    static CLOCK: OnceLock<(Instant, uuid::Uuid)> = OnceLock::new();
    let (start, epoch) = CLOCK.get_or_init(|| (Instant::now(), uuid::Uuid::new_v4()));
    Tick {
        wall: chrono::Utc::now().timestamp(),
        elapsed: start.elapsed().as_nanos().try_into().unwrap_or(u64::MAX),
        epoch: *epoch,
    }
}

#[cfg(test)]
pub(crate) fn tick(wall: i64, elapsed: u64) -> Tick {
    Tick {
        wall,
        elapsed: elapsed.saturating_mul(SECOND),
        ..now()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct Stamp {
    elapsed: u64,
    epoch: uuid::Uuid,
}

impl From<Tick> for Stamp {
    fn from(tick: Tick) -> Self {
        Self {
            elapsed: tick.elapsed,
            epoch: tick.epoch,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct Probe {
    id: uuid::Uuid,
    started: Stamp,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(crate) struct Window {
    used: f64,
    reset: i64,
    observed: i64,
    stamp: Option<Stamp>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub(crate) struct Usage {
    five: Option<Window>,
    week: Option<Window>,
    latch: Option<i64>,
    next_poll: Option<i64>,
    probe: Option<Probe>,
    unavailable: bool,
}

impl Usage {
    pub(crate) fn read(profile: &Profile) -> io::Result<Self> {
        read_state(&profile.usage_directory()?)
    }

    pub(crate) fn blocked(self, now: Tick) -> bool {
        self.latch.is_some_and(|reset| reset > now.wall)
    }

    pub(crate) fn eligible_alternate(self, now: Tick) -> bool {
        !self.blocked(now)
            && !self.unavailable
            && !self.needs_reading()
            && [self.five, self.week].into_iter().all(|window| {
                window.is_some_and(|window| {
                    window.used < 100.0
                        && window.reset > now.wall
                        && observation_age(window, now).is_some_and(|age| age < STALE_SECONDS)
                })
            })
    }

    pub(crate) fn can_resume_claim(self, now: Tick) -> bool {
        !self.blocked(now)
            && !self.needs_reading()
            && !self.unavailable
            && [self.five, self.week].into_iter().all(|window| {
                window.is_some_and(|window| {
                    window.used < 100.0 && window.observed <= now.wall && window.reset > now.wall
                })
            })
    }

    pub(crate) fn rows(self, now: Tick) -> [String; 2] {
        let row = |name, window: Option<Window>| match window {
            Some(window) => {
                let age = observation_age(window, now);
                let stale = self.unavailable
                    || self.needs_reading()
                    || age.is_none_or(|age| age >= STALE_SECONDS)
                    || now.wall >= window.reset;
                format!(
                    "{name} {:.0}% used / {:.0}% left, reset {} · {}{}",
                    window.used,
                    100.0 - window.used,
                    duration(window.reset.saturating_sub(now.wall)),
                    if stale { "stale " } else { "age " },
                    age.map_or_else(
                        || "age unknown".into(),
                        |age| duration(age.try_into().unwrap_or(i64::MAX))
                    )
                )
            }
            None => format!("{name} unavailable"),
        };
        [row("5h", self.five), row("Week", self.week)]
    }

    pub(crate) fn latch_label(self, now: Tick) -> String {
        match self.latch.filter(|reset| *reset > now.wall) {
            Some(i64::MAX) => "Latched · reset unknown".into(),
            Some(reset) => format!("Latched {}", duration(reset.saturating_sub(now.wall))),
            None if self.needs_reading() => "Needs fresh reading".into(),
            None => "No latch".into(),
        }
    }

    fn needs_reading(self) -> bool {
        self.latch.is_some()
    }

    pub(crate) fn codex_due(self, now: Tick) -> bool {
        (!self.blocked(now) || self.latch == Some(i64::MAX))
            && self.probe.is_none_or(|probe| {
                let start = if probe.started.epoch == now.epoch {
                    probe.started.elapsed
                } else {
                    static SEEN: OnceLock<Mutex<HashMap<uuid::Uuid, u64>>> = OnceLock::new();
                    *SEEN
                        .get_or_init(Default::default)
                        .lock()
                        .expect("probe observation lock")
                        .entry(probe.id)
                        .or_insert(now.elapsed)
                };
                now.elapsed.saturating_sub(start) >= POLL_SECONDS * SECOND
            })
            && (self.probe.is_some()
                || self.next_poll.is_none()
                || now.elapsed >= POLL_SECONDS * SECOND)
    }

    pub(crate) fn observe_claude(profile: &Profile, event: &Value, now: Tick) -> io::Result<bool> {
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
            } else if (five.is_some() || week.is_some())
                && (!state.blocked(now) || state.latch == Some(i64::MAX))
            {
                state.latch = None;
            }
            Ok(())
        })?;
        Ok(limited)
    }

    pub(crate) fn record_codex_limit(profile: &Profile, now: Tick) -> io::Result<()> {
        update(profile, |state| {
            let reset = [state.five, state.week]
                .into_iter()
                .flatten()
                .filter(|window| window.used >= 100.0 && window.reset > now.wall)
                .map(|window| window.reset)
                .max();
            state.record_limit(reset, now);
            Ok(())
        })
    }

    fn record_limit(&mut self, reset: Option<i64>, now: Tick) {
        let active = self
            .latch
            .filter(|epoch| *epoch > now.wall && *epoch != i64::MAX);
        self.latch = match reset {
            Some(reset) if reset != i64::MAX => {
                Some(active.map_or(reset, |epoch| epoch.max(reset)))
            }
            _ => Some(active.unwrap_or(i64::MAX)),
        };
        self.unavailable = true;
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

fn observation_age(window: Window, now: Tick) -> Option<u64> {
    let stamp = window.stamp?;
    (window.observed <= now.wall && stamp.epoch == now.epoch)
        .then(|| now.elapsed.checked_sub(stamp.elapsed))
        .flatten()
        .map(|age| age / SECOND)
}

fn window(used: f64, reset: i64, now: Tick) -> Option<Window> {
    ((0.0..=100.0).contains(&used) && (now.wall..i64::MAX).contains(&reset)).then_some(Window {
        used,
        reset,
        observed: now.wall,
        stamp: Some(now.into()),
    })
}

fn claude_window(raw: &Value, now: Tick) -> Option<Window> {
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

pub(crate) fn choices(profiles: &[Profile], preferred: usize, now: Tick) -> io::Result<Vec<usize>> {
    let selected = &profiles[preferred];
    if !Usage::read(selected)?.blocked(now) {
        return Ok(vec![preferred]);
    }
    Ok(alternates(profiles, preferred, now))
}

pub(crate) fn alternates(profiles: &[Profile], preferred: usize, now: Tick) -> Vec<usize> {
    let mut alternates: Vec<_> = profiles
        .iter()
        .enumerate()
        .filter(|(index, profile)| {
            *index != preferred && profile.provider == profiles[preferred].provider
        })
        .filter(|(_, profile)| {
            Usage::read(profile).is_ok_and(|usage| usage.eligible_alternate(now))
        })
        .map(|(index, profile)| (profile.to_string(), index))
        .collect();
    alternates.sort_unstable();
    alternates.into_iter().map(|(_, index)| index).collect()
}

fn codex_windows(
    result: &Value,
    now: Tick,
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

pub(crate) fn refresh_codex(profile: &Profile, now: Tick) -> io::Result<()> {
    if profile.provider != Provider::Codex || !Usage::read(profile)?.codex_due(now) {
        return Ok(());
    }
    let reserved = update(profile, |state| {
        if !state.codex_due(now) {
            return Ok(false);
        }
        state.probe = Some(Probe {
            id: uuid::Uuid::new_v4(),
            started: now.into(),
        });
        state.next_poll = None;
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
            } else if (five.is_some() || week.is_some())
                && (!state.blocked(now) || state.latch == Some(i64::MAX))
            {
                state.latch = None;
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
    fn backward_wall_clock_never_makes_a_future_observation_fresh() {
        let fixture = Fixture::new("backward-clock", "");
        let profile = fixture.profile(Provider::Claude, "selected");
        let observed = tick(CLOCK, 5);
        Usage::observe_claude(&profile, &claude_status("allowed"), observed).unwrap();
        let state = Usage::read(&profile).unwrap();
        assert!(state.eligible_alternate(observed));
        let backwards = tick(CLOCK - 3600, 6);
        assert!(!state.eligible_alternate(backwards));
        assert!(state.rows(backwards)[0].contains("stale age unknown"));
        assert!(state.rows(tick(CLOCK + 700, 10))[0].contains("age 5s"));
    }

    #[test]
    fn expired_limit_needs_fresh_usage_and_future_limits_repair_unknown() {
        let fixture = Fixture::new("past-reset", "");
        let profile = fixture.profile(Provider::Claude, "selected");
        let now = tick(CLOCK, 0);
        let mut rejected = claude_status("rejected");
        rejected["rate_limit_info"]["resetsAt"] = json!(CLOCK - 1);
        Usage::observe_claude(&profile, &rejected, now).unwrap();
        let state = Usage::read(&profile).unwrap();
        assert!(!state.blocked(now));
        assert!(!state.eligible_alternate(now));
        assert_eq!(state.latch_label(now), "Needs fresh reading");
        rejected["rate_limit_info"]["resetsAt"] = Value::Null;
        Usage::observe_claude(&profile, &rejected, now).unwrap();
        assert_eq!(
            Usage::read(&profile).unwrap().latch_label(now),
            "Latched · reset unknown"
        );
        Usage::observe_claude(&profile, &claude_status("rejected"), now).unwrap();
        let reopened = Usage::read(&profile).unwrap();
        assert_eq!(reopened.latch_label(now), "Latched 16m40s");
        assert!(!reopened.blocked(tick(2000, 1000)));
    }

    #[test]
    fn failed_codex_probe_respects_monotonic_backoff_after_clock_jump_and_reopen() {
        let fixture = Fixture::new("failed-probe", "printf 'attempt\n' >> attempts; exit 1");
        let profile = fixture.profile(Provider::Codex, "selected");
        let (_, directory) = profile.isolated_command().unwrap();
        refresh_codex(&profile, tick(CLOCK, 0)).unwrap();
        refresh_codex(&profile, tick(CLOCK + 300, 4)).unwrap();
        let mut reopened = tick(CLOCK + 300, 999);
        reopened.epoch = uuid::Uuid::new_v4();
        refresh_codex(&profile, reopened).unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("attempts")).unwrap(),
            "attempt\n"
        );
        reopened.elapsed += POLL_SECONDS * SECOND;
        refresh_codex(&profile, reopened).unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("attempts")).unwrap(),
            "attempt\nattempt\n"
        );
    }

    #[test]
    fn corrupt_usage_fails_closed_before_poll_or_selection() {
        let fixture = Fixture::new("corrupt-usage", "exit 9");
        let profile = fixture.profile(Provider::Codex, "selected");
        let path = profile.usage_directory().unwrap().join("state.json");
        fs::write(&path, b"invalid JSON").unwrap();
        let now = tick(CLOCK, 0);
        assert!(refresh_codex(&profile, now).is_err());
        assert!(choices(&[profile], 0, now).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"invalid JSON");
    }

    #[test]
    fn forced_latch_survives_reload_and_never_shortens() {
        let fixture = Fixture::new("latch", "");
        let profile = fixture.profile(Provider::Claude, "selected");
        assert!(
            Usage::observe_claude(&profile, &claude_status("rejected"), tick(CLOCK, 0)).unwrap()
        );
        let mut early_reset = claude_status("rejected");
        early_reset["rate_limit_info"]["resetsAt"] = json!(1500);
        Usage::observe_claude(&profile, &early_reset, tick(CLOCK, 0)).unwrap();
        Usage::observe_claude(&profile, &claude_status("allowed"), tick(CLOCK, 0)).unwrap();
        let reopened = fixture.profile(Provider::Claude, "selected");
        let state = Usage::read(&reopened).unwrap();
        assert!(state.blocked(tick(1999, 999)));
        assert!(!state.blocked(tick(2000, 1000)));
        assert_eq!(state.latch_label(tick(1999, 999)), "Latched 1s");
        assert!(state.rows(tick(1950, 950))[0].contains("stale 15m50s"));
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
        assert_eq!(choices(&profiles, 0, tick(CLOCK, 0)).unwrap(), [0]);
        Usage::observe_claude(&profiles[0], &claude_status("rejected"), tick(CLOCK, 0)).unwrap();
        for profile in &profiles[1..3] {
            Usage::observe_claude(profile, &claude_status("allowed"), tick(CLOCK, 0)).unwrap();
        }
        assert_eq!(choices(&profiles, 0, tick(CLOCK, 0)).unwrap(), [2, 1]);
        assert!(choices(
            &profiles,
            0,
            tick(CLOCK + STALE_SECONDS as i64, STALE_SECONDS)
        )
        .unwrap()
        .is_empty());
        assert_eq!(choices(&profiles, 0, tick(2000, 1000)).unwrap(), [0]);
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
        refresh_codex(&profile, tick(CLOCK, 0)).unwrap();
        let reopened = fixture.profile(Provider::Codex, "selected");
        refresh_codex(&reopened, tick(CLOCK + 300, POLL_SECONDS - 1)).unwrap();
        let (_, directory) = profile.isolated_command().unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("calls")).unwrap(),
            "probe\n"
        );
        let rows = Usage::read(&profile).unwrap().rows(tick(CLOCK, 0));
        assert!(rows[0].contains("42% used / 58% left"));
        assert!(rows[1].contains("33% used / 67% left"));
        refresh_codex(&reopened, tick(CLOCK + 300, POLL_SECONDS)).unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("calls")).unwrap(),
            "probe\nprobe\n"
        );
    }
}
