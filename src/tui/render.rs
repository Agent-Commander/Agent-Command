use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    auth::Provider,
    markdown::{self, UiPalette},
    specialists::{self, RosterRow, Status},
    tui::{events::ChangedFile, provider_short_name},
    update::UpdateState,
    App,
};

const RAIL_MIN_WIDTH: u16 = 100;
const RAIL_WIDTH: u16 = 34;
const ABANDON_LABEL: &str = " F4 Abandon ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RailHit {
    ToggleRail,
    SelectMember(usize),
    ToggleSettled,
    OpenDiff,
    OpenPr,
    OpenSettings,
    OpenConnect(Provider),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusBannerHit {
    OpenDiff,
    OpenPr,
    BackToCommander,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WelcomeHit {
    OpenConnect,
    OpenSettings,
    DismissWelcome,
}

fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

fn truncate_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    let ellipsis_w = display_width("…");
    let budget = max_width.saturating_sub(ellipsis_w);
    let mut out = String::new();
    let mut used = 0usize;
    for g in text.graphemes(true) {
        let gw = display_width(g);
        if used + gw > budget {
            break;
        }
        out.push_str(g);
        used += gw;
    }
    out.push('…');
    out
}

fn text_u16(text: &str) -> u16 {
    u16::try_from(display_width(text)).unwrap_or(u16::MAX)
}

fn right_slice(bar: Rect, right_edge: u16, width: u16) -> Rect {
    let clamped = width.min(right_edge.saturating_sub(bar.x));
    Rect::new(
        right_edge.saturating_sub(clamped),
        bar.y,
        clamped,
        bar.height,
    )
}

pub fn draw_single_canvas(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let palette = app.ui_palette.clone();
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg).fg(palette.fg)),
        area,
    );

    app.rail_hits.clear();
    app.focus_banner_hits.clear();
    app.welcome_hits.clear();

    if area.width < 12 || area.height < 6 {
        return;
    }

    let show_rail = app.rail_open && area.width >= RAIL_MIN_WIDTH;
    let main_w = if show_rail {
        area.width.saturating_sub(RAIL_WIDTH)
    } else {
        area.width
    };
    let max_composer_h = area.height.saturating_sub(6).clamp(3, 9);
    let composer_h = app
        .composer
        .desired_total_height(main_w)
        .min(max_composer_h);

    let vertical = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(composer_h),
        Constraint::Length(1),
    ])
    .split(area);

    let top_bar_area = vertical[0];
    let body_area = vertical[1];
    let composer_outer = vertical[2];
    let footer_area = vertical[3];

    draw_top_bar(app, frame, top_bar_area, &palette);

    let (canvas_area, rail_area) = if show_rail {
        let cols = Layout::horizontal([Constraint::Min(24), Constraint::Length(RAIL_WIDTH)])
            .split(body_area);
        (cols[0], Some(cols[1]))
    } else {
        app.crew_area = Rect::default();
        (body_area, None)
    };

    draw_canvas(app, frame, canvas_area, &palette);

    if let Some(rail) = rail_area {
        draw_right_rail(app, frame, rail, &palette);
    }

    let composer_area = Rect::new(
        composer_outer.x,
        composer_outer.y,
        area.width,
        composer_outer.height,
    );
    app.input_area = composer_area;
    app.send_area = Rect::default();
    app.new_area = Rect::default();
    app.pause_area = Rect::default();

    let composer_focused = app.editing
        && !app.rail_focused
        && !app.palette.is_open()
        && !app.diff_modal.is_open()
        && app.panel.is_none();
    app.composer
        .render(frame, composer_area, composer_focused, &palette);

    draw_footer(app, frame, footer_area, &palette);

    if app.autocomplete.is_open()
        && composer_focused
        && !app.palette.is_open()
        && !app.diff_modal.is_open()
        && app.panel.is_none()
    {
        app.autocomplete
            .render(frame, composer_area, area, &palette);
    }

    if app.diff_modal.is_open() {
        app.diff_modal.render(frame, area, &palette);
    }

    if app.palette.is_open() {
        app.palette.render(frame, area, &palette);
    }

    app.draw_panel(frame, area);
}

