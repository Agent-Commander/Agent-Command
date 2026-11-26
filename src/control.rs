use crate::{
    auth,
    bridge::{BackgroundResult, WakeQueue},
    catalog,
    delegation::{
        build_stage_prompt, parse_completion_contract, ClaimKind, CompletionEvent,
        CompletionEventKind, CompletionStatus, CompletionWakeJournal, ExitRecord,
        InspectResolution, ReservationLedger, RunOutcome, RunSpec, SourceBinding, StageCheckpoint,
        StageKind, StagePromptInput, DEFAULT_STAGE_TIMEOUT_SECS, HEARTBEAT_DEAD_SECS,
        HEARTBEAT_INTERVAL_SECS,
    },
    mission, native,
    permissions::PermissionMode,
    recovery::{
        decide_recovery, refusal_fingerprint, AttemptLog, FailedAttemptContext, FailureCause,
        RecoveryDecision, RefusalBackoffStore, RefusalKind,
    },
    routing, settings,
    specialists::{self, Entry, Isolation, Member, Status, Store},
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
pub(crate) const MAX_MODEL_CHARS: usize = 80;
pub(crate) const MAX_PATH_CHARS: usize = 1_024;
pub(crate) const MAX_QUEUE: usize = 16;
pub(crate) const DEFAULT_READ_LIMIT: usize = 50;
const PUMP_POLL: Duration = Duration::from_millis(20);
const ASYNC_SPAWN_NOTE: &str = "Worker spawned asynchronously. You will receive a <background-result> wakeup when it finishes; do not poll or sleep.";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Scope {
    ReadOnly,
    Supervised,
    Commander,
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
            Self::Commander => "commander",
        }
    }

    pub(crate) fn allows_changes(self) -> bool {
        matches!(self, Self::Supervised | Self::Commander)
    }

    pub(crate) fn allows_elevated_worker_modes(self) -> bool {
        self == Self::Commander
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
    settings_path: Option<PathBuf>,
}

