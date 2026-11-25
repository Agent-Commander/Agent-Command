use crate::{
    auth::{self, AuthDetection, Profile, ProfileSource, Provider},
    github,
    permissions::PermissionMode,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const POLICY_ENV_VAR: &str = "AGENT_COMMAND_POLICY_PATH";

const USAGE: &str = "Usage: agent-command onboard [--status] [--json]\n\
       agent-command onboard --provider claude|codex|gemini|github --track TRACK [OPTIONS]\n\n\
Tracks:\n\
  claude: subscription | sso [--email ADDR] | setup-token | bedrock --region REGION [--aws-profile NAME] | vertex --project ID --region REGION | foundry [--resource NAME] | gateway --base-url URL\n\
  codex:  chatgpt | device-auth | api-key\n\
  gemini: google [--project ID] | vertex-adc --project ID --location LOC | service-account --project ID --location LOC --credentials PATH\n\
  github: ghe --hostname HOST [--scope SCOPE]\n\n\
Options:\n\
  --profile NAME     Create or configure an isolated secondary/enterprise profile instead of SystemDefault\n\
  --dry-run          Print the onboarding plan without executing external commands";

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedPolicy {
    pub org_name: Option<String>,
    pub max_permission_mode: Option<PermissionMode>,
    pub allowed_providers: Option<Vec<Provider>>,
    pub allowed_gateways: Option<Vec<String>>,
    pub enforce_prod_gate: Option<bool>,
    pub extra_denied_commands: Vec<String>,
}

impl ManagedPolicy {
    pub fn load_system() -> io::Result<Option<Self>> {
        for path in candidate_policy_paths() {
            if let Some(policy) = Self::load_from_path(&path)? {
                return Ok(Some(policy));
            }
        }
        Ok(None)
    }

    pub fn load_from_path(path: &Path) -> io::Result<Option<Self>> {
        match fs::read(path) {
            Ok(bytes) => {
                let policy: Self = serde_json::from_slice(&bytes).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Managed policy at {} is invalid: {error}", path.display()),
                    )
                })?;
                Ok(Some(policy))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("Cannot read managed policy at {}: {error}", path.display()),
            )),
        }
    }

    pub fn cap_permission(&self, requested: PermissionMode) -> PermissionMode {
        let Some(ceiling) = self.max_permission_mode else {
            return requested;
        };
        if permission_rank(requested) > permission_rank(ceiling) {
            ceiling
        } else {
            requested
        }
    }

    pub fn allows_provider(&self, provider: Provider) -> bool {
        self.allowed_providers
            .as_ref()
            .is_none_or(|allowed| allowed.contains(&provider))
    }

    pub fn allows_gateway(&self, base_url: &str) -> bool {
        let Some(allowed) = &self.allowed_gateways else {
            return true;
        };
        let normalized = base_url.trim().trim_end_matches('/').to_ascii_lowercase();
        allowed.iter().any(|entry| {
            let prefix = entry.trim().trim_end_matches('/').to_ascii_lowercase();
            !prefix.is_empty()
                && (normalized == prefix || normalized.starts_with(&format!("{prefix}/")))
        })
    }

    pub fn prod_gate_enforced(&self) -> bool {
        self.enforce_prod_gate.unwrap_or(true)
    }

    pub fn managed_banner(&self) -> Option<String> {
        self.org_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| format!("Managed by {name}"))
    }
}

fn permission_rank(mode: PermissionMode) -> u8 {
    match mode {
        PermissionMode::ReadOnly => 0,
        PermissionMode::Supervised => 1,
        PermissionMode::AutoAcceptEdits => 2,
        PermissionMode::WorktreeAuto => 3,
        PermissionMode::WorkspaceAuto => 4,
    }
}