fn draw_top_bar(app: &mut App, frame: &mut Frame, bar: Rect, palette: &UiPalette) {
    let bg = palette.surface;
    frame.render_widget(Block::default().style(Style::default().bg(bg)), bar);

    let settings_label = if bar.width >= 96 {
        " ⚙  Settings "
    } else {
        " ⚙  "
    };
    let palette_label = if bar.width >= 80 { " Ctrl+P " } else { " ^P " };

    let settings_w = text_u16(settings_label);
    let palette_w = text_u16(palette_label);

    app.settings_area = right_slice(bar, bar.right(), settings_w);
    app.palette_area = right_slice(bar, app.settings_area.x.saturating_sub(1), palette_w);

    frame.render_widget(
        Paragraph::new(settings_label).style(
            Style::default()
                .fg(palette.fg)
                .bg(palette.surface_alt)
                .add_modifier(Modifier::BOLD),
        ),
        app.settings_area,
    );
    frame.render_widget(
        Paragraph::new(palette_label).style(
            Style::default()
                .fg(palette.accent)
                .bg(palette.surface_alt)
                .add_modifier(Modifier::BOLD),
        ),
        app.palette_area,
    );

    let left_budget = app.palette_area.x.saturating_sub(bar.x).saturating_sub(1) as usize;
    if left_budget == 0 {
        app.profile_area = Rect::default();
        return;
    }

    let workspace_name = app.workspace_display_name();
    let branch_part = app
        .git_snapshot
        .branch
        .as_deref()
        .map(|branch| {
            let dirty = if app.git_snapshot.dirty { "*" } else { "" };
            format!(" ({branch}{dirty})")
        })
        .unwrap_or_default();
    let commander_badge = app.top_bar_commander_badge();
    let gh_badge = app.top_bar_github_badge();
    let quota_badge = app.top_bar_quota_badge();

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    let brand = " agent-command";
    let brand_w = display_width(brand);
    if used + brand_w <= left_budget {
        spans.push(Span::styled(
            brand,
            Style::default()
                .fg(palette.accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ));
        used += brand_w;
    }

    let repo_seg = format!(" · {workspace_name}{branch_part}");
    let repo_w = display_width(&repo_seg);
    if used + repo_w <= left_budget {
        spans.push(Span::styled(
            repo_seg,
            Style::default().fg(palette.fg).bg(bg),
        ));
        used += repo_w;
    } else if left_budget > used + 4 {
        let clipped = truncate_width(&repo_seg, left_budget - used);
        used += display_width(&clipped);
        spans.push(Span::styled(
            clipped,
            Style::default().fg(palette.fg).bg(bg),
        ));
    }

    let profile_start_col = bar.x + used as u16;
    let cmd_seg = format!(" · {commander_badge}");
    let cmd_w = display_width(&cmd_seg);
    if used + cmd_w <= left_budget {
        spans.push(Span::styled(
            cmd_seg,
            Style::default().fg(palette.user_accent).bg(bg),
        ));
        app.profile_area = Rect::new(profile_start_col, bar.y, cmd_w as u16, 1);
        used += cmd_w;
    } else if left_budget > used + 6 {
        let clipped = truncate_width(&cmd_seg, left_budget - used);
        let cw = display_width(&clipped);
        spans.push(Span::styled(
            clipped,
            Style::default().fg(palette.user_accent).bg(bg),
        ));
        app.profile_area = Rect::new(profile_start_col, bar.y, cw as u16, 1);
        used += cw;
    } else {
        app.profile_area = Rect::default();
    }
    if let Some(live) = &mut app.live {
        live.profile_area = app.profile_area;
    }

    if let Some(gh) = gh_badge {
        let seg = format!(" · {gh}");
        let w = display_width(&seg);
        if used + w <= left_budget {
            spans.push(Span::styled(
                seg,
                Style::default().fg(palette.success).bg(bg),
            ));
            used += w;
        }
    }

    if let Some(quota) = quota_badge {
        let seg = format!(" · {quota}");
        let w = display_width(&seg);
        if used + w <= left_budget {
            spans.push(Span::styled(seg, Style::default().fg(palette.muted).bg(bg)));
        }
    }

    let left_area = Rect::new(bar.x, bar.y, left_budget as u16, 1);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
        left_area,
    );
}

