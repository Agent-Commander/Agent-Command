use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::{
    inbox::{DecisionCard, DecisionReadiness},
    ledger::{LedgerEntry, LedgerFilter, TaskStatus},
    markdown::{self, UiPalette},
    specialists::Status,
    token_burn::{self, RING_TILE_HEIGHT},
    tui::{
        render::{display_width, text_u16, truncate_width},
        tabs::{
            ledger_filter_count, DecisionFilter, DecisionModalState, InboxItem, InboxKind,
            LedgerDocViewer, TabHit, ViewTab,
        },
    },
    App,
};

const INBOX_ROW_HEIGHT: u16 = 4;
const WORKSPACE_SECTION_ROWS: u16 = 5;
const LEDGER_SPLIT_MIN_WIDTH: u16 = 86;

fn styled(text: impl Into<String>, fg: Color, bg: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(fg).bg(bg))
}

fn bold(text: impl Into<String>, fg: Color, bg: Color) -> Span<'static> {
    Span::styled(
        text.into(),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}

fn put_line(frame: &mut Frame, rect: Rect, spans: Vec<Span<'static>>, bg: Color) {
    let fitted = token_burn::fit_line(spans, usize::from(rect.width));
    frame.render_widget(Paragraph::new(fitted).style(Style::default().bg(bg)), rect);
}

fn row(area: Rect, y: u16) -> Rect {
    Rect::new(area.x, y, area.width, 1)
}

pub fn tab_label(app: &App, tab: ViewTab) -> String {
    match tab {
        ViewTab::Chat => "Chat".into(),
        ViewTab::Inbox => match app.inbox_count() {
            0 => "Inbox".into(),
            count => format!("Inbox ({count})"),
        },
        ViewTab::Ledger => format!("Ledger {}", app.ledger_badge()),
    }
}

pub fn status_color(status: Status, palette: &UiPalette) -> (&'static str, Color) {
    match status {
        Status::Running => ("●", palette.warning),
        Status::Done => ("✓", palette.success),
        Status::Error => ("✗", palette.danger),
        Status::NeedsApproval => ("!", palette.warning),
        Status::Snoozed => ("◌", palette.muted),
        Status::Idle => ("○", palette.subtle),
    }
}

fn task_status_color(status: TaskStatus, palette: &UiPalette) -> Color {
    match status {
        TaskStatus::Running => palette.accent,
        TaskStatus::NeedsApproval => palette.warning,
        TaskStatus::Error => palette.danger,
        TaskStatus::Done => palette.success,
        TaskStatus::Queued | TaskStatus::Snoozed => palette.muted,
        TaskStatus::Cancelled => palette.subtle,
    }
}

pub fn draw_tab_pills(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) -> u16 {
    let mut x = area.x;
    for tab in ViewTab::ALL {
        let label = format!(" {} ", tab_label(app, tab));
        let width = text_u16(&label);
        if x + width > area.right() {
            break;
        }
        let style = if app.view_tab == tab {
            Style::default()
                .fg(palette.bg)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.fg).bg(palette.surface_alt)
        };
        let rect = Rect::new(x, area.y, width, 1);
        frame.render_widget(Paragraph::new(label).style(style), rect);
        app.left_hits.push((rect, TabHit::SelectTab(tab)));
        x += width + 1;
    }
    x.saturating_sub(area.x)
}

pub fn draw_tab_strip(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    let bg = palette.surface;
    frame.render_widget(Block::default().style(Style::default().bg(bg)), area);
    let pills = Rect::new(area.x + 1, area.y, area.width.saturating_sub(1), 1);
    let used = draw_tab_pills(app, frame, pills, palette);
    let hint = " Alt+1/2/3 · F8 panel ";
    let hint_w = text_u16(hint);
    if used + hint_w + 2 <= area.width {
        let rect = Rect::new(area.right() - hint_w, area.y, hint_w, 1);
        put_line(frame, rect, vec![styled(hint, palette.subtle, bg)], bg);
    }
}

