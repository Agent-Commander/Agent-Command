use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::markdown::UiPalette;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteItem {
    pub id: String,
    pub category: String,
    pub label: String,
    pub detail: String,
    pub shortcut: String,
    pub suggested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i32,
    pub positions: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilteredPaletteItem {
    pub item_index: usize,
    pub score: i32,
    pub label_positions: Vec<usize>,
    pub section: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    None,
    Closed,
    Executed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PaletteRow {
    Header(String),
    Entry(usize),
}

fn is_word_boundary(prev: Option<char>, current: char) -> bool {
    match prev {
        None => true,
        Some(' ' | '-' | '_' | '/' | ':' | '.' | '\\') => true,
        Some(p) if p.is_lowercase() && current.is_uppercase() => true,
        _ => false,
    }
}

pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let query_chars: Vec<char> = trimmed.chars().flat_map(char::to_lowercase).collect();
    if query_chars.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let cand_chars: Vec<char> = candidate.chars().collect();
    let cand_lower: Vec<char> = cand_chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();

    let mut positions = Vec::with_capacity(query_chars.len());
    let mut search_from = 0usize;
    for &qc in &query_chars {
        let mut found = None;
        for (idx, &cc) in cand_lower.iter().enumerate().skip(search_from) {
            if cc == qc {
                let prev = idx.checked_sub(1).and_then(|i| cand_chars.get(i).copied());
                let boundary = is_word_boundary(prev, cand_chars[idx]);
                if found.is_none() || boundary {
                    found = Some(idx);
                    if boundary {
                        break;
                    }
                }
            }
        }
        let pos = found?;
        positions.push(pos);
        search_from = pos + 1;
    }

    let mut score = 100i32;
    let cand_lower_str: String = cand_lower.iter().collect();
    let query_lower_str: String = query_chars.iter().collect();
    if cand_lower_str == query_lower_str {
        score += 200;
    } else if cand_lower_str.starts_with(&query_lower_str) {
        score += 120;
    }

    let mut prev_pos: Option<usize> = None;
    let mut consecutive = 0i32;
    for &pos in &positions {
        let prev_char = pos.checked_sub(1).and_then(|i| cand_chars.get(i).copied());
        if pos == 0 {
            score += 30;
        } else if is_word_boundary(prev_char, cand_chars[pos]) {
            score += 22;
        }
        if let Some(p) = prev_pos {
            if pos == p + 1 {
                consecutive += 1;
                score += 16 + consecutive * 4;
            } else {
                consecutive = 0;
                let gap = (pos - p - 1) as i32;
                score -= gap.min(20) * 2;
            }
        }
        prev_pos = Some(pos);
    }

    score -= (cand_chars.len().saturating_sub(query_chars.len()) as i32).min(40);
    Some(FuzzyMatch { score, positions })
}

fn score_palette_item(query: &str, item: &PaletteItem) -> Option<(i32, Vec<usize>)> {
    let label_match = fuzzy_match(query, &item.label);
    let combined = format!("{} {} {}", item.category, item.label, item.detail);
    let combined_match = fuzzy_match(query, &combined);
    let shortcut_match = if item.shortcut.is_empty() {
        None
    } else {
        fuzzy_match(query, &item.shortcut)
    };

    let mut best: Option<(i32, Vec<usize>)> = None;
    if let Some(m) = label_match {
        best = Some((m.score + 60, m.positions));
    }
    if let Some(m) = combined_match {
        let candidate_score = m.score;
        if best.as_ref().is_none_or(|(s, _)| candidate_score > *s) {
            best = Some((candidate_score, Vec::new()));
        }
    }
    if let Some(m) = shortcut_match {
        let candidate_score = m.score - 10;
        if best.as_ref().is_none_or(|(s, _)| candidate_score > *s) {
            best = Some((candidate_score, Vec::new()));
        }
    }
    best
}

fn prev_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let clamped = cursor.min(text.len());
    text[..clamped]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(idx, _)| idx)
}

fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let clamped = cursor.min(text.len());
    text[clamped..]
        .grapheme_indices(true)
        .next()
        .map_or(text.len(), |(_, g)| clamped + g.len())
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if Span::raw(text).width() <= max_width {
        return text.to_owned();
    }
    let ellipsis_w = Span::raw("…").width();
    let budget = max_width.saturating_sub(ellipsis_w);
    let mut out = String::new();
    let mut used = 0usize;
    for g in text.graphemes(true) {
        let gw = Span::raw(g).width();
        if used + gw > budget {
            break;
        }
        out.push_str(g);
        used += gw;
    }
    out.push('…');
    out
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.right()
        && row >= rect.y
        && row < rect.bottom()
}