fn candidate_policy_paths() -> Vec<PathBuf> {
    if let Some(explicit) = env::var_os(POLICY_ENV_VAR).filter(|v| !v.is_empty()) {
        return vec![PathBuf::from(explicit)];
    }
    let mut paths = Vec::new();
    if cfg!(windows) {
        if let Some(program_data) = env::var_os("ProgramData") {
            paths.push(
                PathBuf::from(program_data)
                    .join("AgentCommand")
                    .join("policy.json"),
            );
        }
        paths.push(PathBuf::from(r"C:\ProgramData\AgentCommand\policy.json"));
    } else if cfg!(target_os = "macos") {
        paths.push(PathBuf::from(
            "/Library/Application Support/AgentCommand/policy.json",
        ));
        paths.push(PathBuf::from("/etc/agent-command/policy.json"));
    } else {
        paths.push(PathBuf::from("/etc/agent-command/policy.json"));
    }
    paths
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthTrack {
    ClaudeSubscription,
    ClaudeSso {
        email: Option<String>,
    },
    ClaudeSetupToken,
    ClaudeBedrock {
        region: String,
        aws_profile: Option<String>,
    },
    ClaudeVertex {
        project: String,
        region: String,
    },
    ClaudeFoundry {
        resource: Option<String>,
    },
    ClaudeGateway {
        base_url: String,
    },
    CodexChatGpt,
    CodexDeviceAuth,
    CodexApiKey,
    GeminiGoogle {
        workspace_project: Option<String>,
    },
    GeminiVertexAdc {
        project: String,
        location: String,
    },
    GeminiServiceAccount {
        project: String,
        location: String,
        credentials_path: PathBuf,
    },
    GitHubEnterprise {
        hostname: String,
        scopes: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OnboardingPlan {
    pub track: AuthTrack,
    pub program: String,
    pub args: Vec<String>,
    pub env_overrides: BTreeMap<String, String>,
    pub profile_name: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OnboardingSnapshot {
    pub detections: Vec<AuthDetection>,
    pub github: github::GitHubAuthStatus,
    pub isolated_profiles: Vec<String>,
    pub managed_policy: Option<ManagedPolicy>,
}

pub fn build_plan(
    track: AuthTrack,
    profile_name: Option<String>,
    policy: Option<&ManagedPolicy>,
) -> Result<OnboardingPlan, String> {
    if let Some(name) = &profile_name {
        auth::validate_name(name).map_err(str::to_owned)?;
    }
    if let Some(policy) = policy {
        if let Some(provider) = track_provider(&track) {
            if !policy.allows_provider(provider) {
                return Err(format!(
                    "{} is disabled by organization policy.",
                    provider.binary()
                ));
            }
        }
        if let AuthTrack::ClaudeGateway { base_url } = &track {
            if !policy.allows_gateway(base_url) {
                return Err(format!(
                    "Gateway {base_url} is not allowed by organization policy."
                ));
            }
        }
    }

    let mut env_overrides = BTreeMap::new();
    let mut notes = Vec::new();
    let (program, args) = match &track {
        AuthTrack::ClaudeSubscription => (
            "claude".to_owned(),
            vec!["auth".into(), "login".into(), "--claudeai".into()],
        ),
        AuthTrack::ClaudeSso { email } => {
            let mut args = vec!["auth".into(), "login".into(), "--sso".into()];
            if let Some(email) = email {
                args.extend(["--email".into(), email.clone()]);
            }
            ("claude".to_owned(), args)
        }
        AuthTrack::ClaudeSetupToken => {
            notes.push(
                "Generates a long-lived subscription token for headless/VPS environments.".into(),
            );
            ("claude".to_owned(), vec!["setup-token".into()])
        }
        AuthTrack::ClaudeBedrock {
            region,
            aws_profile,
        } => {
            env_overrides.insert("CLAUDE_CODE_USE_BEDROCK".into(), "1".into());
            env_overrides.insert("AWS_REGION".into(), region.clone());
            if let Some(aws_profile) = aws_profile {
                env_overrides.insert("AWS_PROFILE".into(), aws_profile.clone());
            }
            notes.push("Uses ambient AWS credentials / IAM role without storing secrets.".into());
            (
                "claude".to_owned(),
                vec!["auth".into(), "status".into(), "--json".into()],
            )
        }
        AuthTrack::ClaudeVertex { project, region } => {
            env_overrides.insert("CLAUDE_CODE_USE_VERTEX".into(), "1".into());
            env_overrides.insert("ANTHROPIC_VERTEX_PROJECT_ID".into(), project.clone());
            env_overrides.insert("CLOUD_ML_REGION".into(), region.clone());
            notes.push("Uses Google Application Default Credentials (gcloud auth application-default login).".into());
            (
                "claude".to_owned(),
                vec!["auth".into(), "status".into(), "--json".into()],
            )
        }
        AuthTrack::ClaudeFoundry { resource } => {
            env_overrides.insert("CLAUDE_CODE_USE_FOUNDRY".into(), "1".into());
            if let Some(resource) = resource {
                env_overrides.insert("ANTHROPIC_FOUNDRY_RESOURCE".into(), resource.clone());
            }
            notes.push("Uses Microsoft Entra ID / Azure Foundry environment credentials.".into());
            (
                "claude".to_owned(),
                vec!["auth".into(), "status".into(), "--json".into()],
            )
        }
        AuthTrack::ClaudeGateway { base_url } => {
            if !base_url.starts_with("https://")
                && !base_url.starts_with("http://127.0.0.1")
                && !base_url.starts_with("http://localhost")
            {
                return Err(
                    "Gateway base URL must use https:// (or localhost for local proxies).".into(),
                );
            }
            env_overrides.insert("ANTHROPIC_BASE_URL".into(), base_url.clone());
            notes.push("Set ANTHROPIC_AUTH_TOKEN in your shell environment; Agent Command never persists API secrets.".into());
            (
                "claude".to_owned(),
                vec!["auth".into(), "status".into(), "--json".into()],
            )
        }
        AuthTrack::CodexChatGpt => ("codex".to_owned(), vec!["login".into()]),
        AuthTrack::CodexDeviceAuth => {
            notes.push("Device-code login works cleanly over SSH and headless hosts.".into());
            (
                "codex".to_owned(),
                vec!["login".into(), "--device-auth".into()],
            )
        }
        AuthTrack::CodexApiKey => {
            notes.push("Pipe OPENAI_API_KEY via stdin to `codex login --with-api-key`.".into());
            (
                "codex".to_owned(),
                vec!["login".into(), "--with-api-key".into()],
            )
        }
        AuthTrack::GeminiGoogle { workspace_project } => {
            if let Some(project) = workspace_project {
                env_overrides.insert("GOOGLE_CLOUD_PROJECT".into(), project.clone());
                notes.push("Configured for Google Workspace / Gemini Code Assist license.".into());
            }
            notes.push("Launch `gemini` interactively and pick 'Sign in with Google' to cache oauth_creds.json.".into());
            ("gemini".to_owned(), Vec::new())
        }
        AuthTrack::GeminiVertexAdc { project, location } => {
            env_overrides.insert("GOOGLE_GENAI_USE_VERTEXAI".into(), "true".into());
            env_overrides.insert("GOOGLE_CLOUD_PROJECT".into(), project.clone());
            env_overrides.insert("GOOGLE_CLOUD_LOCATION".into(), location.clone());
            notes.push(
                "Runs gcloud ADC login; Gemini CLI automatically picks up Vertex AI env vars."
                    .into(),
            );
            (
                "gcloud".to_owned(),
                vec!["auth".into(), "application-default".into(), "login".into()],
            )
        }
        AuthTrack::GeminiServiceAccount {
            project,
            location,
            credentials_path,
        } => {
            env_overrides.insert("GOOGLE_GENAI_USE_VERTEXAI".into(), "true".into());
            env_overrides.insert("GOOGLE_CLOUD_PROJECT".into(), project.clone());
            env_overrides.insert("GOOGLE_CLOUD_LOCATION".into(), location.clone());
            env_overrides.insert(
                "GOOGLE_APPLICATION_CREDENTIALS".into(),
                credentials_path.display().to_string(),
            );
            notes.push("Ensure the service account JSON file has owner-only permissions.".into());
            ("gemini".to_owned(), vec!["--version".into()])
        }
        AuthTrack::GitHubEnterprise { hostname, scopes } => {
            let mut args = vec![
                "auth".into(),
                "login".into(),
                "--hostname".into(),
                hostname.clone(),
                "--web".into(),
            ];
            if !scopes.is_empty() {
                args.extend(["--scopes".into(), scopes.join(",")]);
            }
            ("gh".to_owned(), args)
        }
    };

    Ok(OnboardingPlan {
        track,
        program,
        args,
        env_overrides,
        profile_name,
        notes,
    })
}

fn track_provider(track: &AuthTrack) -> Option<Provider> {
    match track {
        AuthTrack::ClaudeSubscription
        | AuthTrack::ClaudeSso { .. }
        | AuthTrack::ClaudeSetupToken
        | AuthTrack::ClaudeBedrock { .. }
        | AuthTrack::ClaudeVertex { .. }
        | AuthTrack::ClaudeFoundry { .. }
        | AuthTrack::ClaudeGateway { .. } => Some(Provider::Claude),
        AuthTrack::CodexChatGpt | AuthTrack::CodexDeviceAuth | AuthTrack::CodexApiKey => {
            Some(Provider::Codex)
        }
        AuthTrack::GeminiGoogle { .. }
        | AuthTrack::GeminiVertexAdc { .. }
        | AuthTrack::GeminiServiceAccount { .. } => Some(Provider::Gemini),
        AuthTrack::GitHubEnterprise { .. } => None,
    }
}

pub fn execute_plan(plan: &OnboardingPlan, root: &Path) -> io::Result<()> {
    let mut command =
        Command::new(auth::resolve(&plan.program).unwrap_or_else(|| PathBuf::from(&plan.program)));
    command.args(&plan.args);
    for (key, value) in &plan.env_overrides {
        command.env(key, value);
    }
    if let (Some(provider), Some(profile_name)) = (track_provider(&plan.track), &plan.profile_name)
    {
        let profile = Profile::isolated(
            root.to_path_buf(),
            provider,
            profile_name,
            plan.env_overrides.clone(),
        )
        .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
        let directory = profile.prepare_isolated_with_env()?;
        command.env(provider.home_variable(), &directory);
        command.current_dir(&directory);
    }
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} exited with {status}",
            plan.program
        )))
    }
}

