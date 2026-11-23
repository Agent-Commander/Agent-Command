# Rust architecture rules

Applies to every Rust change. Each rule is `blocking` unless marked `nit`.

## 1. Rust only

No new Python, Node or shell runtime dependency to build, test or run. Helpers that need logic are Rust binaries or tests.

## 2. No explanatory source comments

Code explains itself through names, types, small functions and tests.

BAD:

```rust
// wait five minutes between probes
if elapsed > Duration::from_secs(300) {
```

GOOD:

```rust
const CODEX_PROBE_FLOOR: Duration = Duration::from_secs(300);
if elapsed > CODEX_PROBE_FLOOR {
```

## 3. Clean module boundaries

- One concern per module (`auth`, `usage`, `checkpoint`, `routing`, `catalog`, `specialists`, `panel`, `token_burn`).
- Rendering code does not read files or spawn processes; it draws state it is given.
- Platform code lives behind `cfg(unix)` / `cfg(windows)` modules such as `src/auth/unix.rs` and `src/auth/windows.rs`, not scattered `cfg` blocks.
- New `pub` items need a caller outside the module. `nit` otherwise.
- No new dependency without a reason in the PR body; versions stay pinned in `Cargo.lock`.

## 4. No secret leakage

Tokens, API keys, session files and credentials never appear in:

- log lines, feed entries or error messages,
- checkpoint, usage, roster or settings state,
- process arguments (`Command::arg`), which other local users can read.

Pass secrets through the environment or the provider's own profile directory.

BAD:

```rust
Command::new("tool").arg(format!("--key={key}"));
```

GOOD:

```rust
Command::new("tool").env("ARTIFICIAL_ANALYSIS_API_KEY", key);
```

## 5. Cross-platform paths and permissions

- Build paths with `Path::join`, never string concatenation with `/` or `\`.
- Accept non-UTF-8 paths; use `OsStr` / `as_encoded_bytes` rather than `to_str().unwrap()`.
- Private state is user-only: mode `0o700` / `0o600` on Unix, an owner-only ACL on Windows (NTFS required).
- Line endings and case-insensitive file systems must not change behavior on Windows or macOS.
- Every change passes the gates on Linux, macOS and Windows in CI.

## 6. Errors

- No `unwrap()` or `expect()` on input from users, files, processes or the network. `expect` is allowed only for invariants the code itself just established.
- Fail closed: unreadable or corrupt state blocks the action and names the path; it never falls back to a default that grants access.
