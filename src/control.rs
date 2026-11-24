use crate::{
    auth, catalog, mission, native,
    permissions::PermissionMode,
    routing,
    specialists::{self, Entry, Member, Status, Store},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fmt, io,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::RecvTimeoutError,
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) const MAX_WAIT: Duration = Duration::from_secs(600);
pub(crate) const DEFAULT_WAIT: Duration = Duration::from_secs(30);
pub(crate) const MAX_TEXT_CHARS: usize = 16_000;
pub(crate) const MAX_BRIEF_FIELD_CHARS: usize = 4_000;
pub(crate) const MAX_QUERY_CHARS: usize = 200;
pub(crate) const MAX_ROLE_CHARS: usize = 40;
pub(crate) const MAX_PATH_CHARS: usize = 1_024;
pub(crate) const MAX_QUEUE: usize = 16;
pub(crate) const DEFAULT_READ_LIMIT: usize = 50;
const PUMP_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Scope {
    ReadOnly,
    Supervised,
}

impl Scope {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        match text {
            "read-only" => Ok(Self::ReadOnly),
            "supervised" => Ok(Self::Supervised),
            "full-access" | "full" | "bypass" => {
                Err("Full access is never granted to outside callers.".into())
            }
            other => Err(format!(
                "Unknown scope {other}. Use supervised or read-only."
            )),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Supervised => "supervised",
        }
    }

    pub(crate) fn allows_changes(self) -> bool {
        self == Self::Supervised
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Failure {
    Invalid(String),
    Refused(String),
}

impl Failure {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Invalid(message) | Self::Refused(message) => message,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::Invalid(message.into())
}

fn refused(message: impl Into<String>) -> Failure {
    Failure::Refused(message.into())
}

pub(crate) trait Run: Send {
    fn next(&self, timeout: Duration) -> Result<mission::Event, RecvTimeoutError>;
}

impl Run for mission::Mission {
    fn next(&self, timeout: Duration) -> Result<mission::Event, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }
}

pub(crate) trait Engine: Send + Sync + 'static {
    fn start(&self, member: &Member, task: &str, workspace: &Path) -> Result<Box<dyn Run>, String>;
}

pub(crate) struct NativeEngine {
    profile_root: PathBuf,
}

impl NativeEngine {
    pub(crate) fn new(profile_root: PathBuf) -> Self {
        Self { profile_root }
    }

    fn route(
        &self,
        request: routing::Request,
        mode: PermissionMode,
    ) -> Result<mission::Route, String> {
        let state = auth::existing_private_state(&self.profile_root, routing::STATE_DIRECTORY)
            .map_err(|error| error.to_string())?;
        let pins = native::load_existing(&self.profile_root).map_err(|error| error.to_string())?;
        Ok(mission::Route {
            request,
            preferred: None,
            catalog: catalog::Catalog::load(state.as_deref()),
            pins,
            target: None,
            mode,
            limits: mission::LimitPolicy::default(),
        })
    }
}

pub(crate) fn external_mode(requested: PermissionMode) -> PermissionMode {
    match requested {
        PermissionMode::ReadOnly => PermissionMode::ReadOnly,
        PermissionMode::Supervised
        | PermissionMode::AutoAcceptEdits
        | PermissionMode::WorktreeAuto
        | PermissionMode::WorkspaceAuto => PermissionMode::Supervised,
    }
}

