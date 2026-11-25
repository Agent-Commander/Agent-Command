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
    token_burn::{self, RING_TILE_HEIGHT},
    tui::{
        render::{display_width, text_u16, truncate_width},
        tabs::{ledger_filter_count, InboxItem, InboxKind, TabHit, ViewTab},
    },
    App,
};

const INBOX_ROW_HEIGHT: u16 = 4;
const WORKSPACE_SECTION_ROWS: u16 = 5;

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
        let lines = card.ring_tile_lines(tile.width, &deck_palette);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(palette.surface)),
            tile,
        );
        app.left_hits.push((tile, TabHit::UsageTile(index)));
        y += RING_TILE_HEIGHT;
    }
    y
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
    if area.height < 3 || area.width < 20 {
        return;
    }
    let items = app.inbox_items();
    let actionable = items.iter().filter(|item| item.needs_action()).count();
    view_header(
        frame,
        row(area, area.y),
        format!(" INBOX · {} items · {actionable} need action", items.len()),
        " j/k ⏎ a n d p x · Esc chat ",
        palette,
    );
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    if items.is_empty() {
        draw_empty_state(
            frame,
            body,
            "✓ Inbox zero — nothing needs your attention.",
            "Approvals, failures, and finished specialist work land here.",
            palette,
        );
        return;
    }
    app.inbox_selected = app.inbox_selected.min(items.len() - 1);
    let capacity = usize::from((body.height / INBOX_ROW_HEIGHT).max(1));
    let first = app.inbox_selected.saturating_sub(capacity - 1);
    for (offset, item) in items.iter().enumerate().skip(first).take(capacity) {
        let y = body.y + (offset - first) as u16 * INBOX_ROW_HEIGHT;
        draw_inbox_item(
            app,
            frame,
            Rect::new(body.x, y, body.width, INBOX_ROW_HEIGHT - 1),
            offset,
            item,
            palette,
        );
    }
}

fn draw_inbox_item(
    app: &mut App,
    frame: &mut Frame,
    rect: Rect,
    index: usize,
    item: &InboxItem,
    palette: &UiPalette,
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
    app.left_hits
        .push((row(rect, rect.y), TabHit::InboxRow(index)));
    if rows < 2 {
        return;
    }
    put_line(
        frame,
        row(rect, rect.y + 1),
        vec![styled(format!("     {}", item.detail), palette.muted, bg)],
        bg,
    );
    app.left_hits
        .push((row(rect, rect.y + 1), TabHit::InboxRow(index)));
    if rows < 3 {
        return;
    }
    let mut x = rect.x + 5;
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
        app.left_hits
            .push((button, TabHit::InboxAction(index, *action)));
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
        " f filter · ⏎ open · d diff ",
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
    let columns = LedgerColumns::for_width(body.width);
    put_line(frame, row(body, body.y), columns.header(palette), bg);
    app.ledger_selected = app.ledger_selected.min(rows.len() - 1);
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
    title: usize,
    show_branch: bool,
}

const ID_W: usize = 6;
const STATUS_W: usize = 15;
const OWNER_W: usize = 10;
const BRANCH_W: usize = 16;
const FILES_W: usize = 6;
const UPDATED_W: usize = 8;
const TITLE_MIN_W: usize = 14;

impl LedgerColumns {
    fn for_width(width: u16) -> Self {
        let base = 1 + ID_W + STATUS_W + OWNER_W + FILES_W + UPDATED_W;
        let width = usize::from(width);
        let show_branch = width >= base + BRANCH_W + TITLE_MIN_W;
        let fixed = base + if show_branch { BRANCH_W } else { 0 };
        Self {
            title: width.saturating_sub(fixed).max(TITLE_MIN_W),
            show_branch,
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
            " {}{}{}{}",
            Self::cell("ID", ID_W),
            Self::cell("STATUS", STATUS_W),
            Self::cell("OWNER", OWNER_W),
            Self::cell("TITLE", self.title)
        );
        if self.show_branch {
            text.push_str(&Self::cell("BRANCH", BRANCH_W));
        }
        text.push_str(&Self::cell("FILES", FILES_W));
        text.push_str(&Self::cell("UPDATED", UPDATED_W));
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
        let marker = if selected { "▸" } else { " " };
        let mut spans = vec![
            styled(marker, palette.accent, bg),
            styled(Self::cell(&entry.id, ID_W), palette.muted, bg),
            bold(
                Self::cell(entry.status.label(), STATUS_W),
                task_status_color(entry.status, palette),
                bg,
            ),
            styled(Self::cell(&entry.owner, OWNER_W), palette.user_accent, bg),
            styled(Self::cell(&entry.title, self.title), fg, bg),
        ];
        if self.show_branch {
            spans.push(styled(
                Self::cell(entry.branch.as_deref().unwrap_or("-"), BRANCH_W),
                palette.info,
                bg,
            ));
        }
        let files = entry
            .changed_files
            .map_or_else(|| "-".to_owned(), |count| count.to_string());
        spans.push(styled(Self::cell(&files, FILES_W), palette.muted, bg));
        spans.push(styled(updated_label(entry.updated_at), palette.muted, bg));
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