fn draw_canvas(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    if area.width == 0 || area.height == 0 {
        app.transcript_area = Rect::default();
        return;
    }

    let mut content_area = area;
    if app.selected > 0 && area.height >= 4 {
        let banner_area = Rect::new(area.x, area.y, area.width, 1);
        draw_focus_banner(app, frame, banner_area, palette);
        content_area = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    }

    app.transcript_area = content_area;

    let selected_empty = app.active_transcript().entries.is_empty();
    if app.selected == 0 && app.show_welcome && selected_empty {
        draw_welcome_view(app, frame, content_area, palette);
        return;
    }

    let selected_key = app.crew[app.selected].name.clone();
    let activity = app.active_live_activity(&selected_key);
    if selected_key == specialists::COMMANDER {
        markdown::render_transcript_with_activity(
            frame,
            content_area,
            &app.transcript,
            &mut app.scroll,
            palette,
            activity.as_ref(),
        );
    } else {
        let transcript = app
            .specialist_transcripts
            .entry(selected_key.clone())
            .or_default();
        let scroll = app.specialist_scrolls.entry(selected_key).or_default();
        markdown::render_transcript_with_activity(
            frame,
            content_area,
            transcript,
            scroll,
            palette,
            activity.as_ref(),
        );
    }
}

fn draw_focus_banner(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    let member = &app.crew[app.selected];
    let bg = palette.surface_alt;
    frame.render_widget(Block::default().style(Style::default().bg(bg)), area);

    let back_label = " [Esc: Commander] ";
    let pr_label = " [p: PR] ";
    let diff_label = " [d: Diff] ";

    let back_rect = right_slice(area, area.right(), text_u16(back_label));
    let pr_rect = right_slice(area, back_rect.x, text_u16(pr_label));
    let diff_rect = right_slice(area, pr_rect.x, text_u16(diff_label));

    let branch_text = member
        .branch()
        .map(|b| format!(" · {}", b.name))
        .unwrap_or_default();
    let changes_text = member
        .changes
        .as_ref()
        .map(|c| format!(" · {}", c.totals()))
        .unwrap_or_default();
    let left_budget = diff_rect.x.saturating_sub(area.x) as usize;
    let title = truncate_width(
        &format!(
            " ▸ Focused: {} ({}{} · {}{})",
            member.name,
            member.role,
            branch_text,
            member.status.label(),
            changes_text
        ),
        left_budget,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title,
            Style::default()
                .fg(palette.accent_soft)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect::new(area.x, area.y, left_budget as u16, 1),
    );

    for (rect, label, hit, fg) in [
        (
            diff_rect,
            diff_label,
            FocusBannerHit::OpenDiff,
            palette.info,
        ),
        (pr_rect, pr_label, FocusBannerHit::OpenPr, palette.success),
        (
            back_rect,
            back_label,
            FocusBannerHit::BackToCommander,
            palette.muted,
        ),
    ] {
        if rect.width > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    Style::default().fg(fg).bg(bg),
                ))),
                rect,
            );
            app.focus_banner_hits.push((rect, hit));
        }
    }
}

