use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{
    ledger::{LedgerEntry, LedgerFilter, TaskStatus},
    markdown::UiPalette,
    specialists::Status,
    token_burn::{self, AccountCard, RING_ROWS, RING_TILE_MIN_WIDTH},
    tui::render::{display_width, right_slice, text_u16, truncate_width},
    tui::views::{ActiveTab, InboxAction, InboxItem, InboxKind, TabHit},
    App,
};

#[cfg(test)]
mod tests;

const TILE_HEIGHT: u16 = RING_ROWS + 1;
const WORKSPACE_RESERVE: u16 = 8;
const INBOX_ROW_HEIGHT: u16 = 2;

fn line(text: impl Into<String>, style: Style) -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(text.into(), style)))
}

fn header_style(palette: &UiPalette, bg: Color) -> Style {
    Style::default()
        .fg(palette.accent)
        .bg(bg)
        .add_modifier(Modifier::BOLD)
}

pub fn tab_label(app: &App, tab: ActiveTab) -> String {
    match tab {
        ActiveTab::Chat => "Chat".into(),
        ActiveTab::Inbox => match app.inbox_count() {
            0 => "Inbox".into(),
            count => format!("Inbox ({count})"),
        },
        ActiveTab::Ledger => format!(
            "Ledger {}/{}",
            app.ledger().active_count(),
            app.ledger().entries().len()
        ),
    }
}

pub fn status_dot(status: Status, palette: &UiPalette) -> (&'static str, Color) {
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
        TaskStatus::Running | TaskStatus::NeedsApproval => palette.warning,
        TaskStatus::Done => palette.success,
        TaskStatus::Error => palette.danger,
        TaskStatus::Snoozed | TaskStatus::Cancelled => palette.muted,
        TaskStatus::Queued => palette.info,
    }
}

pub fn tab_pills_width(app: &App) -> u16 {
    ActiveTab::ALL
        .into_iter()
        .map(|tab| text_u16(&tab_label(app, tab)) + 3)
        .sum::<u16>()
        .saturating_sub(1)
}

pub fn draw_tab_pills(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) -> u16 {
    let mut x = area.x;
    for tab in ActiveTab::ALL {
        let label = format!(" {} ", tab_label(app, tab));
        let width = text_u16(&label);
        if x + width > area.right() {
            break;
        }
        let active = app.active_tab() == tab;
        let style = if active {
            Style::default()
                .fg(palette.bg)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.fg).bg(palette.surface_alt)
        };
        let rect = Rect::new(x, area.y, width, 1);
        frame.render_widget(line(label, style), rect);
        app.views.hits.push((rect, TabHit::Tab(tab)));
        x += width + 1;
    }
    x.saturating_sub(area.x)
}

pub fn compact_usage_span(app: &App) -> Option<Span<'static>> {
    let (_, remaining) = app.headline_allowance()?;
    Some(token_burn::compact_ring_span(remaining, &app.deck_palette))
}

