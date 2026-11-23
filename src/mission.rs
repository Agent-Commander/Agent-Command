use crate::auth::{Profile, Provider};
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
    Running,
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
    pub(crate) fn launch(profile: Profile, workspace: PathBuf, prompt: String) -> Self {
        let (sender, events) = mpsc::sync_channel(64);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            let result = run(profile, workspace, prompt, &sender, &worker_cancel);
            let _ = sender.send(Event::Finished(result));
        });
        Self { events, cancel }
    }
}

fn run(
    profile: Profile,
    workspace: PathBuf,
    prompt: String,
    sender: &SyncSender<Event>,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let mut command = profile.authenticated_command()?;
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
    let _ = sender.send(Event::Running);
    let reader = thread::spawn(move || read_output(profile.provider, stdout, &stdout_sender));
    let mut stdin = child.stdin.take().expect("mission stdin is piped");
    let writer = thread::spawn(move || stdin.write_all(prompt.as_bytes()));
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            child.wait()?;
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(50));
    };
    reader
        .join()
        .map_err(|_| io::Error::other("Mission output reader stopped"))??;
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
    provider: Provider,
    stdout: impl io::Read,
    sender: &SyncSender<Event>,
) -> io::Result<()> {
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        for text in output_text(provider, &event) {
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