pub fn draw_left_panel(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    let border = if app.left_focused {
        palette.accent
    } else {
        palette.border
    };
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(palette.surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 12 || inner.height < 8 {
        return;
    }
    let bottom = inner.bottom();
    let mut y = draw_view_tabs(app, frame, inner, palette);
    y = draw_usage_deck(app, frame, inner, y + 1, bottom, palette);
    y = draw_workspace_info(app, frame, inner, y + 1, bottom, palette);
    draw_thread_list(app, frame, inner, y + 1, bottom, palette);
    if bottom > inner.y + 1 {
        put_line(
            frame,
            row(inner, bottom - 1),
            vec![styled(
                " Alt+1/2/3 tabs · ⇧Tab focus",
                palette.subtle,
                palette.surface,
            )],
            palette.surface,
        );
    }
}

fn section_header(frame: &mut Frame, rect: Rect, title: &str, palette: &UiPalette) {
    put_line(
        frame,
        rect,
        vec![bold(format!(" {title}"), palette.accent, palette.surface)],
        palette.surface,
    );
}

fn draw_view_tabs(app: &mut App, frame: &mut Frame, inner: Rect, palette: &UiPalette) -> u16 {
    let bg = palette.surface;
    let toggle = "[F8 ◂]";
    let toggle_w = text_u16(toggle);
    let toggle_rect = Rect::new(
        inner.right().saturating_sub(toggle_w + 1),
        inner.y,
        toggle_w,
        1,
    );
    section_header(frame, row(inner, inner.y), "VIEWS", palette);
    frame.render_widget(
        Paragraph::new(Line::from(styled(toggle, palette.subtle, bg))),
        toggle_rect,
    );
    app.left_hits.push((toggle_rect, TabHit::ToggleLeftPanel));
    let mut y = inner.y + 1;
    for tab in ViewTab::ALL {
        let active = app.view_tab == tab;
        let row_bg = if active { palette.selection_bg } else { bg };
        let marker = if active { " ▸ " } else { "   " };
        let fg = if active {
            palette.selection_fg
        } else {
            palette.fg
        };
        let mut spans = vec![
            styled(marker, palette.accent, row_bg),
            Span::styled(
                tab.label(),
                Style::default().fg(fg).bg(row_bg).add_modifier(if active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
        ];
        spans.extend(tab_badge(app, tab, row_bg, palette));
        let rect = row(inner, y);
        frame.render_widget(Block::default().style(Style::default().bg(row_bg)), rect);
        put_line(frame, rect, spans, row_bg);
        app.left_hits.push((rect, TabHit::SelectTab(tab)));
        y += 1;
    }
    y
}

fn tab_badge(app: &App, tab: ViewTab, bg: Color, palette: &UiPalette) -> Vec<Span<'static>> {
    match tab {
        ViewTab::Chat => Vec::new(),
        ViewTab::Inbox => match app.inbox_count() {
            0 => vec![styled("  clear", palette.subtle, bg)],
            count => vec![
                Span::raw(" "),
                bold(format!(" {count} "), palette.bg, palette.warning),
            ],
        },
        ViewTab::Ledger => vec![styled(
            format!("  {}", app.ledger_badge()),
            palette.muted,
            bg,
        )],
    }
}

fn draw_usage_deck(
    app: &mut App,
    frame: &mut Frame,
    inner: Rect,
    start: u16,
    bottom: u16,
    palette: &UiPalette,
) -> u16 {
    app.pending_graphic_rings.clear();
    if start + 2 >= bottom {
        return start;
    }
    section_header(frame, row(inner, start), "USAGE", palette);
    let mut y = start + 1;
    let cards = app.usage_cards();
    if cards.is_empty() {
        put_line(
            frame,
            row(inner, y),
            vec![styled(
                "   No subscriptions yet · F3",
                palette.subtle,
                palette.surface,
            )],
            palette.surface,
        );
        return y + 1;
    }
    let reserve = WORKSPACE_SECTION_ROWS + 3;
    let deck_palette = app.deck_palette;
    let caps = app.graphics;
    for (index, card) in cards {
        if y + RING_TILE_HEIGHT + reserve > bottom && y > start + 1 {
            break;
        }
        let tile = Rect::new(
            inner.x + 1,
            y,
            inner.width.saturating_sub(1),
            RING_TILE_HEIGHT,
        );
        let mut lines =
            card.ring_tile_lines_for_caps(tile.width, palette.surface, &deck_palette, caps);
        annotate_usage_tile(app, index, &card, &mut lines, tile.width, palette);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(palette.surface)),
            tile,
        );
        if caps.use_inline_graphics() {
            app.pending_graphic_rings
                .push(card.ring_slot(tile, palette.surface, deck_palette));
        }
        app.left_hits.push((tile, TabHit::UsageTile(index)));
        y += RING_TILE_HEIGHT;
    }
    y
}