impl NativeEngine {
    pub(crate) fn new(profile_root: PathBuf) -> Self {
        let settings_path = settings::LaunchConfig::from_env().settings_path;
        Self {
            profile_root,
            settings_path,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_settings_path(
        profile_root: PathBuf,
        settings_path: Option<PathBuf>,
    ) -> Self {
        Self {
            profile_root,
            settings_path,
        }
    }

    pub(crate) fn limit_policy(&self) -> mission::LimitPolicy {
        let loaded = self
            .settings_path
            .as_deref()
            .and_then(|path| settings::Settings::load(path).ok())
            .unwrap_or_default();
        mission::LimitPolicy {
            auto_resume_on_reset: loaded.auto_resume_on_reset,
            fallback_profile: loaded.fallback_profile,
        }
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
            limits: self.limit_policy(),
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
        let profiles = auth::all_profiles(&self.profile_root).map_err(|error| error.to_string())?;
        if profiles.is_empty() {
            return Err(format!("{}.", crate::panel::NO_PROVIDER_HINT));
        }
        let request = crate::framed(routing::parse(task)?, |unit| {
            specialists::specialist_prompt(member, unit)
        });
        let route = self.route(request, member.permission)?;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SpawnWorkerRequest {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) task: String,
    pub(crate) model: Option<String>,
    pub(crate) mode: Option<PermissionMode>,
    pub(crate) workspace: Option<String>,
    pub(crate) allow_elevated_mode: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SpawnedWorker {
    pub(crate) worker_id: String,
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<String>,
    pub(crate) workspace: String,
    pub(crate) mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    pub(crate) running: bool,
    pub(crate) note: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Summary {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) status: &'static str,
    pub(crate) mission: String,
    pub(crate) workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<String>,
    pub(crate) mode: &'static str,
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

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DelegatedStageRequest {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) task_id: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) stage: StageKind,
    pub(crate) workspace: Option<String>,
    pub(crate) source_binding: Option<SourceBinding>,
    pub(crate) prior_checkpoint: Option<StageCheckpoint>,
    pub(crate) decision_answer: Option<String>,
    pub(crate) team_directions: Vec<String>,
    pub(crate) retry_context: Option<FailedAttemptContext>,
    pub(crate) allow_elevated_mode: bool,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DelegatedRunHandle {
    pub(crate) reservation_id: String,
    pub(crate) work_class: routing::WorkClass,
    pub(crate) stage: StageKind,
    pub(crate) worker: Summary,
}

struct Thread {
    member: Member,
    sequence: u64,
    queue: VecDeque<String>,
    stop: Option<Arc<AtomicBool>>,
    active_reservation: Option<String>,
    active_task_id: Option<String>,
    active_stage: StageKind,
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
            branch: self.member.branch().map(|branch| branch.name.clone()),
            mode: self.member.permission.flag(),
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
    TimedOut,
    Stalled,
}

struct DelegationRuntime {
    reservations: Option<ReservationLedger>,
    wake_journal: Option<CompletionWakeJournal>,
    attempt_log: Option<AttemptLog>,
    backoff: RefusalBackoffStore,
    backoff_path: Option<PathBuf>,
    stage_timeout: Duration,
    stall_timeout: Duration,
}

impl DelegationRuntime {
    fn open(store: &Store, root: &Path) -> Self {
        let state_dir = store
            .ledger_path_for_workspace(root)
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let (reservations, wake_journal, attempt_log, backoff, backoff_path) = match state_dir {
            Some(dir) => {
                let reservations = ReservationLedger::open(dir.join("delegation")).ok();
                let wake_journal = CompletionWakeJournal::open(dir.join("delegation-wake")).ok();
                let recovery_dir = dir.join("stuck-recovery");
                let attempt_log = Some(AttemptLog::new(recovery_dir.join("attempts.jsonl")));
                let backoff_path = recovery_dir.join("refusal-backoff.json");
                let backoff = RefusalBackoffStore::load(&backoff_path).unwrap_or_default();
                (
                    reservations,
                    wake_journal,
                    attempt_log,
                    backoff,
                    Some(backoff_path),
                )
            }
            None => (None, None, None, RefusalBackoffStore::default(), None),
        };
        Self {
            reservations,
            wake_journal,
            attempt_log,
            backoff,
            backoff_path,
            stage_timeout: Duration::from_secs(DEFAULT_STAGE_TIMEOUT_SECS),
            stall_timeout: Duration::from_secs(HEARTBEAT_DEAD_SECS as u64),
        }
    }

    fn save_backoff(&self) {
        if let Some(path) = &self.backoff_path {
            let _ = self.backoff.save(path);
        }
    }
}

struct Shared {
    state: Mutex<Vec<Thread>>,
    changed: Condvar,
    store: Store,
    root: PathBuf,
    engine: Box<dyn Engine>,
    shutdown: Arc<AtomicBool>,
    wake_queue: Option<WakeQueue>,
    delegation: Mutex<DelegationRuntime>,
}

#[derive(Clone)]
pub(crate) struct Control {
    shared: Arc<Shared>,
}

impl Control {
    pub(crate) fn open(store: Store, root: &Path, engine: Box<dyn Engine>) -> io::Result<Self> {
        Self::open_with(store, root, engine, Arc::new(AtomicBool::new(false)), None)
    }

    pub(crate) fn open_with(
        store: Store,
        root: &Path,
        engine: Box<dyn Engine>,
        shutdown: Arc<AtomicBool>,
        wake_queue: Option<WakeQueue>,
    ) -> io::Result<Self> {
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
                active_reservation: None,
                active_task_id: None,
                active_stage: StageKind::Implementation,
            })
            .collect();
        let delegation = DelegationRuntime::open(&store, &root);
        let control = Self {
            shared: Arc::new(Shared {
                state: Mutex::new(threads),
                changed: Condvar::new(),
                store,
                root,
                engine,
                shutdown,
                wake_queue,
                delegation: Mutex::new(delegation),
            }),
        };
        let _ = control.inspect_reservations(now_epoch());
        Ok(control)
    }

    fn lock_delegation(&self) -> MutexGuard<'_, DelegationRuntime> {
        self.shared
            .delegation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn with_timeouts(&self, stage_timeout: Duration, stall_timeout: Duration) {
        let mut runtime = self.lock_delegation();
        runtime.stage_timeout = stage_timeout;
        runtime.stall_timeout = stall_timeout;
    }

    pub(crate) fn inspect_reservations(&self, now: i64) -> Vec<(String, InspectResolution)> {
        let mut runtime = self.lock_delegation();
        let Some(ledger) = runtime.reservations.as_mut() else {
            return Vec::new();
        };
        let active_ids: Vec<String> = ledger
            .records()
            .into_iter()
            .filter(|record| record.state.holds_slot())
            .map(|record| record.reservation_id)
            .collect();
        let mut resolutions = Vec::new();
        for id in active_ids {
            if let Ok(resolution) = ledger.inspect(&id, now) {
                resolutions.push((id, resolution));
            }
        }
        resolutions
    }

    #[cfg(test)]
    pub(crate) fn reservations(&self) -> Vec<crate::delegation::ReservationRecord> {
        let runtime = self.lock_delegation();
        runtime
            .reservations
            .as_ref()
            .map(ReservationLedger::records)
            .unwrap_or_default()
    }

    pub(crate) fn poll_completion_wakes(&self, now: i64) -> Vec<CompletionEvent> {
        let mut runtime = self.lock_delegation();
        runtime
            .wake_journal
            .as_mut()
            .and_then(|journal| journal.poll_new_events(now).ok())
            .unwrap_or_default()
    }

    pub(crate) fn evaluate_recovery(&self, key: &str, cause: FailureCause) -> RecoveryDecision {
        let runtime = self.lock_delegation();
        let attempts = runtime
            .attempt_log
            .as_ref()
            .and_then(|log| log.for_key(key).ok())
            .unwrap_or_default();
        decide_recovery(cause, &attempts)
    }

    #[cfg(test)]
    pub(crate) fn record_recovery_attempt(
        &self,
        key: &str,
        cause: FailureCause,
        recovery: &str,
        outcome: crate::recovery::AttemptOutcome,
        detail: &str,
        now: i64,
    ) -> io::Result<crate::recovery::RecoveryAttempt> {
        let runtime = self.lock_delegation();
        let log = runtime
            .attempt_log
            .as_ref()
            .ok_or_else(|| io::Error::other("Attempt log unavailable"))?;
        log.record(key, cause, recovery, outcome, detail, now)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.shared.root
    }

    pub(crate) fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.shutdown)
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shared.shutdown.load(Ordering::SeqCst)
    }