pub fn draw_left_panel(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    let border_fg = if app.left_panel_focused() {
        palette.accent
    } else {
        palette.border
    };
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(border_fg))
        .style(Style::default().bg(palette.surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 12 || inner.height < 6 {
        return;
    }
    let mut y = inner.y;
    y = draw_views_section(app, frame, inner, y, palette);
    y = draw_usage_section(app, frame, inner, y + 1, palette);
    draw_workspace_section(app, frame, inner, y + 1, palette);
}

fn draw_views_section(
    app: &mut App,
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    palette: &UiPalette,
) -> u16 {
    let bg = palette.surface;
    let toggle_label = "[F8 ◂]";
    let toggle_rect = right_slice(
        Rect::new(inner.x, y, inner.width, 1),
        inner.right(),
        text_u16(toggle_label),
    );
    frame.render_widget(
        line(" VIEWS", header_style(palette, bg)),
        Rect::new(inner.x, y, inner.width, 1),
    );
    frame.render_widget(
        line(toggle_label, Style::default().fg(palette.subtle).bg(bg)),
        toggle_rect,
    );
    app.views.hits.push((toggle_rect, TabHit::ToggleLeftPanel));
    let mut y = y + 1;
    for (tab, key) in ActiveTab::ALL.into_iter().zip(["Alt+1", "Alt+2", "Alt+3"]) {
        if y >= inner.bottom() {
            break;
        }
        let active = app.active_tab() == tab;
        let row_bg = if active { palette.selection_bg } else { bg };
        let marker = if active { "▸" } else { " " };
        let label = format!(" {marker} {}", tab_label(app, tab));
        let width = usize::from(inner.width);
        let pad = width.saturating_sub(display_width(&label) + display_width(key) + 1);
        let label_fg = match tab {
            ActiveTab::Inbox if app.inbox_count() > 0 => palette.warning,
            _ if active => palette.selection_fg,
            _ => palette.fg,
        };
        let rect = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    label,
                    Style::default()
                        .fg(label_fg)
                        .bg(row_bg)
                        .add_modifier(if active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(" ".repeat(pad), Style::default().bg(row_bg)),
                Span::styled(
                    format!("{key} "),
                    Style::default().fg(palette.subtle).bg(row_bg),
                ),
            ])),
            rect,
        );
        app.views.hits.push((rect, TabHit::Tab(tab)));
        y += 1;
    }
    y
}

fn draw_usage_section(
    app: &mut App,
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    palette: &UiPalette,
) -> u16 {
    let bg = palette.surface;
    if y >= inner.bottom() {
        return y;
    }
    frame.render_widget(
        line(" USAGE", header_style(palette, bg)),
        Rect::new(inner.x, y, inner.width, 1),
    );
    let mut y = y + 1;
    let cards = app.usage_cards();
    if cards.is_empty() {
        if y < inner.bottom() {
            let rect = Rect::new(inner.x, y, inner.width, 1);
            frame.render_widget(
                line(
                    truncate_width("  No subscription connected · F3", usize::from(inner.width)),
                    Style::default().fg(palette.subtle).bg(bg),
                ),
                rect,
            );
            app.views.hits.push((rect, TabHit::UsageTile(usize::MAX)));
            y += 1;
        }
        return y;
    }
    let budget = inner.bottom().saturating_sub(y + WORKSPACE_RESERVE);
    let tile_capacity = if inner.width >= RING_TILE_MIN_WIDTH {
        usize::from(budget / TILE_HEIGHT)
    } else {
        0
    };
    for (position, (profile, card)) in cards.iter().enumerate() {
        if position < tile_capacity {
            y = draw_ring_tile(app, frame, inner, y, (*profile, card));
        } else if y < inner.bottom().saturating_sub(WORKSPACE_RESERVE.min(3)) {
            y = draw_compact_usage_row(app, frame, inner, y, (*profile, card));
        }
    }
    y
}

fn draw_ring_tile(
    app: &mut App,
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    (profile, card): (usize, &AccountCard),
) -> u16 {
    let deck = &app.deck_palette;
    let width = inner.width.saturating_sub(1);
    let rect = Rect::new(
        inner.x + 1,
        y,
        width,
        RING_ROWS.min(inner.bottom().saturating_sub(y)),
    );
    frame.render_widget(
        Paragraph::new(card.ring_tile_lines(width, deck))
            .style(Style::default().bg(app.ui_palette.surface)),
        rect,
    );
    app.views.hits.push((rect, TabHit::UsageTile(profile)));
    y + TILE_HEIGHT
}

