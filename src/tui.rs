pub mod app_ops;
pub mod diff_modal;
pub mod events;
pub mod render;
pub mod side_panel;
pub mod views;

use std::{fs, path::Path};

use ratatui::{buffer::Buffer, style::Color};

use crate::{
    auth::{self, Provider},
    editor::AutocompleteItem,
    markdown, mission,
    palette::PaletteItem,
    specialists::{self, Member},
};

pub fn map_tool_kind(kind: mission::ToolKind) -> markdown::ToolKind {
    match kind {
        mission::ToolKind::Read => markdown::ToolKind::Read,
        mission::ToolKind::Search => markdown::ToolKind::Grep,
        mission::ToolKind::Edit => markdown::ToolKind::Edit,
        mission::ToolKind::Write => markdown::ToolKind::Write,
        mission::ToolKind::Shell => markdown::ToolKind::Bash,
        mission::ToolKind::Mcp => markdown::ToolKind::Mcp,
        mission::ToolKind::Subagent => markdown::ToolKind::Delegate,
        mission::ToolKind::Web | mission::ToolKind::Todo | mission::ToolKind::Generic => {
            markdown::ToolKind::Other
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaletteToggles {
    pub rail_open: bool,
    pub left_panel_open: bool,
}

fn view_palette_items(left_panel_open: bool) -> Vec<PaletteItem> {
    let mut items = vec![PaletteItem {
        id: "view.left_panel".into(),
        category: "View".into(),
        label: if left_panel_open {
            "Hide Left Panel".into()
        } else {
            "Show Left Panel".into()
        },
        detail: "Toggle Views, Usage rings, and Workspace panel".into(),
        shortcut: "F8".into(),
        suggested: true,
    }];
    for (tab, shortcut) in views::ActiveTab::ALL
        .into_iter()
        .zip(["Alt+1", "Alt+2", "Alt+3"])
    {
        items.push(PaletteItem {
            id: format!("tab.{}", tab.label().to_ascii_lowercase()),
            category: "View".into(),
            label: format!("Switch to {}", tab.label()),
            detail: match tab {
                views::ActiveTab::Chat => "Conversation with Commander and specialists".into(),
                views::ActiveTab::Inbox => "Approvals, interruptions and finished work".into(),
                views::ActiveTab::Ledger => "Persistent task ledger for this workspace".into(),
            },
            shortcut: shortcut.into(),
            suggested: tab != views::ActiveTab::Chat,
        });
    }
    items
}

pub fn build_palette_items(
    crew: &[Member],
    profiles: &[auth::Profile],
    toggles: PaletteToggles,
    update_available: Option<&str>,
) -> Vec<PaletteItem> {
    let rail_open = toggles.rail_open;
    let mut items = vec![
        PaletteItem {
            id: "view.diff".into(),
            category: "Git".into(),
            label: "Open Unified Diff Viewer".into(),
            detail: "Inspect turn-scoped and working tree diff".into(),
            shortcut: "d".into(),
            suggested: true,
        },
        PaletteItem {
            id: "view.rail".into(),
            category: "View".into(),
            label: if rail_open {
                "Hide Right Rail".into()
            } else {
                "Show Right Rail".into()
            },
            detail: "Toggle Workers, Changes, and Connections rail".into(),
            shortcut: "Ctrl+B".into(),
            suggested: true,
        },
    ];
    items.extend(view_palette_items(toggles.left_panel_open));
    items.extend([
        PaletteItem {
            id: "settings.open".into(),
            category: "Accounts".into(),
            label: "Open Settings & Accounts".into(),
            detail: "Manage auto-adopted CLIs, isolated profiles, and ProdGate".into(),
            shortcut: "F3".into(),
            suggested: true,
        },
        PaletteItem {
            id: "transcript.copy".into(),
            category: "Transcript".into(),
            label: "Copy Latest Reply".into(),
            detail: "Copy via native clipboard + OSC 52".into(),
            shortcut: "Ctrl+Y".into(),
            suggested: true,
        },
        PaletteItem {
            id: "update.trigger".into(),
            category: "System".into(),
            label: match update_available {
                Some(ver) => format!("Install Update v{ver}"),
                None => "Check for Updates".into(),
            },
            detail: "Check latest GitHub release or self-update in place".into(),
            shortcut: "F6".into(),
            suggested: update_available.is_some(),
        },
        PaletteItem {
            id: "profile.cycle".into(),
            category: "Models".into(),
            label: "Cycle Commander Profile".into(),
            detail: "Switch between Auto routing and signed-in profiles".into(),
            shortcut: "F2".into(),
            suggested: false,
        },
        PaletteItem {
            id: "mode.cycle".into(),
            category: "Permissions".into(),
            label: "Cycle Commander Permission Mode".into(),
            detail: "WorkspaceAuto / AutoAcceptEdits / Supervised / ReadOnly".into(),
            shortcut: "/mode".into(),
            suggested: false,
        },
        PaletteItem {
            id: "prodgate.toggle".into(),
            category: "Permissions".into(),
            label: "Toggle ProdGate Floor".into(),
            detail: "Gate destructive prod/infra/force-push commands".into(),
            shortcut: "".into(),
            suggested: false,
        },
        PaletteItem {
            id: "transcript.toggle_expand".into(),
            category: "Transcript".into(),
            label: "Toggle Expand Thinking / Tool Card".into(),
            detail: "Expand or collapse the latest reasoning or tool card".into(),
            shortcut: "Ctrl+E".into(),
            suggested: false,
        },
        PaletteItem {
            id: "transcript.clear".into(),
            category: "Transcript".into(),
            label: "Clear Transcript View".into(),
            detail: "Reset visible conversation buffer".into(),
            shortcut: "Ctrl+L".into(),
            suggested: false,
        },
        PaletteItem {
            id: "editor.external".into(),
            category: "Composer".into(),
            label: "Edit Prompt in $EDITOR".into(),
            detail: "Open current draft in external editor".into(),
            shortcut: "Ctrl+X e".into(),
            suggested: false,
        },
        PaletteItem {
            id: "git.pr".into(),
            category: "Git".into(),
            label: "Commit / Push / Open PR".into(),
            detail: "Publish selected specialist branch".into(),
            shortcut: "p".into(),
            suggested: false,
        },
        PaletteItem {
            id: "connect.open".into(),
            category: "Accounts".into(),
            label: "Add Isolated / Enterprise Account".into(),
            detail: "Connect additional Claude, Codex, or Gemini account".into(),
            shortcut: "/connect".into(),
            suggested: false,
        },
        PaletteItem {
            id: "help.show".into(),
            category: "Help".into(),
            label: "Show Slash Command Reference".into(),
            detail: "Insert /help reference into transcript".into(),
            shortcut: "/help".into(),
            suggested: false,
        },
        PaletteItem {
            id: "app.quit".into(),
            category: "System".into(),
            label: "Quit Agent Command".into(),
            detail: "Cleanly interrupt active workers and exit".into(),
            shortcut: "Ctrl+Q".into(),
            suggested: false,
        },
    ]);

    for profile in profiles {
        items.push(PaletteItem {
            id: format!("entry:{profile}"),
            category: "Models".into(),
            label: format!("Use {profile}"),
            detail: if profile.is_system_default() {
                "Auto-adopted machine login".into()
            } else {
                "Isolated profile".into()
            },
            shortcut: "".into(),
            suggested: false,
        });
    }

    for (idx, member) in crew.iter().enumerate() {
        items.push(PaletteItem {
            id: format!("member:{idx}"),
            category: "Workers".into(),
            label: format!("Focus {}", member.name),
            detail: format!("{} · {}", member.role, member.status.caption()),
            shortcut: "".into(),
            suggested: false,
        });
    }

    items
}

pub fn build_slash_candidates() -> Vec<AutocompleteItem> {
    specialists::help_entries()
        .iter()
        .map(|entry| {
            let cmd = entry
                .usage
                .split_whitespace()
                .next()
                .unwrap_or(entry.usage)
                .to_owned();
            AutocompleteItem {
                value: cmd,
                label: entry.usage.to_owned(),
                detail: entry.summary.to_owned(),
                kind: "cmd",
            }
        })
        .collect()
}

pub fn build_mention_candidates(workspace: &Path, crew: &[Member]) -> Vec<AutocompleteItem> {
    let mut items = vec![
        AutocompleteItem {
            value: "claude".into(),
            label: "@claude".into(),
            detail: "Route turn to Claude Code".into(),
            kind: "model",
        },
        AutocompleteItem {
            value: "codex".into(),
            label: "@codex".into(),
            detail: "Route turn to Codex CLI".into(),
            kind: "model",
        },
        AutocompleteItem {
            value: "gemini".into(),
            label: "@gemini".into(),
            detail: "Route turn to Gemini CLI".into(),
            kind: "model",
        },
        AutocompleteItem {
            value: "opus".into(),
            label: "@opus".into(),
            detail: "Target Claude Opus tier".into(),
            kind: "model",
        },
    ];

    for member in crew.iter().skip(1) {
        items.push(AutocompleteItem {
            value: member.name.clone(),
            label: format!("@{}", member.name),
            detail: format!("Specialist ({})", member.role),
            kind: "worker",
        });
    }

    if let Ok(entries) = fs::read_dir(workspace) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name == "target" || name == "node_modules" {
                    return None;
                }
                Some(name)
            })
            .collect();
        names.sort();
        for name in names.into_iter().take(40) {
            items.push(AutocompleteItem {
                value: name.clone(),
                label: format!("@{name}"),
                detail: "Workspace path".into(),
                kind: "file",
            });
        }
    }

    items
}