pub fn collect_snapshot(workspace: &Path, root: &Path) -> io::Result<OnboardingSnapshot> {
    let detections = auth::detect_system_defaults();
    let github = github::detect(workspace, None);
    let isolated_profiles = auth::profiles(root)?
        .into_iter()
        .filter(|profile| matches!(profile.source(), ProfileSource::Isolated { .. }))
        .map(|profile| profile.to_string())
        .collect();
    let managed_policy = ManagedPolicy::load_system()?;
    Ok(OnboardingSnapshot {
        detections,
        github,
        isolated_profiles,
        managed_policy,
    })
}

pub fn run(args: &[String]) -> io::Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let workspace = env::current_dir()?;
    let root = auth::root()?;
    let json_mode = args.iter().any(|arg| arg == "--json");
    let status_only =
        args.is_empty() || args.iter().all(|arg| arg == "--status" || arg == "--json");
    if status_only {
        let snapshot = collect_snapshot(&workspace, &root)?;
        if json_mode {
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        } else {
            print_snapshot(&snapshot);
        }
        return Ok(());
    }
    let parsed =
        parse_cli_track(args).map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
    let policy = ManagedPolicy::load_system()?;
    let plan = build_plan(parsed.track, parsed.profile_name, policy.as_ref())
        .map_err(|msg| io::Error::new(io::ErrorKind::PermissionDenied, msg))?;
    if parsed.dry_run {
        if json_mode {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            println!("Command: {} {}", plan.program, plan.args.join(" "));
            for (key, value) in &plan.env_overrides {
                println!("Env:     {key}={value}");
            }
            for note in &plan.notes {
                println!("Note:    {note}");
            }
        }
        return Ok(());
    }
    execute_plan(&plan, &root)
}

