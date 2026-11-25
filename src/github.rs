use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    env, fmt, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
};

const DEFAULT_HOST: &str = "github.com";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubAuthSource {
    SavedToken,
    EnvToken {
        var: String,
    },
    GhCli {
        host: String,
        token_source: Option<String>,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitHubAuthStatus {
    pub authenticated: bool,
    pub host: String,
    pub login: Option<String>,
    pub scopes: Vec<String>,
    pub git_protocol: Option<String>,
    pub source: Option<GitHubAuthSource>,
    pub gh_available: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitHubHostAccount {
    pub host: String,
    pub login: String,
    pub active: bool,
    pub state: String,
    pub scopes: Vec<String>,
    pub git_protocol: Option<String>,
    pub token_source: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PullRequestSummary {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
    pub head_ref: String,
    pub base_ref: String,
    pub is_draft: bool,
    pub author: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PullRequestDetail {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub url: String,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub mergeable: Option<String>,
    pub review_decision: Option<String>,
    pub is_draft: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub state: String,
    pub bucket: Option<String>,
    pub link: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IssueSummary {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
    pub labels: Vec<String>,
    pub author: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IssueDetail {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub url: String,
    pub labels: Vec<String>,
    pub author: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CreatePrRequest {
    pub title: String,
    pub body: String,
    pub base: Option<String>,
    pub head: Option<String>,
    pub draft: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMethod {
    #[default]
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    fn flag(self) -> &'static str {
        match self {
            Self::Squash => "--squash",
            Self::Merge => "--merge",
            Self::Rebase => "--rebase",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MergeResult {
    pub number: u64,
    pub head_sha: String,
    pub method: MergeMethod,
    pub message: String,
}

pub struct SavedGitHubToken(String);

impl fmt::Debug for SavedGitHubToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SavedGitHubToken(redacted)")
    }
}

impl SavedGitHubToken {
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let value = token.into();
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| Self(trimmed.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn detect_async(
    workspace: PathBuf,
    saved_token: Option<SavedGitHubToken>,
) -> Receiver<GitHubAuthStatus> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let status = detect(&workspace, saved_token.as_ref());
        let _ = tx.send(status);
    });
    rx
}

pub fn detect(workspace: &Path, saved_token: Option<&SavedGitHubToken>) -> GitHubAuthStatus {
    let remote_host = remote_hostname(workspace);
    let env_host = non_empty_env("GH_HOST");
    let preferred_host = remote_host
        .as_deref()
        .or(env_host.as_deref())
        .unwrap_or(DEFAULT_HOST)
        .to_owned();
    let gh_available = crate::auth::resolve("gh").is_some();

    if saved_token.is_some() {
        return GitHubAuthStatus {
            authenticated: true,
            host: preferred_host,
            login: None,
            scopes: Vec::new(),
            git_protocol: None,
            source: Some(GitHubAuthSource::SavedToken),
            gh_available,
            error: None,
        };
    }

    if let Some((var, host)) = detect_env_token(&preferred_host) {
        return GitHubAuthStatus {
            authenticated: true,
            host,
            login: None,
            scopes: Vec::new(),
            git_protocol: None,
            source: Some(GitHubAuthSource::EnvToken { var }),
            gh_available,
            error: None,
        };
    }

    if !gh_available {
        return GitHubAuthStatus {
            authenticated: false,
            host: preferred_host,
            gh_available: false,
            error: Some("GitHub CLI (gh) is not installed on PATH.".into()),
            ..GitHubAuthStatus::default()
        };
    }

    match run_gh_auth_status() {
        Ok(bytes) => resolve_status_from_gh_json(&bytes, Some(&preferred_host), true),
        Err(error) => GitHubAuthStatus {
            authenticated: false,
            host: preferred_host,
            gh_available: true,
            error: Some(error.to_string()),
            ..GitHubAuthStatus::default()
        },
    }
}

pub fn detect_with_precedence(
    preferred_host: Option<&str>,
    saved_token: Option<&SavedGitHubToken>,
    env_lookup: impl Fn(&str) -> Option<String>,
    gh_json: Option<&[u8]>,
    gh_available: bool,
) -> GitHubAuthStatus {
    let host = preferred_host
        .map(str::to_owned)
        .or_else(|| env_lookup("GH_HOST").filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_HOST.to_owned());

    if saved_token.is_some() {
        return GitHubAuthStatus {
            authenticated: true,
            host,
            source: Some(GitHubAuthSource::SavedToken),
            gh_available,
            ..GitHubAuthStatus::default()
        };
    }

    for var in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN"] {
        if env_lookup(var).is_some_and(|val| !val.trim().is_empty()) {
            return GitHubAuthStatus {
                authenticated: true,
                host,
                source: Some(GitHubAuthSource::EnvToken {
                    var: var.to_owned(),
                }),
                gh_available,
                ..GitHubAuthStatus::default()
            };
        }
    }

    match gh_json {
        Some(bytes) => resolve_status_from_gh_json(bytes, Some(&host), gh_available),
        None => GitHubAuthStatus {
            authenticated: false,
            host,
            gh_available,
            ..GitHubAuthStatus::default()
        },
    }
}

fn detect_env_token(preferred_host: &str) -> Option<(String, String)> {
    let is_enterprise = !preferred_host.eq_ignore_ascii_case(DEFAULT_HOST);
    let order: &[&str] = if is_enterprise {
        &["GH_ENTERPRISE_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"]
    } else {
        &["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN"]
    };
    for var in order {
        if non_empty_env(var).is_some() {
            let host = if *var == "GH_ENTERPRISE_TOKEN" {
                non_empty_env("GH_HOST").unwrap_or_else(|| preferred_host.to_owned())
            } else {
                preferred_host.to_owned()
            };
            return Some(((*var).to_owned(), host));
        }
    }
    None
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|val| val.trim().to_owned())
        .filter(|val| !val.is_empty())
}

fn run_gh_auth_status() -> io::Result<Vec<u8>> {
    let program = crate::auth::resolve("gh").unwrap_or_else(|| PathBuf::from("gh"));
    let output = Command::new(program)
        .args(["auth", "status", "--json", "hosts"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if output.stdout.is_empty() && !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(io::Error::other(if stderr.is_empty() {
            format!("gh auth status exited with {}", output.status)
        } else {
            stderr
        }));
    }
    Ok(output.stdout)
}

pub fn parse_gh_auth_status_json(
    bytes: &[u8],
    preferred_host: Option<&str>,
) -> Option<GitHubHostAccount> {
    let parsed: Value = serde_json::from_slice(bytes).ok()?;
    let hosts = parsed.get("hosts")?.as_object()?;
    let mut accounts = Vec::new();
    for (host_key, entries) in hosts {
        let Some(list) = entries.as_array() else {
            continue;
        };
        for item in list {
            let state = item["state"].as_str().unwrap_or_default().to_owned();
            let active = item["active"].as_bool().unwrap_or(false);
            let login = item["login"].as_str().unwrap_or_default().trim().to_owned();
            if state != "success" || login.is_empty() {
                continue;
            }
            let host = item["host"]
                .as_str()
                .filter(|h| !h.is_empty())
                .unwrap_or(host_key)
                .to_owned();
            let scopes = item["scopes"]
                .as_str()
                .map(|raw| {
                    raw.split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            accounts.push(GitHubHostAccount {
                host,
                login,
                active,
                state,
                scopes,
                git_protocol: item["gitProtocol"].as_str().map(str::to_owned),
                token_source: item["tokenSource"].as_str().map(str::to_owned),
            });
        }
    }
    if accounts.is_empty() {
        return None;
    }
    if let Some(target) = preferred_host {
        if let Some(matched) = accounts
            .iter()
            .find(|acc| acc.host.eq_ignore_ascii_case(target) && acc.active)
            .or_else(|| {
                accounts
                    .iter()
                    .find(|acc| acc.host.eq_ignore_ascii_case(target))
            })
        {
            return Some(matched.clone());
        }
    }
    accounts
        .iter()
        .find(|acc| acc.host.eq_ignore_ascii_case(DEFAULT_HOST) && acc.active)
        .or_else(|| accounts.iter().find(|acc| acc.active))
        .or_else(|| accounts.first())
        .cloned()
}

fn resolve_status_from_gh_json(
    bytes: &[u8],
    preferred_host: Option<&str>,
    gh_available: bool,
) -> GitHubAuthStatus {
    let fallback_host = preferred_host.unwrap_or(DEFAULT_HOST).to_owned();
    match parse_gh_auth_status_json(bytes, preferred_host) {
        Some(account) => GitHubAuthStatus {
            authenticated: true,
            host: account.host.clone(),
            login: Some(account.login),
            scopes: account.scopes,
            git_protocol: account.git_protocol,
            source: Some(GitHubAuthSource::GhCli {
                host: account.host,
                token_source: account.token_source,
            }),
            gh_available,
            error: None,
        },
        None => GitHubAuthStatus {
            authenticated: false,
            host: fallback_host,
            gh_available,
            error: Some("GitHub CLI is not authenticated. Run `gh auth login`.".into()),
            ..GitHubAuthStatus::default()
        },
    }
}

pub fn remote_hostname(workspace: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_git_remote_host(String::from_utf8_lossy(&output.stdout).trim())
}

pub fn parse_git_remote_host(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(after_scheme) = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .or_else(|| trimmed.strip_prefix("ssh://"))
        .or_else(|| trimmed.strip_prefix("git://"))
    {
        let authority = after_scheme.split('/').next()?;
        let without_user = authority.rsplit('@').next()?;
        let host = without_user.split(':').next()?.trim();
        return (!host.is_empty()).then(|| host.to_ascii_lowercase());
    }
    if let Some((left, _)) = trimmed.split_once(':') {
        if !left.contains('/') && !left.contains('\\') {
            let host = left.rsplit('@').next()?.trim();
            return (!host.is_empty()).then(|| host.to_ascii_lowercase());
        }
    }
    None
}

pub fn gh_prompt_context(status: &GitHubAuthStatus) -> Option<String> {
    if !status.authenticated {
        return None;
    }
    let identity = match &status.login {
        Some(login) => format!("as @{login} on {}", status.host),
        None => format!("on {}", status.host),
    };
    let scopes = if status.scopes.is_empty() {
        String::new()
    } else {
        format!(" (scopes: {})", status.scopes.join(", "))
    };
    Some(format!(
        "GitHub CLI (`gh`) is authenticated {identity}{scopes}. Use `gh` or GitHub tools directly for PRs, issues, CI checks, and releases."
    ))
}

fn gh_command(workspace: &Path) -> Command {
    let program = crate::auth::resolve("gh").unwrap_or_else(|| PathBuf::from("gh"));
    let mut command = Command::new(program);
    command
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn run_gh_json(workspace: &Path, args: &[&str]) -> io::Result<Value> {
    let output = gh_command(workspace).args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(io::Error::other(if stderr.is_empty() {
            format!("gh {} failed ({})", args.join(" "), output.status)
        } else {
            stderr
        }));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

pub fn pr_list(
    workspace: &Path,
    state: Option<&str>,
    limit: usize,
) -> io::Result<Vec<PullRequestSummary>> {
    let state = state.unwrap_or("open");
    let limit_str = limit.clamp(1, 100).to_string();
    let value = run_gh_json(
        workspace,
        &[
            "pr",
            "list",
            "--state",
            state,
            "--limit",
            &limit_str,
            "--json",
            "number,title,state,url,headRefName,baseRefName,isDraft,author",
        ],
    )?;
    parse_pr_list(&value)
}

pub fn parse_pr_list(value: &Value) -> io::Result<Vec<PullRequestSummary>> {
    let items = value
        .as_array()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Expected JSON array of PRs"))?;
    Ok(items
        .iter()
        .filter_map(|item| {
            Some(PullRequestSummary {
                number: item["number"].as_u64()?,
                title: item["title"].as_str()?.to_owned(),
                state: item["state"].as_str().unwrap_or("OPEN").to_owned(),
                url: item["url"].as_str().unwrap_or_default().to_owned(),
                head_ref: item["headRefName"].as_str().unwrap_or_default().to_owned(),
                base_ref: item["baseRefName"].as_str().unwrap_or_default().to_owned(),
                is_draft: item["isDraft"].as_bool().unwrap_or(false),
                author: item["author"]["login"].as_str().map(str::to_owned),
            })
        })
        .collect())
}

pub fn pr_view(workspace: &Path, number: u64) -> io::Result<PullRequestDetail> {
    let num = number.to_string();
    let value = run_gh_json(
        workspace,
        &[
            "pr",
            "view",
            &num,
            "--json",
            "number,title,body,state,url,headRefName,baseRefName,headRefOid,mergeable,reviewDecision,isDraft",
        ],
    )?;
    parse_pr_detail(&value)
}

pub fn parse_pr_detail(value: &Value) -> io::Result<PullRequestDetail> {
    let number = value["number"]
        .as_u64()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "PR JSON missing number"))?;
    Ok(PullRequestDetail {
        number,
        title: value["title"].as_str().unwrap_or_default().to_owned(),
        body: value["body"].as_str().unwrap_or_default().to_owned(),
        state: value["state"].as_str().unwrap_or("OPEN").to_owned(),
        url: value["url"].as_str().unwrap_or_default().to_owned(),
        head_ref: value["headRefName"].as_str().unwrap_or_default().to_owned(),
        base_ref: value["baseRefName"].as_str().unwrap_or_default().to_owned(),
        head_sha: value["headRefOid"].as_str().unwrap_or_default().to_owned(),
        mergeable: value["mergeable"].as_str().map(str::to_owned),
        review_decision: value["reviewDecision"].as_str().map(str::to_owned),
        is_draft: value["isDraft"].as_bool().unwrap_or(false),
    })
}

pub fn pr_create(workspace: &Path, request: &CreatePrRequest) -> io::Result<String> {
    let mut args = vec![
        "pr".to_owned(),
        "create".to_owned(),
        "--title".to_owned(),
        request.title.clone(),
        "--body".to_owned(),
        request.body.clone(),
    ];
    if let Some(base) = &request.base {
        args.extend(["--base".to_owned(), base.clone()]);
    }
    if let Some(head) = &request.head {
        args.extend(["--head".to_owned(), head.clone()]);
    }
    if request.draft {
        args.push("--draft".to_owned());
    }
    let output = gh_command(workspace).args(&args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(io::Error::other(if stderr.is_empty() {
            format!("gh pr create failed ({})", output.status)
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn pr_checks(workspace: &Path, number: u64) -> io::Result<Vec<CheckRun>> {
    let num = number.to_string();
    let value = run_gh_json(
        workspace,
        &["pr", "checks", &num, "--json", "name,state,bucket,link"],
    )?;
    parse_pr_checks(&value)
}

pub fn parse_pr_checks(value: &Value) -> io::Result<Vec<CheckRun>> {
    let items = value.as_array().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "Expected JSON array of checks")
    })?;
    Ok(items
        .iter()
        .filter_map(|item| {
            Some(CheckRun {
                name: item["name"].as_str()?.to_owned(),
                state: item["state"].as_str().unwrap_or("UNKNOWN").to_owned(),
                bucket: item["bucket"].as_str().map(str::to_owned),
                link: item["link"].as_str().map(str::to_owned),
            })
        })
        .collect())
}

pub fn verify_head_sha_guard(
    number: u64,
    expected_head_sha: &str,
    actual_head_sha: &str,
) -> io::Result<()> {
    let expected = expected_head_sha.trim();
    let actual = actual_head_sha.trim();
    if expected.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Refusing PR merge without an expected head SHA.",
        ));
    }
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(io::Error::other(format!(
            "Refusing merge of PR #{number}: remote head SHA moved from {expected} to {actual}. Inspect the new commits first."
        )));
    }
    Ok(())
}

pub fn pr_merge(
    workspace: &Path,
    number: u64,
    expected_head_sha: &str,
    method: MergeMethod,
    delete_branch: bool,
) -> io::Result<MergeResult> {
    let detail = pr_view(workspace, number)?;
    verify_head_sha_guard(number, expected_head_sha, &detail.head_sha)?;
    let num = number.to_string();
    let mut args = vec![
        "pr",
        "merge",
        &num,
        method.flag(),
        "--match-head-commit",
        expected_head_sha.trim(),
    ];
    if delete_branch {
        args.push("--delete-branch");
    }
    let output = gh_command(workspace).args(&args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(io::Error::other(if stderr.is_empty() {
            format!("gh pr merge #{number} failed ({})", output.status)
        } else {
            stderr
        }));
    }
    Ok(MergeResult {
        number,
        head_sha: detail.head_sha,
        method,
        message: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    })
}

pub fn issue_list(
    workspace: &Path,
    state: Option<&str>,
    limit: usize,
) -> io::Result<Vec<IssueSummary>> {
    let state = state.unwrap_or("open");
    let limit_str = limit.clamp(1, 100).to_string();
    let value = run_gh_json(
        workspace,
        &[
            "issue",
            "list",
            "--state",
            state,
            "--limit",
            &limit_str,
            "--json",
            "number,title,state,url,labels,author",
        ],
    )?;
    parse_issue_list(&value)
}

pub fn parse_issue_list(value: &Value) -> io::Result<Vec<IssueSummary>> {
    let items = value.as_array().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "Expected JSON array of issues")
    })?;
    Ok(items
        .iter()
        .filter_map(|item| {
            Some(IssueSummary {
                number: item["number"].as_u64()?,
                title: item["title"].as_str()?.to_owned(),
                state: item["state"].as_str().unwrap_or("OPEN").to_owned(),
                url: item["url"].as_str().unwrap_or_default().to_owned(),
                labels: extract_labels(&item["labels"]),
                author: item["author"]["login"].as_str().map(str::to_owned),
            })
        })
        .collect())
}

pub fn issue_view(workspace: &Path, number: u64) -> io::Result<IssueDetail> {
    let num = number.to_string();
    let value = run_gh_json(
        workspace,
        &[
            "issue",
            "view",
            &num,
            "--json",
            "number,title,body,state,url,labels,author",
        ],
    )?;
    parse_issue_detail(&value)
}

pub fn parse_issue_detail(value: &Value) -> io::Result<IssueDetail> {
    let number = value["number"]
        .as_u64()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Issue JSON missing number"))?;
    Ok(IssueDetail {
        number,
        title: value["title"].as_str().unwrap_or_default().to_owned(),
        body: value["body"].as_str().unwrap_or_default().to_owned(),
        state: value["state"].as_str().unwrap_or("OPEN").to_owned(),
        url: value["url"].as_str().unwrap_or_default().to_owned(),
        labels: extract_labels(&value["labels"]),
        author: value["author"]["login"].as_str().map(str::to_owned),
    })
}

pub fn issue_create(
    workspace: &Path,
    title: &str,
    body: &str,
    labels: &[String],
) -> io::Result<String> {
    let mut args = vec![
        "issue".to_owned(),
        "create".to_owned(),
        "--title".to_owned(),
        title.to_owned(),
        "--body".to_owned(),
        body.to_owned(),
    ];
    for label in labels {
        args.extend(["--label".to_owned(), label.clone()]);
    }
    let output = gh_command(workspace).args(&args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(io::Error::other(if stderr.is_empty() {
            format!("gh issue create failed ({})", output.status)
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn issue_comment(workspace: &Path, number: u64, body: &str) -> io::Result<String> {
    let num = number.to_string();
    let output = gh_command(workspace)
        .args(["issue", "comment", &num, "--body", body])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(io::Error::other(if stderr.is_empty() {
            format!("gh issue comment #{number} failed ({})", output.status)
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn extract_labels(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|label| label["name"].as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE_GH_JSON: &[u8] = br#"{
        "hosts": {
            "github.com": [
                {
                    "state": "success",
                    "active": true,
                    "host": "github.com",
                    "login": "pilot",
                    "tokenSource": "hosts.yml",
                    "scopes": "gist, read:org, repo, workflow",
                    "gitProtocol": "https"
                }
            ],
            "ghe.corp.internal": [
                {
                    "state": "success",
                    "active": true,
                    "host": "ghe.corp.internal",
                    "login": "corp-pilot",
                    "tokenSource": "keyring",
                    "scopes": "repo",
                    "gitProtocol": "ssh"
                }
            ]
        }
    }"#;

    #[test]
    fn parses_gh_auth_status_and_matches_remote_host() {
        let github = parse_gh_auth_status_json(SAMPLE_GH_JSON, Some("github.com")).unwrap();
        assert_eq!(github.login, "pilot");
        assert_eq!(github.scopes, ["gist", "read:org", "repo", "workflow"]);

        let ghe = parse_gh_auth_status_json(SAMPLE_GH_JSON, Some("ghe.corp.internal")).unwrap();
        assert_eq!(ghe.login, "corp-pilot");
        assert_eq!(ghe.host, "ghe.corp.internal");
        assert_eq!(ghe.git_protocol.as_deref(), Some("ssh"));
    }

    #[test]
    fn precedence_prefers_saved_token_then_env_then_gh_cli() {
        let saved = SavedGitHubToken::new("ghp_synthetic_saved").unwrap();
        assert_eq!(format!("{saved:?}"), "SavedGitHubToken(redacted)");

        let status = detect_with_precedence(
            Some("github.com"),
            Some(&saved),
            |name| (name == "GH_TOKEN").then(|| "env_tok".into()),
            Some(SAMPLE_GH_JSON),
            true,
        );
        assert_eq!(status.source, Some(GitHubAuthSource::SavedToken));

        let status = detect_with_precedence(
            Some("github.com"),
            None,
            |name| (name == "GH_TOKEN").then(|| "env_tok".into()),
            Some(SAMPLE_GH_JSON),
            true,
        );
        assert_eq!(
            status.source,
            Some(GitHubAuthSource::EnvToken {
                var: "GH_TOKEN".into()
            })
        );

        let status = detect_with_precedence(
            Some("ghe.corp.internal"),
            None,
            |_| None,
            Some(SAMPLE_GH_JSON),
            true,
        );
        assert!(status.authenticated);
        assert_eq!(status.login.as_deref(), Some("corp-pilot"));
        assert_eq!(status.host, "ghe.corp.internal");
    }

    #[test]
    fn parses_https_and_ssh_git_remote_hosts() {
        assert_eq!(
            parse_git_remote_host("https://github.com/Agent-Commander/Agent-Command.git")
                .as_deref(),
            Some("github.com")
        );
        assert_eq!(
            parse_git_remote_host("git@ghe.corp.internal:org/repo.git").as_deref(),
            Some("ghe.corp.internal")
        );
        assert_eq!(
            parse_git_remote_host("ssh://git@ghe.corp.internal:2222/org/repo.git").as_deref(),
            Some("ghe.corp.internal")
        );
        assert_eq!(parse_git_remote_host(""), None);
    }

    #[test]
    fn head_sha_guard_refuses_merge_when_branch_moved() {
        assert!(verify_head_sha_guard(42, "abc1234", "abc1234").is_ok());
        assert!(verify_head_sha_guard(42, "ABC1234", "abc1234").is_ok());
        let moved = verify_head_sha_guard(42, "abc1234", "def5678").unwrap_err();
        assert!(moved.to_string().contains("PR #42"));
        assert!(moved.to_string().contains("abc1234"));
        assert!(moved.to_string().contains("def5678"));
        assert!(verify_head_sha_guard(42, "", "abc1234").is_err());
    }

    #[test]
    fn prompt_context_and_json_parsers_format_structured_records() {
        let status = detect_with_precedence(
            Some("github.com"),
            None,
            |_| None,
            Some(SAMPLE_GH_JSON),
            true,
        );
        let context = gh_prompt_context(&status).unwrap();
        assert!(context.contains("@pilot"));
        assert!(context.contains("github.com"));
        assert!(context.contains("repo"));

        let prs = parse_pr_list(&json!([{
            "number": 27,
            "title": "Fix updater",
            "state": "OPEN",
            "url": "https://github.com/org/repo/pull/27",
            "headRefName": "fix/update",
            "baseRefName": "main",
            "isDraft": false,
            "author": {"login": "pilot"}
        }]))
        .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 27);
        assert_eq!(prs[0].author.as_deref(), Some("pilot"));

        let detail = parse_pr_detail(&json!({
            "number": 27,
            "title": "Fix updater",
            "body": "Body",
            "state": "OPEN",
            "url": "https://github.com/org/repo/pull/27",
            "headRefName": "fix/update",
            "baseRefName": "main",
            "headRefOid": "deadbeef",
            "mergeable": "MERGEABLE",
            "reviewDecision": "APPROVED",
            "isDraft": false
        }))
        .unwrap();
        assert_eq!(detail.head_sha, "deadbeef");

        let checks = parse_pr_checks(&json!([{
            "name": "cargo test",
            "state": "COMPLETED",
            "bucket": "pass",
            "link": "https://github.com/org/repo/actions/runs/1"
        }]))
        .unwrap();
        assert_eq!(checks[0].bucket.as_deref(), Some("pass"));

        let issues = parse_issue_list(&json!([{
            "number": 5,
            "title": "Add Gemini",
            "state": "OPEN",
            "url": "https://github.com/org/repo/issues/5",
            "labels": [{"name": "enhancement"}],
            "author": {"login": "pilot"}
        }]))
        .unwrap();
        assert_eq!(issues[0].labels, ["enhancement"]);
    }
}
