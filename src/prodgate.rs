use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const BYPASS_DENY_RULES: [&str; 5] = [
    "Bash(kubectl:*)",
    "Bash(terraform:*)",
    "Bash(tofu:*)",
    "Bash(helm:*)",
    "Bash(flyctl:*)",
];

const SHELL_TOOL_NAMES: [&str; 12] = [
    "bash",
    "shell",
    "execute",
    "command_execution",
    "run_command",
    "terminal",
    "sh",
    "zsh",
    "powershell",
    "pwsh",
    "cmd",
    "run_shell_command",
];

const KUBECTL_MUTATING: [&str; 14] = [
    "apply",
    "delete",
    "scale",
    "rollout",
    "drain",
    "cordon",
    "uncordon",
    "taint",
    "patch",
    "replace",
    "edit",
    "create",
    "set",
    "autoscale",
];

const TERRAFORM_MUTATING: [&str; 5] = ["apply", "destroy", "import", "taint", "force-unlock"];

const HELM_MUTATING: [&str; 5] = ["install", "upgrade", "uninstall", "rollback", "delete"];

const AWS_DESTRUCTIVE_EXACT: [&str; 4] = ["terminate-instances", "rm", "deregister", "purge-queue"];

const GCLOUD_AZ_DESTRUCTIVE: [&str; 4] = ["delete", "destroy", "remove", "purge"];

const PUBLISH_BINARIES: [&str; 4] = ["npm", "yarn", "pnpm", "cargo"];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProdGateDecision {
    pub category: String,
    pub reason: String,
    pub matched_command: String,
}

pub struct ProdGate;

impl ProdGate {
    pub fn check(tool_name: &str, input: &Value) -> Option<ProdGateDecision> {
        Self::check_with_extra(tool_name, input, &[])
    }

    pub fn check_with_extra(
        tool_name: &str,
        input: &Value,
        extra_patterns: &[String],
    ) -> Option<ProdGateDecision> {
        let commands = extract_commands(tool_name, input);
        for raw in commands {
            for segment in split_shell_segments(&raw) {
                if let Some(decision) = check_segment(&segment, extra_patterns) {
                    return Some(decision);
                }
            }
        }
        None
    }
}

fn is_shell_tool(tool_name: &str) -> bool {
    let normalized = tool_name.trim().to_ascii_lowercase();
    SHELL_TOOL_NAMES
        .iter()
        .any(|candidate| *candidate == normalized)
}

fn extract_commands(tool_name: &str, input: &Value) -> Vec<String> {
    let mut commands = Vec::new();
    match input {
        Value::String(text) if is_shell_tool(tool_name) => {
            commands.push(text.clone());
        }
        Value::Object(map) => {
            for key in ["command", "cmd", "script", "argv", "args"] {
                if let Some(value) = map.get(key) {
                    collect_command_value(value, &mut commands);
                }
            }
            if commands.is_empty() && is_shell_tool(tool_name) {
                if let Some(Value::String(text)) = map.get("input") {
                    commands.push(text.clone());
                }
            }
        }
        Value::Array(items) if is_shell_tool(tool_name) => {
            collect_argv(items, &mut commands);
        }
        _ => {}
    }
    commands
}

fn collect_command_value(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_owned());
            }
        }
        Value::Array(items) => collect_argv(items, out),
        _ => {}
    }
}

fn collect_argv(items: &[Value], out: &mut Vec<String>) {
    let parts: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
    if parts.is_empty() {
        return;
    }
    if let Some(payload) = unwrap_shell_wrapper(&parts) {
        out.push(payload.to_owned());
    }
    out.push(parts.join(" "));
}

fn unwrap_shell_wrapper<'a>(parts: &[&'a str]) -> Option<&'a str> {
    let head = basename_lower(parts.first()?);
    let is_posix_shell = matches!(head.as_str(), "sh" | "bash" | "zsh" | "dash" | "fish");
    let is_win_shell = matches!(
        head.as_str(),
        "cmd" | "cmd.exe" | "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    );
    if is_posix_shell {
        for (index, token) in parts.iter().enumerate().skip(1) {
            if token.starts_with('-') && token.ends_with('c') && token.len() <= 4 {
                return parts.get(index + 1).copied();
            }
        }
    }
    if is_win_shell {
        for (index, token) in parts.iter().enumerate().skip(1) {
            let lower = token.to_ascii_lowercase();
            if matches!(lower.as_str(), "/c" | "-c" | "-command") {
                return parts.get(index + 1).copied();
            }
        }
    }
    None
}