fn print_snapshot(snapshot: &OnboardingSnapshot) {
    println!("Agent Command Onboarding & Connections");
    if let Some(banner) = snapshot
        .managed_policy
        .as_ref()
        .and_then(ManagedPolicy::managed_banner)
    {
        println!("Policy: {banner}");
    }
    println!("\nSystem Default CLIs (Account #1 — zero re-auth):");
    for detection in &snapshot.detections {
        let state = if detection.logged_in {
            "connected"
        } else {
            "not connected"
        };
        let detail = detection
            .account
            .as_deref()
            .or(detection.auth_method.as_deref())
            .unwrap_or("-");
        println!(
            "  {:<8} {:<14} {}",
            detection.provider.binary(),
            state,
            detail
        );
        if let Some(warning) = &detection.warning {
            println!("           warning: {warning}");
        }
    }
    let gh_state = if snapshot.github.authenticated {
        format!(
            "connected ({} on {})",
            snapshot
                .github
                .login
                .as_deref()
                .map(|l| format!("@{l}"))
                .unwrap_or_else(|| "token".into()),
            snapshot.github.host
        )
    } else {
        "not connected".into()
    };
    println!("  {:<8} {}", "github", gh_state);
    if !snapshot.isolated_profiles.is_empty() {
        println!("\nIsolated Secondary / Enterprise Profiles:");
        for profile in &snapshot.isolated_profiles {
            println!("  {profile}");
        }
    }
    println!("\nUse `agent-command onboard --help` to add SSO, Bedrock/Vertex/Foundry, GHE, or secondary accounts.");
}

