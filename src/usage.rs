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
const UNSCHEDULED_RESET: i64 = 0;
const GEMINI_PROBE_TIMEOUT: Duration = Duration::from_secs(18);
const CLAUDE_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const CLAUDE_USAGE_ARGS: [&str; 10] = [
    "-p",
    "--output-format",
    "stream-json",
    "--verbose",
    "--no-session-persistence",
    "--strict-mcp-config",
    "--tools",
    "",
    "--",
    "/usage",
];
pub const SHORT_WINDOW_BUFFER_PCT: f64 = 15.0;
pub const WEEKLY_WINDOW_BUFFER_PCT: f64 = 5.0;
pub const RESET_TIE_HYSTERESIS_SECS: i64 = 600;

#[derive(Clone, Copy, Debug)]
pub struct Tick {
    pub(crate) wall: i64,
    pub(crate) elapsed: u64,
    epoch: uuid::Uuid,
}

pub fn now() -> Tick {
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Allowance {
    pub five: Option<f64>,
    pub week: Option<f64>,
    pub reset_in: Option<i64>,
    pub latched: bool,
    pub stale: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct Usage {
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

    pub(crate) fn known_reset(self, now: Tick) -> Option<i64> {
        self.latch
            .filter(|reset| *reset > now.wall && *reset != i64::MAX)
    }

    pub(crate) fn eligible_alternate(self, now: Tick) -> bool {
        !self.blocked(now)
            && ![self.five, self.week]
                .into_iter()
                .flatten()
                .any(|window| window.used >= 100.0 && window.reset > now.wall)
    }

    fn open_windows(self, now: Tick) -> impl Iterator<Item = Window> {
        [self.five, self.week]
            .into_iter()
            .flatten()
            .filter(move |window| window_is_open(*window, now))
    }

    pub(crate) fn remaining(self, now: Tick) -> Option<f64> {
        self.open_windows(now)
            .map(|window| (100.0 - window.used).clamp(0.0, 100.0))
            .min_by(f64::total_cmp)
    }

    pub(crate) fn headroom_rate(self, now: Tick) -> Option<f64> {
        self.open_windows(now)
            .filter(|window| window.reset > now.wall)
            .map(|window| {
                (100.0 - window.used).clamp(0.0, 100.0) / (window.reset - now.wall) as f64
            })
            .min_by(f64::total_cmp)
    }

    pub fn rows(self, now: Tick) -> [String; 2] {
        let row = |name, window: Option<Window>| match window {
            Some(window) => {
                let age = observation_age(window, now);
                let stale = self.unavailable
                    || self.needs_reading()
                    || age.is_none_or(|age| age >= STALE_SECONDS)
                    || (window.reset != UNSCHEDULED_RESET && now.wall >= window.reset);
                let reset_text = if window.reset == UNSCHEDULED_RESET {
                    "-".into()
                } else {
                    duration(window.reset.saturating_sub(now.wall))
                };
                format!(
                    "{name} {:.0}% used / {:.0}% left, reset {reset_text} · {}{}",
                    window.used.clamp(0.0, 100.0).ceil(),
                    (100.0 - window.used).clamp(0.0, 100.0).floor(),
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

    pub(crate) fn allowance(self, now: Tick) -> Allowance {
        let open = |window: Option<Window>| window.filter(|window| window_is_open(*window, now));
        let left = |window: Window| (100.0 - window.used).clamp(0.0, 100.0);
        let five_win = open(self.five);
        let week_win = open(self.week);
        let mut five = five_win.map(left);
        let mut week = week_win.map(left);
        let latch = self.latch.filter(|reset| *reset > now.wall);
        if let Some(latch_reset) = latch {
            let already_zero = [five, week].into_iter().flatten().any(|rem| rem <= 0.0);
            if !already_zero {
                if week_win.is_some_and(|w| w.reset > now.wall && w.reset == latch_reset) {
                    week = Some(0.0);
                } else if five_win.is_some_and(|w| w.reset > now.wall && w.reset == latch_reset) {
                    five = Some(0.0);
                } else {
                    match (five, week) {
                        (Some(f), Some(w)) => {
                            if w < f {
                                week = Some(0.0);
                            } else {
                                five = Some(0.0);
                            }
                        }
                        (Some(_), None) => five = Some(0.0),
                        (None, Some(_)) => week = Some(0.0),
                        (None, None) => five = Some(0.0),
                    }
                }
            }
        }
        let constraining = match (five_win, week_win) {
            (Some(f), Some(w)) => {
                let wf = left(f);
                let ww = left(w);
                if ww < wf || (ww == wf && f.reset <= now.wall && w.reset > now.wall) {
                    Some(w)
                } else {
                    Some(f)
                }
            }
            (Some(f), None) => Some(f),
            (None, Some(w)) => Some(w),
            (None, None) => None,
        };
        let reset = match latch {
            Some(reset) => Some(reset).filter(|reset| *reset != i64::MAX),
            None => constraining
                .map(|window| window.reset)
                .filter(|reset| *reset > now.wall),
        };
        let stale = if latch.is_some() {
            false
        } else {
            match constraining {
                Some(window) => {
                    self.unavailable
                        || self.needs_reading()
                        || now.wall < window.observed
                        || (now.wall - window.observed) as u64 >= STALE_SECONDS
                }
                None => false,
            }
        };
        Allowance {
            five,
            week,
            reset_in: reset.map(|reset| reset - now.wall),
            latched: latch.is_some(),
            stale,
        }
    }

    pub fn latch_label(self, now: Tick) -> String {
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

    pub(crate) fn poll_due(self, now: Tick) -> bool {
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
        let kind = info["rateLimitType"].as_str();
        let top_reset = info["resetsAt"].as_i64();
        update(profile, |state| {
            let windows = &info["unifiedWindows"];
            let mut five = claude_window(&windows["five_hour"], now);
            let mut week = [
                "seven_day",
                "seven_day_opus",
                "seven_day_sonnet",
                "seven_day_overage_included",
                "overage",
            ]
            .into_iter()
            .fold(None, |acc, key| {
                tighter_window(acc, claude_window(&windows[key], now))
            });
            match kind {
                Some("five_hour") => five = tighter_window(five, claude_window(info, now)),
                Some(k) if is_claude_weekly_type(k) => {
                    week = tighter_window(week, claude_window(info, now));
                }
                _ => {}
            }
            if limited {
                let fallback_five_reset = top_reset
                    .or_else(|| five.map(|w| w.reset))
                    .or_else(|| state.five.filter(|w| w.reset > now.wall).map(|w| w.reset));
                let fallback_week_reset = top_reset
                    .or_else(|| week.map(|w| w.reset))
                    .or_else(|| state.week.filter(|w| w.reset > now.wall).map(|w| w.reset));
                match kind {
                    Some("five_hour") => {
                        if let Some(reset) = fallback_five_reset {
                            five = tighter_window(five, window(100.0, reset, now));
                        }
                    }
                    Some(k) if is_claude_weekly_type(k) => {
                        if let Some(reset) = fallback_week_reset {
                            week = tighter_window(week, window(100.0, reset, now));
                        }
                    }
                    _ => {}
                }
            }
            state.five = five.or(state.five);
            state.week = week.or(state.week);
            state.unavailable = five.is_none() && week.is_none();
            if limited {
                let latch_reset = top_reset.or_else(|| match kind {
                    Some("five_hour") => state.five.filter(|w| w.reset > now.wall).map(|w| w.reset),
                    Some(k) if is_claude_weekly_type(k) => {
                        state.week.filter(|w| w.reset > now.wall).map(|w| w.reset)
                    }
                    _ => None,
                });
                state.record_limit(latch_reset, now);
            } else if (five.is_some() || week.is_some())
                && (!state.blocked(now) || state.latch == Some(i64::MAX))
            {
                state.latch = None;
            }
            Ok(())
        })?;
        Ok(limited)
    }

    pub fn sync_peer_accounts(profiles: &[Profile], source: &Profile, now: Tick) {
        let Some(source_id) = source.account_identity() else {
            return;
        };
        let Ok(source_usage) = Self::read(source) else {
            return;
        };
        let snap = PeerSnapshot {
            five: source_usage.five.filter(|w| window_is_open(*w, now)),
            week: source_usage.week.filter(|w| window_is_open(*w, now)),
            blocked: source_usage.blocked(now),
            known_reset: source_usage.known_reset(now),
            fresh: !source_usage.unavailable,
        };
        if !snap.blocked && (!snap.fresh || (snap.five.is_none() && snap.week.is_none())) {
            return;
        }
        let source_name = source.to_string();
        for peer in profiles {
            if peer.provider != source.provider || peer.to_string() == source_name {
                continue;
            }
            if peer.account_identity().as_deref() != Some(source_id.as_str()) {
                continue;
            }
            let Ok(mut preview) = Self::read(peer) else {
                continue;
            };
            if !apply_peer_snapshot(&mut preview, snap, now, true) {
                continue;
            }
            let _ = update_if_changed(peer, |state| {
                Ok(apply_peer_snapshot(state, snap, now, true))
            });
        }
    }

    pub fn reconcile_peer_accounts(profiles: &[Profile], now: Tick) {
        let mut groups: HashMap<(Provider, String), Vec<usize>> = HashMap::new();
        for (index, profile) in profiles.iter().enumerate() {
            if let Some(id) = profile.account_identity() {
                groups
                    .entry((profile.provider, id))
                    .or_default()
                    .push(index);
            }
        }
        for indices in groups.into_values() {
            if indices.len() < 2 {
                continue;
            }
            let mut snapshots = Vec::new();
            for index in indices {
                if let Ok(usage) = Self::read(&profiles[index]) {
                    snapshots.push((index, usage));
                }
            }
            if snapshots.len() < 2 {
                continue;
            }
            let mut merged_five: Option<Window> = None;
            let mut merged_week: Option<Window> = None;
            let mut any_blocked = false;
            let mut merged_reset: Option<i64> = None;
            let mut any_fresh_window = false;
            for (_, usage) in &snapshots {
                let blocked = usage.blocked(now);
                let fresh = !usage.unavailable;
                if blocked {
                    any_blocked = true;
                    if let Some(reset) = usage.known_reset(now) {
                        merged_reset = Some(merged_reset.map_or(reset, |cur| cur.max(reset)));
                    }
                }
                if blocked || fresh {
                    if let Some(w) = usage.five.filter(|w| window_is_open(*w, now)) {
                        if should_mirror_window(merged_five, w, now) {
                            merged_five = Some(w);
                        }
                        if fresh {
                            any_fresh_window = true;
                        }
                    }
                    if let Some(w) = usage.week.filter(|w| window_is_open(*w, now)) {
                        if should_mirror_window(merged_week, w, now) {
                            merged_week = Some(w);
                        }
                        if fresh {
                            any_fresh_window = true;
                        }
                    }
                }
            }
            let snap = PeerSnapshot {
                five: merged_five,
                week: merged_week,
                blocked: any_blocked,
                known_reset: merged_reset,
                fresh: any_fresh_window && !any_blocked,
            };
            if !snap.blocked && (!snap.fresh || (snap.five.is_none() && snap.week.is_none())) {
                continue;
            }
            for (index, current) in snapshots {
                let mut preview = current;
                if !apply_peer_snapshot(&mut preview, snap, now, false) {
                    continue;
                }
                let _ = update_if_changed(&profiles[index], |state| {
                    Ok(apply_peer_snapshot(state, snap, now, false))
                });
            }
        }
    }

    pub(crate) fn record_codex_limit(profile: &Profile, now: Tick) -> io::Result<()> {
        update(profile, |state| {
            let exhausted_reset = [state.five, state.week]
                .into_iter()
                .flatten()
                .filter(|window| window.used >= 100.0 && window.reset > now.wall)
                .map(|window| window.reset)
                .max();
            let reset = if exhausted_reset.is_some() {
                exhausted_reset
            } else {
                let target = match (
                    state.five.filter(|w| w.reset > now.wall),
                    state.week.filter(|w| w.reset > now.wall),
                ) {
                    (Some(f), Some(w)) => {
                        if w.used > f.used || (w.used == f.used && w.reset < f.reset) {
                            Some(true)
                        } else {
                            Some(false)
                        }
                    }
                    (Some(_), None) => Some(false),
                    (None, Some(_)) => Some(true),
                    (None, None) => None,
                };
                match target {
                    Some(false) => {
                        if let Some(w) = state.five {
                            state.five = window(100.0, w.reset, now).or(state.five);
                            Some(w.reset)
                        } else {
                            None
                        }
                    }
                    Some(true) => {
                        if let Some(w) = state.week {
                            state.week = window(100.0, w.reset, now).or(state.week);
                            Some(w.reset)
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            };
            state.record_limit(reset, now);
            Ok(())
        })
    }

    pub fn record_gemini_limit(profile: &Profile, reset: Option<i64>, now: Tick) -> io::Result<()> {
        update(profile, |state| {
            let valid_reset = reset.filter(|epoch| *epoch > now.wall && *epoch != i64::MAX);
            state.record_limit(valid_reset, now);
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

fn window_is_open(window: Window, now: Tick) -> bool {
    window.reset > now.wall || (window.reset == UNSCHEDULED_RESET && window.used <= 0.0)
}

fn window(used: f64, reset: i64, now: Tick) -> Option<Window> {
    (used.is_finite() && used >= 0.0 && (now.wall..i64::MAX).contains(&reset)).then_some(Window {
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

fn is_claude_weekly_type(kind: &str) -> bool {
    matches!(
        kind,
        "seven_day"
            | "seven_day_opus"
            | "seven_day_sonnet"
            | "seven_day_overage_included"
            | "overage"
    )
}

fn tighter_window(a: Option<Window>, b: Option<Window>) -> Option<Window> {
    match (a, b) {
        (Some(x), Some(y)) => {
            if y.used > x.used || (y.used == x.used && y.reset > x.reset) {
                Some(y)
            } else {
                Some(x)
            }
        }
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

#[derive(Clone, Copy)]
struct PeerSnapshot {
    five: Option<Window>,
    week: Option<Window>,
    blocked: bool,
    known_reset: Option<i64>,
    fresh: bool,
}

fn same_window(a: Option<Window>, b: Option<Window>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => {
            x.used.to_bits() == y.used.to_bits()
                && x.reset == y.reset
                && x.observed == y.observed
                && x.stamp == y.stamp
        }
        (None, None) => true,
        _ => false,
    }
}

fn window_is_fresh(window: Window, now: Tick) -> bool {
    now.wall >= window.observed && ((now.wall - window.observed) as u64) < STALE_SECONDS
}

fn should_mirror_window(peer: Option<Window>, src: Window, now: Tick) -> bool {
    match peer.filter(|w| window_is_open(*w, now)) {
        None => true,
        Some(existing) => {
            src.observed > existing.observed
                || (src.observed == existing.observed
                    && (src.used > existing.used || src.reset > existing.reset))
        }
    }
}

fn apply_peer_snapshot(
    state: &mut Usage,
    snap: PeerSnapshot,
    now: Tick,
    allow_clear_unknown_latch: bool,
) -> bool {
    let prev_five = state.five;
    let prev_week = state.week;
    let prev_latch = state.latch;
    let prev_unavailable = state.unavailable;
    let prev_latest_observed = [state.five, state.week]
        .into_iter()
        .flatten()
        .map(|w| w.observed)
        .max();

    let mut mirrored_window = false;
    let mut mirrored_fresh_newer = false;
    let mut latest_mirrored: Option<i64> = None;

    if let Some(src) = snap.five {
        if should_mirror_window(state.five, src, now) {
            state.five = Some(src);
            mirrored_window = true;
            latest_mirrored =
                Some(latest_mirrored.map_or(src.observed, |cur| cur.max(src.observed)));
            if window_is_fresh(src, now)
                && prev_latest_observed.is_none_or(|obs| src.observed > obs)
            {
                mirrored_fresh_newer = true;
            }
        }
    }
    if let Some(src) = snap.week {
        if should_mirror_window(state.week, src, now) {
            state.week = Some(src);
            mirrored_window = true;
            latest_mirrored =
                Some(latest_mirrored.map_or(src.observed, |cur| cur.max(src.observed)));
            if window_is_fresh(src, now)
                && prev_latest_observed.is_none_or(|obs| src.observed > obs)
            {
                mirrored_fresh_newer = true;
            }
        }
    }

    if snap.blocked {
        state.record_limit(snap.known_reset, now);
    } else if mirrored_window && snap.fresh {
        let can_clear_latch = match state.latch {
            None => true,
            Some(i64::MAX) => allow_clear_unknown_latch && mirrored_fresh_newer,
            Some(reset) if reset > now.wall => false,
            Some(expired) => latest_mirrored.is_some_and(|obs| obs >= expired),
        };
        if can_clear_latch {
            state.latch = None;
            state.unavailable = false;
        }
    }

    !same_window(prev_five, state.five)
        || !same_window(prev_week, state.week)
        || prev_latch != state.latch
        || prev_unavailable != state.unavailable
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

fn update_if_changed(
    profile: &Profile,
    edit: impl FnOnce(&mut Usage) -> io::Result<bool>,
) -> io::Result<bool> {
    let directory = profile.usage_directory()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("lock"))?;
    lock.lock()?;
    let mut state = read_state(&directory)?;
    let changed = edit(&mut state)?;
    if changed {
        let temporary = directory.join("state.next");
        let mut file = File::create(&temporary)?;
        serde_json::to_writer(&mut file, &state)?;
        file.sync_all()?;
        drop(file);
        fs::rename(temporary, directory.join("state.json"))?;
    }
    Ok(changed)
}

#[cfg(all(test, unix))]
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
        .filter_map(|(index, profile)| {
            let usage = Usage::read(profile).ok()?;
            usage
                .eligible_alternate(now)
                .then(|| (!usage.within_safety_buffer(now), profile.to_string(), index))
        })
        .collect();
    alternates.sort_unstable();
    alternates.into_iter().map(|(_, _, index)| index).collect()
}

pub(crate) fn cross_provider_fallback(
    profiles: &[Profile],
    interrupted: usize,
    configured: Option<&str>,
    now: Tick,
) -> Option<usize> {
    let configured = configured?;
    profiles.iter().position(|profile| {
        profile.to_string() == configured
            && profile.provider != profiles[interrupted].provider
            && Usage::read(profile).is_ok_and(|usage| usage.eligible_alternate(now))
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolSelection {
    pub index: usize,
    pub inside_buffer: bool,
    pub reason: String,
}

impl Usage {
    pub(crate) fn within_safety_buffer(self, now: Tick) -> bool {
        let short_ok = self
            .five
            .filter(|w| window_is_open(*w, now))
            .is_none_or(|w| (100.0 - w.used).clamp(0.0, 100.0) >= SHORT_WINDOW_BUFFER_PCT);
        let week_ok = self
            .week
            .filter(|w| window_is_open(*w, now))
            .is_none_or(|w| (100.0 - w.used).clamp(0.0, 100.0) >= WEEKLY_WINDOW_BUFFER_PCT);
        short_ok && week_ok
    }

    pub(crate) fn longest_active_reset(self, now: Tick) -> Option<i64> {
        self.week
            .filter(|w| w.reset > now.wall)
            .or_else(|| self.five.filter(|w| w.reset > now.wall))
            .map(|w| w.reset)
    }
}

pub fn select_pool_account(
    profiles: &[Profile],
    provider: Provider,
    sticky_current: Option<usize>,
    now: Tick,
) -> Option<PoolSelection> {
    let entries: Vec<(usize, Usage)> = profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| profile.provider == provider)
        .filter_map(|(index, profile)| Usage::read(profile).ok().map(|usage| (index, usage)))
        .collect();
    select_pool_account_from_usages(&entries, sticky_current, now)
}

pub fn select_pool_account_from_usages(
    candidates: &[(usize, Usage)],
    sticky_current: Option<usize>,
    now: Tick,
) -> Option<PoolSelection> {
    let eligible: Vec<(usize, Usage)> = candidates
        .iter()
        .copied()
        .filter(|(_, usage)| usage.eligible_alternate(now))
        .collect();
    if eligible.is_empty() {
        return None;
    }

    let safe: Vec<(usize, Usage)> = eligible
        .iter()
        .copied()
        .filter(|(_, usage)| usage.within_safety_buffer(now))
        .collect();

    if !safe.is_empty() {
        let best_reset = safe
            .iter()
            .filter_map(|(_, usage)| usage.longest_active_reset(now))
            .min();

        if let Some(current_idx) = sticky_current {
            if let Some((_, current_usage)) = safe.iter().find(|(idx, _)| *idx == current_idx) {
                let keep_sticky = match (current_usage.longest_active_reset(now), best_reset) {
                    (Some(cur_reset), Some(soonest)) => {
                        cur_reset <= soonest.saturating_add(RESET_TIE_HYSTERESIS_SECS)
                    }
                    (None, None) => true,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                };
                if keep_sticky {
                    return Some(PoolSelection {
                        index: current_idx,
                        inside_buffer: false,
                        reason: "sticky account within 15%/5% buffer".into(),
                    });
                }
            }
        }

        let (chosen, _) = safe.iter().copied().min_by(|(idx_a, a), (idx_b, b)| {
            let reset_a = a.longest_active_reset(now).unwrap_or(i64::MAX);
            let reset_b = b.longest_active_reset(now).unwrap_or(i64::MAX);
            reset_a
                .cmp(&reset_b)
                .then_with(|| {
                    let rem_a = a.remaining(now).unwrap_or(100.0);
                    let rem_b = b.remaining(now).unwrap_or(100.0);
                    rem_b.total_cmp(&rem_a)
                })
                .then_with(|| idx_a.cmp(idx_b))
        })?;

        return Some(PoolSelection {
            index: chosen,
            inside_buffer: false,
            reason: "soonest-expiring window within 15%/5% buffer".into(),
        });
    }

    let (chosen, _) = eligible.iter().copied().max_by(|(idx_a, a), (idx_b, b)| {
        let rem_a = a.remaining(now).unwrap_or(0.0);
        let rem_b = b.remaining(now).unwrap_or(0.0);
        rem_a
            .total_cmp(&rem_b)
            .then_with(|| {
                let reset_a = a.longest_active_reset(now).unwrap_or(i64::MAX);
                let reset_b = b.longest_active_reset(now).unwrap_or(i64::MAX);
                reset_b.cmp(&reset_a)
            })
            .then_with(|| idx_b.cmp(idx_a))
    })?;

    Some(PoolSelection {
        index: chosen,
        inside_buffer: true,
        reason: "all accounts inside safety buffer; using most remaining capacity".into(),
    })
}

fn codex_windows(
    result: &Value,
    now: Tick,
) -> (Option<Window>, Option<Window>, Option<Option<i64>>) {
    let limits = result["rateLimitsByLimitId"]
        .get("codex")
        .unwrap_or(&result["rateLimits"]);
    let buckets = [&limits["primary"], &limits["secondary"]];
    let parsed_bucket = |raw: &Value| {
        let mins = raw["windowDurationMins"].as_i64()?;
        let win = window(raw["usedPercent"].as_f64()?, raw["resetsAt"].as_i64()?, now)?;
        Some((mins, win))
    };
    let pick_bucket = |exact: i64, predicate: fn(i64) -> bool| {
        buckets
            .into_iter()
            .filter_map(parsed_bucket)
            .filter(|(mins, _)| predicate(*mins))
            .max_by(|(mins_a, win_a), (mins_b, win_b)| {
                (*mins_a == exact)
                    .cmp(&(*mins_b == exact))
                    .then_with(|| win_a.used.total_cmp(&win_b.used))
                    .then_with(|| win_a.reset.cmp(&win_b.reset))
            })
            .map(|(_, win)| win)
    };
    let five = pick_bucket(300, |mins| mins > 0 && mins <= 1440);
    let week = pick_bucket(10080, |mins| mins > 1440);
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
    (five, week, limited.then_some(reset))
}

pub(crate) fn refresh_codex(profile: &Profile, now: Tick) -> io::Result<()> {
    if profile.provider != Provider::Codex || !Usage::read(profile)?.poll_due(now) {
        return Ok(());
    }
    let reserved = update(profile, |state| {
        if !state.poll_due(now) {
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
            state.unavailable = five.is_none() && week.is_none();
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

pub fn refresh_gemini(profile: &Profile, now: Tick) -> io::Result<Option<Allowance>> {
    refresh_gemini_with(profile, now, gemini_readback)
}

fn refresh_gemini_with(
    profile: &Profile,
    now: Tick,
    readback: impl FnOnce(&Profile) -> io::Result<String>,
) -> io::Result<Option<Allowance>> {
    if profile.provider != Provider::Gemini || !reserve_poll(profile, now)? {
        return Ok(None);
    }
    let result = readback(profile);
    update(profile, |state| match result {
        Ok(output) => {
            let (five, week, limit) = gemini_windows(&output, now);
            Ok(apply_polled_windows(state, five, week, limit, now).then(|| state.allowance(now)))
        }
        Err(_) => {
            state.unavailable = true;
            Ok(None)
        }
    })
}

fn gemini_readback(profile: &Profile) -> io::Result<String> {
    let mut command = profile.authenticated_command()?;
    command.args([
        "-p",
        "/usage",
        "--output-format",
        "json",
        "--print-timeout",
        "15s",
    ]);
    run_with_timeout(&mut command, GEMINI_PROBE_TIMEOUT)
}

pub fn refresh_claude(profile: &Profile, now: Tick) -> io::Result<Option<Allowance>> {
    refresh_claude_with(profile, now, claude_readback)
}

pub(crate) fn refresh_claude_with(
    profile: &Profile,
    now: Tick,
    readback: impl FnOnce(&Profile) -> io::Result<String>,
) -> io::Result<Option<Allowance>> {
    if profile.provider != Provider::Claude || !reserve_poll(profile, now)? {
        return Ok(None);
    }
    let report = readback(profile).and_then(|raw| parse_claude_usage(&raw, now));
    let allowance = update(profile, |state| match &report {
        Ok(report) => Ok(
            apply_polled_windows(state, report.five, report.week, report.limit, now)
                .then(|| state.allowance(now)),
        ),
        Err(_) => {
            state.unavailable = true;
            Ok(None)
        }
    })?;
    let Ok(report) = report else {
        return Ok(None);
    };
    if report.rate_limit_events.is_empty() {
        return Ok(allowance);
    }
    for event in &report.rate_limit_events {
        Usage::observe_claude(profile, event, now)?;
    }
    Ok(Some(Usage::read(profile)?.allowance(now)))
}

fn reserve_poll(profile: &Profile, now: Tick) -> io::Result<bool> {
    if !Usage::read(profile)?.poll_due(now) {
        return Ok(false);
    }
    update(profile, |state| {
        if !state.poll_due(now) {
            return Ok(false);
        }
        state.probe = Some(Probe {
            id: uuid::Uuid::new_v4(),
            started: now.into(),
        });
        state.next_poll = None;
        Ok(true)
    })
}

fn apply_polled_windows(
    state: &mut Usage,
    five: Option<Window>,
    week: Option<Window>,
    limit: Option<Option<i64>>,
    now: Tick,
) -> bool {
    state.five = five.or(state.five);
    state.week = week.or(state.week);
    state.unavailable = five.is_none() && week.is_none();
    if let Some(reset) = limit {
        state.record_limit(reset, now);
    } else if (five.is_some() || week.is_some())
        && (!state.blocked(now) || state.latch == Some(i64::MAX))
    {
        state.latch = None;
    }
    five.is_some() || week.is_some() || limit.is_some()
}

fn claude_readback(profile: &Profile) -> io::Result<String> {
    let mut command = profile.authenticated_command()?;
    command
        .current_dir(profile.usage_directory()?)
        .args(CLAUDE_USAGE_ARGS);
    run_with_timeout(&mut command, CLAUDE_PROBE_TIMEOUT)
}

#[derive(Debug, Default)]
struct ClaudeUsageReport {
    five: Option<Window>,
    week: Option<Window>,
    limit: Option<Option<i64>>,
    rate_limit_events: Vec<Value>,
}

fn parse_claude_usage(raw: &str, now: Tick) -> io::Result<ClaudeUsageReport> {
    let mut parsed = ParsedWindows::default();
    let mut found_report = false;
    let mut result_text = None;
    let mut rate_limit_events = Vec::new();
    for line in raw.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if event["type"] == "result" {
            if claude_result_billed(&event) {
                return Err(io::Error::other("Claude usage readback was billable"));
            }
            result_text = event["result"].as_str().map(str::to_owned);
        }
        if event["type"] == "rate_limit_event" {
            rate_limit_events.push(event.clone());
        }
        if let Some(limits) = find_usage_report(&event, USAGE_REPORT_SEARCH_DEPTH)
            .and_then(|report| report["rate_limits"]["limits"].as_array())
        {
            found_report = true;
            for limit in limits {
                record_claude_limit(&mut parsed, limit, now);
            }
        }
    }
    if !found_report {
        if let Some(text) = &result_text {
            record_claude_usage_text(&mut parsed, text, now);
        }
    }
    let (five, week, limit) = parsed.finish();
    Ok(ClaudeUsageReport {
        five,
        week,
        limit,
        rate_limit_events,
    })
}

const USAGE_REPORT_SEARCH_DEPTH: usize = 4;

fn claude_result_billed(result: &Value) -> bool {
    result["num_turns"].as_u64().is_some_and(|turns| turns > 0)
        || result["total_cost_usd"]
            .as_f64()
            .is_some_and(|cost| cost > 0.0)
}

fn find_usage_report(value: &Value, depth: usize) -> Option<&Value> {
    if let Some(report) = value
        .get("usage_report")
        .filter(|report| report.is_object())
    {
        return Some(report);
    }
    if depth == 0 {
        return None;
    }
    match value {
        Value::Object(map) => map
            .values()
            .find_map(|child| find_usage_report(child, depth - 1)),
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_usage_report(child, depth - 1)),
        _ => None,
    }
}

fn classify_claude_label(label: &str) -> Option<UsageBucket> {
    let lower = label.to_ascii_lowercase();
    if ["five", "5h", "5-hour", "5_hour", "session"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        Some(UsageBucket::FiveHour)
    } else if ["week", "seven_day", "7d"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        Some(UsageBucket::Weekly)
    } else {
        None
    }
}

fn claude_limit_used_percent(limit: &Value) -> Option<f64> {
    limit["percent"]
        .as_f64()
        .or_else(|| limit["used_percent"].as_f64())
        .or_else(|| {
            limit["utilization"]
                .as_f64()
                .map(|fraction| fraction * 100.0)
        })
        .filter(|used| used.is_finite() && *used >= 0.0)
}

fn claude_limit_reset(limit: &Value) -> Option<i64> {
    let raw = &limit["resets_at"];
    raw.as_str()
        .and_then(|text| parse_rfc3339_utc(text.trim()))
        .or_else(|| raw.as_i64())
}

fn record_claude_limit(parsed: &mut ParsedWindows, limit: &Value, now: Tick) {
    if limit["active"] == false {
        return;
    }
    let label = ["kind", "group", "id", "window"]
        .iter()
        .filter_map(|key| limit[*key].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let (Some(bucket), Some(used)) = (
        classify_claude_label(&label),
        claude_limit_used_percent(limit),
    ) else {
        return;
    };
    parsed.record(bucket, used, claude_limit_reset(limit), now);
}

fn record_claude_usage_text(parsed: &mut ParsedWindows, text: &str, now: Tick) {
    let mut section = None;
    let mut pending: Option<(UsageBucket, f64)> = None;
    for line in text.lines() {
        if let Some(used) = used_percent_in_line(line) {
            if let Some((bucket, previous)) = pending.take() {
                parsed.record(bucket, previous, None, now);
            }
            if let Some(bucket) = classify_claude_label(line).or(section) {
                pending = Some((bucket, used));
            }
            continue;
        }
        if line.to_ascii_lowercase().contains("reset") {
            if let Some((bucket, used)) = pending.take() {
                parsed.record(bucket, used, rfc3339_in_line(line), now);
            }
            continue;
        }
        if let Some(bucket) = classify_claude_label(line) {
            section = Some(bucket);
        }
    }
    if let Some((bucket, used)) = pending {
        parsed.record(bucket, used, None, now);
    }
}

fn used_percent_in_line(line: &str) -> Option<f64> {
    let lower = line.to_ascii_lowercase();
    let marker = lower.find("% used")?;
    let digits_start = lower[..marker]
        .rfind(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .map_or(0, |index| index + 1);
    lower[digits_start..marker]
        .parse::<f64>()
        .ok()
        .filter(|used| used.is_finite())
}

fn rfc3339_in_line(line: &str) -> Option<i64> {
    line.split_whitespace()
        .map(|token| token.trim_matches(|ch: char| matches!(ch, '(' | ')' | ',' | '.' | ';')))
        .find_map(parse_rfc3339_utc)
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped output");
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let read_result = io::Read::read_to_end(&mut stdout, &mut bytes).map(|_| bytes);
        let _ = sender.send(read_result);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            if receiver.recv_timeout(Duration::from_millis(200)).is_ok() {
                let _ = reader.join();
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "usage readback timed out",
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(Duration::from_millis(25).min(remaining));
    };
    let bytes = receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| io::Error::other("usage output unavailable"))??;
    let _ = reader.join();
    if !status.success() {
        return Err(io::Error::other("usage readback failed"));
    }
    String::from_utf8(bytes).map_err(|_| io::Error::other("usage readback was not valid UTF-8"))
}

fn parse_ascii_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value = 0u32;
    for &byte in bytes {
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some(value)
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn parse_rfc3339_utc(input: &str) -> Option<i64> {
    let bytes = input.trim().as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = i64::from(parse_ascii_u32(&bytes[0..4])?);
    let month = parse_ascii_u32(&bytes[5..7])?;
    let day = parse_ascii_u32(&bytes[8..10])?;
    let hour = parse_ascii_u32(&bytes[11..13])?;
    let minute = parse_ascii_u32(&bytes[14..16])?;
    let second = parse_ascii_u32(&bytes[17..19])?;

    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut pos = 19;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        let frac_start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if pos == frac_start {
            return None;
        }
    }

    let tz = &bytes[pos..];
    let offset_secs: i64 = match tz {
        [b'Z'] | [b'z'] => 0,
        [sign @ (b'+' | b'-'), h0, h1, b':', m0, m1] => {
            let off_h = parse_ascii_u32(&[*h0, *h1])?;
            let off_m = parse_ascii_u32(&[*m0, *m1])?;
            if off_h > 23 || off_m > 59 {
                return None;
            }
            let total = i64::from(off_h) * 3600 + i64::from(off_m) * 60;
            if *sign == b'+' {
                total
            } else {
                -total
            }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let day_secs = i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    days.checked_mul(86_400)?
        .checked_add(day_secs)?
        .checked_sub(offset_secs)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum UsageBucket {
    FiveHour,
    Weekly,
}

#[derive(Default)]
struct ParsedWindows {
    five: Option<Window>,
    week: Option<Window>,
    limited: bool,
    exhausted_reset: Option<i64>,
}

impl ParsedWindows {
    fn record(&mut self, bucket: UsageBucket, used: f64, reset: Option<i64>, now: Tick) {
        if !used.is_finite() {
            return;
        }
        let clamped = used.clamp(0.0, 100.0);
        if clamped >= 100.0 {
            self.limited = true;
            if let Some(valid) = reset.filter(|epoch| *epoch > now.wall && *epoch != i64::MAX) {
                self.exhausted_reset =
                    Some(self.exhausted_reset.map_or(valid, |cur| cur.max(valid)));
            }
        }
        let parsed = gemini_window(clamped, reset, now);
        match bucket {
            UsageBucket::FiveHour => self.five = tighter_window(self.five, parsed),
            UsageBucket::Weekly => self.week = tighter_window(self.week, parsed),
        }
    }

    fn finish(self) -> (Option<Window>, Option<Window>, Option<Option<i64>>) {
        (
            self.five,
            self.week,
            self.limited.then_some(self.exhausted_reset),
        )
    }
}

fn gemini_window(used: f64, reset: Option<i64>, now: Tick) -> Option<Window> {
    if !used.is_finite() || used < 0.0 {
        return None;
    }
    let clamped = used.clamp(0.0, 100.0);
    if let Some(epoch) = reset.filter(|epoch| (now.wall..i64::MAX).contains(epoch)) {
        return window(clamped, epoch, now);
    }
    if clamped <= 0.0 && reset.is_none_or(|epoch| epoch <= UNSCHEDULED_RESET) {
        return Some(Window {
            used: 0.0,
            reset: UNSCHEDULED_RESET,
            observed: now.wall,
            stamp: Some(now.into()),
        });
    }
    None
}

fn classify_gemini_json_bucket(bucket: &Value) -> Option<UsageBucket> {
    let id = bucket["id"].as_str();
    let window_kind = bucket["window"].as_str();
    if id.is_some_and(|v| v.eq_ignore_ascii_case("gemini-5h"))
        || window_kind.is_some_and(|v| v.eq_ignore_ascii_case("5h"))
    {
        Some(UsageBucket::FiveHour)
    } else if id.is_some_and(|v| v.eq_ignore_ascii_case("gemini-weekly"))
        || window_kind
            .is_some_and(|v| v.eq_ignore_ascii_case("weekly") || v.eq_ignore_ascii_case("7d"))
    {
        Some(UsageBucket::Weekly)
    } else {
        None
    }
}

fn classify_gemini_tsv_label(label: &str) -> Option<UsageBucket> {
    let lower = label.trim().to_ascii_lowercase();
    if lower.contains("five hour") || lower.contains("5h") {
        Some(UsageBucket::FiveHour)
    } else if lower.contains("weekly") || lower.contains("7d") {
        Some(UsageBucket::Weekly)
    } else {
        None
    }
}

fn gemini_windows_from_groups(
    groups: &[Value],
    now: Tick,
) -> (Option<Window>, Option<Window>, Option<Option<i64>>) {
    let mut parsed = ParsedWindows::default();
    for group in groups {
        if !group["name"]
            .as_str()
            .is_some_and(|name| name.trim().eq_ignore_ascii_case("gemini models"))
        {
            continue;
        }
        let Some(buckets) = group["buckets"].as_array() else {
            continue;
        };
        for bucket in buckets {
            let Some(kind) = classify_gemini_json_bucket(bucket) else {
                continue;
            };
            let Some(remaining_fraction) = bucket["remaining_fraction"]
                .as_f64()
                .filter(|frac| frac.is_finite())
            else {
                continue;
            };
            let used = ((1.0 - remaining_fraction) * 100.0).clamp(0.0, 100.0);
            let reset = bucket["reset_time"].as_str().and_then(parse_rfc3339_utc);
            parsed.record(kind, used, reset, now);
        }
    }
    parsed.finish()
}

fn gemini_windows_from_tsv(
    tsv: &str,
    now: Tick,
) -> (Option<Window>, Option<Window>, Option<Option<i64>>) {
    let mut parsed = ParsedWindows::default();
    for line in tsv.lines() {
        let mut columns = line.split('\t');
        let Some(group) = columns.next() else {
            continue;
        };
        if !group.trim().eq_ignore_ascii_case("gemini models") {
            continue;
        }
        let Some(label) = columns.next() else {
            continue;
        };
        let Some(kind) = classify_gemini_tsv_label(label) else {
            continue;
        };
        let Some(percent_raw) = columns.next() else {
            continue;
        };
        let Some(remaining_pct) = percent_raw
            .trim()
            .strip_suffix('%')
            .and_then(|digits| digits.trim().parse::<f64>().ok())
            .filter(|pct| pct.is_finite() && *pct >= 0.0)
        else {
            continue;
        };
        let used = (100.0 - remaining_pct.clamp(0.0, 100.0)).clamp(0.0, 100.0);
        let reset = columns
            .next()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .and_then(parse_rfc3339_utc);
        parsed.record(kind, used, reset, now);
    }
    parsed.finish()
}

fn gemini_windows(raw: &str, now: Tick) -> (Option<Window>, Option<Window>, Option<Option<i64>>) {
    if let Ok(value) = serde_json::from_str::<Value>(raw) {
        if value["status"]
            .as_str()
            .is_some_and(|status| !status.eq_ignore_ascii_case("SUCCESS"))
        {
            return (None, None, None);
        }
        if let Some(groups) = value["command"]["data"]["groups"].as_array() {
            let structured = gemini_windows_from_groups(groups, now);
            if structured.0.is_some() || structured.1.is_some() || structured.2.is_some() {
                return structured;
            }
        }
        if let Some(response) = value["response"].as_str() {
            return gemini_windows_from_tsv(response, now);
        }
        return (None, None, None);
    }
    gemini_windows_from_tsv(raw, now)
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

    pub(crate) const READ_PROMPT: &str = r#"read_prompt() {
    case "$*" in
        *"--input-format stream-json"*)
            IFS= read -r line
            line=${line#*'"text":"'}
            line=${line%'","type":"text"}'*}
            printf '%b' "$(printf '%s' "$line" | sed 's/\\"/"/g')" ;;
        *) cat ;;
    esac
}"#;

    const CLAUDE_USAGE_PROBE_EXIT: &str = r#"case "$*" in *"-- /usage") exit 0 ;; esac"#;

    pub(crate) struct Fixture {
        pub(crate) root: PathBuf,
        pub(crate) executable: PathBuf,
    }

    impl Fixture {
        pub(crate) fn new(name: &str, script: &str) -> Self {
            let root = std::env::temp_dir().join(format!("ac-{name}-{}", std::process::id()));
            fs::create_dir(&root).unwrap();
            let executable = root.join("provider");
            fs::write(
                &executable,
                format!("#!/bin/sh\n{CLAUDE_USAGE_PROBE_EXIT}\n{READ_PROMPT}\n{script}"),
            )
            .unwrap();
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
        assert!(state.eligible_alternate(backwards));
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
        assert!(state.eligible_alternate(now));
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
    fn cross_provider_fallback_needs_the_configured_eligible_other_provider() {
        let fixture = Fixture::new("cross-fallback", "");
        let profiles = [
            fixture.profile(Provider::Claude, "selected"),
            fixture.profile(Provider::Claude, "sibling"),
            fixture.profile(Provider::Codex, "backup"),
        ];
        let now = tick(CLOCK, 0);
        assert_eq!(cross_provider_fallback(&profiles, 0, None, now), None);
        assert_eq!(
            cross_provider_fallback(&profiles, 0, Some("claude/sibling"), now),
            None
        );
        assert_eq!(
            cross_provider_fallback(&profiles, 0, Some("codex/missing"), now),
            None
        );
        assert_eq!(
            cross_provider_fallback(&profiles, 0, Some("codex/backup"), now),
            Some(2)
        );
        update(&profiles[2], |state| {
            state.record_limit(Some(CLOCK + 60), now);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            cross_provider_fallback(&profiles, 0, Some("codex/backup"), now),
            None
        );
        assert_eq!(
            cross_provider_fallback(&profiles, 0, Some("codex/backup"), tick(CLOCK + 61, 61)),
            Some(2)
        );
    }

    #[test]
    fn known_reset_ignores_past_and_unknown_latches() {
        let fixture = Fixture::new("known-reset", "");
        let profile = fixture.profile(Provider::Claude, "selected");
        let now = tick(CLOCK, 0);
        assert_eq!(Usage::read(&profile).unwrap().known_reset(now), None);
        Usage::observe_claude(&profile, &claude_status("rejected"), now).unwrap();
        assert_eq!(Usage::read(&profile).unwrap().known_reset(now), Some(2000));
        assert_eq!(
            Usage::read(&profile).unwrap().known_reset(tick(2000, 1000)),
            None
        );
        let unknown = fixture.profile(Provider::Codex, "unknown");
        Usage::record_codex_limit(&unknown, now).unwrap();
        let state = Usage::read(&unknown).unwrap();
        assert!(state.blocked(now));
        assert_eq!(state.known_reset(now), None);
    }

    #[test]
    fn rotation_order_accepts_stale_and_unreported_same_provider_allowances() {
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
        assert_eq!(choices(&profiles, 0, tick(CLOCK, 0)).unwrap(), [2, 4, 1]);
        assert_eq!(
            choices(
                &profiles,
                0,
                tick(CLOCK + STALE_SECONDS as i64, STALE_SECONDS)
            )
            .unwrap(),
            [2, 4, 1]
        );
        assert_eq!(choices(&profiles, 0, tick(2000, 1000)).unwrap(), [0]);
    }

    #[test]
    fn codex_weekly_only_readback_is_current_and_eligible() {
        let fixture = Fixture::new(
            "codex-weekly-only",
            r#"
while IFS= read -r request; do
  case "$request" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}' ;;
    *'"method":"account/rateLimits/read"'*)
      printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":97,"resetsAt":5000,"windowDurationMins":10080}}}}'
      ;;
  esac
done
"#,
        );
        let profile = fixture.profile(Provider::Codex, "weekly");
        let now = tick(CLOCK, 0);
        refresh_codex(&profile, now).unwrap();
        let state = Usage::read(&profile).unwrap();
        let [five, week] = state.rows(now);
        assert_eq!(five, "5h unavailable");
        assert!(week.contains("97% used / 3% left") && week.ends_with("age 0s"));
        assert!(state.eligible_alternate(tick(CLOCK + 901, 901)));
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

    #[test]
    fn gemini_subprocess_probe_parses_json_and_enforces_monotonic_backoff() {
        let log_path = std::env::temp_dir().join(format!(
            "ac-gemini-probe-calls-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let script = format!(
            r#"
test "$1" = "-p" || exit 2
test "$2" = "/usage" || exit 3
test "$3" = "--output-format" || exit 4
test "$4" = "json" || exit 5
test "$5" = "--print-timeout" || exit 6
test "$6" = "15s" || exit 7
printf 'probe\n' >> '{log}'
printf '%s\n' '{{"conversation_id":"","status":"SUCCESS","response":"","command":{{"name":"usage","data":{{"groups":[{{"name":"Gemini Models","buckets":[{{"id":"gemini-weekly","window":"weekly","remaining_fraction":0.87,"reset_time":"2026-12-02T00:53:44Z"}},{{"id":"gemini-5h","window":"5h","remaining_fraction":0.82,"reset_time":"2026-11-26T03:17:50Z"}}]}}]}}}}}}'
"#,
            log = log_path.display()
        );
        let fixture = Fixture::new("gemini-probe", &script);
        let profile = fixture.profile(Provider::Gemini, "selected");
        let (_, directory) = profile.isolated_command().unwrap();
        let cache = directory
            .join(".gemini")
            .join("antigravity-cli")
            .join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(
            cache.join("onboarding.json"),
            r#"{"onboardingComplete":true}"#,
        )
        .unwrap();

        let now_wall = parse_rfc3339_utc("2026-11-26T01:00:00Z").unwrap();
        let first = refresh_gemini(&profile, tick(now_wall, 0))
            .unwrap()
            .expect("first gemini allowance");
        assert!((first.five.unwrap() - 82.0).abs() < 1e-6);
        assert!((first.week.unwrap() - 87.0).abs() < 1e-6);
        assert!(!first.latched);
        assert!(!first.stale);

        let reopened = fixture.profile(Provider::Gemini, "selected");
        assert!(
            refresh_gemini(&reopened, tick(now_wall + 600, POLL_SECONDS - 1))
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read_to_string(&log_path).unwrap(), "probe\n");

        assert!(
            refresh_gemini(&reopened, tick(now_wall + 600, POLL_SECONDS))
                .unwrap()
                .is_some()
        );
        assert_eq!(fs::read_to_string(&log_path).unwrap(), "probe\nprobe\n");
        let _ = fs::remove_file(log_path);
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;
    use serde_json::json;

    fn usage_with(
        five_used: f64,
        five_reset: i64,
        week_used: f64,
        week_reset: i64,
        now: Tick,
    ) -> Usage {
        Usage {
            five: window(five_used, five_reset, now),
            week: window(week_used, week_reset, now),
            ..Usage::default()
        }
    }

    #[test]
    fn sticky_pool_respects_15_5_buffers_soonest_reset_and_hysteresis() {
        let now = now();
        let soon = usage_with(50.0, now.wall + 3600, 40.0, now.wall + 10_000, now);
        let later = usage_with(10.0, now.wall + 7200, 10.0, now.wall + 20_000, now);
        let close = usage_with(30.0, now.wall + 3600, 30.0, now.wall + 10_300, now);

        let picked = select_pool_account_from_usages(&[(0, later), (1, soon)], None, now).unwrap();
        assert_eq!(picked.index, 1);
        assert!(!picked.inside_buffer);

        let sticky =
            select_pool_account_from_usages(&[(0, soon), (1, close)], Some(1), now).unwrap();
        assert_eq!(sticky.index, 1);

        let near_short_cap = usage_with(90.0, now.wall + 3600, 10.0, now.wall + 8_000, now);
        let healthy = usage_with(20.0, now.wall + 3600, 20.0, now.wall + 50_000, now);
        let rotated =
            select_pool_account_from_usages(&[(0, near_short_cap), (1, healthy)], Some(0), now)
                .unwrap();
        assert_eq!(rotated.index, 1);
        assert!(!rotated.inside_buffer);

        let near_week_cap = usage_with(10.0, now.wall + 3600, 96.0, now.wall + 8_000, now);
        let rotated_week =
            select_pool_account_from_usages(&[(0, near_week_cap), (1, healthy)], Some(0), now)
                .unwrap();
        assert_eq!(rotated_week.index, 1);

        let tighter = usage_with(94.0, now.wall + 3600, 96.0, now.wall + 8_000, now);
        let looser = usage_with(88.0, now.wall + 3600, 90.0, now.wall + 20_000, now);
        let fallback =
            select_pool_account_from_usages(&[(0, tighter), (1, looser)], None, now).unwrap();
        assert_eq!(fallback.index, 1);
        assert!(fallback.inside_buffer);
    }

    struct TempRoot(std::path::PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "ac-usage-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn profile(&self, provider: Provider, name: &str) -> Profile {
            let profile = Profile::new(self.0.join("profiles"), provider, name).unwrap();
            profile.prepare().unwrap();
            profile
        }

        fn profile_dir(&self, profile: &Profile) -> std::path::PathBuf {
            self.0
                .join("profiles")
                .join(profile.provider.binary())
                .join(profile.name())
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn observe_claude_handles_model_weekly_buckets_and_utilizationless_rejections() {
        let temp = TempRoot::new("claude-weekly");
        let now = tick(10_000, 0);

        for kind in [
            "seven_day",
            "seven_day_opus",
            "seven_day_sonnet",
            "seven_day_overage_included",
            "overage",
        ] {
            let profile = temp.profile(Provider::Claude, &kind.replace('_', "-"));
            let rejected = serde_json::json!({
                "type": "rate_limit_event",
                "rate_limit_info": {
                    "status": "rejected",
                    "rateLimitType": kind,
                    "resetsAt": now.wall + 32_000
                }
            });
            assert!(Usage::observe_claude(&profile, &rejected, now).unwrap());
            let state = Usage::read(&profile).unwrap();
            assert!(state.blocked(now), "{kind}");
            let allowance = state.allowance(now);
            assert_eq!(allowance.week, Some(0.0), "{kind}");
            assert_eq!(allowance.reset_in, Some(32_000), "{kind}");
            assert!(allowance.latched, "{kind}");
            assert!(!allowance.stale, "{kind}");
        }

        let unified = temp.profile(Provider::Claude, "unified");
        let mixed = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "allowed",
                "unifiedWindows": {
                    "five_hour": {"utilization": 0.20, "resetsAt": now.wall + 3600},
                    "seven_day": {"utilization": 0.40, "resetsAt": now.wall + 86_400},
                    "seven_day_overage_included": {"utilization": 0.92, "resetsAt": now.wall + 90_000}
                }
            }
        });
        assert!(!Usage::observe_claude(&unified, &mixed, now).unwrap());
        let allowance = Usage::read(&unified).unwrap().allowance(now);
        assert_eq!(allowance.five, Some(80.0));
        assert!((allowance.week.unwrap() - 8.0).abs() < 1e-6);
        assert_eq!(allowance.reset_in, Some(90_000));
        assert!(!allowance.stale);
    }

    #[test]
    fn allowance_uses_constraining_reset_wall_clock_staleness_and_floors_rows() {
        let now = tick(20_000, 100);
        let usage = Usage {
            five: window(20.0, now.wall + 3600, now),
            week: window(99.4, now.wall + 86_400, now),
            ..Usage::default()
        };
        let fresh = usage.allowance(now);
        assert_eq!(fresh.reset_in, Some(86_400));
        assert!(!fresh.stale);
        let [_, week_row] = usage.rows(now);
        assert!(week_row.contains("100% used / 0% left"), "{week_row}");

        let mut after_restart = tick(now.wall + STALE_SECONDS as i64, 5);
        after_restart.epoch = uuid::Uuid::new_v4();
        let stale = usage.allowance(after_restart);
        assert!(stale.stale);

        let latched = Usage {
            five: window(25.0, now.wall + 7200, now),
            week: window(60.0, now.wall + 86_400, now),
            latch: Some(now.wall + 7200),
            unavailable: true,
            ..Usage::default()
        };
        let latched_allowance = latched.allowance(now);
        assert!(latched_allowance.latched);
        assert!(!latched_allowance.stale);
        assert_eq!(latched_allowance.five, Some(0.0));
        assert_eq!(latched_allowance.week, Some(40.0));
        assert_eq!(latched_allowance.reset_in, Some(7200));

        let expired_latch = latched.allowance(tick(now.wall + 7200, 7300));
        assert!(!expired_latch.latched);
        assert!(expired_latch.stale);
    }

    #[test]
    fn sync_peer_accounts_mirrors_shared_accounts_and_preserves_latches_and_corrupt_peers() {
        let temp = TempRoot::new("sync-peers");
        let now = tick(30_000, 0);
        let later = tick(30_120, 120);

        let a = temp.profile(Provider::Claude, "alpha");
        let b = temp.profile(Provider::Claude, "bravo");
        let other = temp.profile(Provider::Claude, "other");
        let corrupt = temp.profile(Provider::Claude, "corrupt");

        for (profile, acct) in [
            (&a, "acct-shared"),
            (&b, "acct-shared"),
            (&other, "acct-separate"),
            (&corrupt, "acct-shared"),
        ] {
            profile.prepare().unwrap();
            let dir = temp.profile_dir(profile);
            fs::write(
                dir.join(".claude.json"),
                format!(
                    r#"{{"oauthAccount":{{"accountUuid":"{acct}","organizationUuid":"org-1"}}}}"#
                ),
            )
            .unwrap();
        }
        let corrupt_path = corrupt.usage_directory().unwrap().join("state.json");
        fs::write(&corrupt_path, b"not json").unwrap();

        let profiles = [a.clone(), b.clone(), other.clone(), corrupt.clone()];

        let stale_ok = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "allowed",
                "unifiedWindows": {
                    "seven_day": {"utilization": 0.77, "resetsAt": now.wall + 50_000}
                }
            }
        });
        Usage::observe_claude(&a, &stale_ok, now).unwrap();

        let rejected = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "rejected",
                "rateLimitType": "seven_day",
                "resetsAt": later.wall + 32_000
            }
        });
        Usage::observe_claude(&b, &rejected, later).unwrap();

        Usage::sync_peer_accounts(&profiles, &b, later);

        let mirrored_a = Usage::read(&a).unwrap();
        assert!(mirrored_a.blocked(later));
        assert_eq!(mirrored_a.allowance(later).week, Some(0.0));
        assert_eq!(mirrored_a.known_reset(later), Some(later.wall + 32_000));

        let untouched_other = Usage::read(&other).unwrap();
        assert!(!untouched_other.blocked(later));
        assert_eq!(untouched_other.allowance(later).week, None);
        assert_eq!(fs::read(&corrupt_path).unwrap(), b"not json");

        let shorter = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "rejected",
                "rateLimitType": "seven_day",
                "resetsAt": later.wall + 10_000
            }
        });
        let c = temp.profile(Provider::Claude, "charlie");
        c.prepare().unwrap();
        let c_dir = temp.profile_dir(&c);
        fs::write(
            c_dir.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"acct-shared","organizationUuid":"org-1"}}"#,
        )
        .unwrap();
        Usage::observe_claude(&c, &shorter, later).unwrap();
        Usage::sync_peer_accounts(&[a.clone(), c.clone()], &c, later);
        assert_eq!(
            Usage::read(&a).unwrap().known_reset(later),
            Some(later.wall + 32_000)
        );
    }

    #[test]
    fn record_codex_limit_uses_constraining_open_window_and_codex_windows_maps_durations() {
        let temp = TempRoot::new("codex-limit");
        let now = tick(40_000, 0);

        let non_standard = serde_json::json!({
            "rateLimits": {
                "primary": {"usedPercent": 45.0, "resetsAt": now.wall + 7_200, "windowDurationMins": 1440},
                "secondary": {"usedPercent": 80.0, "resetsAt": now.wall + 200_000, "windowDurationMins": 20160}
            }
        });
        let (five, week, limit) = codex_windows(&non_standard, now);
        assert_eq!(five.unwrap().used, 45.0);
        assert_eq!(week.unwrap().used, 80.0);
        assert!(limit.is_none());

        let profile = temp.profile(Provider::Codex, "cached");
        update(&profile, |state| {
            state.five = window(30.0, now.wall + 3_600, now);
            state.week = window(96.0, now.wall + 60_000, now);
            Ok(())
        })
        .unwrap();
        Usage::record_codex_limit(&profile, now).unwrap();
        let state = Usage::read(&profile).unwrap();
        assert!(state.blocked(now));
        assert_eq!(state.known_reset(now), Some(now.wall + 60_000));
        assert_eq!(state.allowance(now).week, Some(0.0));
        assert_eq!(state.allowance(now).five, Some(70.0));

        let empty = temp.profile(Provider::Codex, "empty");
        Usage::record_codex_limit(&empty, now).unwrap();
        let empty_state = Usage::read(&empty).unwrap();
        assert!(empty_state.blocked(now));
        assert_eq!(empty_state.known_reset(now), None);
    }

    #[test]
    fn record_gemini_limit_latches_with_known_or_unknown_reset() {
        let temp = TempRoot::new("gemini-limit");
        let now = tick(50_000, 0);

        let known = temp.profile(Provider::Gemini, "known");
        Usage::record_gemini_limit(&known, Some(now.wall + 1_800), now).unwrap();
        let state = Usage::read(&known).unwrap();
        assert!(state.blocked(now));
        assert_eq!(state.known_reset(now), Some(now.wall + 1_800));
        let allowance = state.allowance(now);
        assert!(allowance.latched);
        assert_eq!(allowance.five, Some(0.0));
        assert_eq!(allowance.reset_in, Some(1_800));

        Usage::record_gemini_limit(&known, Some(now.wall + 600), now).unwrap();
        assert_eq!(
            Usage::read(&known).unwrap().known_reset(now),
            Some(now.wall + 1_800)
        );

        let unknown = temp.profile(Provider::Gemini, "unknown");
        Usage::record_gemini_limit(&unknown, None, now).unwrap();
        let unknown_state = Usage::read(&unknown).unwrap();
        assert!(unknown_state.blocked(now));
        assert_eq!(unknown_state.known_reset(now), None);
        assert_eq!(unknown_state.latch_label(now), "Latched · reset unknown");
    }

    #[test]
    fn alternates_ranks_safe_profiles_ahead_of_critically_low_with_alphabetical_ties() {
        let temp = TempRoot::new("alternates-rank");
        let now = tick(60_000, 0);
        let selected = temp.profile(Provider::Claude, "selected");
        let alpha_low = temp.profile(Provider::Claude, "alpha");
        let zebra_safe = temp.profile(Provider::Claude, "zebra");
        let bravo_safe = temp.profile(Provider::Claude, "bravo");
        let delta_low = temp.profile(Provider::Claude, "delta");

        for (profile, five_used, week_used) in [
            (&alpha_low, 10.0, 97.0),
            (&zebra_safe, 20.0, 40.0),
            (&bravo_safe, 30.0, 50.0),
            (&delta_low, 90.0, 10.0),
        ] {
            update(profile, |state| {
                state.five = window(five_used, now.wall + 3_600, now);
                state.week = window(week_used, now.wall + 86_400, now);
                Ok(())
            })
            .unwrap();
        }
        let profiles = [selected, alpha_low, zebra_safe, bravo_safe, delta_low];
        assert_eq!(alternates(&profiles, 0, now), vec![3, 2, 1, 4]);
    }

    #[test]
    fn reconcile_peer_accounts_syncs_latched_main_into_stale_default_skips_noops_and_preserves_unknown_latch(
    ) {
        let temp = TempRoot::new("reconcile-peers");
        let stale_time = tick(100_000, 0);
        let now = tick(100_000 + 16 * 3600, 16 * 3600);

        let default_like = temp.profile(Provider::Claude, "default-like");
        let main = temp.profile(Provider::Claude, "main");
        let separate = temp.profile(Provider::Claude, "separate");

        for (profile, acct) in [
            (&default_like, "acct-shared"),
            (&main, "acct-shared"),
            (&separate, "acct-other"),
        ] {
            let dir = temp.profile_dir(profile);
            fs::write(
                dir.join(".claude.json"),
                format!(
                    r#"{{"oauthAccount":{{"accountUuid":"{acct}","organizationUuid":"org-1"}}}}"#
                ),
            )
            .unwrap();
        }

        let stale_ok = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "allowed",
                "unifiedWindows": {
                    "seven_day": {"utilization": 0.77, "resetsAt": now.wall + 100_000}
                }
            }
        });
        Usage::observe_claude(&default_like, &stale_ok, stale_time).unwrap();

        let latched_main = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "rejected",
                "rateLimitType": "seven_day",
                "resetsAt": now.wall + 32_000
            }
        });
        Usage::observe_claude(&main, &latched_main, now).unwrap();

        let profiles = [default_like.clone(), main.clone(), separate.clone()];
        Usage::reconcile_peer_accounts(&profiles, now);

        let synced_default = Usage::read(&default_like).unwrap();
        assert!(synced_default.blocked(now));
        assert_eq!(synced_default.allowance(now).week, Some(0.0));
        assert_eq!(synced_default.known_reset(now), Some(now.wall + 32_000));
        assert!(!Usage::read(&separate).unwrap().blocked(now));

        let default_path = default_like.usage_directory().unwrap().join("state.json");
        let main_path = main.usage_directory().unwrap().join("state.json");
        let pretty_default = serde_json::to_vec_pretty(&synced_default).unwrap();
        let pretty_main = serde_json::to_vec_pretty(&Usage::read(&main).unwrap()).unwrap();
        fs::write(&default_path, &pretty_default).unwrap();
        fs::write(&main_path, &pretty_main).unwrap();

        Usage::reconcile_peer_accounts(&profiles, now);
        Usage::sync_peer_accounts(&profiles, &main, now);
        assert_eq!(fs::read(&default_path).unwrap(), pretty_default);
        assert_eq!(fs::read(&main_path).unwrap(), pretty_main);

        let unknown_latched = temp.profile(Provider::Codex, "unknown-latched");
        let stale_unblocked = temp.profile(Provider::Codex, "stale-unblocked");
        for profile in [&unknown_latched, &stale_unblocked] {
            let dir = temp.profile_dir(profile);
            fs::write(
                dir.join("auth.json"),
                r#"{"tokens":{"access_token":"tok","account_id":"codex-shared"}}"#,
            )
            .unwrap();
        }
        update(&stale_unblocked, |state| {
            state.five = window(20.0, now.wall + 7_200, stale_time);
            state.unavailable = false;
            Ok(())
        })
        .unwrap();
        Usage::record_codex_limit(&unknown_latched, now).unwrap();
        assert_eq!(
            Usage::read(&unknown_latched).unwrap().latch_label(now),
            "Latched · reset unknown"
        );

        let codex_profiles = [unknown_latched.clone(), stale_unblocked.clone()];
        Usage::sync_peer_accounts(&codex_profiles, &stale_unblocked, now);
        assert!(Usage::read(&unknown_latched).unwrap().blocked(now));
        assert_eq!(
            Usage::read(&unknown_latched).unwrap().latch_label(now),
            "Latched · reset unknown"
        );

        Usage::reconcile_peer_accounts(&codex_profiles, now);
        assert!(Usage::read(&unknown_latched).unwrap().blocked(now));
        assert!(Usage::read(&stale_unblocked).unwrap().blocked(now));
    }

    #[test]
    fn parse_rfc3339_utc_handles_epoch_leap_years_fractions_and_offsets() {
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_utc("2000-02-29T00:00:00Z"),
            Some(
                chrono::DateTime::parse_from_rfc3339("2000-02-29T00:00:00Z")
                    .unwrap()
                    .timestamp()
            )
        );
        assert_eq!(
            parse_rfc3339_utc("2024-02-29T23:59:59Z"),
            Some(
                chrono::DateTime::parse_from_rfc3339("2024-02-29T23:59:59Z")
                    .unwrap()
                    .timestamp()
            )
        );
        assert_eq!(
            parse_rfc3339_utc("2026-11-26T03:17:50Z"),
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-11-26T03:17:50Z")
                    .unwrap()
                    .timestamp()
            )
        );
        assert_eq!(
            parse_rfc3339_utc("2026-12-02T00:53:44.987654Z"),
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-12-02T00:53:44Z")
                    .unwrap()
                    .timestamp()
            )
        );
        assert_eq!(
            parse_rfc3339_utc("2026-11-26T05:17:50+02:00"),
            parse_rfc3339_utc("2026-11-26T03:17:50Z")
        );
        assert_eq!(
            parse_rfc3339_utc("2026-11-25T21:47:50-05:30"),
            parse_rfc3339_utc("2026-11-26T03:17:50Z")
        );
        for invalid in [
            "",
            "not-a-date",
            "2023-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-11-26T24:00:00Z",
            "2026-11-26T03:17:50",
            "2026-11-26T03:17:50.Z",
            "2026-11-26T03:17:50Zextra",
        ] {
            assert_eq!(parse_rfc3339_utc(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn gemini_windows_parses_structured_json_and_tsv_fallback() {
        let now_wall = parse_rfc3339_utc("2026-11-26T02:00:00Z").unwrap();
        let now = tick(now_wall, 10);
        let five_reset = parse_rfc3339_utc("2026-11-26T03:17:50Z").unwrap();
        let week_reset = parse_rfc3339_utc("2026-12-02T00:53:44Z").unwrap();

        let real_payload = serde_json::json!({
            "conversation_id": "",
            "status": "SUCCESS",
            "response": "Gemini Models\tWeekly Limit Remaining\t87%\t2026-12-02T00:53:44Z\nGemini Models\tFive Hour Limit Remaining\t82%\t2026-11-26T03:17:50Z\nClaude and GPT models\tWeekly Limit Remaining\t100%\t1970-01-01T00:00:00Z\nClaude and GPT models\tFive Hour Limit Remaining\t100%\t1970-01-01T00:00:00Z\n",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Gemini Models",
                            "buckets": [
                                {
                                    "id": "gemini-weekly",
                                    "window": "weekly",
                                    "remaining_fraction": 0.8699474334716797,
                                    "reset_time": "2026-12-02T00:53:44Z"
                                },
                                {
                                    "id": "gemini-5h",
                                    "window": "5h",
                                    "remaining_fraction": 0.8187379240989685,
                                    "reset_time": "2026-11-26T03:17:50Z"
                                }
                            ]
                        },
                        {
                            "name": "Claude and GPT models",
                            "buckets": [
                                {
                                    "id": "3p-weekly",
                                    "window": "weekly",
                                    "remaining_fraction": 0.1,
                                    "reset_time": "2026-12-02T00:53:44Z"
                                },
                                {
                                    "id": "3p-5h",
                                    "window": "5h",
                                    "remaining_fraction": 0.0,
                                    "reset_time": "2026-11-26T03:17:50Z"
                                }
                            ]
                        }
                    ]
                }
            }
        })
        .to_string();

        let (five, week, limit) = gemini_windows(&real_payload, now);
        assert!(limit.is_none());
        let five = five.expect("5h window");
        let week = week.expect("weekly window");
        assert!((five.used - 18.12620759010315).abs() < 1e-6);
        assert_eq!(five.reset, five_reset);
        assert!((week.used - 13.005256652832031).abs() < 1e-6);
        assert_eq!(week.reset, week_reset);

        let usage = Usage {
            five: Some(five),
            week: Some(week),
            ..Usage::default()
        };
        let allowance = usage.allowance(now);
        assert!((allowance.five.unwrap() - 81.87379240989685).abs() < 1e-6);
        assert!((allowance.week.unwrap() - 86.99474334716797).abs() < 1e-6);
        assert_eq!(allowance.reset_in, Some(five_reset - now_wall));
        assert!(!allowance.latched);
        assert!(!allowance.stale);

        let tsv_only_json = serde_json::json!({
            "status": "SUCCESS",
            "response": "Gemini Models\tWeekly Limit Remaining\t87%\t2026-12-02T00:53:44Z\r\nGemini Models\tFive Hour Limit Remaining\t82%\t2026-11-26T03:17:50Z\r\n"
        })
        .to_string();
        let (tsv_five, tsv_week, tsv_limit) = gemini_windows(&tsv_only_json, now);
        assert!(tsv_limit.is_none());
        assert!((tsv_five.unwrap().used - 18.0).abs() < 1e-6);
        assert!((tsv_week.unwrap().used - 13.0).abs() < 1e-6);

        let raw_tsv = "Claude and GPT models\tFive Hour Limit Remaining\t0%\t2026-11-26T03:17:50Z\nGemini Models\tFive Hour Limit Remaining\t75%\t2026-11-26T03:17:50Z\n";
        let (raw_five, raw_week, raw_limit) = gemini_windows(raw_tsv, now);
        assert!(raw_limit.is_none());
        assert!((raw_five.unwrap().used - 25.0).abs() < 1e-6);
        assert!(raw_week.is_none());

        let failed_json = serde_json::json!({
            "status": "ERROR",
            "response": "Gemini Models\tFive Hour Limit Remaining\t82%\t2026-11-26T03:17:50Z\n"
        })
        .to_string();
        assert!(matches!(
            gemini_windows(&failed_json, now),
            (None, None, None)
        ));
    }

    #[test]
    fn gemini_untouched_full_bucket_with_epoch_zero_stays_open_without_reset() {
        let now_wall = parse_rfc3339_utc("2026-11-26T02:00:00Z").unwrap();
        let now = tick(now_wall, 50);

        let untouched_json = serde_json::json!({
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [{
                        "name": "Gemini Models",
                        "buckets": [
                            {
                                "id": "gemini-weekly",
                                "window": "weekly",
                                "remaining_fraction": 1,
                                "reset_time": "1970-01-01T00:00:00Z"
                            },
                            {
                                "id": "gemini-5h",
                                "window": "5h",
                                "remaining_fraction": 1.0,
                                "reset_time": "1970-01-01T00:00:00Z"
                            }
                        ]
                    }]
                }
            }
        })
        .to_string();

        let (five, week, limit) = gemini_windows(&untouched_json, now);
        assert!(limit.is_none());
        let usage = Usage {
            five,
            week,
            ..Usage::default()
        };
        let allowance = usage.allowance(now);
        assert_eq!(allowance.five, Some(100.0));
        assert_eq!(allowance.week, Some(100.0));
        assert_eq!(allowance.reset_in, None);
        assert!(!allowance.latched);
        assert!(!allowance.stale);
        assert_eq!(usage.remaining(now), Some(100.0));
        assert_eq!(usage.headroom_rate(now), None);
        assert_eq!(usage.longest_active_reset(now), None);
        assert!(usage.within_safety_buffer(now));
        let [five_row, week_row] = usage.rows(now);
        assert!(
            five_row.contains("0% used / 100% left, reset - · age 0s"),
            "{five_row}"
        );
        assert!(
            week_row.contains("0% used / 100% left, reset - · age 0s"),
            "{week_row}"
        );

        let mixed_tsv = "Gemini Models\tFive Hour Limit Remaining\t100%\t1970-01-01T00:00:00Z\nGemini Models\tWeekly Limit Remaining\t87%\t2026-12-02T00:53:44Z\n";
        let (mixed_five, mixed_week, mixed_limit) = gemini_windows(mixed_tsv, now);
        assert!(mixed_limit.is_none());
        let mixed = Usage {
            five: mixed_five,
            week: mixed_week,
            ..Usage::default()
        };
        let mixed_allowance = mixed.allowance(now);
        assert_eq!(mixed_allowance.five, Some(100.0));
        assert_eq!(mixed_allowance.week, Some(87.0));
        assert_eq!(
            mixed_allowance.reset_in,
            Some(parse_rfc3339_utc("2026-12-02T00:53:44Z").unwrap() - now_wall)
        );
    }

    #[test]
    fn refresh_gemini_exhausted_bucket_latches_and_never_shortens_existing_latch() {
        let temp = TempRoot::new("gemini-exhausted");
        let profile = temp.profile(Provider::Gemini, "main");
        let now_wall = parse_rfc3339_utc("2026-11-26T02:00:00Z").unwrap();
        let now = tick(now_wall, 0);
        let reset_far = parse_rfc3339_utc("2026-11-26T07:00:00Z").unwrap();
        let reset_near = parse_rfc3339_utc("2026-11-26T04:00:00Z").unwrap();

        let exhausted_far = serde_json::json!({
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [{
                        "name": "Gemini Models",
                        "buckets": [
                            {
                                "id": "gemini-5h",
                                "window": "5h",
                                "remaining_fraction": 0.0,
                                "reset_time": "2026-11-26T07:00:00Z"
                            },
                            {
                                "id": "gemini-weekly",
                                "window": "weekly",
                                "remaining_fraction": 0.65,
                                "reset_time": "2026-12-02T00:53:44Z"
                            }
                        ]
                    }]
                }
            }
        })
        .to_string();

        let allowance = refresh_gemini_with(&profile, now, |_| Ok(exhausted_far))
            .unwrap()
            .expect("exhausted allowance");
        assert!(allowance.latched);
        assert_eq!(allowance.five, Some(0.0));
        assert!((allowance.week.unwrap() - 65.0).abs() < 1e-6);
        assert_eq!(allowance.reset_in, Some(reset_far - now_wall));

        let state = Usage::read(&profile).unwrap();
        assert!(state.blocked(now));
        assert_eq!(state.known_reset(now), Some(reset_far));
        assert!(!state.poll_due(tick(now_wall + 600, POLL_SECONDS + 1)));

        Usage::record_gemini_limit(&profile, Some(reset_near), now).unwrap();
        assert_eq!(
            Usage::read(&profile).unwrap().known_reset(now),
            Some(reset_far)
        );

        let unknown_profile = temp.profile(Provider::Gemini, "unknown-exhausted");
        let exhausted_unknown = serde_json::json!({
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [{
                        "name": "Gemini Models",
                        "buckets": [{
                            "id": "gemini-5h",
                            "window": "5h",
                            "remaining_fraction": 0.0,
                            "reset_time": "1970-01-01T00:00:00Z"
                        }]
                    }]
                }
            }
        })
        .to_string();
        let unknown_allowance =
            refresh_gemini_with(&unknown_profile, now, |_| Ok(exhausted_unknown))
                .unwrap()
                .expect("unknown reset latch allowance");
        assert!(unknown_allowance.latched);
        assert_eq!(unknown_allowance.reset_in, None);
        assert_eq!(
            Usage::read(&unknown_profile).unwrap().latch_label(now),
            "Latched · reset unknown"
        );
    }

    #[test]
    fn refresh_gemini_enforces_five_minute_monotonic_floor_and_fails_closed_on_corrupt_state() {
        let temp = TempRoot::new("gemini-floor");
        let now_wall = parse_rfc3339_utc("2026-11-26T02:00:00Z").unwrap();

        let claude = temp.profile(Provider::Claude, "skip");
        let mut calls = 0usize;
        assert!(refresh_gemini_with(&claude, tick(now_wall, 0), |_| {
            calls += 1;
            Ok(String::new())
        })
        .unwrap()
        .is_none());
        assert_eq!(calls, 0);

        let corrupt = temp.profile(Provider::Gemini, "corrupt");
        let corrupt_path = corrupt.usage_directory().unwrap().join("state.json");
        fs::write(&corrupt_path, b"{ broken").unwrap();
        assert!(refresh_gemini_with(&corrupt, tick(now_wall, 0), |_| {
            calls += 1;
            Ok(String::new())
        })
        .is_err());
        assert_eq!(calls, 0);
        assert_eq!(fs::read(&corrupt_path).unwrap(), b"{ broken");

        let profile = temp.profile(Provider::Gemini, "paced");
        assert!(refresh_gemini_with(&profile, tick(now_wall, 0), |_| {
            calls += 1;
            Err(io::Error::other("offline"))
        })
        .unwrap()
        .is_none());
        assert_eq!(calls, 1);

        assert!(
            refresh_gemini_with(&profile, tick(now_wall + 1_000, POLL_SECONDS - 1), |_| {
                calls += 1;
                Ok(String::new())
            })
            .unwrap()
            .is_none()
        );
        assert_eq!(calls, 1);

        let mut reopened = tick(now_wall + 2_000, 999);
        reopened.epoch = uuid::Uuid::new_v4();
        assert!(refresh_gemini_with(&profile, reopened, |_| {
            calls += 1;
            Ok(String::new())
        })
        .unwrap()
        .is_none());
        assert_eq!(calls, 1);

        reopened.elapsed += POLL_SECONDS * SECOND;
        let healthy = "Gemini Models\tFive Hour Limit Remaining\t82%\t2026-11-26T03:17:50Z\n";
        let recovered = refresh_gemini_with(&profile, reopened, |_| {
            calls += 1;
            Ok(healthy.to_owned())
        })
        .unwrap()
        .expect("recovered after monotonic floor");
        assert_eq!(calls, 2);
        assert_eq!(recovered.five, Some(82.0));
        assert!(!recovered.stale);
    }

    fn claude_usage_stream(limits: Value) -> String {
        [
            json!({"type": "system", "subtype": "init", "tools": []}),
            json!({"type": "system", "subtype": "usage_report", "usage_report": {"rate_limits": {"limits": limits}}}),
            json!({"type": "result", "subtype": "success", "num_turns": 0, "total_cost_usd": 0.0, "result": ""}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
    }

    #[test]
    fn refresh_claude_parses_weekly_only_usage_report() {
        let temp = TempRoot::new("claude-weekly");
        let profile = temp.profile(Provider::Claude, "main");
        let now_wall = parse_rfc3339_utc("2026-11-26T02:00:00Z").unwrap();
        let now = tick(now_wall, 0);
        let weekly_reset = parse_rfc3339_utc("2026-12-01T09:00:00Z").unwrap();
        let stream = claude_usage_stream(json!([
            {"kind": "weekly_all", "group": "overall", "percent": 38, "resets_at": "2026-12-01T09:00:00Z"},
            {"kind": "weekly_scoped", "group": "opus", "percent": 12, "resets_at": "2026-12-01T09:00:00Z"}
        ]));
        let mut ran_args = Vec::new();
        let allowance = refresh_claude_with(&profile, now, |_| {
            ran_args.extend(CLAUDE_USAGE_ARGS.iter().map(|arg| arg.to_string()));
            Ok(stream)
        })
        .unwrap()
        .expect("weekly allowance");
        assert_eq!(ran_args.last().map(String::as_str), Some("/usage"));
        assert!(ran_args.iter().any(|arg| arg == "--no-session-persistence"));
        assert_eq!(allowance.five, None);
        assert!((allowance.week.unwrap() - 62.0).abs() < 1e-6);
        assert_eq!(allowance.reset_in, Some(weekly_reset - now_wall));
        assert!(!allowance.latched);
        assert!(!Usage::read(&profile)
            .unwrap()
            .poll_due(tick(now_wall + 60, 60)));
        assert!(refresh_claude_with(&profile, tick(now_wall + 60, 60), |_| {
            panic!("Claude usage must not be polled before the five-minute cadence")
        })
        .unwrap()
        .is_none());
    }

    #[test]
    fn refresh_claude_parses_five_hour_and_weekly_with_tightest_window() {
        let temp = TempRoot::new("claude-dual");
        let profile = temp.profile(Provider::Claude, "main");
        let now_wall = parse_rfc3339_utc("2026-11-26T02:00:00Z").unwrap();
        let now = tick(now_wall, 0);
        let session_reset = parse_rfc3339_utc("2026-11-26T04:30:00Z").unwrap();
        let stream = claude_usage_stream(json!([
            {"kind": "session", "group": "5-hour", "percent": 41.5, "resets_at": "2026-11-26T04:30:00Z"},
            {"kind": "weekly_all", "group": "overall", "percent": 20, "resets_at": "2026-12-01T09:00:00Z"},
            {"kind": "weekly_scoped", "group": "sonnet", "percent": 27, "resets_at": "2026-12-01T09:00:00Z"}
        ]));
        let allowance = refresh_claude_with(&profile, now, |_| Ok(stream))
            .unwrap()
            .expect("dual allowance");
        assert!((allowance.five.unwrap() - 58.5).abs() < 1e-6);
        assert!((allowance.week.unwrap() - 73.0).abs() < 1e-6);
        assert_eq!(allowance.reset_in, Some(session_reset - now_wall));
        assert!(!allowance.latched);
    }

    #[test]
    fn refresh_claude_latches_fully_used_window_and_rejects_billable_output() {
        let temp = TempRoot::new("claude-latched");
        let profile = temp.profile(Provider::Claude, "main");
        let now_wall = parse_rfc3339_utc("2026-11-26T02:00:00Z").unwrap();
        let now = tick(now_wall, 0);
        let session_reset = parse_rfc3339_utc("2026-11-26T03:15:00Z").unwrap();
        let stream = claude_usage_stream(json!([
            {"kind": "five_hour", "group": "session", "percent": 100, "resets_at": "2026-11-26T03:15:00Z"},
            {"kind": "weekly_all", "group": "overall", "percent": 64, "resets_at": "2026-12-01T09:00:00Z"}
        ]));
        let allowance = refresh_claude_with(&profile, now, |_| Ok(stream))
            .unwrap()
            .expect("latched allowance");
        assert!(allowance.latched);
        assert_eq!(allowance.five, Some(0.0));
        assert_eq!(allowance.reset_in, Some(session_reset - now_wall));
        assert!(Usage::read(&profile).unwrap().blocked(now));

        let billed_profile = temp.profile(Provider::Claude, "billed");
        let billed = [
            json!({"type": "system", "usage_report": {"rate_limits": {"limits": [
                {"kind": "weekly_all", "percent": 10, "resets_at": "2026-12-01T09:00:00Z"}
            ]}}}),
            json!({"type": "result", "num_turns": 1, "total_cost_usd": 0.02}),
        ]
        .map(|event| event.to_string())
        .join("\n");
        assert!(refresh_claude_with(&billed_profile, now, |_| Ok(billed))
            .unwrap()
            .is_none());
        assert_eq!(
            Usage::read(&billed_profile).unwrap().allowance(now).week,
            None
        );
    }

    #[test]
    fn refresh_claude_falls_back_to_result_text_and_rate_limit_events() {
        let temp = TempRoot::new("claude-text");
        let profile = temp.profile(Provider::Claude, "main");
        let now_wall = parse_rfc3339_utc("2026-11-26T02:00:00Z").unwrap();
        let now = tick(now_wall, 0);
        let text = "Current session\n  12% used\n  Resets 2026-11-26T05:00:00Z\nCurrent week (all models)\n  30% used\n  Resets 2026-12-01T09:00:00Z\n";
        let stream = json!({"type": "result", "num_turns": 0, "total_cost_usd": 0, "result": text})
            .to_string();
        let allowance = refresh_claude_with(&profile, now, |_| Ok(stream))
            .unwrap()
            .expect("text allowance");
        assert!((allowance.five.unwrap() - 88.0).abs() < 1e-6);
        assert!((allowance.week.unwrap() - 70.0).abs() < 1e-6);

        let event_profile = temp.profile(Provider::Claude, "events");
        let event = json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "allowed",
                "unifiedWindows": {
                    "five_hour": {"utilization": 0.5, "resetsAt": now_wall + 3600}
                }
            }
        })
        .to_string();
        let from_event = refresh_claude_with(&event_profile, now, |_| Ok(event))
            .unwrap()
            .expect("event allowance");
        assert_eq!(from_event.five, Some(50.0));
        assert_eq!(from_event.reset_in, Some(3600));
    }
}
