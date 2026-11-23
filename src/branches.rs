use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::Path,
    process::{Command as Process, Stdio},
};

pub(crate) const BRANCH_PREFIX: &str = "ac/";
const TOPIC_CHARS: usize = 24;
const FALLBACK_TOPIC: &str = "task";
const MAX_BRANCH_SUFFIX: usize = 99;
const DEFAULT_REMOTE: &str = "origin";
const TITLE_CHARS: usize = 72;
const GH_PROGRAM: &str = "gh";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Branch {
    pub name: String,
    pub base: String,
    pub base_commit: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub added: u64,
    pub removed: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiffStat {
    pub files: Vec<FileChange>,
}

impl DiffStat {
    pub fn added(&self) -> u64 {
        self.files.iter().map(|file| file.added).sum()
    }

    pub fn removed(&self) -> u64 {
        self.files.iter().map(|file| file.removed).sum()
    }

    pub fn totals(&self) -> String {
        format!("+{} −{}", self.added(), self.removed())
    }

    pub fn label(&self) -> String {
        let count = self.files.len();
        let noun = if count == 1 { "file" } else { "files" };
        format!("{} · {count} {noun}", self.totals())
    }
}

pub(crate) fn git(directory: &Path, args: &[&str]) -> io::Result<String> {
    Ok(git_raw(directory, args)?.trim().to_owned())
}

fn git_command(directory: &Path) -> Process {
    let mut command = Process::new("git");
    command
        .current_dir(directory)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null());
    command
}

fn git_raw(directory: &Path, args: &[&str]) -> io::Result<String> {
    let output = git_command(directory).args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_succeeds(directory: &Path, args: &[&str]) -> bool {
    git_command(directory)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(crate) fn slug(text: &str, limit: usize) -> String {
    let mut slug = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= limit {
            break;
        }
    }
    slug.trim_end_matches('-').to_owned()
}

pub(crate) fn branch_name(name: &str, topic: &str) -> String {
    let topic = match slug(topic, TOPIC_CHARS) {
        topic if topic.is_empty() => FALLBACK_TOPIC.to_owned(),
        topic => topic,
    };
    format!("{BRANCH_PREFIX}{}-{topic}", slug(name, usize::MAX))
}

fn branch_exists(repository: &Path, branch: &str) -> bool {
    git_succeeds(
        repository,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

pub(crate) fn unused_branch(repository: &Path, preferred: &str) -> io::Result<String> {
    if !branch_exists(repository, preferred) {
        return Ok(preferred.to_owned());
    }
    (2..=MAX_BRANCH_SUFFIX)
        .map(|suffix| format!("{preferred}-{suffix}"))
        .find(|candidate| !branch_exists(repository, candidate))
        .ok_or_else(|| io::Error::other(format!("Every branch name like {preferred} is taken")))
}

fn base_of(repository: &Path) -> io::Result<(String, String)> {
    let commit = git(repository, &["rev-parse", "HEAD"])?;
    let label = git(repository, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_else(|_| commit.clone());
    Ok((label, commit))
}

pub(crate) fn add_worktree(
    repository: &Path,
    path: &Path,
    name: &str,
    topic: &str,
) -> io::Result<Branch> {
    let (base, base_commit) = base_of(repository)?;
    let branch = unused_branch(repository, &branch_name(name, topic))?;
    git(
        repository,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            path_text(path)?,
            &base_commit,
        ],
    )?;
    Ok(Branch {
        name: branch,
        base,
        base_commit,
    })
}

pub(crate) fn delete_if_merged(repository: &Path, branch: &str) -> bool {
    git_succeeds(repository, &["branch", "-d", branch])
}

pub(crate) fn path_text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::other("Workspace path must be valid UTF-8"))
}

fn parse_numstat(text: &str) -> Vec<FileChange> {
    text.split('\0')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').splitn(3, '\t');
            let added = fields.next()?;
            let removed = fields.next()?;
            let path = fields.next().filter(|path| !path.is_empty())?;
            Some(FileChange {
                path: path.to_owned(),
                added: added.parse().unwrap_or(0),
                removed: removed.parse().unwrap_or(0),
            })
        })
        .collect()
}

fn text_lines(bytes: &[u8]) -> u64 {
    if bytes.contains(&0) || bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    newlines + u64::from(!bytes.ends_with(b"\n"))
}

fn untracked(worktree: &Path) -> io::Result<Vec<FileChange>> {
    let listing = git_raw(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    Ok(listing
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| FileChange {
            path: path.to_owned(),
            added: fs::read(worktree.join(path)).map_or(0, |bytes| text_lines(&bytes)),
            removed: 0,
        })
        .collect())
}