struct ParsedOnboardArgs {
    track: AuthTrack,
    profile_name: Option<String>,
    dry_run: bool,
}

fn parse_cli_track(args: &[String]) -> Result<ParsedOnboardArgs, String> {
    let mut provider: Option<String> = None;
    let mut track: Option<String> = None;
    let mut profile_name: Option<String> = None;
    let mut email: Option<String> = None;
    let mut region: Option<String> = None;
    let mut aws_profile: Option<String> = None;
    let mut project: Option<String> = None;
    let mut location: Option<String> = None;
    let mut resource: Option<String> = None;
    let mut base_url: Option<String> = None;
    let mut credentials: Option<PathBuf> = None;
    let mut hostname: Option<String> = None;
    let mut scopes: Vec<String> = Vec::new();
    let mut dry_run = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--provider" => provider = Some(next_value(&mut iter, "--provider")?),
            "--track" => track = Some(next_value(&mut iter, "--track")?),
            "--profile" => profile_name = Some(next_value(&mut iter, "--profile")?),
            "--email" => email = Some(next_value(&mut iter, "--email")?),
            "--region" => region = Some(next_value(&mut iter, "--region")?),
            "--aws-profile" => aws_profile = Some(next_value(&mut iter, "--aws-profile")?),
            "--project" => project = Some(next_value(&mut iter, "--project")?),
            "--location" => location = Some(next_value(&mut iter, "--location")?),
            "--resource" => resource = Some(next_value(&mut iter, "--resource")?),
            "--base-url" => base_url = Some(next_value(&mut iter, "--base-url")?),
            "--credentials" => {
                credentials = Some(PathBuf::from(next_value(&mut iter, "--credentials")?))
            }
            "--hostname" => hostname = Some(next_value(&mut iter, "--hostname")?),
            "--scope" => scopes.push(next_value(&mut iter, "--scope")?),
            "--dry-run" => dry_run = true,
            "--json" | "--status" => {}
            other => return Err(format!("Unknown onboard option {other}.\n{USAGE}")),
        }
    }

    let provider = provider.ok_or_else(|| format!("--provider is required.\n{USAGE}"))?;
    let track_name = track.ok_or_else(|| format!("--track is required.\n{USAGE}"))?;

    let built = match (
        provider.to_ascii_lowercase().as_str(),
        track_name.to_ascii_lowercase().as_str(),
    ) {
        ("claude", "subscription") => AuthTrack::ClaudeSubscription,
        ("claude", "sso") => AuthTrack::ClaudeSso { email },
        ("claude", "setup-token") => AuthTrack::ClaudeSetupToken,
        ("claude", "bedrock") => AuthTrack::ClaudeBedrock {
            region: region.ok_or("--region is required for claude bedrock")?,
            aws_profile,
        },
        ("claude", "vertex") => AuthTrack::ClaudeVertex {
            project: project.ok_or("--project is required for claude vertex")?,
            region: region.ok_or("--region is required for claude vertex")?,
        },
        ("claude", "foundry") => AuthTrack::ClaudeFoundry { resource },
        ("claude", "gateway") => AuthTrack::ClaudeGateway {
            base_url: base_url.ok_or("--base-url is required for claude gateway")?,
        },
        ("codex", "chatgpt") => AuthTrack::CodexChatGpt,
        ("codex", "device-auth") => AuthTrack::CodexDeviceAuth,
        ("codex", "api-key") => AuthTrack::CodexApiKey,
        ("gemini", "google" | "workspace") => AuthTrack::GeminiGoogle {
            workspace_project: project,
        },
        ("gemini", "vertex-adc") => AuthTrack::GeminiVertexAdc {
            project: project.ok_or("--project is required for gemini vertex-adc")?,
            location: location.ok_or("--location is required for gemini vertex-adc")?,
        },
        ("gemini", "service-account") => AuthTrack::GeminiServiceAccount {
            project: project.ok_or("--project is required for gemini service-account")?,
            location: location.ok_or("--location is required for gemini service-account")?,
            credentials_path: credentials
                .ok_or("--credentials is required for gemini service-account")?,
        },
        ("github", "ghe") => AuthTrack::GitHubEnterprise {
            hostname: hostname.ok_or("--hostname is required for github ghe")?,
            scopes,
        },
        _ => {
            return Err(format!(
                "Unsupported onboarding track {provider}/{track_name}.\n{USAGE}"
            ))
        }
    };

    Ok(ParsedOnboardArgs {
        track: built,
        profile_name,
        dry_run,
    })
}