fn draw_welcome_view(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    if area.width < 24 || area.height < 5 {
        return;
    }
    let card_w = area.width.saturating_sub(4).clamp(24, 72);
    let card_h = area.height.saturating_sub(2).clamp(5, 16);
    let card = Rect::new(
        area.x + (area.width.saturating_sub(card_w)) / 2,
        area.y + (area.height.saturating_sub(card_h)) / 2,
        card_w,
        card_h,
    );

    let block = Block::default()
        .title(" Welcome to Agent Command ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(palette.surface).fg(palette.fg))
        .border_style(Style::default().fg(palette.accent));
    let inner = block.inner(card);
    frame.render_widget(block, card);
    if inner.width < 8 || inner.height < 3 {
        return;
    }

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            " No authenticated CLI provider detected yet.",
            Style::default()
                .fg(palette.warning)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " Sign in with any installed CLI or run `agent-command onboard`.",
            Style::default().fg(palette.muted),
        )),
        Line::default(),
    ];

    for provider in Provider::ALL {
        let detected = app.auth_detections.iter().find(|d| d.provider == provider);
        let status = match detected {
            Some(d) if d.logged_in => ("● ready", palette.success),
            _ if crate::auth::found(provider).is_some() => {
                ("○ installed, sign in", palette.warning)
            }
            _ => ("✗ not installed", palette.muted),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("   {:<12}", provider_short_name(provider)),
                Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(status.0, Style::default().fg(status.1)),
        ]));
    }

    let gh_status = if app.github_status.authenticated {
        format!(
            "● @{}",
            app.github_status
                .login
                .as_deref()
                .unwrap_or("authenticated")
        )
    } else if app.github_status.gh_available {
        "○ installed, run `gh auth login`".to_owned()
    } else {
        "✗ gh not installed".to_owned()
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("   {:<12}", "GitHub"),
            Style::default().fg(palette.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            gh_status,
            Style::default().fg(if app.github_status.authenticated {
                palette.success
            } else {
                palette.muted
            }),
        ),
    ]));
    lines.push(Line::default());

    let btn_y = inner.y + (lines.len() as u16).min(inner.height.saturating_sub(1));
    let connect_btn = " [ Connect CLI ] ";
    let settings_btn = " [ Settings (F3) ] ";
    let dismiss_btn = " [ Dismiss ] ";

    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            connect_btn,
            Style::default()
                .fg(palette.bg)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            settings_btn,
            Style::default().fg(palette.fg).bg(palette.surface_alt),
        ),
        Span::raw("  "),
        Span::styled(
            dismiss_btn,
            Style::default().fg(palette.muted).bg(palette.surface_alt),
        ),
    ]));

    let mut cx = inner.x + 1;
    let cw = text_u16(connect_btn);
    if cx + cw <= inner.right() && btn_y < inner.bottom() {
        app.welcome_hits
            .push((Rect::new(cx, btn_y, cw, 1), WelcomeHit::OpenConnect));
    }
    cx = cx.saturating_add(cw + 2);
    let sw = text_u16(settings_btn);
    if cx + sw <= inner.right() && btn_y < inner.bottom() {
        app.welcome_hits
            .push((Rect::new(cx, btn_y, sw, 1), WelcomeHit::OpenSettings));
    }
    cx = cx.saturating_add(sw + 2);
    let dw = text_u16(dismiss_btn);
    if cx + dw <= inner.right() && btn_y < inner.bottom() {
        app.welcome_hits
            .push((Rect::new(cx, btn_y, dw, 1), WelcomeHit::DismissWelcome));
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(palette.surface)),
        inner,
    );
}