impl Engine for NativeEngine {
    fn start(&self, member: &Member, task: &str, workspace: &Path) -> Result<Box<dyn Run>, String> {
        let profiles = auth::profiles(&self.profile_root).map_err(|error| error.to_string())?;
        if profiles.is_empty() {
            return Err(format!("{}.", crate::panel::NO_PROVIDER_HINT));
        }
        let request = crate::framed(routing::parse(task)?, |unit| {
            specialists::specialist_prompt(member, unit)
        });
        let route = self.route(request, external_mode(member.permission))?;
        Ok(Box::new(mission::Mission::launch(
            profiles,
            route,
            workspace.to_path_buf(),
        )))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Brief {
    pub(crate) role: String,
    pub(crate) goal: String,
    pub(crate) context: String,
    pub(crate) requirements: String,
    pub(crate) done_when: String,
    pub(crate) do_not: String,
    #[serde(rename = "return")]
    pub(crate) deliverable: String,
}

impl Brief {
    fn fields(&self) -> [(&'static str, &str); 7] {
        [
            ("ROLE", &self.role),
            ("GOAL", &self.goal),
            ("CONTEXT", &self.context),
            ("REQUIREMENTS", &self.requirements),
            ("DONE WHEN", &self.done_when),
            ("DO NOT", &self.do_not),
            ("RETURN", &self.deliverable),
        ]
    }

    pub(crate) fn validate(&self) -> Result<(), Failure> {
        for (label, value) in self.fields() {
            bounded_text(label, value, MAX_BRIEF_FIELD_CHARS)?;
        }
        Ok(())
    }

    pub(crate) fn render(&self) -> String {
        self.fields()
            .iter()
            .map(|(label, value)| format!("{label}: {}", value.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Summary {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) status: &'static str,
    pub(crate) mission: String,
    pub(crate) workspace: String,
    pub(crate) running: bool,
    pub(crate) queued: usize,
    pub(crate) entries: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Line {
    pub(crate) seq: u64,
    pub(crate) at: String,
    pub(crate) text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Transcript {
    pub(crate) thread: Summary,
    pub(crate) entries: Vec<Line>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Waited {
    pub(crate) thread: Summary,
    pub(crate) finished: bool,
}

struct Thread {
    member: Member,
    sequence: u64,
    queue: VecDeque<String>,
    stop: Option<Arc<AtomicBool>>,
}

impl Thread {
    fn running(&self) -> bool {
        self.stop.is_some()
    }

    fn summary(&self) -> Summary {
        Summary {
            name: self.member.name.clone(),
            role: self.member.role.clone(),
            status: self.member.status.label(),
            mission: self.member.mission.clone(),
            workspace: specialists::workspace_label(&self.member),
            running: self.running(),
            queued: self.queue.len(),
            entries: self.sequence,
        }
    }

    fn lines_since(&self, since: u64, limit: usize) -> Vec<Line> {
        let first = self.sequence - self.member.history.len() as u64;
        let mut lines: Vec<Line> = self
            .member
            .history
            .iter()
            .enumerate()
            .map(|(offset, entry)| Line {
                seq: first + offset as u64 + 1,
                at: entry.at.clone(),
                text: entry.text.clone(),
            })
            .filter(|line| line.seq > since)
            .collect();
        let skip = lines.len().saturating_sub(limit);
        lines.drain(..skip);
        lines
    }

    fn matches(&self, needle: &str) -> bool {
        [&self.member.name, &self.member.role, &self.member.mission]
            .into_iter()
            .chain(self.member.history.iter().map(|entry| &entry.text))
            .any(|text| text.to_lowercase().contains(needle))
    }
}

enum Outcome {
    Finished(io::Result<()>),
    Interrupted,
}

struct Shared {
    state: Mutex<Vec<Thread>>,
    changed: Condvar,
    store: Store,
    root: PathBuf,
    engine: Box<dyn Engine>,
}

#[derive(Clone)]
pub(crate) struct Control {
    shared: Arc<Shared>,
}

impl Control {
    pub(crate) fn open(store: Store, root: &Path, engine: Box<dyn Engine>) -> io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::other("Workspace root must be a directory"));
        }
        let threads = store
            .load_roster()?
            .into_iter()
            .map(|member| Thread {
                sequence: member.history.len() as u64,
                member,
                queue: VecDeque::new(),
                stop: None,
            })
            .collect();
        Ok(Self {
            shared: Arc::new(Shared {
                state: Mutex::new(threads),
                changed: Condvar::new(),
                store,
                root,
                engine,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Thread>> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn list(&self) -> Vec<Summary> {
        self.lock().iter().map(Thread::summary).collect()
    }

    pub(crate) fn count(&self) -> usize {
        self.lock().len()
    }

    pub(crate) fn search(&self, query: &str) -> Result<Vec<Summary>, Failure> {
        bounded_text("query", query, MAX_QUERY_CHARS)?;
        let needle = query.trim().to_lowercase();
        Ok(self
            .lock()
            .iter()
            .filter(|thread| thread.matches(&needle))
            .map(Thread::summary)
            .collect())
    }

    pub(crate) fn read(&self, name: &str, since: u64, limit: usize) -> Result<Transcript, Failure> {
        let threads = self.lock();
        let thread = &threads[locate(&threads, name)?];
        Ok(Transcript {
            thread: thread.summary(),
            entries: thread.lines_since(since, limit),
        })
    }

    pub(crate) fn create(
        &self,
        name: &str,
        role: &str,
        task: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<Summary, Failure> {
        validate_role(role)?;
        if let Some(task) = task {
            bounded_text("task", task, MAX_TEXT_CHARS)?;
        }
        let base = resolve_workspace(&self.shared.root, workspace)?;
        let mut threads = self.lock();
        let crew: Vec<Member> = threads.iter().map(|thread| thread.member.clone()).collect();
        specialists::validate_name(name, &crew).map_err(invalid)?;
        let isolated = self
            .shared
            .store
            .isolate(&base, name, task.map(str::trim).unwrap_or(role.trim()))
            .map_err(|error| refused(format!("Cannot isolate {name}: {error}")))?;
        let member = Member::new(name, role.trim(), task.map(str::trim), Some(isolated));
        let permission = external_mode(member.permission);
        let member = member.with_permission(Some(permission));
        threads.push(Thread {
            member,
            sequence: 0,
            queue: VecDeque::new(),
            stop: None,
        });
        self.save(&threads);
        let summary = threads[threads.len() - 1].summary();
        self.shared.changed.notify_all();
        Ok(summary)
    }

    pub(crate) fn launch(&self, name: &str, task: &str) -> Result<Summary, Failure> {
        bounded_text("task", task, MAX_TEXT_CHARS)?;
        let mut threads = self.lock();
        let index = locate(&threads, name)?;
        if threads[index].running() {
            return Err(refused(format!(
                "{} is already running. Use message_thread to queue work or interrupt_thread to stop it.",
                threads[index].member.name
            )));
        }
        self.start(&mut threads, index, task.trim().to_owned())?;
        Ok(threads[index].summary())
    }

    pub(crate) fn message(&self, name: &str, text: &str) -> Result<Summary, Failure> {
        bounded_text("message", text, MAX_TEXT_CHARS)?;
        let mut threads = self.lock();
        let index = locate(&threads, name)?;
        if !threads[index].running() {
            self.start(&mut threads, index, text.trim().to_owned())?;
            return Ok(threads[index].summary());
        }
        if threads[index].queue.len() >= MAX_QUEUE {
            return Err(refused(format!(
                "{} already has {MAX_QUEUE} queued messages.",
                threads[index].member.name
            )));
        }
        threads[index].queue.push_back(text.trim().to_owned());
        self.record(&mut threads[index], format!("QUEUED  {}", text.trim()));
        Ok(threads[index].summary())
    }

    pub(crate) fn delegate(
        &self,
        name: &str,
        brief: &Brief,
        workspace: Option<&str>,
    ) -> Result<Summary, Failure> {
        brief.validate()?;
        let role = brief_role(&brief.role);
        let exists = locate(&self.lock(), name).is_ok();
        if !exists {
            self.create(name, &role, Some(brief.goal.trim()), workspace)?;
        } else if workspace.is_some() {
            return Err(invalid(format!(
                "{name} already exists; its workspace cannot be changed."
            )));
        }
        self.launch(name, &brief.render())
    }

    pub(crate) fn wait(&self, name: &str, timeout: Duration) -> Result<Waited, Failure> {
        if timeout > MAX_WAIT {
            return Err(invalid(format!(
                "timeout_ms must be at most {}.",
                MAX_WAIT.as_millis()
            )));
        }
        let deadline = Instant::now() + timeout;
        let mut threads = self.lock();
        loop {
            let index = locate(&threads, name)?;
            let thread = &threads[index];
            if !thread.running() && thread.queue.is_empty() {
                return Ok(Waited {
                    thread: thread.summary(),
                    finished: true,
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Waited {
                    thread: thread.summary(),
                    finished: false,
                });
            }
            threads = self
                .shared
                .changed
                .wait_timeout(threads, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    pub(crate) fn interrupt(&self, name: &str) -> Result<Summary, Failure> {
        let mut threads = self.lock();
        let index = locate(&threads, name)?;
        let dropped = threads[index].queue.len();
        threads[index].queue.clear();
        let Some(stop) = threads[index].stop.clone() else {
            return Ok(threads[index].summary());
        };
        stop.store(true, Ordering::SeqCst);
        self.record(
            &mut threads[index],
            format!("SYSTEM  Interrupt requested · {dropped} queued dropped"),
        );
        Ok(threads[index].summary())
    }

    pub(crate) fn changed_since(&self, name: &str, seen: u64, timeout: Duration) -> bool {
        let threads = self.lock();
        let current = |threads: &Vec<Thread>| {
            locate(threads, name)
                .map(|index| threads[index].sequence)
                .unwrap_or(seen)
        };
        if current(&threads) != seen {
            return true;
        }
        let (threads, _) = self
            .shared
            .changed
            .wait_timeout(threads, timeout)
            .unwrap_or_else(PoisonError::into_inner);
        current(&threads) != seen
    }

    fn start(&self, threads: &mut [Thread], index: usize, task: String) -> Result<(), Failure> {
        let thread = &mut threads[index];
        if thread.member.paused {
            return Err(refused(format!("{} is paused.", thread.member.name)));
        }
        let workspace = thread
            .member
            .workspace
            .as_ref()
            .map(|workspace| workspace.path.clone())
            .ok_or_else(|| refused(format!("{} has no isolated workspace.", thread.member.name)))?;
        let run = self
            .shared
            .engine
            .start(&thread.member, &task, &workspace)
            .map_err(refused)?;
        let stop = Arc::new(AtomicBool::new(false));
        thread.stop = Some(Arc::clone(&stop));
        thread.member.status = Status::Running;
        thread.member.mission = task.clone();
        self.record(thread, format!("TASK  {task}"));
        self.save(threads);
        let control = self.clone();
        let name = threads[index].member.name.clone();
        thread::spawn(move || control.pump(&name, run, &stop));
        Ok(())
    }

    fn pump(&self, name: &str, run: Box<dyn Run>, stop: &AtomicBool) {
        let mut reply = String::new();
        let outcome = loop {
            if stop.load(Ordering::SeqCst) {
                break Outcome::Interrupted;
            }
            match run.next(PUMP_POLL) {
                Ok(mission::Event::Output(text)) => {
                    reply.push_str(&text);
                    reply.push('\n');
                    self.record_output(name, &text);
                }
                Ok(mission::Event::Running {
                    reason: Some(reason),
                    ..
                }) => self.record_named(name, format!("SYSTEM  {reason}")),
                Ok(mission::Event::Continuing { .. }) => {
                    self.record_named(name, "SYSTEM  Continuing on another profile".into())
                }
                Ok(mission::Event::Snoozed { until, .. }) => self.record_named(
                    name,
                    format!("SYSTEM  Snoozed until the limit resets at epoch {until}"),
                ),
                Ok(mission::Event::Running { reason: None, .. }) => {}
                Ok(mission::Event::Finished(result)) => break Outcome::Finished(result),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    break Outcome::Finished(Err(io::Error::other("Mission worker stopped")))
                }
            }
        };
        drop(run);
        self.finish(name, outcome, &reply);
    }

    fn finish(&self, name: &str, outcome: Outcome, reply: &str) {
        let mut threads = self.lock();
        let Ok(index) = locate(&threads, name) else {
            return;
        };
        let thread = &mut threads[index];
        thread.stop = None;
        let (status, detail) = match outcome {
            Outcome::Finished(Ok(())) => (Status::Done, specialists::summary(reply)),
            Outcome::Finished(Err(error)) => (Status::Error, error.to_string()),
            Outcome::Interrupted => (Status::Error, "Interrupted".into()),
        };
        thread.member.status = status;
        let label = status.label();
        self.record(thread, format!("{name}  {label} · {detail}"));
        if let Some(next) = thread.queue.pop_front() {
            if let Err(failure) = self.start(&mut threads, index, next) {
                self.record(&mut threads[index], format!("{name}  ERROR · {failure}"));
            }
        }
        self.save(&threads);
        self.shared.changed.notify_all();
    }

    fn record_output(&self, name: &str, text: &str) {
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            self.record_named(name, format!("{name}  {line}"));
        }
    }

    fn record_named(&self, name: &str, text: String) {
        let mut threads = self.lock();
        if let Ok(index) = locate(&threads, name) {
            self.record(&mut threads[index], text);
        }
    }

    fn record(&self, thread: &mut Thread, text: String) {
        let entry = Entry::now(text);
        if let Err(error) = self
            .shared
            .store
            .append_history(&thread.member.name, &entry)
        {
            eprintln!("History not saved for {}: {error}", thread.member.name);
        }
        thread.member.remember(entry);
        thread.sequence += 1;
        self.shared.changed.notify_all();
    }

    fn save(&self, threads: &[Thread]) {
        let crew: Vec<Member> = threads.iter().map(|thread| thread.member.clone()).collect();
        if let Err(error) = self.shared.store.save_roster(&crew) {
            eprintln!("Roster not saved: {error}");
        }
    }
}

fn locate(threads: &[Thread], name: &str) -> Result<usize, Failure> {
    threads
        .iter()
        .position(|thread| thread.member.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| refused(format!("No thread named {name}.")))
}

fn brief_role(role: &str) -> String {
    let compact: String = role
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
        .take(MAX_ROLE_CHARS)
        .collect();
    let compact = compact.trim();
    if compact.is_empty() {
        "specialist".into()
    } else {
        compact.into()
    }
}

pub(crate) fn bounded_text(label: &str, value: &str, limit: usize) -> Result<(), Failure> {
    if value.trim().is_empty() {
        return Err(invalid(format!("{label} must not be empty.")));
    }
    if value.chars().count() > limit {
        return Err(invalid(format!(
            "{label} must be at most {limit} characters."
        )));
    }
    if value.contains('\0') {
        return Err(invalid(format!("{label} must not contain NUL.")));
    }
    Ok(())
}

pub(crate) fn validate_role(role: &str) -> Result<(), Failure> {
    bounded_text("role", role, MAX_ROLE_CHARS)?;
    if role
        .trim()
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ')
    {
        Ok(())
    } else {
        Err(invalid(
            "role uses ASCII letters, digits, spaces, - or _ only.",
        ))
    }
}

pub(crate) fn resolve_workspace(root: &Path, requested: Option<&str>) -> Result<PathBuf, Failure> {
    let Some(requested) = requested else {
        return Ok(root.to_path_buf());
    };
    bounded_text("workspace", requested, MAX_PATH_CHARS)?;
    let path = Path::new(requested.trim());
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(invalid("workspace must not contain .. segments."));
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let resolved = joined
        .canonicalize()
        .map_err(|_| invalid("workspace must be an existing directory inside the root."))?;
    if !resolved.starts_with(root) || !resolved.is_dir() {
        return Err(invalid("workspace escapes the configured workspace root."));
    }
    Ok(resolved)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        fs,
        sync::mpsc::{self, Receiver, Sender},
    };

    pub(crate) struct Scratch(pub(crate) PathBuf);

    impl Scratch {
        pub(crate) fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ac-control-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(path.join("root")).unwrap();
            Self(path)
        }

        pub(crate) fn root(&self) -> PathBuf {
            self.0.join("root")
        }

        pub(crate) fn store(&self) -> Store {
            Store::new(self.0.join("profiles"))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct ScriptedRun {
        events: Receiver<mission::Event>,
        _open: Option<Sender<mission::Event>>,
    }

    impl Run for ScriptedRun {
        fn next(&self, timeout: Duration) -> Result<mission::Event, RecvTimeoutError> {
            self.events.recv_timeout(timeout)
        }
    }

    pub(crate) struct ScriptedEngine {
        pub(crate) hold: bool,
    }

    impl Engine for ScriptedEngine {
        fn start(
            &self,
            member: &Member,
            task: &str,
            _workspace: &Path,
        ) -> Result<Box<dyn Run>, String> {
            if task.contains("explode") {
                return Err("engine refused".into());
            }
            let (sender, events) = mpsc::channel();
            sender
                .send(mission::Event::Running {
                    profile: 0,
                    reason: None,
                })
                .unwrap();
            sender
                .send(mission::Event::Output(format!(
                    "{} handled: {}",
                    member.name,
                    task.lines().next().unwrap_or_default()
                )))
                .unwrap();
            let hold = self.hold || task.contains("hold");
            if task.contains("slow") {
                let delayed = sender.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(200));
                    let _ = delayed.send(mission::Event::Finished(Ok(())));
                });
            } else if !hold {
                sender.send(mission::Event::Finished(Ok(()))).unwrap();
            }
            Ok(Box::new(ScriptedRun {
                events,
                _open: hold.then_some(sender),
            }))
        }
    }

    pub(crate) fn control(scratch: &Scratch, hold: bool) -> Control {
        Control::open(
            scratch.store(),
            &scratch.root(),
            Box::new(ScriptedEngine { hold }),
        )
        .unwrap()
    }

    fn brief() -> Brief {
        Brief {
            role: "builder".into(),
            goal: "Ship it".into(),
            context: "Repo".into(),
            requirements: "Tests".into(),
            done_when: "Green".into(),
            do_not: "Break main".into(),
            deliverable: "PR URL".into(),
        }
    }

    #[test]
    fn scope_never_grants_full_access() {
        assert_eq!(Scope::parse("supervised").unwrap(), Scope::Supervised);
        assert_eq!(Scope::parse("read-only").unwrap(), Scope::ReadOnly);
        assert!(Scope::parse("full-access").unwrap_err().contains("never"));
        assert!(Scope::parse("root").is_err());
        assert!(!Scope::ReadOnly.allows_changes());
    }

    #[test]
    fn outside_threads_never_run_above_supervised() {
        for (requested, granted) in [
            (PermissionMode::ReadOnly, PermissionMode::ReadOnly),
            (PermissionMode::Supervised, PermissionMode::Supervised),
            (PermissionMode::AutoAcceptEdits, PermissionMode::Supervised),
            (PermissionMode::WorktreeAuto, PermissionMode::Supervised),
            (PermissionMode::WorkspaceAuto, PermissionMode::Supervised),
        ] {
            assert_eq!(external_mode(requested), granted);
        }
        let scratch = Scratch::new("modes");
        let control = control(&scratch, false);
        control.create("quill", "doc writer", None, None).unwrap();
        control.create("audit", "qa reviewer", None, None).unwrap();
        let crew = scratch.store().load_roster().unwrap();
        assert_eq!(crew[0].permission, PermissionMode::Supervised);
        assert_eq!(crew[1].permission, PermissionMode::ReadOnly);
    }

    #[test]
    fn workspace_paths_stay_inside_the_root() {
        let scratch = Scratch::new("paths");
        let root = scratch.root().canonicalize().unwrap();
        fs::create_dir(root.join("inner")).unwrap();
        assert_eq!(resolve_workspace(&root, None).unwrap(), root);
        assert_eq!(
            resolve_workspace(&root, Some("inner")).unwrap(),
            root.join("inner")
        );
        let outside = scratch.0.canonicalize().unwrap();
        for bad in [
            "../",
            "inner/../../",
            outside.to_str().unwrap(),
            "missing",
            "",
            "a\0b",
        ] {
            assert!(
                matches!(
                    resolve_workspace(&root, Some(bad)),
                    Err(Failure::Invalid(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn brief_renders_all_seven_fields_and_rejects_blanks() {
        let rendered = brief().render();
        for label in [
            "ROLE: builder",
            "GOAL: Ship it",
            "CONTEXT: Repo",
            "REQUIREMENTS: Tests",
            "DONE WHEN: Green",
            "DO NOT: Break main",
            "RETURN: PR URL",
        ] {
            assert!(rendered.contains(label), "{label}");
        }
        let mut blank = brief();
        blank.do_not = "  ".into();
        assert!(blank.validate().is_err());
        assert_eq!(brief_role("Rust builder (impl)"), "Rust builder");
        assert_eq!(brief_role("(x)"), "specialist");
    }

    #[test]
    fn threads_launch_finish_and_persist_history() {
        let scratch = Scratch::new("launch");
        let control = control(&scratch, false);
        control
            .create("nova", "reviewer", Some("Review"), None)
            .unwrap();
        assert!(matches!(
            control.create("NOVA", "x", None, None),
            Err(Failure::Invalid(_))
        ));
        control.launch("nova", "Check tests").unwrap();
        let waited = control.wait("nova", Duration::from_secs(5)).unwrap();
        assert!(waited.finished);
        assert_eq!(waited.thread.status, "DONE");
        let transcript = control.read("nova", 0, 10).unwrap();
        let texts: Vec<&str> = transcript.entries.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts[0], "TASK  Check tests");
        assert!(texts.contains(&"nova  nova handled: Check tests"));
        assert!(texts.last().unwrap().starts_with("nova  DONE"));
        assert_eq!(control.read("nova", 2, 10).unwrap().entries[0].seq, 3);
        assert_eq!(control.search("handled").unwrap().len(), 1);
        assert!(control.search("absent").unwrap().is_empty());
        let reopened = super::tests::control(&scratch, false);
        assert_eq!(reopened.list()[0].status, "DONE");
        assert_eq!(reopened.read("nova", 0, 50).unwrap().entries.len(), 3);
    }

    #[test]
    fn running_threads_queue_messages_and_interrupt_cleanly() {
        let scratch = Scratch::new("interrupt");
        let control = control(&scratch, true);
        control.create("atlas", "builder", None, None).unwrap();
        control.launch("atlas", "Long job").unwrap();
        assert!(matches!(
            control.launch("atlas", "Again"),
            Err(Failure::Refused(_))
        ));
        assert_eq!(control.message("atlas", "Follow up").unwrap().queued, 1);
        let pending = control.wait("atlas", Duration::from_millis(100)).unwrap();
        assert!(!pending.finished && pending.thread.running);
        control.interrupt("atlas").unwrap();
        let waited = control.wait("atlas", Duration::from_secs(5)).unwrap();
        assert!(waited.finished);
        assert_eq!(waited.thread.status, "ERROR");
        assert_eq!(waited.thread.queued, 0);
        assert!(control
            .read("atlas", 0, 50)
            .unwrap()
            .entries
            .iter()
            .any(|line| line.text == "atlas  ERROR · Interrupted"));
    }

    #[test]
    fn queued_messages_run_after_the_current_mission() {
        let scratch = Scratch::new("queue");
        let control = control(&scratch, false);
        control.create("echo", "writer", None, None).unwrap();
        control.launch("echo", "slow first").unwrap();
        assert_eq!(control.message("echo", "second").unwrap().queued, 1);
        let waited = control.wait("echo", Duration::from_secs(5)).unwrap();
        assert!(waited.finished);
        assert_eq!(waited.thread.mission, "second");
        let texts: Vec<String> = control
            .read("echo", 0, 50)
            .unwrap()
            .entries
            .into_iter()
            .map(|line| line.text)
            .collect();
        let first_done = texts
            .iter()
            .position(|t| t.starts_with("echo  DONE"))
            .unwrap();
        let second_task = texts.iter().position(|t| t == "TASK  second").unwrap();
        assert!(first_done < second_task);
        assert!(texts.contains(&"QUEUED  second".to_owned()));
    }

    #[test]
    fn delegate_creates_and_runs_with_the_rendered_brief() {
        let scratch = Scratch::new("delegate");
        let control = control(&scratch, false);
        control.delegate("forge", &brief(), None).unwrap();
        let waited = control.wait("forge", Duration::from_secs(5)).unwrap();
        assert_eq!(waited.thread.role, "builder");
        assert_eq!(waited.thread.status, "DONE");
        assert!(waited
            .thread
            .mission
            .starts_with("ROLE: builder\nGOAL: Ship it"));
        assert!(matches!(
            control.delegate("forge", &brief(), Some(".")),
            Err(Failure::Invalid(_))
        ));
        assert!(matches!(
            control.launch("forge", "explode"),
            Err(Failure::Refused(_))
        ));
        assert!(matches!(
            control.wait("forge", MAX_WAIT + Duration::from_secs(1)),
            Err(Failure::Invalid(_))
        ));
        assert!(matches!(
            control.read("ghost", 0, 1),
            Err(Failure::Refused(_))
        ));
    }
}