pub(crate) fn diff_stat(worktree: &Path, branch: &Branch) -> io::Result<DiffStat> {
    let tracked = git_raw(
        worktree,
        &[
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            &branch.base_commit,
            "--",
        ],
    )?;
    let mut files = parse_numstat(&tracked);
    files.extend(untracked(worktree)?);
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(DiffStat { files })
}

pub(crate) struct PullRequest<'a> {
    pub(crate) specialist: &'a str,
    pub(crate) role: &'a str,
    pub(crate) mission: &'a str,
    pub(crate) worktree: &'a Path,
    pub(crate) branch: &'a Branch,
    pub(crate) commit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Link {
    Opened(String),
    Missing(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Published {
    pub(crate) branch: String,
    pub(crate) remote: String,
    pub(crate) committed: bool,
    pub(crate) link: Link,
}

impl Published {
    pub(crate) fn describe(&self) -> String {
        let committed = if self.committed {
            "committed changes, "
        } else {
            ""
        };
        let link = match &self.link {
            Link::Opened(url) => format!("PR {url}"),
            Link::Missing(reason) => format!("PR not opened: {reason}"),
        };
        format!(
            "{committed}pushed {} to {} · {link}",
            self.branch, self.remote
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Tools {
    pub(crate) gh: OsString,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            gh: GH_PROGRAM.into(),
        }
    }
}

fn title(request: &PullRequest<'_>) -> String {
    let text = format!("{}: {}", request.specialist, request.mission);
    if text.chars().count() <= TITLE_CHARS {
        return text;
    }
    let kept: String = text.chars().take(TITLE_CHARS - 1).collect();
    format!("{kept}…")
}

fn body(request: &PullRequest<'_>) -> String {
    format!(
        "Opened by Agent Command from specialist {} ({}).\n\nTask: {}\nBranch: {} (base {})",
        request.specialist, request.role, request.mission, request.branch.name, request.branch.base
    )
}

fn has_changes(worktree: &Path) -> io::Result<bool> {
    Ok(!git(worktree, &["status", "--porcelain"])?.is_empty())
}

fn commit_all(request: &PullRequest<'_>) -> Result<(), String> {
    git(request.worktree, &["add", "--all"]).map_err(|error| error.to_string())?;
    git(
        request.worktree,
        &["commit", "--quiet", "-m", &title(request)],
    )
    .map(|_| ())
    .map_err(|error| format!("Commit failed: {error}"))
}

fn commits_ahead(request: &PullRequest<'_>) -> Result<u64, String> {
    let range = format!("{}..HEAD", request.branch.base_commit);
    git(request.worktree, &["rev-list", "--count", &range])
        .map_err(|error| error.to_string())
        .map(|count| count.parse().unwrap_or(0))
}

fn remote(worktree: &Path) -> Result<String, String> {
    let remotes = git(worktree, &["remote"]).map_err(|error| error.to_string())?;
    let names: Vec<&str> = remotes.lines().map(str::trim).collect();
    names
        .iter()
        .find(|name| **name == DEFAULT_REMOTE)
        .or_else(|| names.first())
        .filter(|name| !name.is_empty())
        .map(|name| (*name).to_owned())
        .ok_or_else(|| {
            "No Git remote is configured. Add one with `git remote add origin URL`.".to_owned()
        })
}

fn push(request: &PullRequest<'_>, remote: &str) -> Result<(), String> {
    git(
        request.worktree,
        &["push", "--quiet", "-u", remote, &request.branch.name],
    )
    .map(|_| ())
    .map_err(|error| format!("Push failed (never forced): {error}"))
}

fn program_available(program: &OsStr) -> bool {
    Process::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn base_is_branch(branch: &Branch) -> bool {
    branch.base != branch.base_commit
}

fn open_with_gh(request: &PullRequest<'_>, tools: &Tools) -> Link {
    if !program_available(&tools.gh) {
        return Link::Missing(
            "GitHub CLI `gh` is not installed or not on PATH. Install it, run `gh auth login`, then retry or open the PR in the browser.".into(),
        );
    }
    let mut command = Process::new(&tools.gh);
    command
        .current_dir(request.worktree)
        .args(["pr", "create", "--head", &request.branch.name])
        .args(["--title", &title(request), "--body", &body(request)])
        .stdin(Stdio::null());
    if base_is_branch(request.branch) {
        command.args(["--base", &request.branch.base]);
    }
    match command.output() {
        Ok(output) if output.status.success() => Link::Opened(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("created")
                .to_owned(),
        ),
        Ok(output) => Link::Missing(format!(
            "gh pr create failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => Link::Missing(format!("gh pr create failed: {error}")),
    }
}

pub(crate) fn publish(request: &PullRequest<'_>, tools: &Tools) -> Result<Published, String> {
    let dirty = has_changes(request.worktree).map_err(|error| error.to_string())?;
    if dirty && !request.commit {
        return Err(format!(
            "{} has uncommitted changes. Confirm with --commit to commit them before the PR.",
            request.specialist
        ));
    }
    if dirty {
        commit_all(request)?;
    }
    if commits_ahead(request)? == 0 {
        return Err(format!(
            "{} has no commits beyond {}; nothing to open a PR for.",
            request.branch.name, request.branch.base
        ));
    }
    let remote = remote(request.worktree)?;
    push(request, &remote)?;
    Ok(Published {
        branch: request.branch.name.clone(),
        remote,
        committed: dirty,
        link: open_with_gh(request, tools),
    })
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::specialists::tests::{git_repository, Scratch};
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn configure_identity(repository: &Path) {
        for (key, value) in [
            ("user.name", "Fixture"),
            ("user.email", "fixture@example.invalid"),
            ("commit.gpgSign", "false"),
        ] {
            git(repository, &["config", key, value]).unwrap();
        }
    }

    fn worktree(scratch: &Scratch, name: &str, topic: &str) -> (std::path::PathBuf, Branch) {
        let repository = scratch.0.join("repo");
        if !repository.exists() {
            git_repository(&repository);
            configure_identity(&repository);
        }
        let path = scratch.0.join(format!("tree-{name}"));
        let branch = add_worktree(&repository, &path, name, topic).unwrap();
        (path, branch)
    }

    #[test]
    fn slugs_and_branch_names_are_ascii_bounded_and_never_empty() {
        assert_eq!(slug("Review the  README!", 24), "review-the-readme");
        assert_eq!(slug("Réviser le λ README", 24), "r-viser-le-readme");
        assert_eq!(slug("修复错误", 24), "");
        assert_eq!(slug(&"ab-".repeat(20), 10), "ab-ab-ab-a");
        assert_eq!(branch_name("Nova_2", "修复错误"), "ac/nova-2-task");
        assert_eq!(
            branch_name("atlas", "Fix the flaky integration tests now please"),
            "ac/atlas-fix-the-flaky-integratio"
        );
        for topic in ["", "--", "x/../y", "a.lock", " trailing "] {
            let name = branch_name("nova", topic);
            assert!(name.starts_with("ac/nova-"));
            assert!(!name.ends_with('-') && !name.contains("..") && !name.ends_with(".lock"));
        }
    }

    #[test]
    fn named_branches_are_unique_when_a_name_is_taken() {
        let scratch = Scratch::new("branch-unique");
        let (first_path, first) = worktree(&scratch, "nova", "Review");
        let repository = scratch.0.join("repo");
        git(&repository, &["branch", "ac/nova-review-2"]).unwrap();
        let second_path = scratch.0.join("tree-nova-again");
        let second = add_worktree(&repository, &second_path, "nova", "Review").unwrap();
        assert_eq!(first.name, "ac/nova-review");
        assert_eq!(second.name, "ac/nova-review-3");
        assert_eq!(first.base, second.base);
        assert_eq!(first.base_commit.len(), 40);
        assert_eq!(
            git(&first_path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            "ac/nova-review"
        );
        assert!(second_path.join("README").is_file());
        git(
            &repository,
            &[
                "worktree",
                "remove",
                "--force",
                second_path.to_str().unwrap(),
            ],
        )
        .unwrap();
        assert!(delete_if_merged(&repository, &second.name));
        assert!(!branch_exists(&repository, &second.name));
    }

    #[test]
    fn diff_stat_counts_commits_edits_and_untracked_unicode_files() {
        let scratch = Scratch::new("branch-diff");
        let (path, branch) = worktree(&scratch, "nova", "Diff");
        assert_eq!(diff_stat(&path, &branch).unwrap(), DiffStat::default());
        fs::write(path.join("README"), "fixture\nmore\n").unwrap();
        git(&path, &["commit", "-qam", "More"]).unwrap();
        fs::write(path.join("README"), "changed\nmore\nλ line\n").unwrap();
        fs::write(path.join("λ-notes.md"), "één\ntwee").unwrap();
        fs::write(path.join("binary.bin"), [0u8, 1, 2]).unwrap();
        let stat = diff_stat(&path, &branch).unwrap();
        assert_eq!(
            stat.files,
            [
                FileChange {
                    path: "README".into(),
                    added: 3,
                    removed: 1
                },
                FileChange {
                    path: "binary.bin".into(),
                    added: 0,
                    removed: 0
                },
                FileChange {
                    path: "λ-notes.md".into(),
                    added: 2,
                    removed: 0
                },
            ]
        );
        assert_eq!(stat.label(), "+5 −1 · 3 files");
        assert_eq!(
            DiffStat {
                files: vec![FileChange::default()]
            }
            .label(),
            "+0 −0 · 1 file"
        );
        assert_eq!(
            parse_numstat("-\t-\tbin\0\n3\t1\tsrc/ü.rs\0"),
            [
                FileChange {
                    path: "bin".into(),
                    added: 0,
                    removed: 0
                },
                FileChange {
                    path: "src/ü.rs".into(),
                    added: 3,
                    removed: 1
                }
            ]
        );
    }

    fn fake_gh(scratch: &Scratch) -> OsString {
        let path = scratch.0.join("gh");
        fs::write(
            &path,
            "#!/bin/sh\ntest \"$1\" = --version && exit 0\nprintf '%s\\n' \"$*\" > \"$(dirname \"$0\")/gh-args\"\nprintf 'https://example.invalid/pull/7\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path.into_os_string()
    }

    fn request<'a>(path: &'a Path, branch: &'a Branch, commit: bool) -> PullRequest<'a> {
        PullRequest {
            specialist: "nova",
            role: "reviewer",
            mission: "Tidy the README λ",
            worktree: path,
            branch,
            commit,
        }
    }

    #[test]
    fn publish_requires_confirmation_a_remote_and_reports_missing_gh() {
        let scratch = Scratch::new("branch-publish");
        let (path, branch) = worktree(&scratch, "nova", "Tidy");
        let tools = Tools {
            gh: scratch.0.join("missing-gh").into_os_string(),
        };
        assert!(publish(&request(&path, &branch, false), &tools)
            .unwrap_err()
            .contains("no commits beyond"));
        fs::write(path.join("README"), "tidy λ\n").unwrap();
        let refused = publish(&request(&path, &branch, false), &tools).unwrap_err();
        assert!(refused.contains("Confirm with --commit"), "{refused}");
        assert!(has_changes(&path).unwrap());
        let no_remote = publish(&request(&path, &branch, true), &tools).unwrap_err();
        assert!(
            no_remote.contains("No Git remote is configured"),
            "{no_remote}"
        );
        assert!(!has_changes(&path).unwrap());
        let remote = scratch.0.join("remote.git");
        git(
            &scratch.0,
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        )
        .unwrap();
        git(
            &path,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .unwrap();
        let published = publish(&request(&path, &branch, false), &tools).unwrap();
        assert!(!published.committed);
        assert!(
            matches!(&published.link, Link::Missing(reason) if reason.contains("`gh` is not installed"))
        );
        assert!(published
            .describe()
            .starts_with("pushed ac/nova-tidy to origin · PR not opened"));
        assert_eq!(
            git(&remote, &["log", "-1", "--format=%s", "ac/nova-tidy"]).unwrap(),
            "nova: Tidy the README λ"
        );
    }

    #[test]
    fn publish_commits_pushes_and_opens_a_pr_with_gh() {
        let scratch = Scratch::new("branch-gh");
        let (path, branch) = worktree(&scratch, "atlas", "Ship");
        let remote = scratch.0.join("remote.git");
        git(
            &scratch.0,
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        )
        .unwrap();
        git(
            &path,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .unwrap();
        fs::write(path.join("shipped"), "yes\n").unwrap();
        let tools = Tools {
            gh: fake_gh(&scratch),
        };
        let published = publish(&request(&path, &branch, true), &tools).unwrap();
        assert_eq!(
            published,
            Published {
                branch: "ac/atlas-ship".into(),
                remote: "origin".into(),
                committed: true,
                link: Link::Opened("https://example.invalid/pull/7".into()),
            }
        );
        let arguments = fs::read_to_string(scratch.0.join("gh-args")).unwrap();
        assert!(arguments.starts_with("pr create --head ac/atlas-ship --title nova: Tidy"));
        assert!(arguments.contains(&format!("--base {}", branch.base)));
        fs::write(path.join("shipped"), "again\n").unwrap();
        git(&path, &["commit", "-qam", "Again"]).unwrap();
        git(&path, &["reset", "-q", "--hard", "HEAD~2"]).unwrap();
        fs::write(path.join("other"), "diverged\n").unwrap();
        git(&path, &["add", "other"]).unwrap();
        git(&path, &["commit", "-qm", "Diverged"]).unwrap();
        let rejected = publish(&request(&path, &branch, false), &tools).unwrap_err();
        assert!(
            rejected.starts_with("Push failed (never forced)"),
            "{rejected}"
        );
    }
}