fn draw_right_rail(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    let border_fg = if app.rail_focused {
        palette.accent
    } else {
        palette.border
    };
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(border_fg))
        .style(Style::default().bg(palette.surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 8 || inner.height < 6 {
        app.crew_area = Rect::default();
        return;
    }

    let bg = palette.surface;
    let w = inner.width as usize;
    let mut y = inner.y;
    let bottom = inner.bottom();

    let worker_count = app.crew.len().saturating_sub(1);
    let header_left = format!(" WORKERS ({worker_count})");
    let toggle_label = "[Ctrl+B]";
    let toggle_w = text_u16(toggle_label);
    let toggle_rect = Rect::new(inner.right().saturating_sub(toggle_w + 1), y, toggle_w, 1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(&header_left, w.saturating_sub(usize::from(toggle_w) + 2)),
            Style::default()
                .fg(palette.accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect::new(inner.x, y, inner.width.saturating_sub(toggle_w + 1), 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            toggle_label,
            Style::default().fg(palette.subtle).bg(bg),
        ))),
        toggle_rect,
    );
    app.rail_hits.push((toggle_rect, RailHit::ToggleRail));
    y += 1;

    let workers_start_y = y;
    let max_worker_rows = ((bottom.saturating_sub(y)) / 2).clamp(2, 10) as usize;
    let rows = app.roster_rows();
    for row in rows.into_iter().take(max_worker_rows) {
        if y >= bottom.saturating_sub(1) {
            break;
        }
        let row_rect = Rect::new(inner.x, y, inner.width, 1);
        match row {
            RosterRow::Member(idx) => {
                let member = &app.crew[idx];
                let selected = idx == app.selected;
                let row_bg = if selected { palette.selection_bg } else { bg };
                let (dot, dot_fg) = match member.status {
                    Status::Running => ("●", palette.warning),
                    Status::Done => ("✓", palette.success),
                    Status::Error => ("✗", palette.danger),
                    Status::NeedsApproval => ("!", palette.warning),
                    Status::Snoozed => ("◌", palette.muted),
                    Status::Idle => ("○", palette.subtle),
                };
                let status_owned = if member.paused {
                    "PAUSED".to_owned()
                } else if member.status == Status::Running {
                    match app.running_elapsed_secs(&member.name) {
                        Some(secs) => format!("RUNNING {secs}s"),
                        None => member.status.label().to_owned(),
                    }
                } else {
                    member.status.label().to_owned()
                };
                let status_str = status_owned.as_str();
                let status_w = display_width(status_str);
                let name_budget = w.saturating_sub(status_w + 5).max(4);
                let name_text = truncate_width(&member.name, name_budget);
                let pad = w
                    .saturating_sub(3 + display_width(&name_text) + status_w + 1)
                    .max(1);
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(format!(" {dot} "), Style::default().fg(dot_fg).bg(row_bg)),
                        Span::styled(
                            name_text,
                            Style::default()
                                .fg(if selected {
                                    palette.selection_fg
                                } else {
                                    palette.fg
                                })
                                .bg(row_bg)
                                .add_modifier(if selected {
                                    Modifier::BOLD
                                } else {
                                    Modifier::empty()
                                }),
                        ),
                        Span::styled(" ".repeat(pad), Style::default().bg(row_bg)),
                        Span::styled(
                            format!("{status_str} "),
                            Style::default().fg(dot_fg).bg(row_bg),
                        ),
                    ])),
                    row_rect,
                );
                app.rail_hits.push((row_rect, RailHit::SelectMember(idx)));
                y += 1;
            }
            RosterRow::Settled { count, open } => {
                let marker = if open { "▾" } else { "▸" };
                let text = truncate_width(&format!(" {marker} Settled ({count}) [F7]"), w);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        text,
                        Style::default().fg(palette.muted).bg(bg),
                    ))),
                    row_rect,
                );
                app.rail_hits.push((row_rect, RailHit::ToggleSettled));
                y += 1;
            }
        }
    }
    app.crew_area = Rect::new(
        inner.x,
        workers_start_y,
        inner.width,
        y.saturating_sub(workers_start_y),
    );

    if y + 2 < bottom {
        y += 1;
        let changes_header_rect = Rect::new(inner.x, y, inner.width, 1);
        let changes = app.visible_changes();
        let hdr = truncate_width(&format!(" CHANGES ({})", changes.len()), w);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hdr,
                Style::default()
                    .fg(palette.accent)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ))),
            changes_header_rect,
        );
        app.rail_hits.push((changes_header_rect, RailHit::OpenDiff));
        y += 1;

        if changes.is_empty() {
            if y < bottom.saturating_sub(6) {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "   Clean working tree",
                        Style::default().fg(palette.subtle).bg(bg),
                    ))),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        } else {
            let max_change_rows = bottom.saturating_sub(y + 7).min(6) as usize;
            for file in changes.iter().take(max_change_rows.max(1)) {
                if y >= bottom.saturating_sub(6) {
                    break;
                }
                draw_change_row(frame, Rect::new(inner.x, y, inner.width, 1), file, palette);
                app.rail_hits
                    .push((Rect::new(inner.x, y, inner.width, 1), RailHit::OpenDiff));
                y += 1;
            }
        }
    }

    if y + 2 < bottom {
        y += 1;
        let conn_hdr_rect = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " CONNECTIONS",
                Style::default()
                    .fg(palette.accent)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ))),
            conn_hdr_rect,
        );
        app.rail_hits.push((conn_hdr_rect, RailHit::OpenSettings));
        y += 1;

        for provider in Provider::ALL {
            if y >= bottom.saturating_sub(2) {
                break;
            }
            let (connected, detail) = app.connection_summary(provider);
            let dot_fg = if connected {
                palette.success
            } else {
                palette.subtle
            };
            let dot = if connected { "●" } else { "○" };
            let row_rect = Rect::new(inner.x, y, inner.width, 1);
            let name = format!("{:<7}", provider_short_name(provider));
            let detail_budget = w.saturating_sub(11);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!("  {dot} "), Style::default().fg(dot_fg).bg(bg)),
                    Span::styled(name, Style::default().fg(palette.fg).bg(bg)),
                    Span::styled(
                        truncate_width(&detail, detail_budget),
                        Style::default().fg(palette.muted).bg(bg),
                    ),
                ])),
                row_rect,
            );
            app.rail_hits
                .push((row_rect, RailHit::OpenConnect(provider)));
            y += 1;
        }

        if y < bottom.saturating_sub(1) {
            let gh_ok = app.github_status.authenticated;
            let gh_dot = if gh_ok { "●" } else { "○" };
            let gh_fg = if gh_ok {
                palette.success
            } else {
                palette.subtle
            };
            let gh_detail = app
                .github_status
                .login
                .as_deref()
                .map(|login| format!("@{login}"))
                .unwrap_or_else(|| {
                    if app.github_status.gh_available {
                        "not signed in".into()
                    } else {
                        "gh missing".into()
                    }
                });
            let row_rect = Rect::new(inner.x, y, inner.width, 1);
            let detail_budget = w.saturating_sub(11);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!("  {gh_dot} "), Style::default().fg(gh_fg).bg(bg)),
                    Span::styled(
                        format!("{:<7}", "GitHub"),
                        Style::default().fg(palette.fg).bg(bg),
                    ),
                    Span::styled(
                        truncate_width(&gh_detail, detail_budget),
                        Style::default().fg(palette.muted).bg(bg),
                    ),
                ])),
                row_rect,
            );
            app.rail_hits.push((row_rect, RailHit::OpenSettings));
        }
    }

    if bottom > inner.y + 1 {
        let hint_rect = Rect::new(inner.x, bottom - 1, inner.width, 1);
        let hint = truncate_width(" [Tab] Rail · [d] Diff · [p] PR", w);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(palette.subtle).bg(bg),
            ))),
            hint_rect,
        );
        app.rail_hits.push((hint_rect, RailHit::OpenPr));
    }
}