fn next_value(iter: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    iter.next()
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_policy_caps_permissions_and_enforces_provider_and_gateway_lists() {
        let policy = ManagedPolicy {
            org_name: Some("Acme Corp".into()),
            max_permission_mode: Some(PermissionMode::AutoAcceptEdits),
            allowed_providers: Some(vec![Provider::Claude, Provider::Gemini]),
            allowed_gateways: Some(vec!["https://ai.acme.internal".into()]),
            enforce_prod_gate: Some(true),
            extra_denied_commands: vec!["curl http://169.254.169.254".into()],
        };
        assert_eq!(
            policy.managed_banner().as_deref(),
            Some("Managed by Acme Corp")
        );
        assert_eq!(
            policy.cap_permission(PermissionMode::WorkspaceAuto),
            PermissionMode::AutoAcceptEdits
        );
        assert_eq!(
            policy.cap_permission(PermissionMode::ReadOnly),
            PermissionMode::ReadOnly
        );
        assert!(policy.allows_provider(Provider::Claude));
        assert!(policy.allows_provider(Provider::Gemini));
        assert!(!policy.allows_provider(Provider::Codex));
        assert!(policy.allows_gateway("https://ai.acme.internal/v1"));
        assert!(!policy.allows_gateway("https://evil.example/v1"));
        assert!(policy.prod_gate_enforced());
    }

    #[test]
    fn managed_policy_fails_closed_on_malformed_json() {
        let dir = env::temp_dir().join(format!("ac-policy-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("policy.json");
        assert_eq!(ManagedPolicy::load_from_path(&path).unwrap(), None);
        fs::write(
            &path,
            r#"{"org_name":"Acme","max_permission_mode":"auto-accept-edits"}"#,
        )
        .unwrap();
        let loaded = ManagedPolicy::load_from_path(&path).unwrap().unwrap();
        assert_eq!(loaded.org_name.as_deref(), Some("Acme"));
        assert_eq!(
            loaded.max_permission_mode,
            Some(PermissionMode::AutoAcceptEdits)
        );

        fs::write(&path, r#"{"unknown_field": true}"#).unwrap();
        assert_eq!(
            ManagedPolicy::load_from_path(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn builds_enterprise_plans_and_rejects_disallowed_providers_or_gateways() {
        let sso = build_plan(
            AuthTrack::ClaudeSso {
                email: Some("dev@acme.test".into()),
            },
            Some("corp".into()),
            None,
        )
        .unwrap();
        assert_eq!(sso.program, "claude");
        assert_eq!(
            sso.args,
            ["auth", "login", "--sso", "--email", "dev@acme.test"]
        );

        let device = build_plan(AuthTrack::CodexDeviceAuth, None, None).unwrap();
        assert_eq!(device.program, "codex");
        assert_eq!(device.args, ["login", "--device-auth"]);

        let bedrock = build_plan(
            AuthTrack::ClaudeBedrock {
                region: "us-east-1".into(),
                aws_profile: Some("eng".into()),
            },
            Some("aws-eng".into()),
            None,
        )
        .unwrap();
        assert_eq!(bedrock.env_overrides["CLAUDE_CODE_USE_BEDROCK"], "1");
        assert_eq!(bedrock.env_overrides["AWS_REGION"], "us-east-1");
        assert_eq!(bedrock.env_overrides["AWS_PROFILE"], "eng");

        let vertex = build_plan(
            AuthTrack::GeminiVertexAdc {
                project: "acme-ai".into(),
                location: "us-central1".into(),
            },
            None,
            None,
        )
        .unwrap();
        assert_eq!(vertex.program, "gcloud");
        assert_eq!(vertex.env_overrides["GOOGLE_GENAI_USE_VERTEXAI"], "true");

        let ghe = build_plan(
            AuthTrack::GitHubEnterprise {
                hostname: "ghe.acme.internal".into(),
                scopes: vec!["repo".into(), "read:org".into()],
            },
            None,
            None,
        )
        .unwrap();
        assert_eq!(ghe.program, "gh");
        assert!(ghe.args.contains(&"ghe.acme.internal".to_owned()));

        let policy = ManagedPolicy {
            allowed_providers: Some(vec![Provider::Claude]),
            allowed_gateways: Some(vec!["https://gw.acme.internal".into()]),
            ..ManagedPolicy::default()
        };
        assert!(build_plan(AuthTrack::CodexDeviceAuth, None, Some(&policy)).is_err());
        assert!(build_plan(
            AuthTrack::ClaudeGateway {
                base_url: "https://other.example".into()
            },
            None,
            Some(&policy),
        )
        .is_err());
    }

    #[test]
    fn parses_cli_onboarding_flags() {
        let to_vec = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let parsed = parse_cli_track(&to_vec(&[
            "--provider",
            "claude",
            "--track",
            "sso",
            "--email",
            "pilot@acme.test",
            "--profile",
            "work",
            "--dry-run",
        ]))
        .unwrap();
        assert_eq!(
            parsed.track,
            AuthTrack::ClaudeSso {
                email: Some("pilot@acme.test".into())
            }
        );
        assert_eq!(parsed.profile_name.as_deref(), Some("work"));
        assert!(parsed.dry_run);
    }
}