    pub(crate) fn wake_queue(&self) -> Option<&WakeQueue> {
        self.shared.wake_queue.as_ref()
    }

    pub(crate) fn shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        {
            let mut threads = self.lock();
            for thread in threads.iter_mut() {
                thread.queue.clear();
                if let Some(stop) = &thread.stop {
                    stop.store(true, Ordering::SeqCst);
                }
            }
        }
        if let Some(queue) = &self.shared.wake_queue {
            queue.shutdown();
        }
        self.shared.changed.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Thread>> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn sync_external(&self, threads: &mut Vec<Thread>) {
        let Ok(disk_members) = self.shared.store.load_roster() else {
            return;
        };
        for member in disk_members {
            let exists = threads
                .iter()
                .any(|thread| thread.member.name.eq_ignore_ascii_case(&member.name));
            if !exists {
                threads.push(Thread {
                    sequence: member.history.len() as u64,
                    member,
                    queue: VecDeque::new(),
                    stop: None,
                    active_reservation: None,
                    active_task_id: None,
                    active_stage: StageKind::Implementation,
                });
            }
        }
    }

    pub(crate) fn list(&self) -> Vec<Summary> {
        let _ = self.poll_completion_wakes(now_epoch());
        let mut threads = self.lock();
        self.sync_external(&mut threads);
        threads.iter().map(Thread::summary).collect()
    }

    pub(crate) fn count(&self) -> usize {
        let mut threads = self.lock();
        self.sync_external(&mut threads);
        threads.len()
    }

    pub(crate) fn search(&self, query: &str) -> Result<Vec<Summary>, Failure> {
        bounded_text("query", query, MAX_QUERY_CHARS)?;
        let needle = query.trim().to_lowercase();
        let mut threads = self.lock();
        self.sync_external(&mut threads);
        Ok(threads
            .iter()
            .filter(|thread| thread.matches(&needle))
            .map(Thread::summary)
            .collect())
    }

    pub(crate) fn read(&self, name: &str, since: u64, limit: usize) -> Result<Transcript, Failure> {
        let mut threads = self.lock();
        self.sync_external(&mut threads);
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
        self.create_with_mode(name, role, task, workspace, None, false)
    }

    fn create_with_mode(
        &self,
        name: &str,
        role: &str,
        task: Option<&str>,
        workspace: Option<&str>,
        requested_mode: Option<PermissionMode>,
        allow_elevated: bool,
    ) -> Result<Summary, Failure> {
        if self.is_shutting_down() {
            return Err(refused("Control plane is shutting down."));
        }
        validate_role(role)?;
        if let Some(task) = task {
            bounded_text("task", task, MAX_TEXT_CHARS)?;
        }
        let base = resolve_workspace(&self.shared.root, workspace)?;
        let mut threads = self.lock();
        self.sync_external(&mut threads);
        let crew: Vec<Member> = threads.iter().map(|thread| thread.member.clone()).collect();
        specialists::validate_name(name, &crew).map_err(invalid)?;
        let isolated = self
            .shared
            .store
            .isolate(&base, name, task.map(str::trim).unwrap_or(role.trim()))
            .map_err(|error| refused(format!("Cannot isolate {name}: {error}")))?;
        let base_default = PermissionMode::default_for_role(role.trim());
        let chosen_mode = if allow_elevated {
            let mode = requested_mode.unwrap_or_else(|| {
                if base_default == PermissionMode::ReadOnly {
                    PermissionMode::ReadOnly
                } else if matches!(isolated.isolation, Isolation::Worktree { .. }) {
                    PermissionMode::WorktreeAuto
                } else {
                    PermissionMode::WorkspaceAuto
                }
            });
            if let Err(refusal) = mode.require_isolation(Some(&isolated.isolation)) {
                return Err(invalid(refusal));
            }
            mode
        } else {
            external_mode(requested_mode.unwrap_or(base_default))
        };
        let member = Member::new(name, role.trim(), task.map(str::trim), Some(isolated))
            .with_permission(Some(chosen_mode));
        threads.push(Thread {
            member,
            sequence: 0,
            queue: VecDeque::new(),
            stop: None,
            active_reservation: None,
            active_task_id: None,
            active_stage: StageKind::Implementation,
        });
        self.save(&threads);
        let summary = threads[threads.len() - 1].summary();
        self.shared.changed.notify_all();
        Ok(summary)
    }