fn draw_compact_usage_row(
    app: &mut App,
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    (profile, card): (usize, &AccountCard),
) -> u16 {
    let deck = &app.deck_palette;
    let bg = app.ui_palette.surface;
    let rect = Rect::new(inner.x, y, inner.width, 1);
    let headline = card.ring_layout().headline();
    let spans = vec![
        Span::raw(" "),
        token_burn::compact_ring_span(headline, deck),
        Span::raw(" "),
        Span::styled(
            format!("{} · {}", card.provider, card.profile),
            Style::default().fg(deck.foreground),
        ),
    ];
    frame.render_widget(
        Paragraph::new(token_burn::fit_line(spans, usize::from(inner.width)))
            .style(Style::default().bg(bg)),
        rect,
    );
    app.views.hits.push((rect, TabHit::UsageTile(profile)));
    y + 1
}

fn draw_workspace_section(
    app: &mut App,
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    palette: &UiPalette,
) {
    let bg = palette.surface;
    let width = usize::from(inner.width);
    if y + 1 >= inner.bottom() {
        return;
    }
    frame.render_widget(
        line(" WORKSPACE", header_style(palette, bg)),
        Rect::new(inner.x, y, inner.width, 1),
    );
    let mut y = y + 1;
    let branch = app
        .git_snapshot
        .branch
        .as_deref()
        .map(|branch| {
            let dirty = if app.git_snapshot.dirty { "*" } else { "" };
            format!(" ⎇ {branch}{dirty}")
        })
        .unwrap_or_else(|| " ⎇ no git branch".into());
    let rows = [
        (format!(" ▣ {}", app.workspace_display_name()), palette.fg),
        (
            branch,
            if app.git_snapshot.dirty {
                palette.warning
            } else {
                palette.muted
            },
        ),
        (
            format!(" ⛨ {}", app.settings.commander_permission.flag()),
            palette.muted,
        ),
        if app.settings.prod_gate {
            (" ⛊ ProdGate on".to_owned(), palette.success)
        } else {
            (" ⛊ ProdGate off".to_owned(), palette.warning)
        },
    ];
    for (text, fg) in rows {
        if y >= inner.bottom() {
            return;
        }
        frame.render_widget(
            line(truncate_width(&text, width), Style::default().fg(fg).bg(bg)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }
    if y + 1 >= inner.bottom() {
        return;
    }
    y += 1;
    frame.render_widget(
        line(" THREADS", header_style(palette, bg)),
        Rect::new(inner.x, y, inner.width, 1),
    );
    y += 1;
    let threads: Vec<(usize, String, Status)> = app
        .crew
        .iter()
        .enumerate()
        .filter(|(index, member)| *index == 0 || !member.settled)
        .map(|(index, member)| (index, member.name.clone(), member.status))
        .collect();
    for (index, name, status) in threads {
        if y >= inner.bottom() {
            break;
        }
        let selected = index == app.selected && app.active_tab() == ActiveTab::Chat;
        let row_bg = if selected { palette.selection_bg } else { bg };
        let running = app.live.as_ref().is_some_and(|live| live.running(&name));
        let (dot, dot_fg) = status_dot(if running { Status::Running } else { status }, palette);
        let rect = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {dot} "), Style::default().fg(dot_fg).bg(row_bg)),
                Span::styled(
                    truncate_width(&name, width.saturating_sub(4)),
                    Style::default()
                        .fg(if selected {
                            palette.selection_fg
                        } else {
                            palette.fg
                        })
                        .bg(row_bg),
                ),
            ]))
            .style(Style::default().bg(row_bg)),
            rect,
        );
        app.views.hits.push((rect, TabHit::Thread(index)));
        y += 1;
    }
}

pub fn draw_inbox_view(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    let items = app.inbox_items();
    let title = match items.len() {
        0 => " INBOX · all clear".to_owned(),
        count => format!(" INBOX · {count} need attention"),
    };
    draw_view_header(
        frame,
        area,
        &title,
        " ↑↓ select · Enter open · a approve · x dismiss · Esc chat",
        palette,
    );
    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1),
    );
    if items.is_empty() {
        draw_empty_state(
            frame,
            body,
            "✓ Inbox zero — nothing needs your attention.",
            "Approvals, interrupted turns and finished specialists land here.",
            palette,
        );
        return;
    }
    let visible = usize::from(body.height / INBOX_ROW_HEIGHT).max(1);
    let selected = app.view_selection().min(items.len() - 1);
    let offset = (selected + 1).saturating_sub(visible);
    for (row, (index, item)) in items
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
        let y = body.y + row as u16 * INBOX_ROW_HEIGHT;
        let is_selected = index == selected;
        draw_inbox_row(
            app,
            frame,
            Rect::new(body.x, y, body.width, INBOX_ROW_HEIGHT),
            (index, item),
            is_selected,
            palette,
        );
    }
}