fn annotate_usage_tile(
    app: &App,
    index: usize,
    card: &token_burn::AccountCard,
    lines: &mut [Line<'static>],
    tile_width: u16,
    palette: &UiPalette,
) {
    let Some(live) = &app.live else {
        return;
    };
    let Some(profile) = live.profiles.get(index) else {
        return;
    };
    let allowance = live.allowances.get(index).copied().unwrap_or_default();
    let dual = card.ring_windows().len() > 1;
    let max_width = usize::from(tile_width);
    let bg = palette.surface;
    let Some(last) = lines.get_mut(3) else {
        return;
    };

    let has_five = token_burn::clamp_percent(allowance.five).is_some();
    let has_week = token_burn::clamp_percent(allowance.week).is_some();

    if profile.provider == crate::auth::Provider::Gemini {
        if allowance.latched {
            let label = match (dual, allowance.reset_in.is_none()) {
                (true, true) => " · reset unknown",
                (true, false) => " · latched",
                (false, true) => "reset unknown",
                (false, false) => "latched",
            };
            let mut spans = std::mem::take(&mut last.spans);
            spans.push(styled(label, palette.warning, bg));
            *last = token_burn::fit_line(spans, max_width);
            return;
        }
        if !has_five && !has_week {
            let mut spans = std::mem::take(&mut last.spans);
            spans.push(styled("no CLI quota", palette.subtle, bg));
            *last = token_burn::fit_line(spans, max_width);
            return;
        }
    }

    if allowance.latched && allowance.reset_in.is_none() {
        let mut spans = std::mem::take(&mut last.spans);
        let label = if dual {
            " · reset unknown"
        } else {
            "reset unknown"
        };
        spans.push(styled(label, palette.warning, bg));
        *last = token_burn::fit_line(spans, max_width);
        return;
    }

    if allowance.stale && (has_five || has_week) {
        let mut spans = std::mem::take(&mut last.spans);
        if dual {
            spans.push(styled(" · stale", palette.subtle, bg));
        } else {
            let label = match (has_five, has_week) {
                (false, true) => "7d · stale",
                (true, false) => "5h · stale",
                _ => "stale",
            };
            spans.push(styled(label, palette.subtle, bg));
        }
        *last = token_burn::fit_line(spans, max_width);
        return;
    }

    if !dual {
        let window_label = match (has_five, has_week) {
            (false, true) => Some("7d window"),
            (true, false) => Some("5h window"),
            _ => None,
        };
        if let Some(window_label) = window_label {
            let mut spans = std::mem::take(&mut last.spans);
            spans.push(styled(window_label, palette.subtle, bg));
            *last = token_burn::fit_line(spans, max_width);
        }
    }
}

fn draw_workspace_info(
    app: &mut App,
    frame: &mut Frame,
    inner: Rect,
    start: u16,
    bottom: u16,
    palette: &UiPalette,
) -> u16 {
    if start + 3 >= bottom {
        return start;
    }
    let bg = palette.surface;
    section_header(frame, row(inner, start), "WORKSPACE", palette);
    let branch = app.git_snapshot.branch.as_deref().map_or_else(
        || "no git branch".to_owned(),
        |branch| {
            let dirty = if app.git_snapshot.dirty { "*" } else { "" };
            format!("{branch}{dirty}")
        },
    );
    let gate = if app.settings.prod_gate {
        ("ProdGate on", palette.success)
    } else {
        ("ProdGate off", palette.warning)
    };
    let rows = [
        vec![bold(
            format!("   {}", app.workspace_display_name()),
            palette.fg,
            bg,
        )],
        vec![
            styled("   ⎇ ", palette.subtle, bg),
            styled(branch, palette.info, bg),
        ],
        vec![styled(
            format!("   {}", app.settings.commander_permission.flag()),
            palette.muted,
            bg,
        )],
        vec![styled("   ", bg, bg), styled(gate.0, gate.1, bg)],
    ];
    let mut y = start + 1;
    for spans in rows {
        if y >= bottom.saturating_sub(1) {
            break;
        }
        put_line(frame, row(inner, y), spans, bg);
        y += 1;
    }
    y
}

fn draw_thread_list(
    app: &mut App,
    frame: &mut Frame,
    inner: Rect,
    start: u16,
    bottom: u16,
    palette: &UiPalette,
) {
    if start + 2 >= bottom {
        return;
    }
    let bg = palette.surface;
    section_header(frame, row(inner, start), "THREADS", palette);
    for (y, index) in (start + 1..bottom.saturating_sub(1)).zip(0..app.crew.len()) {
        let member = &app.crew[index];
        if app.decommissioned_bots.contains(&member.name) {
            continue;
        }
        let selected = index == app.selected && app.view_tab == ViewTab::Chat;
        let row_bg = if selected { palette.selection_bg } else { bg };
        let (dot, color) = status_color(member.status, palette);
        let name = truncate_width(&member.name, usize::from(inner.width).saturating_sub(6));
        let rect = row(inner, y);
        frame.render_widget(Block::default().style(Style::default().bg(row_bg)), rect);
        put_line(
            frame,
            rect,
            vec![
                styled(format!("   {dot} "), color, row_bg),
                styled(name, palette.fg, row_bg),
            ],
            row_bg,
        );
        app.left_hits.push((rect, TabHit::JumpThread(index)));
    }
}

fn inbox_icon(item: &InboxItem, palette: &UiPalette) -> (&'static str, Color) {
    match item.kind {
        InboxKind::ProdGate { .. } => ("⛨", palette.danger),
        InboxKind::AskUser { .. } => ("?", palette.warning),
        InboxKind::Continuation => ("↺", palette.warning),
        InboxKind::Member(status) => status_color(status, palette),
        InboxKind::Wake { is_error: true } => ("✗", palette.danger),
        InboxKind::Wake { is_error: false } => ("✓", palette.success),
    }
}

fn decision_chip(card: &DecisionCard, palette: &UiPalette) -> (&'static str, Color) {
    if card.is_current_blocker && card.readiness.is_open() {
        return ("Blocker", palette.danger);
    }
    if card.rewritten_since_explain {
        return ("Rewritten", palette.info);
    }
    if card.has_draft() && card.readiness.is_open() {
        return ("Draft", palette.warning);
    }
    match card.readiness {
        DecisionReadiness::Ready => ("Ready", palette.accent),
        DecisionReadiness::NotReady => ("Prep", palette.muted),
        DecisionReadiness::Answered => ("Answered", palette.success),
        DecisionReadiness::ResolvedStale => ("Past", palette.subtle),
    }
}

fn view_header(frame: &mut Frame, rect: Rect, title: String, hint: &str, palette: &UiPalette) {
    let bg = palette.surface_alt;
    frame.render_widget(Block::default().style(Style::default().bg(bg)), rect);
    let hint_w = text_u16(hint).min(rect.width / 2);
    let title_rect = Rect::new(rect.x, rect.y, rect.width.saturating_sub(hint_w), 1);
    put_line(frame, title_rect, vec![bold(title, palette.accent, bg)], bg);
    let hint_rect = Rect::new(rect.right().saturating_sub(hint_w), rect.y, hint_w, 1);
    put_line(frame, hint_rect, vec![styled(hint, palette.subtle, bg)], bg);
}

pub fn draw_inbox_view(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    let bg = palette.bg;
    frame.render_widget(Block::default().style(Style::default().bg(bg)), area);
    if area.height < 4 || area.width < 20 {
        return;
    }
    let live_items = app.filtered_live_inbox_items();
    let decision_cards = app.filtered_decision_cards();
    let past_cards = app.past_decision_cards();
    let total_visible = live_items.len() + decision_cards.len();
    let actionable = live_items.iter().filter(|item| item.needs_action()).count()
        + decision_cards
            .iter()
            .filter(|card| card.readiness == DecisionReadiness::Ready)
            .count();
    view_header(
        frame,
        row(area, area.y),
        format!(" INBOX · {total_visible} items · {actionable} need action"),
        " f filter · / search · ⏎ open · Esc chat ",
        palette,
    );
    let modal_open = app.inbox_modal.is_some();
    draw_inbox_filter_bar(app, frame, row(area, area.y + 1), palette, !modal_open);
    let body = Rect::new(area.x, area.y + 2, area.width, area.height - 2);
    if total_visible == 0 {
        if !app.inbox_search.is_empty() && !past_cards.is_empty() {
            draw_past_section(frame, body, &past_cards, palette);
        } else if app.inbox_filter != DecisionFilter::All || !app.inbox_search.is_empty() {
            draw_empty_state(
                frame,
                body,
                "No inbox items match this filter.",
                "Press f to cycle filters or Esc to clear search.",
                palette,
            );
        } else {
            draw_empty_state(
                frame,
                body,
                "✓ Inbox zero — nothing needs your attention.",
                "Approvals, decisions, failures, and finished work land here.",
                palette,
            );
        }
    } else {
        app.inbox_selected = app.inbox_selected.min(total_visible - 1);
        let capacity = usize::from((body.height / INBOX_ROW_HEIGHT).max(1));
        let first = app.inbox_selected.saturating_sub(capacity - 1);
        let mut rendered_rows = 0usize;
        for index in first..total_visible.min(first + capacity) {
            let y = body.y + rendered_rows as u16 * INBOX_ROW_HEIGHT;
            let item_rect = Rect::new(body.x, y, body.width, INBOX_ROW_HEIGHT - 1);
            if index < live_items.len() {
                draw_inbox_item(
                    app,
                    frame,
                    item_rect,
                    index,
                    &live_items[index],
                    palette,
                    !modal_open,
                );
            } else {
                let card_idx = index - live_items.len();
                draw_decision_card_row(
                    app,
                    frame,
                    item_rect,
                    index,
                    card_idx,
                    &decision_cards[card_idx],
                    palette,
                    !modal_open,
                );
            }
            rendered_rows += 1;
        }
        let used_h = rendered_rows as u16 * INBOX_ROW_HEIGHT;
        if app.inbox_filter == DecisionFilter::All
            && !past_cards.is_empty()
            && used_h + 2 <= body.height
        {
            let past_rect = Rect::new(
                body.x,
                body.y + used_h,
                body.width,
                body.height.saturating_sub(used_h),
            );
            draw_past_section(frame, past_rect, &past_cards, palette);
        }
    }
    if modal_open {
        draw_decision_modal(app, frame, area, palette);
    }
}

fn draw_inbox_filter_bar(
    app: &mut App,
    frame: &mut Frame,
    rect: Rect,
    palette: &UiPalette,
    push_hits: bool,
) {
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg)),
        rect,
    );
    let mut x = rect.x + 1;
    for filter in DecisionFilter::ALL {
        let count = app.decision_filter_count(filter);
        let label = if count > 0 {
            format!(" {} {count} ", filter.label())
        } else {
            format!(" {} ", filter.label())
        };
        let width = text_u16(&label);
        if x + width > rect.right() {
            break;
        }
        let style = if app.inbox_filter == filter {
            Style::default()
                .fg(palette.bg)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.fg).bg(palette.surface_alt)
        };
        let pill = Rect::new(x, rect.y, width, 1);
        frame.render_widget(Paragraph::new(label).style(style), pill);
        if push_hits {
            app.left_hits.push((pill, TabHit::DecisionFilter(filter)));
        }
        x += width + 1;
    }
    let remaining = rect.right().saturating_sub(x);
    if remaining >= 10 {
        let search_text = if app.inbox_searching {
            format!(" /{}_ ", app.inbox_search)
        } else if !app.inbox_search.is_empty() {
            format!(" /{} ", app.inbox_search)
        } else {
            " / search ".into()
        };
        let search_w = text_u16(&search_text).min(remaining);
        let search_rect = Rect::new(rect.right().saturating_sub(search_w), rect.y, search_w, 1);
        let style = if app.inbox_searching {
            Style::default()
                .fg(palette.bg)
                .bg(palette.info)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.subtle).bg(palette.surface)
        };
        frame.render_widget(
            Paragraph::new(truncate_width(&search_text, usize::from(search_w))).style(style),
            search_rect,
        );
        if push_hits {
            app.left_hits.push((search_rect, TabHit::InboxSearch));
        }
    }
}