#[derive(Clone, Debug)]
pub struct CommandPalette {
    open: bool,
    query: String,
    cursor: usize,
    items: Vec<PaletteItem>,
    filtered: Vec<FilteredPaletteItem>,
    selected: usize,
    scroll_offset: usize,
    modal_area: Rect,
    row_hit_rects: Vec<(Rect, String)>,
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl CommandPalette {
    pub fn new(items: Vec<PaletteItem>) -> Self {
        let mut palette = Self {
            open: false,
            query: String::new(),
            cursor: 0,
            items,
            filtered: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            modal_area: Rect::default(),
            row_hit_rects: Vec::new(),
        };
        palette.rebuild_filtered();
        palette
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.cursor = 0;
        self.selected = 0;
        self.scroll_offset = 0;
        self.rebuild_filtered();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.row_hit_rects.clear();
    }

    pub fn set_items(&mut self, items: Vec<PaletteItem>) {
        self.items = items;
        self.rebuild_filtered();
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        self.cursor = self.query.len();
        self.selected = 0;
        self.scroll_offset = 0;
        self.rebuild_filtered();
    }

    pub fn filtered(&self) -> &[FilteredPaletteItem] {
        &self.filtered
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected_item(&self) -> Option<&PaletteItem> {
        let entry = self.filtered.get(self.selected)?;
        self.items.get(entry.item_index)
    }

    pub fn modal_area(&self) -> Rect {
        self.modal_area
    }

    pub fn row_hit_rects(&self) -> &[(Rect, String)] {
        &self.row_hit_rects
    }

    fn rebuild_filtered(&mut self) {
        let trimmed = self.query.trim();
        self.filtered.clear();

        if trimmed.is_empty() {
            for (idx, item) in self.items.iter().enumerate() {
                if item.suggested {
                    self.filtered.push(FilteredPaletteItem {
                        item_index: idx,
                        score: 0,
                        label_positions: Vec::new(),
                        section: "Suggested".to_owned(),
                    });
                }
            }
            let mut category_order: Vec<String> = Vec::new();
            for item in &self.items {
                if !item.suggested && !category_order.contains(&item.category) {
                    category_order.push(item.category.clone());
                }
            }
            for category in category_order {
                for (idx, item) in self.items.iter().enumerate() {
                    if !item.suggested && item.category == category {
                        self.filtered.push(FilteredPaletteItem {
                            item_index: idx,
                            score: 0,
                            label_positions: Vec::new(),
                            section: item.category.clone(),
                        });
                    }
                }
            }
        } else {
            let mut scored: Vec<FilteredPaletteItem> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| {
                    let (score, positions) = score_palette_item(trimmed, item)?;
                    Some(FilteredPaletteItem {
                        item_index: idx,
                        score,
                        label_positions: positions,
                        section: item.category.clone(),
                    })
                })
                .collect();
            scored.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| {
                        let sa = self.items[a.item_index].suggested;
                        let sb = self.items[b.item_index].suggested;
                        sb.cmp(&sa)
                    })
                    .then_with(|| {
                        self.items[a.item_index]
                            .label
                            .cmp(&self.items[b.item_index].label)
                    })
            });
            self.filtered = scored;
        }

        if self.filtered.is_empty() {
            self.selected = 0;
            self.scroll_offset = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
    }

    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.filtered.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.filtered.len();
    }

    pub fn page_up(&mut self, step: usize) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(step.max(1));
    }

    pub fn page_down(&mut self, step: usize) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = (self.selected + step.max(1)).min(self.filtered.len() - 1);
    }

    pub fn handle_scroll(&mut self, delta: isize) {
        if delta < 0 {
            for _ in 0..delta.unsigned_abs() {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
        } else if delta > 0 && !self.filtered.is_empty() {
            let max_idx = self.filtered.len() - 1;
            self.selected = (self.selected + delta as usize).min(max_idx);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PaletteAction {
        if !self.open {
            return PaletteAction::None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                self.close();
                PaletteAction::Closed
            }
            (KeyCode::Enter, _) => {
                if let Some(item) = self.selected_item().cloned() {
                    self.close();
                    PaletteAction::Executed(item.id)
                } else {
                    PaletteAction::None
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('p' | 'k'), KeyModifiers::CONTROL) => {
                self.move_up();
                PaletteAction::None
            }
            (KeyCode::Down, _) | (KeyCode::Char('n' | 'j'), KeyModifiers::CONTROL) => {
                self.move_down();
                PaletteAction::None
            }
            (KeyCode::PageUp, _) => {
                self.page_up(6);
                PaletteAction::None
            }
            (KeyCode::PageDown, _) => {
                self.page_down(6);
                PaletteAction::None
            }
            (KeyCode::Left, _) => {
                self.cursor = prev_grapheme_boundary(&self.query, self.cursor);
                PaletteAction::None
            }
            (KeyCode::Right, _) => {
                self.cursor = next_grapheme_boundary(&self.query, self.cursor);
                PaletteAction::None
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.cursor = 0;
                PaletteAction::None
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                self.cursor = self.query.len();
                PaletteAction::None
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.query.drain(..self.cursor);
                self.cursor = 0;
                self.selected = 0;
                self.scroll_offset = 0;
                self.rebuild_filtered();
                PaletteAction::None
            }
            (KeyCode::Backspace, _)
            | (KeyCode::Char('\x7f' | '\x08'), _)
            | (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
                if self.cursor > 0 {
                    let prev = prev_grapheme_boundary(&self.query, self.cursor);
                    self.query.drain(prev..self.cursor);
                    self.cursor = prev;
                    self.selected = 0;
                    self.scroll_offset = 0;
                    self.rebuild_filtered();
                }
                PaletteAction::None
            }
            (KeyCode::Delete, _) => {
                if self.cursor < self.query.len() {
                    let next = next_grapheme_boundary(&self.query, self.cursor);
                    self.query.drain(self.cursor..next);
                    self.selected = 0;
                    self.scroll_offset = 0;
                    self.rebuild_filtered();
                }
                PaletteAction::None
            }
            (KeyCode::Char(c), mods)
                if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT) =>
            {
                self.query.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.selected = 0;
                self.scroll_offset = 0;
                self.rebuild_filtered();
                PaletteAction::None
            }
            _ => PaletteAction::None,
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> PaletteAction {
        if !self.open {
            return PaletteAction::None;
        }
        if let Some((_, id)) = self
            .row_hit_rects
            .iter()
            .find(|(rect, _)| rect_contains(*rect, col, row))
            .cloned()
        {
            if let Some(pos) = self
                .filtered
                .iter()
                .position(|entry| self.items[entry.item_index].id == id)
            {
                self.selected = pos;
            }
            self.close();
            return PaletteAction::Executed(id);
        }
        if !rect_contains(self.modal_area, col, row) {
            self.close();
            return PaletteAction::Closed;
        }
        PaletteAction::None
    }

    fn build_rows(&self) -> Vec<PaletteRow> {
        let mut rows = Vec::new();
        let show_sections = self.query.trim().is_empty();
        let mut current_section: Option<&str> = None;
        for (filtered_idx, entry) in self.filtered.iter().enumerate() {
            if show_sections && current_section != Some(entry.section.as_str()) {
                rows.push(PaletteRow::Header(entry.section.clone()));
                current_section = Some(entry.section.as_str());
            }
            rows.push(PaletteRow::Entry(filtered_idx));
        }
        rows
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &UiPalette) {
        self.row_hit_rects.clear();
        if !self.open || area.width < 12 || area.height < 6 {
            self.modal_area = Rect::default();
            return;
        }

        let modal_w = area.width.saturating_sub(4).clamp(12, 76);
        let modal_h = area.height.saturating_sub(2).clamp(6, 22);
        let modal_x = area.x + (area.width.saturating_sub(modal_w)) / 2;
        let modal_y = area.y + (area.height.saturating_sub(modal_h)) / 2;
        let modal = Rect::new(modal_x, modal_y, modal_w, modal_h);
        self.modal_area = modal;

        frame.render_widget(Clear, modal);
        let outer = Block::default()
            .title(" Command Palette ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.surface).fg(theme.fg))
            .border_style(Style::default().fg(theme.accent));
        let inner = outer.inner(modal);
        frame.render_widget(outer, modal);

        if inner.width == 0 || inner.height < 3 {
            return;
        }

        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

        let search_area = chunks[0];
        let sep_area = chunks[1];
        let list_area = chunks[2];
        let footer_area = chunks[3];

        let prompt_prefix = " > ";
        let prefix_w = Span::raw(prompt_prefix).width();
        let search_line = if self.query.is_empty() {
            Line::from(vec![
                Span::styled(prompt_prefix, Style::default().fg(theme.accent)),
                Span::styled(
                    "Type a command, model, or setting…",
                    Style::default().fg(theme.subtle),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled(prompt_prefix, Style::default().fg(theme.accent)),
                Span::styled(self.query.clone(), Style::default().fg(theme.fg)),
            ])
        };
        frame.render_widget(
            Paragraph::new(search_line).style(Style::default().bg(theme.surface)),
            search_area,
        );

        let cursor_offset_w = Span::raw(&self.query[..self.cursor]).width();
        let cursor_x = search_area
            .x
            .saturating_add((prefix_w + cursor_offset_w) as u16)
            .min(search_area.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, search_area.y));

        let sep = "─".repeat(sep_area.width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                sep,
                Style::default().fg(theme.border),
            )))
            .style(Style::default().bg(theme.surface)),
            sep_area,
        );

        let rows = self.build_rows();
        let visible_h = list_area.height as usize;

        if rows.is_empty() {
            let empty = Line::from(Span::styled(
                "  No matching commands",
                Style::default().fg(theme.muted),
            ));
            frame.render_widget(
                Paragraph::new(empty).style(Style::default().bg(theme.surface)),
                list_area,
            );
        } else {
            let selected_row_idx = rows
                .iter()
                .position(|r| matches!(r, PaletteRow::Entry(i) if *i == self.selected))
                .unwrap_or(0);
            if selected_row_idx < self.scroll_offset {
                self.scroll_offset = selected_row_idx;
            } else if visible_h > 0 && selected_row_idx >= self.scroll_offset + visible_h {
                self.scroll_offset = selected_row_idx + 1 - visible_h;
            }
            let max_scroll = rows.len().saturating_sub(visible_h);
            self.scroll_offset = self.scroll_offset.min(max_scroll);

            let mut lines: Vec<Line<'static>> = Vec::with_capacity(visible_h);
            for (rel_y, row) in rows
                .iter()
                .skip(self.scroll_offset)
                .take(visible_h)
                .enumerate()
            {
                let row_rect =
                    Rect::new(list_area.x, list_area.y + rel_y as u16, list_area.width, 1);
                match row {
                    PaletteRow::Header(title) => {
                        let label =
                            truncate_to_width(&format!(" {title}"), list_area.width as usize);
                        lines.push(Line::from(Span::styled(
                            label,
                            Style::default()
                                .fg(theme.accent)
                                .bg(theme.surface)
                                .add_modifier(Modifier::BOLD),
                        )));
                    }
                    PaletteRow::Entry(filtered_idx) => {
                        let entry = &self.filtered[*filtered_idx];
                        let item = &self.items[entry.item_index];
                        let is_selected = *filtered_idx == self.selected;
                        self.row_hit_rects.push((row_rect, item.id.clone()));
                        lines.push(render_palette_entry_line(
                            item,
                            &entry.label_positions,
                            is_selected,
                            list_area.width as usize,
                            theme,
                        ));
                    }
                }
            }
            frame.render_widget(
                Paragraph::new(lines).style(Style::default().bg(theme.surface)),
                list_area,
            );
        }

        let footer_text = truncate_to_width(
            " ↑↓ navigate · Enter run · Esc close",
            footer_area.width as usize,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                footer_text,
                Style::default().fg(theme.muted).bg(theme.surface),
            ))),
            footer_area,
        );
    }
}