fn inbox_icon(item: &InboxItem, palette: &UiPalette) -> (&'static str, Color) {
    match &item.kind {
        InboxKind::Approval { .. } => ("⛨", palette.warning),
        InboxKind::Continuation => ("↺", palette.warning),
        InboxKind::Specialist(status) => status_dot(*status, palette),
        InboxKind::Wake { is_error: true } => ("✗", palette.danger),
        InboxKind::Wake { is_error: false } => ("✉", palette.info),
    }
}

fn draw_inbox_row(
    app: &mut App,
    frame: &mut Frame,
    rect: Rect,
    (index, item): (usize, &InboxItem),
    selected: bool,
    palette: &UiPalette,
) {
    let width = usize::from(rect.width);
    let row_bg = if selected {
        palette.selection_bg
    } else {
        palette.bg
    };
    frame.render_widget(Block::default().style(Style::default().bg(row_bg)), rect);
    let mut right = rect.right();
    let mut action_hits = Vec::new();
    for action in item.actions().iter().rev() {
        let label = format!(" {} ", action.label());
        let action_rect = right_slice(rect, right, text_u16(&label));
        if action_rect.x <= rect.x + 24 {
            break;
        }
        let style = match action {
            InboxAction::Approve | InboxAction::Resume => Style::default()
                .fg(palette.bg)
                .bg(palette.success)
                .add_modifier(Modifier::BOLD),
            InboxAction::Deny | InboxAction::Abandon => Style::default()
                .fg(palette.bg)
                .bg(palette.danger)
                .add_modifier(Modifier::BOLD),
            _ => Style::default().fg(palette.fg).bg(palette.surface_alt),
        };
        let action_rect = Rect::new(action_rect.x, rect.y, action_rect.width, 1);
        frame.render_widget(line(label, style), action_rect);
        action_hits.push((action_rect, TabHit::InboxAction(index, *action)));
        right = action_rect.x.saturating_sub(1);
    }
    let (icon, icon_fg) = inbox_icon(item, palette);
    let marker = if selected { "▸" } else { " " };
    let title_budget = usize::from(right.saturating_sub(rect.x)).saturating_sub(6);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                Style::default().fg(palette.accent).bg(row_bg),
            ),
            Span::styled(format!("{icon} "), Style::default().fg(icon_fg).bg(row_bg)),
            Span::styled(
                truncate_width(&item.title, title_budget),
                Style::default()
                    .fg(palette.fg)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        Rect::new(rect.x, rect.y, right.saturating_sub(rect.x), 1),
    );
    if rect.height > 1 {
        frame.render_widget(
            line(
                truncate_width(&format!("     {}", item.detail.replace('\n', " ")), width),
                Style::default().fg(palette.muted).bg(row_bg),
            ),
            Rect::new(rect.x, rect.y + 1, rect.width, 1),
        );
    }
    app.views.hits.extend(action_hits);
    app.views.hits.push((rect, TabHit::InboxRow(index)));
}

fn draw_view_header(frame: &mut Frame, area: Rect, title: &str, hint: &str, palette: &UiPalette) {
    let bg = palette.surface_alt;
    let rect = Rect::new(area.x, area.y, area.width, 1);
    frame.render_widget(Block::default().style(Style::default().bg(bg)), rect);
    let width = usize::from(area.width);
    let title_w = display_width(title);
    frame.render_widget(
        line(truncate_width(title, width), header_style(palette, bg)),
        rect,
    );
    let hint_room = width.saturating_sub(title_w + 2);
    if hint_room > 12 {
        let hint = truncate_width(hint, hint_room);
        let hint_rect = right_slice(rect, rect.right(), text_u16(&hint));
        frame.render_widget(
            line(hint, Style::default().fg(palette.subtle).bg(bg)),
            hint_rect,
        );
    }
}