fn draw_past_section(
    frame: &mut Frame,
    rect: Rect,
    past_cards: &[DecisionCard],
    palette: &UiPalette,
) {
    if rect.height < 2 {
        return;
    }
    put_line(
        frame,
        row(rect, rect.y),
        vec![bold(
            format!(
                " Past decisions ({}) · press f for Past tab",
                past_cards.len()
            ),
            palette.subtle,
            palette.bg,
        )],
        palette.bg,
    );
    for (offset, card) in past_cards
        .iter()
        .take(usize::from(rect.height.saturating_sub(1)))
        .enumerate()
    {
        let status = card
            .closure
            .as_ref()
            .map(|c| c.source_status.as_str())
            .unwrap_or_else(|| card.readiness.label());
        put_line(
            frame,
            row(rect, rect.y + 1 + offset as u16),
            vec![
                styled(format!("   {} ", card.id), palette.subtle, palette.bg),
                styled(format!("[{status}] "), palette.muted, palette.bg),
                styled(card.title.clone(), palette.muted, palette.bg),
            ],
            palette.bg,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_decision_card_row(
    app: &mut App,
    frame: &mut Frame,
    rect: Rect,
    row_index: usize,
    card_index: usize,
    card: &DecisionCard,
    palette: &UiPalette,
    push_hits: bool,
) {
    let selected = row_index == app.inbox_selected;
    let bg = if selected {
        palette.selection_bg
    } else {
        palette.surface
    };
    frame.render_widget(Block::default().style(Style::default().bg(bg)), rect);
    let (chip_label, chip_color) = decision_chip(card, palette);
    let marker = if selected { " ▸ " } else { "   " };
    let mut top = vec![
        styled(marker, palette.accent, bg),
        bold(format!("[{chip_label}] "), chip_color, bg),
        bold(format!("{} ", card.id), palette.fg, bg),
        styled(
            format!("{} · {}", card.owner_name, card.project),
            palette.muted,
            bg,
        ),
    ];
    if !card.group.is_empty() {
        top.push(styled(format!(" / {}", card.group), palette.subtle, bg));
    }
    put_line(frame, row(rect, rect.y), top, bg);
    if push_hits {
        app.left_hits.push((rect, TabHit::DecisionRow(card_index)));
    }
    if rect.height < 2 {
        return;
    }
    put_line(
        frame,
        row(rect, rect.y + 1),
        vec![styled(
            format!("     {} — {}", card.title, card.question),
            palette.fg,
            bg,
        )],
        bg,
    );
    if rect.height < 3 {
        return;
    }
    put_line(
        frame,
        row(rect, rect.y + 2),
        vec![
            styled("     Rec: ", palette.accent, bg),
            styled(card.recommendation.clone(), palette.muted, bg),
        ],
        bg,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_inbox_item(
    app: &mut App,
    frame: &mut Frame,
    rect: Rect,
    index: usize,
    item: &InboxItem,
    palette: &UiPalette,
    push_hits: bool,
) {
    let selected = index == app.inbox_selected;
    let bg = if selected {
        palette.selection_bg
    } else {
        palette.surface
    };
    let rows = rect.height;
    frame.render_widget(Block::default().style(Style::default().bg(bg)), rect);
    let (icon, color) = inbox_icon(item, palette);
    let marker = if selected { " ▸ " } else { "   " };
    put_line(
        frame,
        row(rect, rect.y),
        vec![
            styled(marker, palette.accent, bg),
            bold(format!("{icon} "), color, bg),
            bold(item.title.clone(), palette.fg, bg),
        ],
        bg,
    );
    if push_hits {
        app.left_hits
            .push((row(rect, rect.y), TabHit::InboxRow(index)));
    }
    if rows < 2 {
        return;
    }
    put_line(
        frame,
        row(rect, rect.y + 1),
        vec![styled(format!("     {}", item.detail), palette.muted, bg)],
        bg,
    );
    if push_hits {
        app.left_hits
            .push((row(rect, rect.y + 1), TabHit::InboxRow(index)));
    }
    if rows < 3 {
        return;
    }
    let mut x = rect.x + 4;
    for (action, label) in item.actions() {
        let text = format!(" {} {label} ", action.key_hint());
        let width = text_u16(&text);
        if x + width > rect.right() {
            break;
        }
        let button = Rect::new(x, rect.y + 2, width, 1);
        frame.render_widget(
            Paragraph::new(text).style(
                Style::default()
                    .fg(palette.fg)
                    .bg(palette.surface_alt)
                    .add_modifier(Modifier::BOLD),
            ),
            button,
        );
        if push_hits {
            app.left_hits
                .push((button, TabHit::InboxAction(index, *action)));
        }
        x += width + 1;
    }
}

pub fn draw_decision_modal(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    let Some(modal) = app.inbox_modal.clone() else {
        return;
    };
    let Some(card) = app
        .decision_cards
        .iter()
        .find(|entry| entry.id == modal.card_id)
        .cloned()
    else {
        return;
    };
    let modal_w = area.width.saturating_sub(4).clamp(32, 92).min(area.width);
    let modal_h = area.height.saturating_sub(2).clamp(12, 32).min(area.height);
    let modal_rect = Rect::new(
        area.x + area.width.saturating_sub(modal_w) / 2,
        area.y + area.height.saturating_sub(modal_h) / 2,
        modal_w,
        modal_h,
    );
    frame.render_widget(Clear, modal_rect);
    let block = Block::default()
        .title(format!(
            " {} · {} · {} ",
            card.id,
            card.owner_name,
            card.readiness.label()
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette.accent))
        .style(Style::default().bg(palette.surface));
    let inner = block.inner(modal_rect);
    frame.render_widget(block, modal_rect);
    if inner.width < 12 || inner.height < 6 {
        return;
    }

    let mut y = inner.y;
    let bottom = inner.bottom();
    let footer_rows: u16 = 2;
    let body_bottom = bottom.saturating_sub(footer_rows);

    if let Some(banner) = &modal.conflict_banner {
        if y < body_bottom {
            let banner_rect = row(inner, y);
            put_line(
                frame,
                banner_rect,
                vec![
                    bold(" CONFLICT: ", palette.bg, palette.danger),
                    styled(
                        format!(" {banner} [Ctrl+R reconcile]"),
                        palette.warning,
                        palette.surface,
                    ),
                ],
                palette.surface,
            );
            app.left_hits
                .push((banner_rect, TabHit::DecisionModalReconcile));
            y += 1;
        }
    }

    let options_rows = card.alternatives.len().min(4) as u16;
    let info_bottom = body_bottom.saturating_sub(options_rows + 2);
    if info_bottom > y {
        let info_rect = Rect::new(inner.x, y, inner.width, info_bottom - y);
        let mut lines = vec![
            Line::from(bold(card.question.clone(), palette.fg, palette.surface)),
            Line::from(vec![
                bold("Where things stand: ", palette.accent, palette.surface),
                styled(card.current_state.clone(), palette.fg, palette.surface),
            ]),
            Line::from(vec![
                bold("Why this matters: ", palette.accent, palette.surface),
                styled(card.why_it_matters.clone(), palette.muted, palette.surface),
            ]),
            Line::from(vec![
                bold("Owner recommendation: ", palette.success, palette.surface),
                styled(card.recommendation.clone(), palette.fg, palette.surface),
            ]),
        ];
        if let Some(constraints) = &card.constraints {
            lines.push(Line::from(vec![
                bold("Constraints: ", palette.warning, palette.surface),
                styled(constraints.clone(), palette.muted, palette.surface),
            ]));
        }
        if let Some(detail) = &card.detail {
            lines.push(Line::from(vec![
                bold("Notes: ", palette.subtle, palette.surface),
                styled(detail.clone(), palette.muted, palette.surface),
            ]));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((modal.scroll, 0))
                .style(Style::default().bg(palette.surface)),
            info_rect,
        );
        y = info_bottom;
    }

    for (idx, option) in card.alternatives.iter().take(4).enumerate() {
        if y >= body_bottom {
            break;
        }
        let chosen = modal.selected_option == Some(idx);
        let row_bg = if chosen {
            palette.selection_bg
        } else {
            palette.surface_alt
        };
        let bullet = if chosen { "(●)" } else { "( )" };
        let opt_rect = row(inner, y);
        put_line(
            frame,
            opt_rect,
            vec![
                bold(format!(" {bullet} [{}] ", idx + 1), palette.accent, row_bg),
                styled(option.label.clone(), palette.fg, row_bg),
            ],
            row_bg,
        );
        app.left_hits
            .push((opt_rect, TabHit::DecisionModalOption(idx)));
        y += 1;
    }

    if y < body_bottom {
        draw_modal_input_line(frame, row(inner, y), &modal, palette);
    }

    draw_modal_actions(app, frame, row(inner, bottom.saturating_sub(1)), palette);
}

fn draw_modal_input_line(
    frame: &mut Frame,
    rect: Rect,
    modal: &DecisionModalState,
    palette: &UiPalette,
) {
    let (label, value, accent) = if modal.explain_mode {
        (
            " What is unclear?: ",
            modal.explain_note.as_str(),
            palette.warning,
        )
    } else {
        (" Write-in: ", modal.write_in.as_str(), palette.info)
    };
    let bg = palette.surface_alt;
    put_line(
        frame,
        rect,
        vec![
            bold(label, accent, bg),
            styled(format!("{value}_"), palette.fg, bg),
        ],
        bg,
    );
}

fn draw_modal_actions(app: &mut App, frame: &mut Frame, rect: Rect, palette: &UiPalette) {
    let buttons = [
        (
            " ⏎ Submit ",
            TabHit::DecisionModalSubmit,
            palette.accent,
            palette.bg,
        ),
        (
            " ^S Draft ",
            TabHit::DecisionModalSaveDraft,
            palette.surface_alt,
            palette.fg,
        ),
        (
            " ^E Explain ",
            TabHit::DecisionModalExplain,
            palette.surface_alt,
            palette.warning,
        ),
        (
            " ^X Retire ",
            TabHit::DecisionModalRetire,
            palette.surface_alt,
            palette.muted,
        ),
        (
            " Esc Close ",
            TabHit::DecisionModalClose,
            palette.surface_alt,
            palette.subtle,
        ),
    ];
    let mut x = rect.x;
    for (label, hit, bg, fg) in buttons {
        let width = text_u16(label);
        if x + width > rect.right() {
            break;
        }
        let button_rect = Rect::new(x, rect.y, width, 1);
        frame.render_widget(
            Paragraph::new(label)
                .style(Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)),
            button_rect,
        );
        app.left_hits.push((button_rect, hit));
        x += width + 1;
    }
}

fn draw_empty_state(frame: &mut Frame, body: Rect, title: &str, hint: &str, palette: &UiPalette) {
    if body.height == 0 {
        return;
    }
    let y = body.y + body.height / 3;
    for (offset, (text, color)) in [(title, palette.success), (hint, palette.muted)]
        .into_iter()
        .enumerate()
    {
        let line_y = y + offset as u16;
        if line_y >= body.bottom() {
            break;
        }
        let text = truncate_width(text, usize::from(body.width).saturating_sub(2));
        let pad = (usize::from(body.width).saturating_sub(display_width(&text))) / 2;
        put_line(
            frame,
            row(body, line_y),
            vec![styled(
                format!("{}{text}", " ".repeat(pad)),
                color,
                palette.bg,
            )],
            palette.bg,
        );
    }
}

pub fn draw_ledger_view(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    let bg = palette.bg;
    frame.render_widget(Block::default().style(Style::default().bg(bg)), area);
    if area.height < 4 || area.width < 24 {
        return;
    }
    view_header(
        frame,
        row(area, area.y),
        format!(
            " LEDGER · {} active · {} tracked",
            app.ledger.active_count(),
            app.ledger.total()
        ),
        " f filter · ⏎ doc · g thread · d diff ",
        palette,
    );
    draw_filter_pills(app, frame, row(area, area.y + 1), palette);
    let rows = app.ledger_rows();
    let body = Rect::new(area.x, area.y + 2, area.width, area.height - 2);
    if rows.is_empty() {
        let title = if app.ledger.total() == 0 {
            "No tracked tasks yet."
        } else {
            "No tasks match this filter."
        };
        draw_empty_state(
            frame,
            body,
            title,
            "Message Commander, /spawn, /goal or /review to start tracked work.",
            palette,
        );
        return;
    }
    app.ledger_selected = app.ledger_selected.min(rows.len() - 1);
    if let Some(viewer) = app.ledger_viewer.clone() {
        if body.width >= LEDGER_SPLIT_MIN_WIDTH {
            let panes =
                Layout::horizontal([Constraint::Percentage(44), Constraint::Percentage(56)])
                    .split(body);
            draw_ledger_list(app, frame, panes[0], &rows, palette);
            draw_ledger_doc_pane(app, frame, panes[1], &viewer, palette);
        } else {
            draw_ledger_doc_pane(app, frame, body, &viewer, palette);
        }
        return;
    }
    draw_ledger_list(app, frame, body, &rows, palette);
}

fn draw_ledger_list(
    app: &mut App,
    frame: &mut Frame,
    body: Rect,
    rows: &[LedgerEntry],
    palette: &UiPalette,
) {
    let bg = palette.bg;
    let columns = LedgerColumns::for_width(body.width);
    put_line(frame, row(body, body.y), columns.header(palette), bg);
    let capacity = usize::from(body.height.saturating_sub(1).max(1));
    let first = app.ledger_selected.saturating_sub(capacity - 1);
    for (offset, entry) in rows.iter().enumerate().skip(first).take(capacity) {
        let y = body.y + 1 + (offset - first) as u16;
        let selected = offset == app.ledger_selected;
        let row_bg = if selected { palette.selection_bg } else { bg };
        let rect = row(body, y);
        frame.render_widget(Block::default().style(Style::default().bg(row_bg)), rect);
        put_line(
            frame,
            rect,
            columns.entry(entry, selected, row_bg, palette),
            row_bg,
        );
        app.left_hits.push((rect, TabHit::LedgerRow(offset)));
    }
}

fn draw_ledger_doc_pane(
    app: &mut App,
    frame: &mut Frame,
    area: Rect,
    viewer: &LedgerDocViewer,
    palette: &UiPalette,
) {
    let block = Block::default()
        .title(format!(
            " {} · {} ",
            viewer.entry_id,
            truncate_width(&viewer.title, 40)
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette.accent))
        .style(Style::default().bg(palette.surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 12 || inner.height < 3 {
        return;
    }
    let content_h = inner.height.saturating_sub(1);
    let content_rect = Rect::new(inner.x, inner.y, inner.width, content_h);
    let lines = markdown::render_markdown(&viewer.markdown, usize::from(inner.width), palette);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((viewer.scroll, 0))
            .style(Style::default().bg(palette.surface)),
        content_rect,
    );
    let action_bar = row(inner, inner.bottom().saturating_sub(1));
    let buttons = [
        (
            " g Jump to thread ",
            TabHit::LedgerDocJump,
            palette.accent,
            palette.bg,
        ),
        (
            " d Diff ",
            TabHit::LedgerDocDiff,
            palette.surface_alt,
            palette.fg,
        ),
        (
            " Esc Back ",
            TabHit::LedgerDocClose,
            palette.surface_alt,
            palette.subtle,
        ),
    ];
    let mut x = action_bar.x;
    for (label, hit, bg, fg) in buttons {
        let width = text_u16(label);
        if x + width > action_bar.right() {
            break;
        }
        let rect = Rect::new(x, action_bar.y, width, 1);
        frame.render_widget(
            Paragraph::new(label)
                .style(Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)),
            rect,
        );
        app.left_hits.push((rect, hit));
        x += width + 1;
    }
}

fn draw_filter_pills(app: &mut App, frame: &mut Frame, rect: Rect, palette: &UiPalette) {
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg)),
        rect,
    );
    let mut x = rect.x + 1;
    for filter in LedgerFilter::ALL {
        let count = ledger_filter_count(app.ledger_entries(), filter);
        let label = format!(" {} {count} ", filter.label());
        let width = text_u16(&label);
        if x + width > rect.right() {
            break;
        }
        let style = if app.ledger_filter == filter {
            Style::default()
                .fg(palette.bg)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.fg).bg(palette.surface_alt)
        };
        let pill = Rect::new(x, rect.y, width, 1);
        frame.render_widget(Paragraph::new(label).style(style), pill);
        app.left_hits.push((pill, TabHit::LedgerFilter(filter)));
        x += width + 1;
    }
}