fn draw_change_row(frame: &mut Frame, rect: Rect, file: &ChangedFile, palette: &UiPalette) {
    let bg = palette.surface;
    let w = rect.width as usize;
    let stat = format!("+{} -{}", file.added, file.removed);
    let stat_w = display_width(&stat);
    let path_budget = w.saturating_sub(stat_w + 6).max(4);
    let path_text = truncate_width(&file.path, path_budget);
    let pad = w
        .saturating_sub(4 + display_width(&path_text) + stat_w + 1)
        .max(1);
    let status_fg = match file.status.as_str() {
        "A" => palette.success,
        "D" => palette.danger,
        _ => palette.warning,
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("  {} ", file.status),
                Style::default()
                    .fg(status_fg)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(path_text, Style::default().fg(palette.fg).bg(bg)),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
            Span::styled(
                format!("{stat} "),
                Style::default().fg(palette.muted).bg(bg),
            ),
        ])),
        rect,
    );
}

fn draw_footer(app: &mut App, frame: &mut Frame, bar: Rect, palette: &UiPalette) {
    let bg = palette.surface;
    frame.render_widget(Block::default().style(Style::default().bg(bg)), bar);

    let pending_continuation = app
        .live
        .as_ref()
        .is_some_and(|live| live.pending.is_some() && !live.running(specialists::COMMANDER));

    let raw_pill = app.update_pill_text(bar.width);
    let pill_label = format!(" {raw_pill} ");
    let pill_w = text_u16(&pill_label);
    app.update_area = right_slice(bar, bar.right(), pill_w);

    let abandon_w = if pending_continuation {
        text_u16(ABANDON_LABEL)
    } else {
        0
    };
    let abandon_rect = right_slice(bar, app.update_area.x, abandon_w);
    if let Some(live) = &mut app.live {
        live.abandon_area = abandon_rect;
    }

    let left_w = abandon_rect.x.saturating_sub(bar.x) as usize;
    let mode_label = app.footer_mode_label();
    let left_text = if app.esc_armed_active() {
        format!(" {mode_label} · Press Esc again to cancel active turn")
    } else if !app.notice.is_empty() {
        format!(" {mode_label} · {}", app.notice)
    } else if pending_continuation {
        format!(" {mode_label} · Interrupted continuation pending · F4 Abandon")
    } else {
        format!(" {mode_label} · Esc×2 cancel · Ctrl+P palette · /help")
    };
    let left_fg = if app.esc_armed_active() {
        palette.warning
    } else {
        palette.muted
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(&left_text, left_w),
            Style::default().fg(left_fg).bg(bg),
        ))),
        Rect::new(bar.x, bar.y, left_w as u16, 1),
    );

    if abandon_w > 0 {
        frame.render_widget(
            Paragraph::new(ABANDON_LABEL).style(
                Style::default()
                    .fg(palette.warning)
                    .bg(palette.surface_alt)
                    .add_modifier(Modifier::BOLD),
            ),
            abandon_rect,
        );
    }

    let pill_style = match &app.update_state {
        UpdateState::Downloading { .. } => Style::default()
            .fg(palette.bg)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD),
        UpdateState::Available { .. } => Style::default()
            .fg(palette.bg)
            .bg(palette.warning)
            .add_modifier(Modifier::BOLD),
        _ if app.update_available.is_some() => Style::default()
            .fg(palette.bg)
            .bg(palette.warning)
            .add_modifier(Modifier::BOLD),
        UpdateState::Failed(_) => Style::default()
            .fg(palette.warning)
            .bg(palette.selection_bg)
            .add_modifier(Modifier::BOLD),
        UpdateState::Checking => Style::default()
            .fg(palette.accent_soft)
            .bg(palette.selection_bg)
            .add_modifier(Modifier::BOLD),
        UpdateState::Idle | UpdateState::UpToDate { .. } => Style::default()
            .fg(palette.selection_fg)
            .bg(palette.selection_bg)
            .add_modifier(Modifier::BOLD),
    };
    frame.render_widget(
        Paragraph::new(pill_label).style(pill_style),
        app.update_area,
    );
}
