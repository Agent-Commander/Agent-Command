# TUI invariants

Applies to rendering, input handling, layout and the Token Burn deck. Each rule is `blocking`.

## 1. Layout at 80x24 and 120x40

Every screen renders fully at both sizes: no panic, no overlap, text bounded to its region, controls visible and clickable. Checks live in `tests/terminal_layout.rs` (Ratatui `TestBackend`).

BAD: a new panel tested only at 120x40, whose button falls off-screen at 80x24.

GOOD: the test renders at both sizes, asserts the button's rectangle is inside the frame, clicks its center and compares the result with the keyboard action.

## 2. Unicode input safety

Input, drafts, names and feed lines may contain multi-byte and wide characters. Edit by `char` or grapheme, measure by display width.

BAD:

```rust
draft.truncate(draft.len() - 1);
let shown = &line[..width];
```

GOOD:

```rust
draft.pop();
let shown: String = line.chars().take(width).collect();
```

Resizing keeps Unicode drafts, missions and selection intact.

## 3. Mouse and keyboard parity

Every clickable control has a keyboard path and every keyboard action that targets a visible control can be clicked. Tests drive both and assert identical state.

BAD: a new Abandon button that only responds to a click.

GOOD: Abandon works on click and on F4, and one test checks both produce the same state.

## 4. Terminal restoration

On normal exit, error return, suspension for a provider sign-in or installer, and panic, the terminal leaves the alternate screen, disables raw mode and mouse capture, and shows the cursor.

BAD: `enable_raw_mode()?` followed by code that can `?`-return or panic before `disable_raw_mode()`.

GOOD: restoration runs on every exit path (a guard or a panic hook that restores before the default hook prints), and suspend/resume pairs are tested.

## 5. Token Burn: unknown is muted

Unreported windows, unknown tiers, missing resets and stale readings render in the `muted` / `unknown` palette with a placeholder. Never draw them as 0%, 100%, an empty gauge or a full gauge.

BAD:

```rust
let used = window.used_percent.unwrap_or(0.0);
```

GOOD:

```rust
match window.used_percent {
    Some(used) if used.is_finite() => gauge(used, palette.allowance_color(Some(used))),
    _ => placeholder("-", palette.unknown),
}
```

`NaN` and infinite values are unknown, not numbers. Colors come from `theme.toml`; an invalid theme falls back to the built-in palette.