fn draw_empty_state(
    frame: &mut Frame,
    area: Rect,
    headline: &str,
    detail: &str,
    palette: &UiPalette,
) {
    if area.height < 2 {
        return;
    }
    let width = usize::from(area.width);
    let y = area.y + area.height.saturating_sub(2) / 2;
    for (offset, (text, style)) in [
        (
            headline,
            Style::default()
                .fg(palette.success)
                .add_modifier(Modifier::BOLD),
        ),
        (detail, Style::default().fg(palette.muted)),
    ]
    .into_iter()
    .enumerate()
    {
        let text = truncate_width(text, width.saturating_sub(2));
        let w = text_u16(&text);
        let x = area.x + area.width.saturating_sub(w) / 2;
        frame.render_widget(
            line(text, style.bg(palette.bg)),
            Rect::new(x, y + offset as u16, w, 1),
        );
    }
}

pub fn relative_age(updated_at: i64, now: i64) -> String {
    let seconds = (now - updated_at).max(0);
    match seconds {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

pub fn draw_ledger_view(app: &mut App, frame: &mut Frame, area: Rect, palette: &UiPalette) {
    let active = app.ledger().active_count();
    let total = app.ledger().entries().len();
    draw_view_header(
        frame,
        area,
        &format!(" LEDGER · {active} active / {total} total"),
        " ↑↓ select · Enter thread · d diff · f filter · Esc chat",
        palette,
    );
    if area.height < 3 {
        return;
    }
    draw_filter_pills(
        app,
        frame,
        Rect::new(area.x, area.y + 1, area.width, 1),
        palette,
    );
    let rows = app.ledger_rows();
    let body = Rect::new(
        area.x,
        area.y + 2,
        area.width,
        area.height.saturating_sub(2),
    );
    if rows.is_empty() {
        let headline = if total == 0 {
            "No tracked tasks yet.".to_owned()
        } else {
            format!("No tasks match {}.", app.ledger_filter().label())
        };
        draw_empty_state(
            frame,
            body,
            &headline,
            "Commander missions, /spawn, /goal and /review rounds appear here automatically.",
            palette,
        );
        return;
    }
    let columns = LedgerColumns::for_width(body.width);
    frame.render_widget(
        line(
            columns.header(),
            Style::default()
                .fg(palette.subtle)
                .bg(palette.bg)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(body.x, body.y, body.width, 1),
    );
    let list = Rect::new(
        body.x,
        body.y + 1,
        body.width,
        body.height.saturating_sub(1),
    );
    let visible = usize::from(list.height).max(1);
    let selected = app.view_selection().min(rows.len() - 1);
    let offset = (selected + 1).saturating_sub(visible);
    let now = chrono::Utc::now().timestamp();
    for (row, (index, entry)) in rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
        let rect = Rect::new(list.x, list.y + row as u16, list.width, 1);
        let row_bg = if index == selected {
            palette.selection_bg
        } else {
            palette.bg
        };
        frame.render_widget(
            Paragraph::new(columns.row(entry, now, palette, row_bg))
                .style(Style::default().bg(row_bg)),
            rect,
        );
        app.views.hits.push((rect, TabHit::LedgerRow(index)));
    }
}

fn draw_filter_pills(app: &mut App, frame: &mut Frame, rect: Rect, palette: &UiPalette) {
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.bg)),
        rect,
    );
    let mut x = rect.x + 1;
    for filter in LedgerFilter::ALL {
        let count = app
            .ledger()
            .entries()
            .iter()
            .filter(|entry| filter.matches(entry.status))
            .count();
        let label = format!(" {} {count} ", filter.label());
        let width = text_u16(&label);
        if x + width > rect.right() {
            break;
        }
        let style = if app.ledger_filter() == filter {
            Style::default()
                .fg(palette.bg)
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.fg).bg(palette.surface_alt)
        };
        let pill = Rect::new(x, rect.y, width, 1);
        frame.render_widget(line(label, style), pill);
        app.views.hits.push((pill, TabHit::LedgerFilter(filter)));
        x += width + 1;
    }
    let hint = "f cycle";
    if x + text_u16(hint) < rect.right() {
        frame.render_widget(
            line(hint, Style::default().fg(palette.subtle).bg(palette.bg)),
            Rect::new(x + 1, rect.y, text_u16(hint), 1),
        );
    }
}