    pub(crate) fn spawn_worker(
        &self,
        request: &SpawnWorkerRequest,
    ) -> Result<SpawnedWorker, Failure> {
        if self.is_shutting_down() {
            return Err(refused("Control plane is shutting down."));
        }
        validate_role(&request.role)?;
        bounded_text("task", &request.task, MAX_TEXT_CHARS)?;
        let model = match request.model.as_deref() {
            Some(model) => {
                bounded_text("model", model, MAX_MODEL_CHARS)?;
                Some(model.trim().to_owned())
            }
            None => None,
        };
        let exists = {
            let mut threads = self.lock();
            self.sync_external(&mut threads);
            locate(&threads, &request.name).is_ok()
        };
        if !exists {
            self.create_with_mode(
                &request.name,
                &request.role,
                Some(&request.task),
                request.workspace.as_deref(),
                request.mode,
                request.allow_elevated_mode,
            )?;
        } else if request.workspace.is_some() {
            return Err(invalid(format!(
                "{} already exists; its workspace cannot be changed.",
                request.name
            )));
        }
        let formatted_task = format_worker_task(&request.task, model.as_deref());
        let summary = self.launch(&request.name, &formatted_task)?;
        Ok(SpawnedWorker {
            worker_id: summary.name.clone(),
            name: summary.name,
            role: summary.role,
            status: summary.status,
            branch: summary.branch,
            workspace: summary.workspace,
            mode: summary.mode,
            model,
            running: summary.running,
            note: ASYNC_SPAWN_NOTE.into(),
        })
    }

    pub(crate) fn launch(&self, name: &str, task: &str) -> Result<Summary, Failure> {
        if self.is_shutting_down() {
            return Err(refused("Control plane is shutting down."));
        }
        bounded_text("task", task, MAX_TEXT_CHARS)?;
        let mut threads = self.lock();
        self.sync_external(&mut threads);
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
        if self.is_shutting_down() {
            return Err(refused("Control plane is shutting down."));
        }
        bounded_text("message", text, MAX_TEXT_CHARS)?;
        let mut threads = self.lock();
        self.sync_external(&mut threads);
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
        let exists = {
            let mut threads = self.lock();
            self.sync_external(&mut threads);
            locate(&threads, name).is_ok()
        };
        if !exists {
            self.create(name, &role, Some(brief.goal.trim()), workspace)?;
        } else if workspace.is_some() {
            return Err(invalid(format!(
                "{name} already exists; its workspace cannot be changed."
            )));
        }
        self.launch(name, &brief.render())
    }

