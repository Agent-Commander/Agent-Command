# sub-auth

Manage multiple subscription logins for Codex CLI (`codex`) and Claude Code (`claude`). Each worker shell selects a named account. Requires macOS or Linux, Python 3.7+, and the corresponding executable on your PATH.

## Sign in

From this directory, run one login at a time and select the intended account in the provider's browser window.

```sh
./sub-auth login claude work
./sub-auth login claude personal
./sub-auth login codex work
./sub-auth login codex personal
```

Each provider has its own set of profile names. Use lowercase letters, digits, underscores and hyphens, starting with a letter or digit.

## Use a profile

```sh
./sub-auth list
./sub-auth status codex personal
./sub-auth run codex personal exec "Summarize this repository"
./sub-auth run claude work -p "Summarize this repository"
./sub-auth logout claude work
```

A worker can invoke the absolute path to `sub-auth` from its own working directory. Pass the provider, profile name and agent arguments as separate arguments. After checking the cached login method, `run` replaces itself with the provider CLI, preserving the worker's directory, standard streams and exit status. Different workers can select different profiles concurrently.

`list` shows local profile names, including signed-out profiles. `status` returns zero for a cached subscription login. The provider checks account access when the agent makes a request.

## Credential storage

Profiles live under `~/.local/share/sub-auth/<provider>/<profile>` in directories accessible to the current OS user. Set `SUB_AUTH_DIR` to an absolute path to choose a different private storage directory. Run login and workers as the same OS user with the same storage path.

Claude Code uses a profile-specific `CLAUDE_CONFIG_DIR`, including a separate Keychain entry per directory on macOS. Codex uses a profile-specific `CODEX_HOME` and stores its ChatGPT credentials in `auth.json`. The provider CLIs own token storage and refresh. Treat the profile directory as sensitive. [Claude credential management](https://code.claude.com/docs/en/authentication#credential-management) · [Codex authentication](https://developers.openai.com/codex/auth/)