struct LedgerColumns {
    title: usize,
    owner: usize,
    branch: usize,
}

const ID_W: usize = 6;
const STATUS_W: usize = 15;
const FILES_W: usize = 6;
const AGE_W: usize = 5;
const BRANCH_W: usize = 14;
const BRANCH_COLUMNS_MIN_WIDTH: usize = 76;

impl LedgerColumns {
    fn for_width(width: u16) -> Self {
        let width = usize::from(width);
        let fixed = 1 + ID_W + STATUS_W + AGE_W;
        let show_branch = width >= BRANCH_COLUMNS_MIN_WIDTH;
        let owner = 12;
        let branch = if show_branch { BRANCH_W } else { 0 };
        let files = if show_branch { FILES_W } else { 0 };
        let title = width
            .saturating_sub(fixed + owner + branch + files + 1)
            .max(8);
        Self {
            title,
            owner,
            branch,
        }
    }

    fn header(&self) -> String {
        let mut text = format!(
            " {:<ID_W$}{:<STATUS_W$}{:<owner$}{:<title$}",
            "ID",
            "STATUS",
            "OWNER",
            "TITLE",
            owner = self.owner,
            title = self.title
        );
        if self.branch > 0 {
            text.push_str(&format!(
                "{:<branch$}{:<FILES_W$}",
                "BRANCH",
                "FILES",
                branch = self.branch
            ));
        }
        text.push_str(&format!("{:>AGE_W$}", "AGE"));
        text
    }

    fn row(&self, entry: &LedgerEntry, now: i64, palette: &UiPalette, bg: Color) -> Line<'static> {
        let cell = |text: &str, width: usize| {
            let clipped = truncate_width(text, width.saturating_sub(1));
            let pad = width.saturating_sub(display_width(&clipped));
            format!("{clipped}{}", " ".repeat(pad))
        };
        let mut spans = vec![
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(
                cell(&entry.id, ID_W),
                Style::default().fg(palette.subtle).bg(bg),
            ),
            Span::styled(
                cell(entry.status.label(), STATUS_W),
                Style::default()
                    .fg(task_status_color(entry.status, palette))
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                cell(&entry.owner, self.owner),
                Style::default().fg(palette.user_accent).bg(bg),
            ),
            Span::styled(
                cell(
                    &format!("{} · {}", entry.kind.label(), entry.title),
                    self.title,
                ),
                Style::default().fg(palette.fg).bg(bg),
            ),
        ];
        if self.branch > 0 {
            let files = entry
                .changed_files
                .map(|count| count.to_string())
                .unwrap_or_else(|| "-".into());
            spans.push(Span::styled(
                cell(entry.branch.as_deref().unwrap_or("-"), self.branch),
                Style::default().fg(palette.muted).bg(bg),
            ));
            spans.push(Span::styled(
                cell(&files, FILES_W),
                Style::default().fg(palette.muted).bg(bg),
            ));
        }
        spans.push(Span::styled(
            format!("{:>AGE_W$}", relative_age(entry.updated_at, now)),
            Style::default().fg(palette.subtle).bg(bg),
        ));
        Line::from(spans)
    }
}