struct LedgerColumns {
    summary: usize,
    show_updated: bool,
}

const ID_W: usize = 6;
const STATUS_W: usize = 15;
const OWNER_W: usize = 10;
const UPDATED_W: usize = 8;
const SUMMARY_MIN_W: usize = 18;

impl LedgerColumns {
    fn for_width(width: u16) -> Self {
        let base = 2 + ID_W + STATUS_W + OWNER_W;
        let width = usize::from(width);
        let show_updated = width >= base + UPDATED_W + SUMMARY_MIN_W;
        let fixed = base + if show_updated { UPDATED_W } else { 0 };
        Self {
            summary: width.saturating_sub(fixed).max(SUMMARY_MIN_W),
            show_updated,
        }
    }

    fn cell(text: &str, width: usize) -> String {
        let clipped = truncate_width(text, width.saturating_sub(1));
        let pad = width.saturating_sub(display_width(&clipped));
        format!("{clipped}{}", " ".repeat(pad))
    }

    fn header(&self, palette: &UiPalette) -> Vec<Span<'static>> {
        let bg = palette.bg;
        let mut text = format!(
            "  {}{}{}{}",
            Self::cell("ID", ID_W),
            Self::cell("STATUS", STATUS_W),
            Self::cell("OWNER", OWNER_W),
            Self::cell("SUMMARY (⏎ DOCUMENT)", self.summary)
        );
        if self.show_updated {
            text.push_str(&Self::cell("UPDATED", UPDATED_W));
        }
        vec![bold(text, palette.subtle, bg)]
    }

    fn entry(
        &self,
        entry: &LedgerEntry,
        selected: bool,
        bg: Color,
        palette: &UiPalette,
    ) -> Vec<Span<'static>> {
        let fg = if selected {
            palette.selection_fg
        } else {
            palette.fg
        };
        let marker = if selected { "▸ " } else { "  " };
        let mut spans = vec![
            styled(marker, palette.accent, bg),
            styled(Self::cell(&entry.id, ID_W), palette.muted, bg),
            bold(
                Self::cell(entry.status.label(), STATUS_W),
                task_status_color(entry.status, palette),
                bg,
            ),
            styled(Self::cell(&entry.owner, OWNER_W), palette.user_accent, bg),
            styled(Self::cell(entry.display_summary(), self.summary), fg, bg),
        ];
        if self.show_updated {
            spans.push(styled(updated_label(entry.updated_at), palette.muted, bg));
        }
        spans
    }
}

fn updated_label(epoch: i64) -> String {
    let Some(stamp) = chrono::DateTime::from_timestamp(epoch, 0) else {
        return "-".into();
    };
    let local = stamp.with_timezone(&chrono::Local);
    if local.date_naive() == chrono::Local::now().date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%m-%d").to_string()
    }
}
