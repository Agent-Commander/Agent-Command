use std::{
    collections::{HashMap, VecDeque},
    env, fs, io,
    ops::Range,
    process::Command,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

use crate::{markdown::UiPalette, palette::fuzzy_match};

pub const MIN_EDITOR_LINES: u16 = 1;
pub const MAX_EDITOR_LINES: u16 = 6;
pub const LARGE_PASTE_LINE_THRESHOLD: usize = 10;
pub const LARGE_PASTE_BYTE_THRESHOLD: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefixMode {
    Normal,
    Command,
    Mention,
    Shell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerKind {
    Command,
    Mention,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggerToken {
    pub kind: TriggerKind,
    pub range: Range<usize>,
    pub query: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorAction {
    None,
    Changed,
    Submit { text: String, steer: bool },
    OpenExternalEditor,
    RecallQueued(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PastedChunk {
    pub id: usize,
    pub placeholder: String,
    pub content: String,
    pub line_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DraftSnapshot {
    text: String,
    cursor: usize,
    pasted_chunks: Vec<PastedChunk>,
    next_paste_id: usize,
}

#[derive(Clone, Debug)]
struct VisualRow {
    text: String,
    byte_start: usize,
    byte_end: usize,
    ends_with_newline: bool,
}

fn display_width(text: &str) -> usize {
    Span::raw(text).width()
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

fn is_word_char(g: &str) -> bool {
    g.chars().any(|c| c.is_alphanumeric() || c == '_')
}

fn prev_word_boundary(text: &str, cursor: usize) -> usize {
    let clamped = cursor.min(text.len());
    let graphemes: Vec<(usize, &str)> = text[..clamped].grapheme_indices(true).collect();
    let mut idx = graphemes.len();
    while idx > 0 && !is_word_char(graphemes[idx - 1].1) {
        idx -= 1;
    }
    while idx > 0 && is_word_char(graphemes[idx - 1].1) {
        idx -= 1;
    }
    graphemes.get(idx).map_or(0, |(byte_idx, _)| *byte_idx)
}

fn next_word_boundary(text: &str, cursor: usize) -> usize {
    let clamped = cursor.min(text.len());
    let graphemes: Vec<(usize, &str)> = text[clamped..]
        .grapheme_indices(true)
        .map(|(off, g)| (clamped + off, g))
        .collect();
    let mut idx = 0usize;
    while idx < graphemes.len() && is_word_char(graphemes[idx].1) {
        idx += 1;
    }
    while idx < graphemes.len() && !is_word_char(graphemes[idx].1) {
        idx += 1;
    }
    graphemes
        .get(idx)
        .map_or(text.len(), |(byte_idx, _)| *byte_idx)
}

fn logical_line_bounds(text: &str, cursor: usize) -> (usize, usize) {
    let clamped = cursor.min(text.len());
    let start = text[..clamped].rfind('\n').map_or(0, |i| i + 1);
    let end = text[clamped..]
        .find('\n')
        .map_or(text.len(), |off| clamped + off);
    (start, end)
}

fn byte_offset_for_display_col(slice: &str, target_col: usize) -> usize {
    let mut used = 0usize;
    for (idx, g) in slice.grapheme_indices(true) {
        let gw = display_width(g);
        if used + gw > target_col {
            return idx;
        }
        used += gw;
    }
    slice.len()
}

fn normalize_newlines(input: &str) -> String {
    input.replace("\r\n", "\n").replace('\r', "\n")
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
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

fn build_visual_rows(text: &str, width: usize) -> Vec<VisualRow> {
    let wrap_w = width.max(1);
    let mut rows = Vec::new();
    let mut line_start = 0usize;

    for raw_segment in text.split('\n') {
        let segment_end = line_start + raw_segment.len();
        let has_nl = segment_end < text.len();
        if raw_segment.is_empty() {
            rows.push(VisualRow {
                text: String::new(),
                byte_start: line_start,
                byte_end: segment_end,
                ends_with_newline: has_nl,
            });
            line_start = segment_end + usize::from(has_nl);
            continue;
        }

        let mut row_start = line_start;
        let mut row_text = String::new();
        let mut used_w = 0usize;
        for (rel_idx, g) in raw_segment.grapheme_indices(true) {
            let gw = display_width(g);
            if used_w > 0 && used_w + gw > wrap_w {
                let row_end = line_start + rel_idx;
                rows.push(VisualRow {
                    text: std::mem::take(&mut row_text),
                    byte_start: row_start,
                    byte_end: row_end,
                    ends_with_newline: false,
                });
                row_start = row_end;
                used_w = 0;
            }
            row_text.push_str(g);
            used_w += gw;
        }
        rows.push(VisualRow {
            text: row_text,
            byte_start: row_start,
            byte_end: segment_end,
            ends_with_newline: has_nl,
        });
        line_start = segment_end + usize::from(has_nl);
    }

    if rows.is_empty() {
        rows.push(VisualRow {
            text: String::new(),
            byte_start: 0,
            byte_end: 0,
            ends_with_newline: false,
        });
    }
    rows
}

fn locate_cursor_visual(rows: &[VisualRow], cursor: usize) -> (usize, usize) {
    for (row_idx, row) in rows.iter().enumerate() {
        let in_row = if cursor < row.byte_end {
            cursor >= row.byte_start
        } else if cursor == row.byte_end {
            row.ends_with_newline || row_idx + 1 == rows.len()
        } else {
            false
        };
        if in_row {
            let rel = cursor.saturating_sub(row.byte_start).min(row.text.len());
            let col = display_width(&row.text[..rel]);
            return (row_idx, col);
        }
    }
    let last_idx = rows.len().saturating_sub(1);
    let last_col = rows.last().map_or(0, |r| display_width(&r.text));
    (last_idx, last_col)
}

#[derive(Clone, Debug)]
pub struct Composer {
    text: String,
    cursor: usize,
    preferred_col: Option<usize>,
    scroll_row: usize,
    pasted_chunks: Vec<PastedChunk>,
    next_paste_id: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_scratch: String,
    drafts: HashMap<String, DraftSnapshot>,
    running: bool,
    queued_messages: VecDeque<String>,
    last_area: Rect,
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            preferred_col: None,
            scroll_row: 0,
            pasted_chunks: Vec::new(),
            next_paste_id: 1,
            history: Vec::new(),
            history_index: None,
            history_scratch: String::new(),
            drafts: HashMap::new(),
            running: false,
            queued_messages: VecDeque::new(),
            last_area: Rect::default(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn scroll_row(&self) -> usize {
        self.scroll_row
    }

    pub fn pasted_chunks(&self) -> &[PastedChunk] {
        &self.pasted_chunks
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn set_running(&mut self, running: bool) {
        self.running = running;
    }

    pub fn queued_messages(&self) -> &VecDeque<String> {
        &self.queued_messages
    }

    pub fn push_queued(&mut self, message: impl Into<String>) {
        self.queued_messages.push_back(message.into());
    }

    pub fn pop_next_queued(&mut self) -> Option<String> {
        self.queued_messages.pop_front()
    }

    pub fn recall_last_queued(&mut self) -> Option<String> {
        let recalled = self.queued_messages.pop_back()?;
        self.set_text(recalled.clone());
        Some(recalled)
    }

    pub fn last_area(&self) -> Rect {
        self.last_area
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = normalize_newlines(&text.into());
        self.cursor = self.text.len();
        self.preferred_col = None;
        self.scroll_row = 0;
        self.pasted_chunks.clear();
        self.history_index = None;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.preferred_col = None;
        self.scroll_row = 0;
        self.pasted_chunks.clear();
        self.history_index = None;
    }

    pub fn push_history(&mut self, entry: impl Into<String>) {
        let text = entry.into();
        if text.trim().is_empty() {
            return;
        }
        if self.history.last().is_some_and(|last| last == &text) {
            return;
        }
        self.history.push(text);
    }

    pub fn prefix_mode(&self) -> PrefixMode {
        let trimmed = self.text.trim_start();
        if trimmed.starts_with('/') {
            PrefixMode::Command
        } else if trimmed.starts_with('!') {
            PrefixMode::Shell
        } else if self
            .active_trigger()
            .is_some_and(|t| t.kind == TriggerKind::Mention)
            || trimmed.starts_with('@')
        {
            PrefixMode::Mention
        } else {
            PrefixMode::Normal
        }
    }

    pub fn active_trigger(&self) -> Option<TriggerToken> {
        let before = &self.text[..self.cursor];
        if before.starts_with('/') && !before.contains(char::is_whitespace) {
            return Some(TriggerToken {
                kind: TriggerKind::Command,
                range: 0..self.cursor,
                query: before[1..].to_owned(),
            });
        }
        let token_start = before
            .rfind(|c: char| c.is_whitespace())
            .map_or(0, |idx| idx + 1);
        let token = &before[token_start..];
        if let Some(query) = token.strip_prefix('@') {
            return Some(TriggerToken {
                kind: TriggerKind::Mention,
                range: token_start..self.cursor,
                query: query.to_owned(),
            });
        }
        None
    }

    pub fn replace_range(&mut self, range: Range<usize>, replacement: &str) {
        let start = range.start.min(self.text.len());
        let end = range.end.min(self.text.len()).max(start);
        if !self.text.is_char_boundary(start) || !self.text.is_char_boundary(end) {
            return;
        }
        self.text.replace_range(start..end, replacement);
        self.cursor = start + replacement.len();
        self.preferred_col = None;
        self.history_index = None;
    }

    pub fn insert_str(&mut self, value: &str) {
        let normalized = normalize_newlines(value);
        self.text.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
        self.preferred_col = None;
        self.history_index = None;
    }

    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.preferred_col = None;
        self.history_index = None;
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn handle_paste(&mut self, pasted: &str) {
        let normalized = normalize_newlines(pasted);
        if normalized.is_empty() {
            return;
        }
        let line_count = normalized.split('\n').count();
        if line_count > LARGE_PASTE_LINE_THRESHOLD || normalized.len() > LARGE_PASTE_BYTE_THRESHOLD
        {
            let id = self.next_paste_id;
            self.next_paste_id += 1;
            let placeholder = format!("[Pasted text #{id} ({line_count} lines)]");
            self.pasted_chunks.push(PastedChunk {
                id,
                placeholder: placeholder.clone(),
                content: normalized,
                line_count,
            });
            self.insert_str(&placeholder);
        } else {
            self.insert_str(&normalized);
        }
    }

    pub fn force_inline_paste(&mut self, pasted: &str) {
        let normalized = normalize_newlines(pasted);
        if !normalized.is_empty() {
            self.insert_str(&normalized);
        }
    }

    pub fn expand_text(&self) -> String {
        let mut expanded = self.text.clone();
        for chunk in &self.pasted_chunks {
            if expanded.contains(&chunk.placeholder) {
                expanded = expanded.replace(&chunk.placeholder, &chunk.content);
            }
        }
        expanded
    }

    pub fn take_submission(&mut self) -> Option<String> {
        let expanded = self.expand_text();
        if expanded.trim().is_empty() {
            return None;
        }
        self.push_history(expanded.clone());
        self.clear();
        Some(expanded)
    }

    pub fn stash_draft(&mut self, thread_id: &str) {
        if self.text.is_empty() && self.pasted_chunks.is_empty() {
            self.drafts.remove(thread_id);
        } else {
            self.drafts.insert(
                thread_id.to_owned(),
                DraftSnapshot {
                    text: self.text.clone(),
                    cursor: self.cursor,
                    pasted_chunks: self.pasted_chunks.clone(),
                    next_paste_id: self.next_paste_id,
                },
            );
        }
    }

    pub fn restore_draft(&mut self, thread_id: &str) {
        if let Some(snapshot) = self.drafts.remove(thread_id) {
            self.text = snapshot.text;
            self.cursor = snapshot.cursor.min(self.text.len());
            self.pasted_chunks = snapshot.pasted_chunks;
            self.next_paste_id = snapshot.next_paste_id;
            self.preferred_col = None;
            self.scroll_row = 0;
            self.history_index = None;
        } else {
            self.clear();
        }
    }

    pub fn switch_thread(&mut self, from_thread_id: &str, to_thread_id: &str) {
        if from_thread_id == to_thread_id {
            return;
        }
        self.stash_draft(from_thread_id);
        self.restore_draft(to_thread_id);
    }

    pub fn move_left(&mut self) {
        self.cursor = prev_grapheme_boundary(&self.text, self.cursor);
        self.preferred_col = None;
    }

    pub fn move_right(&mut self) {
        self.cursor = next_grapheme_boundary(&self.text, self.cursor);
        self.preferred_col = None;
    }

    pub fn move_word_left(&mut self) {
        self.cursor = prev_word_boundary(&self.text, self.cursor);
        self.preferred_col = None;
    }

    pub fn move_word_right(&mut self) {
        self.cursor = next_word_boundary(&self.text, self.cursor);
        self.preferred_col = None;
    }

    pub fn move_home(&mut self) {
        let (line_start, _) = logical_line_bounds(&self.text, self.cursor);
        self.cursor = line_start;
        self.preferred_col = None;
    }

    pub fn move_end(&mut self) {
        let (_, line_end) = logical_line_bounds(&self.text, self.cursor);
        self.cursor = line_end;
        self.preferred_col = None;
    }

    pub fn move_up_or_history(&mut self) -> bool {
        let (line_start, _) = logical_line_bounds(&self.text, self.cursor);
        if line_start > 0 {
            let col = self
                .preferred_col
                .unwrap_or_else(|| display_width(&self.text[line_start..self.cursor]));
            self.preferred_col = Some(col);
            let prev_end = line_start - 1;
            let prev_start = self.text[..prev_end].rfind('\n').map_or(0, |i| i + 1);
            let offset = byte_offset_for_display_col(&self.text[prev_start..prev_end], col);
            self.cursor = prev_start + offset;
            return false;
        }
        self.history_prev()
    }

    pub fn move_down_or_history(&mut self) -> bool {
        let (line_start, line_end) = logical_line_bounds(&self.text, self.cursor);
        if line_end < self.text.len() {
            let col = self
                .preferred_col
                .unwrap_or_else(|| display_width(&self.text[line_start..self.cursor]));
            self.preferred_col = Some(col);
            let next_start = line_end + 1;
            let next_end = self.text[next_start..]
                .find('\n')
                .map_or(self.text.len(), |off| next_start + off);
            let offset = byte_offset_for_display_col(&self.text[next_start..next_end], col);
            self.cursor = next_start + offset;
            return false;
        }
        self.history_next()
    }

    fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next_idx = match self.history_index {
            None => {
                self.history_scratch = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return false,
            Some(idx) => idx - 1,
        };
        self.history_index = Some(next_idx);
        self.text = self.history[next_idx].clone();
        self.cursor = self.text.len();
        self.preferred_col = None;
        self.pasted_chunks.clear();
        true
    }

    fn history_next(&mut self) -> bool {
        let Some(idx) = self.history_index else {
            return false;
        };
        if idx + 1 < self.history.len() {
            let next_idx = idx + 1;
            self.history_index = Some(next_idx);
            self.text = self.history[next_idx].clone();
            self.cursor = self.text.len();
            self.preferred_col = None;
            self.pasted_chunks.clear();
            true
        } else {
            self.history_index = None;
            self.text = std::mem::take(&mut self.history_scratch);
            self.cursor = self.text.len();
            self.preferred_col = None;
            self.pasted_chunks.clear();
            true
        }
    }

    fn prune_removed_chunks(&mut self) {
        self.pasted_chunks
            .retain(|chunk| self.text.contains(&chunk.placeholder));
    }

    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        if let Some(chip_len) = self
            .pasted_chunks
            .iter()
            .find(|c| self.text[..self.cursor].ends_with(&c.placeholder))
            .map(|c| c.placeholder.len())
        {
            let start = self.cursor - chip_len;
            self.text.drain(start..self.cursor);
            self.cursor = start;
            self.preferred_col = None;
            self.history_index = None;
            self.prune_removed_chunks();
            return true;
        }
        let prev = prev_grapheme_boundary(&self.text, self.cursor);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        self.preferred_col = None;
        self.history_index = None;
        self.prune_removed_chunks();
        true
    }

    pub fn delete_forward(&mut self) -> bool {
        if self.cursor >= self.text.len() {
            return false;
        }
        if let Some(chip_len) = self
            .pasted_chunks
            .iter()
            .find(|c| self.text[self.cursor..].starts_with(&c.placeholder))
            .map(|c| c.placeholder.len())
        {
            let end = self.cursor + chip_len;
            self.text.drain(self.cursor..end);
            self.preferred_col = None;
            self.history_index = None;
            self.prune_removed_chunks();
            return true;
        }
        let next = next_grapheme_boundary(&self.text, self.cursor);
        self.text.drain(self.cursor..next);
        self.preferred_col = None;
        self.history_index = None;
        self.prune_removed_chunks();
        true
    }

    pub fn delete_word_backward(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = prev_word_boundary(&self.text, self.cursor);
        if start == self.cursor {
            return false;
        }
        self.text.drain(start..self.cursor);
        self.cursor = start;
        self.preferred_col = None;
        self.history_index = None;
        self.prune_removed_chunks();
        true
    }

    pub fn kill_to_line_start(&mut self) -> bool {
        let (line_start, _) = logical_line_bounds(&self.text, self.cursor);
        if line_start == self.cursor {
            return false;
        }
        self.text.drain(line_start..self.cursor);
        self.cursor = line_start;
        self.preferred_col = None;
        self.history_index = None;
        self.prune_removed_chunks();
        true
    }

    pub fn kill_to_line_end(&mut self) -> bool {
        let (_, line_end) = logical_line_bounds(&self.text, self.cursor);
        if self.cursor == line_end {
            if self.cursor < self.text.len() {
                self.text.drain(self.cursor..self.cursor + 1);
                self.preferred_col = None;
                self.history_index = None;
                return true;
            }
            return false;
        }
        self.text.drain(self.cursor..line_end);
        self.preferred_col = None;
        self.history_index = None;
        self.prune_removed_chunks();
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditorAction {
        let mods = key.modifiers;
        match (key.code, mods) {
            (KeyCode::Enter, m)
                if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) =>
            {
                self.insert_newline();
                EditorAction::Changed
            }
            (KeyCode::Char('j'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.insert_newline();
                EditorAction::Changed
            }
            (KeyCode::Enter, m) if m.contains(KeyModifiers::CONTROL) => {
                if let Some(text) = self.take_submission() {
                    EditorAction::Submit { text, steer: true }
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Enter, _) => {
                if let Some(text) = self.take_submission() {
                    EditorAction::Submit { text, steer: false }
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Up, m) if m.contains(KeyModifiers::ALT) => {
                if let Some(recalled) = self.recall_last_queued() {
                    EditorAction::RecallQueued(recalled)
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Left, m) if m.contains(KeyModifiers::CONTROL) => {
                self.move_word_left();
                EditorAction::None
            }
            (KeyCode::Right, m) if m.contains(KeyModifiers::CONTROL) => {
                self.move_word_right();
                EditorAction::None
            }
            (KeyCode::Char('b'), m) if m.contains(KeyModifiers::ALT) => {
                self.move_word_left();
                EditorAction::None
            }
            (KeyCode::Char('f'), m) if m.contains(KeyModifiers::ALT) => {
                self.move_word_right();
                EditorAction::None
            }
            (KeyCode::Left, _) => {
                self.move_left();
                EditorAction::None
            }
            (KeyCode::Right, _) => {
                self.move_right();
                EditorAction::None
            }
            (KeyCode::Up, _) => {
                if self.move_up_or_history() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Down, _) => {
                if self.move_down_or_history() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.move_home();
                EditorAction::None
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                self.move_end();
                EditorAction::None
            }
            (KeyCode::Backspace, m) if m.contains(KeyModifiers::ALT) => {
                if self.delete_word_backward() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Backspace, _)
            | (KeyCode::Char('\x7f' | '\x08'), _)
            | (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
                if self.backspace() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Delete, _) => {
                if self.delete_forward() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                if self.delete_word_backward() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                if self.kill_to_line_start() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                if self.kill_to_line_end() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
            (KeyCode::Char('g'), KeyModifiers::CONTROL) => EditorAction::OpenExternalEditor,
            (KeyCode::Char('\n'), _) => {
                self.insert_newline();
                EditorAction::Changed
            }
            (KeyCode::Char(c), m)
                if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
            {
                self.insert_char(c);
                EditorAction::Changed
            }
            _ => EditorAction::None,
        }
    }

    pub fn desired_text_lines(&self, total_width: u16) -> u16 {
        let text_w = total_width.saturating_sub(4).max(1) as usize;
        let rows = build_visual_rows(&self.text, text_w);
        (rows.len() as u16).clamp(MIN_EDITOR_LINES, MAX_EDITOR_LINES)
    }

    pub fn desired_total_height(&self, total_width: u16) -> u16 {
        self.desired_text_lines(total_width) + 3
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, theme: &UiPalette) {
        self.last_area = area;
        if area.width < 4 || area.height < 3 {
            return;
        }

        let border_color = match self.prefix_mode() {
            PrefixMode::Shell => theme.warning,
            PrefixMode::Command => theme.accent,
            PrefixMode::Mention => theme.user_accent,
            PrefixMode::Normal => {
                if focused {
                    theme.accent
                } else {
                    theme.border
                }
            }
        };

        let mode_badge = match self.prefix_mode() {
            PrefixMode::Command => " Command ",
            PrefixMode::Mention => " Mention ",
            PrefixMode::Shell => " Shell ! ",
            PrefixMode::Normal => {
                if self.running {
                    " Queue / Steer "
                } else {
                    " Prompt "
                }
            }
        };

        let block = Block::default()
            .title(mode_badge)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.surface).fg(theme.fg))
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 3 || inner.height == 0 {
            return;
        }

        let hint_rows = u16::from(inner.height >= 2);
        let editor_h = inner.height.saturating_sub(hint_rows).max(1);
        let editor_area = Rect::new(inner.x, inner.y, inner.width, editor_h);
        let prompt_prefix = match self.prefix_mode() {
            PrefixMode::Shell => "! ",
            PrefixMode::Command => "/ ",
            PrefixMode::Mention | PrefixMode::Normal => "> ",
        };
        let prefix_w = display_width(prompt_prefix);
        let text_w = (editor_area.width as usize).saturating_sub(prefix_w).max(1);

        let visual_rows = build_visual_rows(&self.text, text_w);
        let (cursor_row, cursor_col) = locate_cursor_visual(&visual_rows, self.cursor);

        let visible_h = editor_h as usize;
        if cursor_row < self.scroll_row {
            self.scroll_row = cursor_row;
        } else if cursor_row >= self.scroll_row + visible_h {
            self.scroll_row = cursor_row + 1 - visible_h;
        }
        let max_scroll = visual_rows.len().saturating_sub(visible_h);
        self.scroll_row = self.scroll_row.min(max_scroll);

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(visible_h);
        if self.text.is_empty() {
            let placeholder = if self.running {
                "Type a follow-up (Enter: Queue · Ctrl+Enter: Steer)…"
            } else {
                "Ask Commander anything… (/ commands · @ files/models · ! shell)"
            };
            lines.push(Line::from(vec![
                Span::styled(
                    prompt_prefix.to_owned(),
                    Style::default()
                        .fg(border_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate_to_width(placeholder, text_w),
                    Style::default().fg(theme.subtle),
                ),
            ]));
        } else {
            for (idx, row) in visual_rows
                .iter()
                .enumerate()
                .skip(self.scroll_row)
                .take(visible_h)
            {
                let gutter = if idx == 0 {
                    Span::styled(
                        prompt_prefix.to_owned(),
                        Style::default()
                            .fg(border_color)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled("· ", Style::default().fg(theme.subtle))
                };
                let mut spans = vec![gutter];
                spans.extend(style_editor_row(&row.text, &self.pasted_chunks, theme));
                lines.push(Line::from(spans));
            }
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(theme.surface)),
            editor_area,
        );

        if hint_rows > 0 {
            let hint_area = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
            let hint_text = if self.running {
                if self.queued_messages.is_empty() {
                    "  Enter: Queue · Ctrl+Enter: Steer · Shift+Enter: newline · Ctrl+G: $EDITOR"
                        .to_owned()
                } else {
                    format!(
                        "  [Queued: {}] Alt+↑ recall · Enter: Queue · Ctrl+Enter: Steer",
                        self.queued_messages.len()
                    )
                }
            } else {
                "  Shift+Enter: newline · / commands · @ files/models · ! shell · Ctrl+G: $EDITOR"
                    .to_owned()
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate_to_width(&hint_text, hint_area.width as usize),
                    Style::default().fg(theme.muted).bg(theme.surface),
                ))),
                hint_area,
            );
        }

        if focused {
            let rel_row = cursor_row.saturating_sub(self.scroll_row) as u16;
            let cursor_x = editor_area
                .x
                .saturating_add((prefix_w + cursor_col) as u16)
                .min(editor_area.right().saturating_sub(1));
            let cursor_y = editor_area
                .y
                .saturating_add(rel_row)
                .min(editor_area.bottom().saturating_sub(1));
            frame.set_cursor_position(Position::new(cursor_x, cursor_y));
        }
    }
}

fn style_editor_row(
    row_text: &str,
    chunks: &[PastedChunk],
    theme: &UiPalette,
) -> Vec<Span<'static>> {
    if chunks.is_empty() {
        return vec![Span::styled(
            row_text.to_owned(),
            Style::default().fg(theme.fg),
        )];
    }
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    while cursor < row_text.len() {
        let next_chip = chunks
            .iter()
            .filter_map(|c| {
                row_text[cursor..]
                    .find(&c.placeholder)
                    .map(|off| (cursor + off, c.placeholder.len()))
            })
            .min_by_key(|&(pos, _)| pos);
        match next_chip {
            Some((pos, len)) => {
                if pos > cursor {
                    spans.push(Span::styled(
                        row_text[cursor..pos].to_owned(),
                        Style::default().fg(theme.fg),
                    ));
                }
                spans.push(Span::styled(
                    row_text[pos..pos + len].to_owned(),
                    Style::default()
                        .fg(theme.bg)
                        .bg(theme.accent_soft)
                        .add_modifier(Modifier::BOLD),
                ));
                cursor = pos + len;
            }
            None => {
                spans.push(Span::styled(
                    row_text[cursor..].to_owned(),
                    Style::default().fg(theme.fg),
                ));
                break;
            }
        }
    }
    spans
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub value: String,
    pub label: String,
    pub detail: String,
    pub kind: &'static str,
}

#[derive(Clone, Debug, Default)]
pub struct AutocompletePopup {
    open: bool,
    items: Vec<AutocompleteItem>,
    selected: usize,
    scroll_offset: usize,
    row_hit_rects: Vec<(Rect, usize)>,
}

impl AutocompletePopup {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_open(&self) -> bool {
        self.open && !self.items.is_empty()
    }

    pub fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.selected = 0;
        self.scroll_offset = 0;
        self.row_hit_rects.clear();
    }

    pub fn update(&mut self, query: &str, candidates: &[AutocompleteItem]) {
        let trimmed = query.trim();
        let mut ranked: Vec<(i32, AutocompleteItem)> = candidates
            .iter()
            .filter_map(|item| {
                if trimmed.is_empty() {
                    Some((0, item.clone()))
                } else {
                    let score = fuzzy_match(trimmed, &item.label)
                        .map(|m| m.score + 40)
                        .or_else(|| fuzzy_match(trimmed, &item.value).map(|m| m.score + 20))
                        .or_else(|| fuzzy_match(trimmed, &item.detail).map(|m| m.score))?;
                    Some((score, item.clone()))
                }
            })
            .collect();
        if !trimmed.is_empty() {
            ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
        }
        self.items = ranked.into_iter().map(|(_, item)| item).collect();
        self.open = !self.items.is_empty();
        self.selected = 0;
        self.scroll_offset = 0;
    }

    pub fn items(&self) -> &[AutocompleteItem] {
        &self.items
    }

    pub fn selected_item(&self) -> Option<&AutocompleteItem> {
        self.items.get(self.selected)
    }

    pub fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.items.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn apply_to(&mut self, composer: &mut Composer) -> bool {
        let Some(trigger) = composer.active_trigger() else {
            self.close();
            return false;
        };
        let Some(item) = self.selected_item().cloned() else {
            self.close();
            return false;
        };
        let prefix = match trigger.kind {
            TriggerKind::Command => "/",
            TriggerKind::Mention => "@",
        };
        let inserted = if item.value.starts_with(prefix) {
            format!("{} ", item.value)
        } else {
            format!("{prefix}{} ", item.value)
        };
        composer.replace_range(trigger.range, &inserted);
        self.close();
        true
    }

    pub fn handle_click(&mut self, col: u16, row: u16, composer: &mut Composer) -> bool {
        if let Some((_, idx)) = self
            .row_hit_rects
            .iter()
            .find(|(rect, _)| {
                rect.width > 0
                    && rect.height > 0
                    && col >= rect.x
                    && col < rect.right()
                    && row >= rect.y
                    && row < rect.bottom()
            })
            .cloned()
        {
            self.selected = idx;
            return self.apply_to(composer);
        }
        false
    }

    pub fn row_hit_rects(&self) -> &[(Rect, usize)] {
        &self.row_hit_rects
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        anchor_area: Rect,
        bounds: Rect,
        theme: &UiPalette,
    ) {
        self.row_hit_rects.clear();
        if !self.is_open() || bounds.width < 16 || anchor_area.y <= bounds.y + 2 {
            return;
        }

        let max_rows = self.items.len().clamp(1, 7) as u16;
        let avail_above = anchor_area.y.saturating_sub(bounds.y);
        let popup_h = (max_rows + 2).min(avail_above);
        if popup_h < 3 {
            return;
        }
        let popup_w = anchor_area.width.min(bounds.width).clamp(16, 68);
        let popup_x = anchor_area.x.min(bounds.right().saturating_sub(popup_w));
        let popup_y = anchor_area.y.saturating_sub(popup_h);
        let popup_rect = Rect::new(popup_x, popup_y, popup_w, popup_h);

        frame.render_widget(Clear, popup_rect);
        let block = Block::default()
            .title(" Suggestions (Tab) ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.surface_alt).fg(theme.fg))
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(popup_rect);
        frame.render_widget(block, popup_rect);

        let visible_h = inner.height as usize;
        if visible_h == 0 {
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible_h {
            self.scroll_offset = self.selected + 1 - visible_h;
        }

        let mut lines = Vec::with_capacity(visible_h);
        for (rel_y, (item_idx, item)) in self
            .items
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(visible_h)
            .enumerate()
        {
            let is_sel = item_idx == self.selected;
            let bg = if is_sel {
                theme.selection_bg
            } else {
                theme.surface_alt
            };
            let fg = if is_sel { theme.selection_fg } else { theme.fg };
            let row_rect = Rect::new(inner.x, inner.y + rel_y as u16, inner.width, 1);
            self.row_hit_rects.push((row_rect, item_idx));

            let kind_tag = format!(" [{}] ", item.kind);
            let label_part = format!(" {} ", item.label);
            let width = inner.width as usize;
            let kind_w = display_width(&kind_tag);
            let label_w = display_width(&label_part);
            let detail_budget = width.saturating_sub(kind_w + label_w + 1);
            let detail_str = truncate_to_width(&item.detail, detail_budget);
            let pad = width.saturating_sub(kind_w + label_w + display_width(&detail_str));
            lines.push(Line::from(vec![
                Span::styled(
                    truncate_to_width(&label_part, width),
                    Style::default().fg(fg).bg(bg).add_modifier(if is_sel {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
                Span::styled(detail_str, Style::default().fg(theme.muted).bg(bg)),
                Span::styled(" ".repeat(pad), Style::default().bg(bg)),
                Span::styled(kind_tag, Style::default().fg(theme.accent).bg(bg)),
            ]));
        }

        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(theme.surface_alt)),
            inner,
        );
    }
}

fn split_editor_command(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in raw.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn resolve_editor_command() -> String {
    for key in ["VISUAL", "EDITOR"] {
        if let Ok(val) = env::var(key) {
            if !val.trim().is_empty() {
                return val;
            }
        }
    }
    if env::consts::OS == "windows" {
        "notepad".to_owned()
    } else {
        "vi".to_owned()
    }
}

pub fn edit_with_command(initial: &str, editor_cmd: &str) -> io::Result<Option<String>> {
    let parts = split_editor_command(editor_cmd);
    let Some((program, args)) = parts.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "external editor command is empty",
        ));
    };

    let temp_path = env::temp_dir().join(format!("agent-command-prompt-{}.md", Uuid::new_v4()));
    fs::write(&temp_path, initial)?;

    let status_result = Command::new(program).args(args).arg(&temp_path).status();
    let read_result = fs::read_to_string(&temp_path);
    let _ = fs::remove_file(&temp_path);

    let status = status_result?;
    if !status.success() {
        return Ok(None);
    }
    let updated = normalize_newlines(&read_result?);
    Ok(Some(updated))
}

pub fn edit_in_external_editor(initial: &str) -> io::Result<Option<String>> {
    let cmd = resolve_editor_command();
    edit_with_command(initial, &cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn press(composer: &mut Composer, code: KeyCode, mods: KeyModifiers) -> EditorAction {
        composer.handle_key(KeyEvent::new(code, mods))
    }

    #[test]
    fn grapheme_cursor_navigation_and_editing_handle_unicode_safely() {
        let mut composer = Composer::new();
        composer.insert_str("café 👨‍👩‍👧‍👦 λ");
        assert_eq!(composer.cursor(), composer.text().len());

        press(&mut composer, KeyCode::Left, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Left, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(composer.text(), "café  λ");

        press(&mut composer, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(composer.cursor(), 0);
        press(&mut composer, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(composer.text(), "afé  λ");

        press(&mut composer, KeyCode::End, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Char('\x7f'), KeyModifiers::NONE);
        assert_eq!(composer.text(), "afé  ");
        press(&mut composer, KeyCode::Char('\x08'), KeyModifiers::NONE);
        assert_eq!(composer.text(), "afé ");
    }

    #[test]
    fn readline_shortcuts_word_jumps_and_multiline_newlines_work() {
        let mut composer = Composer::new();
        composer.insert_str("alpha beta gamma");

        press(&mut composer, KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(&composer.text()[composer.cursor()..], "gamma");

        press(&mut composer, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(composer.text(), "alpha gamma");

        press(&mut composer, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(composer.cursor(), 0);
        press(&mut composer, KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(&composer.text()[composer.cursor()..], "gamma");

        press(&mut composer, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(composer.text(), "alpha ");

        press(&mut composer, KeyCode::Enter, KeyModifiers::SHIFT);
        composer.insert_str("second line");
        press(&mut composer, KeyCode::Char('j'), KeyModifiers::CONTROL);
        composer.insert_str("third");
        assert_eq!(composer.text(), "alpha \nsecond line\nthird");

        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(composer.text(), "alpha \nd line\nthird");

        press(&mut composer, KeyCode::Char('e'), KeyModifiers::CONTROL);
        press(&mut composer, KeyCode::Enter, KeyModifiers::ALT);
        assert!(composer.text().contains("d line\n\nthird"));
    }

    #[test]
    fn bracketed_paste_never_submits_and_collapses_large_pastes_into_chips() {
        let mut composer = Composer::new();
        composer.handle_paste("line 1\nline 2\nline 3");
        assert_eq!(composer.text(), "line 1\nline 2\nline 3");
        assert!(composer.pasted_chunks().is_empty());

        composer.clear();
        let big = (1..=15)
            .map(|i| format!("payload line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        composer.insert_str("Review this: ");
        composer.handle_paste(&big);
        assert_eq!(composer.pasted_chunks().len(), 1);
        assert_eq!(composer.text(), "Review this: [Pasted text #1 (15 lines)]");

        let submitted = composer.take_submission().unwrap();
        assert_eq!(submitted, format!("Review this: {big}"));
        assert!(composer.is_empty());
        assert!(composer.pasted_chunks().is_empty());

        let wide_bytes = "x".repeat(LARGE_PASTE_BYTE_THRESHOLD + 64);
        composer.handle_paste(&wide_bytes);
        assert_eq!(composer.pasted_chunks().len(), 1);
        assert!(composer.backspace());
        assert!(composer.is_empty());
        assert!(composer.pasted_chunks().is_empty());
    }

    #[test]
    fn prompt_history_draft_stash_queue_steer_and_prefix_modes_work() {
        let mut composer = Composer::new();
        composer.insert_str("first prompt");
        assert_eq!(
            press(&mut composer, KeyCode::Enter, KeyModifiers::NONE),
            EditorAction::Submit {
                text: "first prompt".into(),
                steer: false
            }
        );
        composer.insert_str("second prompt");
        assert_eq!(
            press(&mut composer, KeyCode::Enter, KeyModifiers::CONTROL),
            EditorAction::Submit {
                text: "second prompt".into(),
                steer: true
            }
        );

        composer.insert_str("unsent draft");
        composer.move_home();
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(composer.text(), "second prompt");
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(composer.text(), "first prompt");
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(composer.text(), "second prompt");
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(composer.text(), "unsent draft");

        composer.switch_thread("thread-a", "thread-b");
        assert!(composer.is_empty());
        composer.insert_str("/help");
        assert_eq!(composer.prefix_mode(), PrefixMode::Command);
        composer.switch_thread("thread-b", "thread-a");
        assert_eq!(composer.text(), "unsent draft");

        composer.set_text("!cargo test");
        assert_eq!(composer.prefix_mode(), PrefixMode::Shell);
        composer.set_text("check @src/ed");
        assert_eq!(composer.prefix_mode(), PrefixMode::Mention);
        let trigger = composer.active_trigger().unwrap();
        assert_eq!(trigger.kind, TriggerKind::Mention);
        assert_eq!(trigger.query, "src/ed");

        composer.set_running(true);
        composer.push_queued("follow up later");
        assert_eq!(
            press(&mut composer, KeyCode::Up, KeyModifiers::ALT),
            EditorAction::RecallQueued("follow up later".into())
        );
        assert_eq!(composer.text(), "follow up later");
    }

    #[test]
    fn autocomplete_popup_filters_applies_and_renders() {
        let mut composer = Composer::new();
        composer.insert_str("inspect @ed");
        let candidates = vec![
            AutocompleteItem {
                value: "src/editor.rs".into(),
                label: "src/editor.rs".into(),
                detail: "Multiline TUI composer".into(),
                kind: "file",
            },
            AutocompleteItem {
                value: "src/markdown.rs".into(),
                label: "src/markdown.rs".into(),
                detail: "Markdown renderer".into(),
                kind: "file",
            },
        ];

        let mut popup = AutocompletePopup::new();
        let trigger = composer.active_trigger().unwrap();
        popup.update(&trigger.query, &candidates);
        assert!(popup.is_open());
        assert_eq!(popup.selected_item().unwrap().value, "src/editor.rs");

        let theme = UiPalette::dark_deck();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let composer_area = Rect::new(0, 18, 80, 5);
                composer.render(f, composer_area, true, &theme);
                popup.render(f, composer_area, f.area(), &theme);
            })
            .unwrap();
        assert!(!popup.row_hit_rects().is_empty());

        assert!(popup.apply_to(&mut composer));
        assert_eq!(composer.text(), "inspect @src/editor.rs ");
        assert!(!popup.is_open());
    }

    #[test]
    fn auto_height_scrolling_and_cursor_positioning_work_across_sizes() {
        let mut composer = Composer::new();
        let lines = (1..=10)
            .map(|i| format!("Line number {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        composer.set_text(lines);
        assert_eq!(composer.desired_text_lines(80), MAX_EDITOR_LINES);
        assert_eq!(composer.desired_total_height(80), MAX_EDITOR_LINES + 3);

        let theme = UiPalette::dark_deck();
        for (w, h) in [(80, 24), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let composer_h = composer.desired_total_height(w);
            let area = Rect::new(0, h - composer_h, w, composer_h);
            terminal
                .draw(|f| composer.render(f, area, true, &theme))
                .unwrap();
            assert!(composer.scroll_row() > 0);
            let pos = terminal.get_cursor_position().unwrap();
            assert!(pos.x >= area.x && pos.x < area.right());
            assert!(pos.y >= area.y && pos.y < area.bottom());
        }
    }

    #[test]
    fn split_editor_command_preserves_quoted_paths_and_flags() {
        let parsed = split_editor_command("\"C:\\Program Files\\Code\\code.exe\" --wait");
        assert_eq!(parsed, vec!["C:\\Program Files\\Code\\code.exe", "--wait"]);
        assert!(edit_with_command("draft", "   ").is_err());
    }

    #[test]
    fn multiple_paste_chips_and_cjk_wide_cursor_render_accurately() {
        let mut composer = Composer::new();
        let block_a = (1..=12)
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let block_b = (1..=11)
            .map(|i| format!("b{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        composer.handle_paste(&block_a);
        composer.insert_str(" + ");
        composer.handle_paste(&block_b);
        assert_eq!(composer.pasted_chunks().len(), 2);
        assert_eq!(
            composer.take_submission().unwrap(),
            format!("{block_a} + {block_b}")
        );

        composer.insert_str("日本語テスト");
        let theme = UiPalette::dark_deck();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let area = Rect::new(0, 18, 80, 5);
        terminal
            .draw(|f| composer.render(f, area, true, &theme))
            .unwrap();
        let pos = terminal.get_cursor_position().unwrap();
        assert_eq!(pos.x, area.x + 1 + 2 + 12);
    }
}