    #[allow(dead_code)]
    pub(crate) fn delegate_stage(
        &self,
        request: &DelegatedStageRequest,
    ) -> Result<DelegatedRunHandle, Failure> {
        if self.is_shutting_down() {
            return Err(refused("Control plane is shutting down."));
        }
        validate_role(&request.role)?;
        bounded_text("task_id", &request.task_id, MAX_MODEL_CHARS)?;
        bounded_text("title", &request.title, MAX_BRIEF_FIELD_CHARS)?;
        bounded_text("body", &request.body, MAX_TEXT_CHARS)?;
        let prompt = build_stage_prompt(&StagePromptInput {
            task_id: request.task_id.clone(),
            title: request.title.clone(),
            body: request.body.clone(),
            stage: request.stage,
            prior_checkpoint: request.prior_checkpoint.clone(),
            decision_answer: request.decision_answer.clone(),
            team_directions: request.team_directions.clone(),
            source_patch: None,
            source_patch_sha256: request
                .prior_checkpoint
                .as_ref()
                .map(|cp| cp.patch_sha256.clone()),
            retry_context: request.retry_context.clone(),
        });
        let classification = routing::classify_task(
            &request.title,
            &request.body,
            request.stage.is_verification(),
            request.retry_context.is_some(),
            None,
        );
        let requested_mode = if request.stage.is_verification() {
            Some(PermissionMode::ReadOnly)
        } else {
            None
        };
        let exists = {
            let mut threads = self.lock();
            self.sync_external(&mut threads);
            locate(&threads, &request.name).is_ok()
        };
        if !exists {
            self.create_with_mode(
                &request.name,
                &request.role,
                Some(&request.title),
                request.workspace.as_deref(),
                requested_mode,
                request.allow_elevated_mode,
            )?;
        } else if request.workspace.is_some() {
            return Err(invalid(format!(
                "{} already exists; its workspace cannot be changed.",
                request.name
            )));
        }
        let mut threads = self.lock();
        self.sync_external(&mut threads);
        let index = locate(&threads, &request.name)?;
        if threads[index].running() {
            return Err(refused(format!(
                "{} is already running. Use message_thread to queue work or interrupt_thread to stop it.",
                threads[index].member.name
            )));
        }
        let reservation_id = self.start_with_meta(
            &mut threads,
            index,
            prompt,
            Some(request.task_id.clone()),
            request.stage,
            request.source_binding.clone(),
            classification.work_class,
        )?;
        Ok(DelegatedRunHandle {
            reservation_id,
            work_class: classification.work_class,
            stage: request.stage,
            worker: threads[index].summary(),
        })
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
        self.sync_external(&mut threads);
        loop {
            let index = locate(&threads, name)?;
            let thread = &threads[index];
            if !thread.running() && thread.queue.is_empty() {
                return Ok(Waited {
                    thread: thread.summary(),
                    finished: true,
                });
            }
            if self.is_shutting_down() {
                return Ok(Waited {
                    thread: thread.summary(),
                    finished: false,
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
        self.sync_external(&mut threads);
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
        self.shared.changed.notify_all();
        Ok(threads[index].summary())
    }

    pub(crate) fn changed_since(&self, name: &str, seen: u64, timeout: Duration) -> bool {
        let threads = self.lock();
        if self.is_shutting_down() {
            return true;
        }
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
        self.is_shutting_down() || current(&threads) != seen
    }

    fn start(&self, threads: &mut [Thread], index: usize, task: String) -> Result<(), Failure> {
        let classification =
            routing::classify_task(&threads[index].member.name, &task, false, false, None);
        let _ = self.start_with_meta(
            threads,
            index,
            task,
            None,
            StageKind::Implementation,
            None,
            classification.work_class,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_meta(
        &self,
        threads: &mut [Thread],
        index: usize,
        task: String,
        explicit_task_id: Option<String>,
        stage: StageKind,
        source_binding: Option<SourceBinding>,
        work_class: routing::WorkClass,
    ) -> Result<String, Failure> {
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
        let now = now_epoch();
        let task_id = explicit_task_id
            .unwrap_or_else(|| format!("thread:{}", thread.member.name.to_ascii_lowercase()));
        let fingerprint = refusal_fingerprint(&task_id, &task, &thread.member.name);
        let reservation_id = format!("res-{}", uuid::Uuid::new_v4().simple());
        {
            let mut runtime = self.lock_delegation();
            if let Some(backoff) = runtime.backoff.active_backoff(&task_id, &fingerprint, now) {
                return Err(refused(format!(
                    "Task {task_id} is backed off until epoch {}: {}",
                    backoff.until, backoff.reason
                )));
            }
            let timeout_secs = runtime.stage_timeout.as_secs();
            if let Some(ledger) = runtime.reservations.as_mut() {
                let spec = RunSpec {
                    reservation_id: reservation_id.clone(),
                    task_id: task_id.clone(),
                    worker_name: thread.member.name.clone(),
                    stage,
                    work_class,
                    profile: thread.member.role.clone(),
                    requested_model: None,
                    requested_effort: None,
                    workspace: workspace.clone(),
                    source_binding,
                    timeout_secs,
                    created_at: now,
                };
                if let Err(error) = ledger.reserve(&spec, 0.0, None, now) {
                    return Err(refused(error.to_string()));
                }
            }
        }
        let run = match self.shared.engine.start(&thread.member, &task, &workspace) {
            Ok(run) => run,
            Err(error) => {
                let mut runtime = self.lock_delegation();
                if let Some(ledger) = runtime.reservations.as_mut() {
                    let _ = ledger.claim_once(&reservation_id, ClaimKind::Refusal, &error, now);
                    let _ = ledger.finalize(
                        &reservation_id,
                        &ExitRecord {
                            outcome: RunOutcome::Refused,
                            summary: error.clone(),
                            patch_sha256: None,
                            completed_at: now,
                        },
                    );
                }
                runtime.backoff.record_refusal(
                    &task_id,
                    &fingerprint,
                    RefusalKind::Busy,
                    &error,
                    now,
                );
                runtime.save_backoff();
                return Err(refused(error));
            }
        };
        {
            let mut runtime = self.lock_delegation();
            if let Some(ledger) = runtime.reservations.as_mut() {
                let _ = ledger.claim_once(&reservation_id, ClaimKind::Runner, "started", now);
                let _ = ledger.write_pid(&reservation_id, std::process::id(), now);
                let _ = ledger.write_heartbeat(&reservation_id, now, "running");
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        thread.stop = Some(Arc::clone(&stop));
        thread.active_reservation = Some(reservation_id.clone());
        thread.active_task_id = Some(task_id);
        thread.active_stage = stage;
        thread.member.status = Status::Running;
        thread.member.mission = task.clone();
        self.record(thread, format!("TASK  {task}"));
        self.save(threads);
        let control = self.clone();
        let name = threads[index].member.name.clone();
        let pumped_res = reservation_id.clone();
        thread::spawn(move || control.pump(&name, run, &stop, Some(pumped_res)));
        Ok(reservation_id)
    }

    fn pump(
        &self,
        name: &str,
        run: Box<dyn Run>,
        stop: &AtomicBool,
        reservation_id: Option<String>,
    ) {
        let mut reply = String::new();
        let started = Instant::now();
        let mut last_activity = Instant::now();
        let mut last_heartbeat = Instant::now();
        let (stage_timeout, stall_timeout) = {
            let runtime = self.lock_delegation();
            (runtime.stage_timeout, runtime.stall_timeout)
        };
        let heartbeat_every = Duration::from_secs(HEARTBEAT_INTERVAL_SECS);
        let outcome = loop {
            if stop.load(Ordering::SeqCst) || self.is_shutting_down() {
                break Outcome::Interrupted;
            }
            if started.elapsed() >= stage_timeout {
                break Outcome::TimedOut;
            }
            if last_activity.elapsed() >= stall_timeout {
                break Outcome::Stalled;
            }
            if let Some(res_id) = reservation_id.as_deref() {
                if last_heartbeat.elapsed() >= heartbeat_every {
                    let mut runtime = self.lock_delegation();
                    if let Some(ledger) = runtime.reservations.as_mut() {
                        let _ = ledger.write_heartbeat(res_id, now_epoch(), "running");
                    }
                    last_heartbeat = Instant::now();
                }
            }
            match run.next(PUMP_POLL) {
                Ok(mission::Event::Output(text)) => {
                    last_activity = Instant::now();
                    reply.push_str(&text);
                    reply.push('\n');
                    self.record_output(name, &text);
                }
                Ok(mission::Event::Running {
                    reason: Some(reason),
                    ..
                }) => {
                    last_activity = Instant::now();
                    self.record_named(name, format!("SYSTEM  {reason}"));
                }
                Ok(mission::Event::Continuing { .. }) => {
                    last_activity = Instant::now();
                    self.record_named(name, "SYSTEM  Continuing on another profile".into());
                }
                Ok(mission::Event::Snoozed { until, .. }) => {
                    last_activity = Instant::now();
                    self.record_named(
                        name,
                        format!("SYSTEM  Snoozed until the limit resets at epoch {until}"),
                    );
                }
                Ok(mission::Event::Running { reason: None, .. }) => {
                    last_activity = Instant::now();
                }
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
        let now = now_epoch();
        let (wake_event, reservation_done) = {
            let mut threads = self.lock();
            let Ok(index) = locate(&threads, name) else {
                return;
            };
            let thread = &mut threads[index];
            thread.stop = None;
            let reservation_id = thread.active_reservation.take();
            let task_id = thread
                .active_task_id
                .take()
                .unwrap_or_else(|| format!("thread:{}", thread.member.name.to_ascii_lowercase()));
            let (status, detail, run_outcome, failure_cause) = match outcome {
                Outcome::Finished(Ok(())) => match parse_completion_contract(reply) {
                    CompletionStatus::Blocked { reason, .. } => {
                        (Status::NeedsApproval, reason, RunOutcome::Blocked, None)
                    }
                    _ => match specialists::approval_request(reply) {
                        Some(question) => {
                            (Status::NeedsApproval, question, RunOutcome::Blocked, None)
                        }
                        None => (
                            Status::Done,
                            specialists::summary(reply),
                            RunOutcome::Succeeded,
                            None,
                        ),
                    },
                },
                Outcome::Finished(Err(error)) => (
                    Status::Error,
                    error.to_string(),
                    RunOutcome::Failed,
                    Some(FailureCause::ExecutionFailed),
                ),
                Outcome::Interrupted => (
                    Status::Error,
                    "Interrupted".into(),
                    RunOutcome::Interrupted,
                    None,
                ),
                Outcome::TimedOut => (
                    Status::Error,
                    "Stage timed out".into(),
                    RunOutcome::TimedOut,
                    Some(FailureCause::Stalled),
                ),
                Outcome::Stalled => (
                    Status::Error,
                    "Worker stalled without activity".into(),
                    RunOutcome::Stalled,
                    Some(FailureCause::Stalled),
                ),
            };
            if let Some(res_id) = reservation_id.as_deref() {
                let mut runtime = self.lock_delegation();
                if let Some(ledger) = runtime.reservations.as_mut() {
                    let _ = ledger.finalize(
                        res_id,
                        &ExitRecord {
                            outcome: run_outcome,
                            summary: detail.clone(),
                            patch_sha256: None,
                            completed_at: now,
                        },
                    );
                }
                if let Some(journal) = runtime.wake_journal.as_ref() {
                    let _ = journal.record_event(&CompletionEvent {
                        kind: CompletionEventKind::RunReport,
                        task_id: task_id.clone(),
                        reservation_id: res_id.to_owned(),
                        state: status.label().to_ascii_lowercase(),
                        at: now,
                        pid: None,
                    });
                    let _ = journal.record_event(&CompletionEvent {
                        kind: CompletionEventKind::SlotReleased,
                        task_id: task_id.clone(),
                        reservation_id: res_id.to_owned(),
                        state: status.label().to_ascii_lowercase(),
                        at: now,
                        pid: Some(std::process::id()),
                    });
                }
                if runtime.backoff.clear_on_slot_released() > 0 {
                    runtime.save_backoff();
                }
            }
            thread.member.status = status;
            let label = status.label();
            self.record(thread, format!("{name}  {label} · {detail}"));
            let event = BackgroundResult {
                worker_id: thread.member.name.clone(),
                role: thread.member.role.clone(),
                status: label.into(),
                branch: thread.member.branch().map(|branch| branch.name.clone()),
                workspace: specialists::workspace_label(&thread.member),
                summary: detail,
                is_error: status == Status::Error,
                finished_at: chrono::Local::now().format("%H:%M:%S").to_string(),
            };
            if !self.is_shutting_down() {
                if let Some(next) = thread.queue.pop_front() {
                    if let Err(failure) = self.start(&mut threads, index, next) {
                        self.record(&mut threads[index], format!("{name}  ERROR · {failure}"));
                    }
                }
            } else {
                thread.queue.clear();
            }
            self.save(&threads);
            self.shared.changed.notify_all();
            (event, failure_cause.map(|cause| (task_id, cause)))
        };
        if let Some((task_id, cause)) = reservation_done {
            let _ = self.evaluate_recovery(&task_id, cause);
        }
        if let Some(queue) = &self.shared.wake_queue {
            queue.push(wake_event);
        }
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
        if let Err(error) = self.shared.store.save_roster_merging(&crew, &[]) {
            eprintln!("Roster not saved: {error}");
        }
    }
}

fn now_epoch() -> i64 {
    chrono::Utc::now().timestamp()
}

fn format_worker_task(task: &str, model: Option<&str>) -> String {
    let trimmed = task.trim();
    match model {
        Some(model) if !trimmed.starts_with('@') => {
            let tag = if model.starts_with('@') {
                model.to_owned()
            } else {
                format!("@{model}")
            };
            format!("{tag} {trimmed}")
        }
        _ => trimmed.to_owned(),
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
        assert!(Scope::parse("commander").is_err());
        assert!(Scope::parse("root").is_err());
        assert!(!Scope::ReadOnly.allows_changes());
        assert!(Scope::Commander.allows_elevated_worker_modes());
        assert!(!Scope::Supervised.allows_elevated_worker_modes());
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
    fn native_engine_loads_limit_policy_from_settings_toml() {
        let scratch = Scratch::new("settings-policy");
        let settings_file = scratch.0.join("settings.toml");
        fs::write(
            &settings_file,
            "auto_resume_on_reset = true\nfallback_profile = \"codex/backup\"\n",
        )
        .unwrap();
        let engine = NativeEngine::with_settings_path(scratch.0.clone(), Some(settings_file));
        let policy = engine.limit_policy();
        assert!(policy.auto_resume_on_reset);
        assert_eq!(policy.fallback_profile.as_deref(), Some("codex/backup"));

        let missing = NativeEngine::with_settings_path(
            scratch.0.clone(),
            Some(scratch.0.join("missing.toml")),
        );
        assert_eq!(missing.limit_policy(), mission::LimitPolicy::default());
    }

    #[test]
    fn cooperative_shutdown_stops_running_workers_and_refuses_new_work() {
        let scratch = Scratch::new("shutdown");
        let shutdown = Arc::new(AtomicBool::new(false));
        let wake = WakeQueue::new();
        let control = Control::open_with(
            scratch.store(),
            &scratch.root(),
            Box::new(ScriptedEngine { hold: true }),
            Arc::clone(&shutdown),
            Some(wake.clone()),
        )
        .unwrap();
        control.create("atlas", "builder", None, None).unwrap();
        control.launch("atlas", "hold work").unwrap();
        control.shutdown();
        let waited = control.wait("atlas", Duration::from_secs(2)).unwrap();
        assert!(!waited.thread.running || control.is_shutting_down());
        assert!(matches!(
            control.launch("atlas", "after shutdown"),
            Err(Failure::Refused(_))
        ));
    }

    #[test]
    fn external_roster_additions_are_reloaded_without_clobbering() {
        let scratch = Scratch::new("roster-sync");
        let control = control(&scratch, false);
        control
            .create("daemon_worker", "builder", None, None)
            .unwrap();

        let tui_member = Member::new("tui_worker", "reviewer", Some("Check UI"), None);
        scratch
            .store()
            .save_roster_merging(&[Member::commander(), tui_member], &[])
            .unwrap();

        let listed = control.list();
        let names: Vec<&str> = listed.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"daemon_worker"), "{names:?}");
        assert!(names.contains(&"tui_worker"), "{names:?}");

        control
            .create("second_daemon", "writer", None, None)
            .unwrap();
        let persisted = scratch.store().load_roster().unwrap();
        let persisted_names: Vec<&str> = persisted.iter().map(|m| m.name.as_str()).collect();
        assert!(
            persisted_names.contains(&"tui_worker"),
            "{persisted_names:?}"
        );
        assert!(
            persisted_names.contains(&"daemon_worker"),
            "{persisted_names:?}"
        );
        assert!(
            persisted_names.contains(&"second_daemon"),
            "{persisted_names:?}"
        );
    }

    #[test]
    fn spawn_worker_is_non_blocking_and_pushes_background_result_to_wake_queue() {
        let scratch = Scratch::new("spawn-worker");
        let wake = WakeQueue::new();
        let control = Control::open_with(
            scratch.store(),
            &scratch.root(),
            Box::new(ScriptedEngine { hold: false }),
            Arc::new(AtomicBool::new(false)),
            Some(wake.clone()),
        )
        .unwrap();
        let spawned = control
            .spawn_worker(&SpawnWorkerRequest {
                name: "forge".into(),
                role: "builder".into(),
                task: "slow build".into(),
                model: Some("codex".into()),
                mode: None,
                workspace: None,
                allow_elevated_mode: true,
            })
            .unwrap();
        assert_eq!(spawned.worker_id, "forge");
        assert!(spawned.running);
        assert_eq!(spawned.mode, "workspace-auto");
        assert!(spawned.note.contains("<background-result>"));

        let result = wake.wait_next(Duration::from_secs(3)).unwrap();
        assert_eq!(result.worker_id, "forge");
        assert_eq!(result.status, "DONE");
        assert!(!result.is_error);
        assert!(result.summary.contains("@codex slow build"));
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

    #[test]
    fn delegate_stage_creates_reservation_wake_events_and_enforces_timeouts_and_recovery() {
        let scratch = Scratch::new("stage-delegation");
        let ctrl = control(&scratch, false);
        let handle = ctrl
            .delegate_stage(&DelegatedStageRequest {
                name: "verifier".into(),
                role: "reviewer".into(),
                task_id: "M-042".into(),
                title: "Verify inbox schema".into(),
                body: "Inspect src/inbox.rs without modifying files.".into(),
                stage: StageKind::Verification,
                workspace: None,
                source_binding: Some(SourceBinding {
                    repo: "Agent-Commander/Agent-Command".into(),
                    source_revision: "rev-42".into(),
                    target_revision: "main".into(),
                }),
                prior_checkpoint: None,
                decision_answer: Some("Keep atomic JSON.".into()),
                team_directions: vec!["No source comments.".into()],
                retry_context: None,
                allow_elevated_mode: true,
            })
            .unwrap();
        assert_eq!(handle.work_class, routing::WorkClass::Verification);
        assert_eq!(handle.stage, StageKind::Verification);
        assert_eq!(handle.worker.mode, "read-only");

        let waited = ctrl.wait("verifier", Duration::from_secs(5)).unwrap();
        assert!(waited.finished);
        assert_eq!(waited.thread.status, "DONE");
        assert!(ctrl
            .reservations()
            .iter()
            .any(|res| res.reservation_id == handle.reservation_id
                && res.state == crate::delegation::ReservationState::Completed));
        let wakes = ctrl.poll_completion_wakes(now_epoch() + 1);
        assert!(!wakes.is_empty());

        let hold_scratch = Scratch::new("stage-timeout");
        let hold_control = control(&hold_scratch, true);
        hold_control.with_timeouts(Duration::from_millis(80), Duration::from_secs(5));
        hold_control
            .create("slowpoke", "builder", None, None)
            .unwrap();
        hold_control.launch("slowpoke", "hold forever").unwrap();
        let timed_out = hold_control
            .wait("slowpoke", Duration::from_secs(3))
            .unwrap();
        assert!(timed_out.finished);
        assert_eq!(timed_out.thread.status, "ERROR");
        assert!(hold_control
            .read("slowpoke", 0, 50)
            .unwrap()
            .entries
            .iter()
            .any(|line| line.text.contains("Stage timed out")));

        let key = "agent-command:M-042:rev-42";
        assert!(matches!(
            ctrl.evaluate_recovery(key, FailureCause::ExecutionFailed),
            RecoveryDecision::Retry(_)
        ));
        ctrl.record_recovery_attempt(
            key,
            FailureCause::ExecutionFailed,
            "requeue",
            crate::recovery::AttemptOutcome::Failed,
            "Requeue failed with exit code 1",
            now_epoch(),
        )
        .unwrap();
        assert!(matches!(
            ctrl.evaluate_recovery(key, FailureCause::ExecutionFailed),
            RecoveryDecision::Escalate(_)
        ));
    }
}