pub fn provider_short_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
        Provider::Gemini => "Gemini",
    }
}

pub fn render_compact_svg(buffer: &Buffer, bg_hex: &str) -> String {
    let w = buffer.area.width;
    let h = buffer.area.height;
    let px_w = u32::from(w) * 9;
    let px_h = u32::from(h) * 18;
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {px_w} {px_h}\"><rect width=\"100%\" height=\"100%\" fill=\"{bg_hex}\"/>"
    );
    let css_color = |c: Color, fallback: &str| match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => fallback.to_owned(),
    };

    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let bg = css_color(buffer[(x, y)].bg, bg_hex);
            let start_x = x;
            while x < w && css_color(buffer[(x, y)].bg, bg_hex) == bg {
                x += 1;
            }
            if bg != bg_hex {
                svg.push_str(&format!(
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"18\" fill=\"{bg}\"/>",
                    u32::from(start_x) * 9,
                    u32::from(y) * 18,
                    u32::from(x - start_x) * 9,
                ));
            }
        }
    }

    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let cell = &buffer[(x, y)];
            if cell.symbol().trim().is_empty() {
                x += 1;
                continue;
            }
            let fg = css_color(cell.fg, "#e6e8ec");
            let bold = cell.modifier.contains(ratatui::style::Modifier::BOLD);
            let start_x = x;
            let mut run = String::new();
            while x < w {
                let c = &buffer[(x, y)];
                let c_fg = css_color(c.fg, "#e6e8ec");
                let c_bold = c.modifier.contains(ratatui::style::Modifier::BOLD);
                if c_fg != fg || c_bold != bold {
                    break;
                }
                let sym = c.symbol();
                if sym.is_empty() {
                    x += 1;
                    continue;
                }
                run.push_str(sym);
                x += 1;
            }
            let trimmed_end = run.trim_end();
            if !trimmed_end.is_empty() {
                let escaped = trimmed_end
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                let weight = if bold { "bold" } else { "normal" };
                svg.push_str(&format!(
                    "<text x=\"{}\" y=\"{}\" font-family=\"monospace\" font-weight=\"{weight}\" font-size=\"14\" xml:space=\"preserve\" fill=\"{fg}\">{escaped}</text>",
                    u32::from(start_x) * 9,
                    u32::from(y) * 18 + 14,
                ));
            }
        }
    }
    svg.push_str("</svg>");
    svg
}