fn render_palette_entry_line(
    item: &PaletteItem,
    label_positions: &[usize],
    is_selected: bool,
    width: usize,
    theme: &UiPalette,
) -> Line<'static> {
    let bg = if is_selected {
        theme.selection_bg
    } else {
        theme.surface
    };
    let base_fg = if is_selected {
        theme.selection_fg
    } else {
        theme.fg
    };
    let detail_fg = if is_selected { theme.fg } else { theme.muted };
    let shortcut_fg = if is_selected {
        theme.warning
    } else {
        theme.subtle
    };

    let marker = if is_selected { " ▸ " } else { "   " };
    let shortcut_text = if item.shortcut.is_empty() {
        String::new()
    } else {
        format!(" {} ", item.shortcut)
    };
    let shortcut_w = Span::raw(shortcut_text.as_str()).width();
    let marker_w = Span::raw(marker).width();
    let content_budget = width.saturating_sub(marker_w + shortcut_w);

    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        marker.to_owned(),
        Style::default().fg(theme.accent).bg(bg),
    )];

    let mut used = 0usize;
    for (char_idx, ch) in item.label.chars().enumerate() {
        let s = ch.to_string();
        let cw = Span::raw(s.as_str()).width();
        if used + cw > content_budget {
            break;
        }
        let style = if label_positions.contains(&char_idx) {
            Style::default()
                .fg(theme.accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(base_fg).bg(bg)
        };
        spans.push(Span::styled(s, style));
        used += cw;
    }

    if !item.detail.is_empty() && used + 3 < content_budget {
        let remaining = content_budget - used - 2;
        let detail = truncate_to_width(&item.detail, remaining);
        let detail_w = Span::raw(detail.as_str()).width();
        spans.push(Span::styled("  ", Style::default().bg(bg)));
        spans.push(Span::styled(detail, Style::default().fg(detail_fg).bg(bg)));
        used += 2 + detail_w;
    }

    let pad = content_budget.saturating_sub(used);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }
    if !shortcut_text.is_empty() {
        spans.push(Span::styled(
            shortcut_text,
            Style::default().fg(shortcut_fg).bg(bg),
        ));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn sample_items() -> Vec<PaletteItem> {
        vec![
            PaletteItem {
                id: "model.switch".into(),
                category: "Models".into(),
                label: "Switch Model".into(),
                detail: "Pick Claude, Codex, or Gemini".into(),
                shortcut: "Ctrl+M".into(),
                suggested: true,
            },
            PaletteItem {
                id: "rail.toggle".into(),
                category: "View".into(),
                label: "Toggle Right Rail".into(),
                detail: "Show or hide workers and changes".into(),
                shortcut: "Ctrl+B".into(),
                suggested: true,
            },
            PaletteItem {
                id: "git.diff".into(),
                category: "Git".into(),
                label: "Open Diff Viewer".into(),
                detail: "Inspect turn-scoped unified diff".into(),
                shortcut: "d".into(),
                suggested: false,
            },
            PaletteItem {
                id: "update.check".into(),
                category: "System".into(),
                label: "Check for Updates".into(),
                detail: "Verify latest GitHub release".into(),
                shortcut: "F6".into(),
                suggested: false,
            },
        ]
    }

    #[test]
    fn fuzzy_scorer_ranks_prefix_and_word_boundary_highest() {
        let exact = fuzzy_match("diff", "diff").unwrap();
        let boundary = fuzzy_match("odv", "Open Diff Viewer").unwrap();
        let scattered = fuzzy_match("odv", "OxxxxxDxxxxxVxxx").unwrap();
        assert!(exact.score > boundary.score);
        assert!(boundary.score > scattered.score);
        assert_eq!(boundary.positions, vec![0, 5, 10]);
        assert!(fuzzy_match("zzz", "Open Diff Viewer").is_none());
    }

    #[test]
    fn empty_query_places_suggested_items_first() {
        let mut palette = CommandPalette::new(sample_items());
        palette.open();
        let sections: Vec<&str> = palette
            .filtered()
            .iter()
            .map(|f| f.section.as_str())
            .collect();
        assert_eq!(sections, vec!["Suggested", "Suggested", "Git", "System"]);
        assert_eq!(palette.selected_item().unwrap().id, "model.switch");
    }

    #[test]
    fn keyboard_navigation_filtering_and_mouse_click_work_equivalently() {
        let mut palette = CommandPalette::new(sample_items());
        palette.open();
        for ch in "diff".chars() {
            assert_eq!(
                palette.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
                PaletteAction::None
            );
        }
        assert_eq!(palette.selected_item().unwrap().id, "git.diff");

        let action = palette.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, PaletteAction::Executed("git.diff".into()));
        assert!(!palette.is_open());

        palette.open();
        let theme = UiPalette::dark_deck();
        for (w, h) in [(80, 24), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|frame| palette.render(frame, frame.area(), &theme))
                .unwrap();
            assert!(!palette.row_hit_rects().is_empty());
        }
        let (target_rect, target_id) = palette
            .row_hit_rects()
            .iter()
            .find(|(_, id)| id == "update.check")
            .cloned()
            .unwrap();
        let click_action = palette.handle_click(target_rect.x + 1, target_rect.y);
        assert_eq!(click_action, PaletteAction::Executed(target_id));
        assert!(!palette.is_open());
    }

    #[test]
    fn esc_and_outside_click_close_palette() {
        let mut palette = CommandPalette::new(sample_items());
        palette.open();
        assert_eq!(
            palette.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            PaletteAction::Closed
        );
        assert!(!palette.is_open());

        palette.open();
        let theme = UiPalette::dark_deck();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| palette.render(frame, frame.area(), &theme))
            .unwrap();
        assert_eq!(palette.handle_click(0, 0), PaletteAction::Closed);
        assert!(!palette.is_open());
    }
}
