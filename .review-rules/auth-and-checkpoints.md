# Auth and checkpoint rules

Applies to profiles, usage, routing, continuations, checkpoints and any durable state. Each rule is `blocking`.

## 1. Subscription-only

- Missions run only on signed-in subscription profiles under the isolated `sub-auth` root (`~/.local/share/sub-auth` or an absolute `SUB_AUTH_DIR`).
- Never fall back to a provider API key, the provider's standalone login, or another user's profile.
- A relative `SUB_AUTH_DIR` is refused.
- Tests always use a temporary profile root.

## 2. SHA-256 checkpoint integrity

- A checkpoint's digest covers HEAD, the index, and tracked and non-ignored working files, using SHA-256.
- The workspace is re-hashed before every continuation launch; any mismatch refuses the launch.
- Nested repositories, submodules, symlinks, special files and unreadable entries refuse continuation and name the path.
- Checkpoint state never stores the task text, transcript, file contents or credentials.
- A launched or abandoned claim is never resumed. One claim starts at most one continuation.

## 3. Atomic, durable state writes

Every state file is written to a temporary file in the same directory, flushed with `sync_all`, renamed over the target, and, for checkpoint fences, followed by a sync of the parent directory.

BAD:

```rust
fs::write(&path, bytes)?;
```

GOOD:

```rust
let mut file = fs::File::create(&temporary)?;
file.write_all(&bytes)?;
file.sync_all()?;
fs::rename(&temporary, &path)?;
```

Corrupt or unreadable state fails closed: the profile is not selected, polled or continued.

## 4. No synthetic Claude probes

Claude usage comes only from the native CLI's structured stream during a real mission. Never send a prompt, ping or empty mission to Claude to read usage. An unreported window stays unavailable.

Codex readbacks keep the five-minute monotonic floor per profile, across failures, wall-clock jumps and app restarts.

## 5. Latches

- A reported limit latches the profile until the provider's reset; a latch never shortens.
- No reliable reset means the latch stays and shows "reset unknown".
- Rotation and continuation skip latched, exhausted and corrupt profiles.