fn split_shell_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            current.push(ch);
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            current.push(ch);
            continue;
        }
        if !in_single && !in_double {
            match ch {
                ';' | '\n' | '\r' => {
                    push_segment(&mut segments, &mut current);
                    continue;
                }
                '&' if chars.peek() == Some(&'&') => {
                    chars.next();
                    push_segment(&mut segments, &mut current);
                    continue;
                }
                '|' => {
                    if chars.peek() == Some(&'|') {
                        chars.next();
                    }
                    push_segment(&mut segments, &mut current);
                    continue;
                }
                _ => {}
            }
        }
        current.push(ch);
    }
    push_segment(&mut segments, &mut current);
    segments
}

fn push_segment(segments: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_owned());
    }
    current.clear();
}

fn tokenize(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in segment.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn strip_leading_env_and_wrappers(tokens: &[String]) -> &[String] {
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        let base = basename_lower(token);
        if is_env_assignment(token)
            || matches!(base.as_str(), "sudo" | "env" | "command" | "nohup" | "time")
        {
            index += 1;
            continue;
        }
        break;
    }
    &tokens[index..]
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn basename_lower(token: &str) -> String {
    token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase()
}

fn decision(category: &str, reason: impl Into<String>, segment: &str) -> ProdGateDecision {
    ProdGateDecision {
        category: category.into(),
        reason: reason.into(),
        matched_command: segment.trim().to_owned(),
    }
}

fn check_segment(segment: &str, extra_patterns: &[String]) -> Option<ProdGateDecision> {
    if let Some(hit) = check_extra_patterns(segment, extra_patterns) {
        return Some(hit);
    }
    if let Some(hit) = check_destructive_sql(segment) {
        return Some(hit);
    }
    let raw_tokens = tokenize(segment);
    let tokens = strip_leading_env_and_wrappers(&raw_tokens);
    let first = tokens.first()?;
    let bin = basename_lower(first);
    let bin = bin.strip_suffix(".exe").unwrap_or(&bin);
    let rest = &tokens[1..];

    if matches!(
        bin,
        "sh" | "bash" | "zsh" | "dash" | "powershell" | "pwsh" | "cmd"
    ) {
        let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
        if let Some(payload) = unwrap_shell_wrapper(&refs) {
            for inner in split_shell_segments(payload) {
                if let Some(hit) = check_segment(&inner, extra_patterns) {
                    return Some(hit);
                }
            }
        }
    }

    match bin {
        "kubectl" => check_kubectl(rest, segment),
        "terraform" | "tofu" => check_terraform(bin, rest, segment),
        "helm" => check_helm(rest, segment),
        "git" => check_git(rest, segment),
        "rm" => check_rm(rest, segment),
        "aws" => check_aws(rest, segment),
        "gcloud" => check_gcloud_or_az("gcloud", rest, segment),
        "az" => check_gcloud_or_az("az", rest, segment),
        "docker" | "podman" => check_container_push(bin, rest, segment),
        "flyctl" | "fly" => check_fly(rest, segment),
        "vercel" => check_vercel(rest, segment),
        "wrangler" => check_wrangler(rest, segment),
        "gem" => check_gem(rest, segment),
        "twine" => check_twine(rest, segment),
        "gh" => check_gh(rest, segment),
        other if PUBLISH_BINARIES.contains(&other) => check_package_publish(other, rest, segment),
        _ => None,
    }
}

fn check_extra_patterns(segment: &str, extra_patterns: &[String]) -> Option<ProdGateDecision> {
    let lower = segment.to_ascii_lowercase();
    for pattern in extra_patterns {
        let needle = pattern.trim().to_ascii_lowercase();
        if !needle.is_empty() && lower.contains(&needle) {
            return Some(decision(
                "policy",
                format!("Command matches organization policy deny rule `{pattern}`."),
                segment,
            ));
        }
    }
    None
}

fn first_subcommand(args: &[String]) -> Option<&str> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg.starts_with('-') {
            if !arg.contains('=')
                && matches!(
                    arg.as_str(),
                    "-n" | "--namespace"
                        | "--context"
                        | "--kubeconfig"
                        | "-C"
                        | "-chdir"
                        | "--profile"
                        | "--region"
                )
            {
                skip_next = true;
            }
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

fn check_kubectl(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    let sub = first_subcommand(args)?.to_ascii_lowercase();
    if KUBECTL_MUTATING.contains(&sub.as_str()) {
        return Some(decision(
            "kubernetes",
            format!("`kubectl {sub}` mutates cluster state and requires confirmation."),
            segment,
        ));
    }
    None
}

fn check_terraform(bin: &str, args: &[String], segment: &str) -> Option<ProdGateDecision> {
    let non_flags: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .map(|arg| arg.to_ascii_lowercase())
        .collect();
    let sub = non_flags.first()?.as_str();
    if TERRAFORM_MUTATING.contains(&sub) {
        return Some(decision(
            "infrastructure",
            format!("`{bin} {sub}` modifies cloud infrastructure and requires confirmation."),
            segment,
        ));
    }
    if sub == "state"
        && non_flags
            .get(1)
            .is_some_and(|op| matches!(op.as_str(), "rm" | "mv" | "push" | "replace-provider"))
    {
        return Some(decision(
            "infrastructure",
            format!("`{bin} state` mutation requires confirmation."),
            segment,
        ));
    }
    None
}

fn check_helm(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    let sub = first_subcommand(args)?.to_ascii_lowercase();
    if HELM_MUTATING.contains(&sub.as_str()) {
        return Some(decision(
            "kubernetes",
            format!("`helm {sub}` modifies cluster releases and requires confirmation."),
            segment,
        ));
    }
    None
}

fn check_git(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    let position = args.iter().position(|arg| arg == "push")?;
    let push_args = &args[position + 1..];
    for arg in push_args {
        if matches!(
            arg.as_str(),
            "-f" | "--force" | "--force-with-lease" | "--delete" | "-d"
        ) || arg.starts_with("--force-with-lease=")
            || arg.starts_with("--force-if-includes")
        {
            return Some(decision(
                "git-destructive",
                format!("`git push {arg}` rewrites or deletes remote history and requires confirmation."),
                segment,
            ));
        }
        if arg.starts_with('+') && arg.len() > 1 {
            return Some(decision(
                "git-destructive",
                "Force refspec push (`+ref`) rewrites remote history and requires confirmation.",
                segment,
            ));
        }
    }
    None
}

fn check_rm(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    let mut recursive = false;
    let mut force = false;
    let mut targets = Vec::new();
    let mut after_double_dash = false;
    for arg in args {
        if after_double_dash {
            targets.push(arg.as_str());
            continue;
        }
        if arg == "--" {
            after_double_dash = true;
            continue;
        }
        if arg.starts_with("--") {
            match arg.as_str() {
                "--recursive" => recursive = true,
                "--force" => force = true,
                _ => {}
            }
        } else if arg.starts_with('-') && arg.len() > 1 {
            for flag in arg[1..].chars() {
                match flag {
                    'r' | 'R' => recursive = true,
                    'f' => force = true,
                    _ => {}
                }
            }
        } else {
            targets.push(arg.as_str());
        }
    }
    if !(recursive && force) {
        return None;
    }
    for target in targets {
        let trimmed = target.trim();
        let dangerous = matches!(
            trimmed,
            "/" | "/*"
                | "~"
                | "~/"
                | "~/*"
                | "$HOME"
                | "${HOME}"
                | "$HOME/"
                | "${HOME}/"
                | "C:\\"
                | "C:/"
                | "c:\\"
                | "c:/"
        ) || trimmed.starts_with("/etc")
            || trimmed.starts_with("/usr")
            || trimmed.starts_with("/var")
            || trimmed.starts_with("/System");
        if dangerous {
            return Some(decision(
                "filesystem-wipe",
                format!("Recursive forced deletion of `{trimmed}` requires confirmation."),
                segment,
            ));
        }
    }
    None
}

fn check_aws(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    for arg in args {
        if arg.starts_with('-') {
            continue;
        }
        let lower = arg.to_ascii_lowercase();
        if AWS_DESTRUCTIVE_EXACT.contains(&lower.as_str())
            || lower.starts_with("delete-")
            || lower.starts_with("remove-")
            || lower.starts_with("terminate-")
            || lower.starts_with("deregister-")
        {
            return Some(decision(
                "cloud-destructive",
                format!("`aws ... {lower}` performs a destructive cloud action and requires confirmation."),
                segment,
            ));
        }
    }
    None
}

fn check_gcloud_or_az(bin: &str, args: &[String], segment: &str) -> Option<ProdGateDecision> {
    for arg in args {
        if arg.starts_with('-') {
            continue;
        }
        let lower = arg.to_ascii_lowercase();
        if GCLOUD_AZ_DESTRUCTIVE.contains(&lower.as_str()) {
            return Some(decision(
                "cloud-destructive",
                format!("`{bin} ... {lower}` deletes cloud resources and requires confirmation."),
                segment,
            ));
        }
    }
    None
}

fn check_container_push(bin: &str, args: &[String], segment: &str) -> Option<ProdGateDecision> {
    if first_subcommand(args).is_some_and(|sub| sub.eq_ignore_ascii_case("push")) {
        return Some(decision(
            "publish",
            format!("`{bin} push` publishes container images and requires confirmation."),
            segment,
        ));
    }
    None
}

fn check_fly(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    let sub = first_subcommand(args)?.to_ascii_lowercase();
    if matches!(sub.as_str(), "deploy" | "destroy" | "scale") {
        return Some(decision(
            "deploy",
            format!("`fly {sub}` affects production deployment and requires confirmation."),
            segment,
        ));
    }
    None
}

fn check_vercel(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--prod" | "--target=production"))
    {
        return Some(decision(
            "deploy",
            "`vercel --prod` deploys to production and requires confirmation.",
            segment,
        ));
    }
    None
}

fn check_wrangler(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    let sub = first_subcommand(args)?.to_ascii_lowercase();
    if matches!(sub.as_str(), "deploy" | "publish" | "delete") {
        return Some(decision(
            "deploy",
            format!("`wrangler {sub}` modifies edge deployments and requires confirmation."),
            segment,
        ));
    }
    None
}

fn check_gem(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    let sub = first_subcommand(args)?.to_ascii_lowercase();
    if matches!(sub.as_str(), "push" | "publish") {
        return Some(decision(
            "publish",
            format!("`gem {sub}` publishes a public package and requires confirmation."),
            segment,
        ));
    }
    None
}

fn check_twine(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    if first_subcommand(args).is_some_and(|sub| sub.eq_ignore_ascii_case("upload")) {
        return Some(decision(
            "publish",
            "`twine upload` publishes Python packages and requires confirmation.",
            segment,
        ));
    }
    None
}

fn check_gh(args: &[String], segment: &str) -> Option<ProdGateDecision> {
    let non_flags: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .map(|arg| arg.to_ascii_lowercase())
        .collect();
    if non_flags.len() >= 2
        && non_flags[0] == "release"
        && matches!(non_flags[1].as_str(), "create" | "delete")
    {
        return Some(decision(
            "publish",
            format!(
                "`gh release {}` mutates public releases and requires confirmation.",
                non_flags[1]
            ),
            segment,
        ));
    }
    None
}

fn check_package_publish(bin: &str, args: &[String], segment: &str) -> Option<ProdGateDecision> {
    let sub = first_subcommand(args)?.to_ascii_lowercase();
    if sub == "publish"
        && !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--dry-run" | "-n"))
    {
        return Some(decision(
            "publish",
            format!("`{bin} publish` publishes a package publicly and requires confirmation."),
            segment,
        ));
    }
    None
}

fn check_destructive_sql(segment: &str) -> Option<ProdGateDecision> {
    let normalized = collapse_ascii_whitespace(segment).to_ascii_uppercase();
    for keyword in [
        "DROP TABLE",
        "DROP DATABASE",
        "DROP SCHEMA",
        "TRUNCATE TABLE",
    ] {
        if contains_sql_phrase(&normalized, keyword) {
            return Some(decision(
                "sql-destructive",
                format!("Destructive SQL `{keyword}` requires confirmation."),
                segment,
            ));
        }
    }
    if contains_sql_phrase(&normalized, "TRUNCATE ")
        && (normalized.contains("PSQL")
            || normalized.contains("MYSQL")
            || normalized.contains("SQLITE")
            || normalized.contains("SQL")
            || normalized.starts_with("TRUNCATE "))
    {
        return Some(decision(
            "sql-destructive",
            "Destructive SQL `TRUNCATE` requires confirmation.",
            segment,
        ));
    }
    if let Some(pos) = normalized.find("DELETE FROM ") {
        let tail = &normalized[pos..];
        if !tail.contains(" WHERE ") {
            return Some(decision(
                "sql-destructive",
                "Unfiltered SQL `DELETE FROM` without `WHERE` requires confirmation.",
                segment,
            ));
        }
    }
    None
}

fn collapse_ascii_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn contains_sql_phrase(haystack: &str, phrase: &str) -> bool {
    let Some(pos) = haystack.find(phrase) else {
        return false;
    };
    let before_ok = pos == 0 || !haystack.as_bytes()[pos - 1].is_ascii_alphanumeric();
    let end = pos + phrase.len();
    let after_ok = end == haystack.len() || !haystack.as_bytes()[end].is_ascii_alphanumeric();
    before_ok && after_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn blocks_destructive_prod_and_infra_commands() {
        let blocked = [
            (
                "Bash",
                json!({"command": "kubectl apply -f prod.yaml"}),
                "kubernetes",
            ),
            (
                "Bash",
                json!({"command": "kubectl -n prod delete pod web-1"}),
                "kubernetes",
            ),
            (
                "Bash",
                json!({"command": "terraform apply -auto-approve"}),
                "infrastructure",
            ),
            ("Bash", json!({"command": "tofu destroy"}), "infrastructure"),
            (
                "Bash",
                json!({"command": "helm upgrade web ./chart"}),
                "kubernetes",
            ),
            (
                "Bash",
                json!({"command": "git push origin main --force"}),
                "git-destructive",
            ),
            (
                "Bash",
                json!({"command": "git push -f origin main"}),
                "git-destructive",
            ),
            (
                "Bash",
                json!({"command": "git push origin +main"}),
                "git-destructive",
            ),
            (
                "Bash",
                json!({"command": "psql -c 'DROP TABLE users;'"}),
                "sql-destructive",
            ),
            (
                "Bash",
                json!({"command": "sqlite3 app.db \"DELETE FROM sessions\""}),
                "sql-destructive",
            ),
            ("Bash", json!({"command": "rm -rf /"}), "filesystem-wipe"),
            ("Bash", json!({"command": "rm -fr ~"}), "filesystem-wipe"),
            (
                "Bash",
                json!({"command": "aws ec2 terminate-instances --instance-ids i-123"}),
                "cloud-destructive",
            ),
            (
                "Bash",
                json!({"command": "gcloud compute instances delete web"}),
                "cloud-destructive",
            ),
            (
                "Bash",
                json!({"command": "az group delete -n prod"}),
                "cloud-destructive",
            ),
            ("Bash", json!({"command": "cargo publish"}), "publish"),
            ("Bash", json!({"command": "npm publish"}), "publish"),
            (
                "Bash",
                json!({"command": "docker push myorg/app:latest"}),
                "publish",
            ),
            ("Bash", json!({"command": "flyctl deploy"}), "deploy"),
            ("Bash", json!({"command": "vercel --prod"}), "deploy"),
            ("Bash", json!({"command": "wrangler deploy"}), "deploy"),
        ];
        for (tool, input, expected_category) in blocked {
            let hit = ProdGate::check(tool, &input)
                .unwrap_or_else(|| panic!("expected ProdGate hit for {input}"));
            assert_eq!(hit.category, expected_category, "{input}");
        }
    }

    #[test]
    fn catches_codex_argv_arrays_and_chained_commands() {
        let argv =
            json!({"command": ["bash", "-lc", "cd infra && terraform destroy -auto-approve"]});
        let hit = ProdGate::check("command_execution", &argv).unwrap();
        assert_eq!(hit.category, "infrastructure");

        let direct_argv = json!({"argv": ["kubectl", "delete", "deployment", "api"]});
        let hit = ProdGate::check("execute", &direct_argv).unwrap();
        assert_eq!(hit.category, "kubernetes");

        let nested = json!({"command": "echo ok && sh -c 'git push --force-with-lease'"});
        let hit = ProdGate::check("Bash", &nested).unwrap();
        assert_eq!(hit.category, "git-destructive");
    }

    #[test]
    fn allows_routine_dev_and_read_only_commands() {
        let allowed = [
            ("Bash", json!({"command": "cargo test --all-targets"})),
            ("Bash", json!({"command": "cargo publish --dry-run"})),
            (
                "Bash",
                json!({"command": "git status && git diff && git commit -m 'fix'"}),
            ),
            ("Bash", json!({"command": "git push origin feat/clean"})),
            ("Bash", json!({"command": "kubectl get pods -n default"})),
            ("Bash", json!({"command": "terraform plan"})),
            ("Bash", json!({"command": "helm list"})),
            ("Bash", json!({"command": "rm -rf target/tmp"})),
            (
                "Bash",
                json!({"command": "sqlite3 app.db \"DELETE FROM sessions WHERE id = 1\""}),
            ),
            ("Bash", json!({"command": "gh pr view 42 --json state"})),
            (
                "Edit",
                json!({"file_path": "src/main.rs", "old_string": "a", "new_string": "b"}),
            ),
        ];
        for (tool, input) in allowed {
            assert_eq!(ProdGate::check(tool, &input), None, "{tool} {input}");
        }
    }

    #[test]
    fn honors_extra_organization_deny_patterns() {
        let input = json!({"command": "stripe charges create --amount=5000"});
        assert!(ProdGate::check("Bash", &input).is_none());
        let hit = ProdGate::check_with_extra("Bash", &input, &["stripe charges".into()]).unwrap();
        assert_eq!(hit.category, "policy");
    }
}
