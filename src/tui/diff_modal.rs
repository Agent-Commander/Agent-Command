use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::markdown::{render_unified_diff, UiPalette};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffModalAction {
    None,
    Closed,
    CopyDiff,
}

#[derive(Clone, Debug, Default)]
pub struct DiffModal {
    open: bool,
    title: String,
    diff_text: String,
    scroll: usize,
    total_lines: usize,
    viewport_height: usize,
    modal_area: Rect,
}

impl DiffModal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, title: impl Into<String>, diff_text: impl Into<String>) {
        self.open = true;
        self.title = title.into();
        self.diff_text = diff_text.into();
        self.scroll = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn diff_text(&self) -> &str {
        &self.diff_text
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    fn max_scroll(&self) -> usize {
        self.total_lines.saturating_sub(self.viewport_height)
    }

    pub fn handle_scroll(&mut self, delta: isize) {
        if delta < 0 {
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs());
        } else if delta > 0 {
            self.scroll = self
                .scroll
                .saturating_add(delta as usize)
                .min(self.max_scroll());
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DiffModalAction {
        if !self.open {
            return DiffModalAction::None;
        }
        let step = self.viewport_height.saturating_sub(2).max(1);
        let half = (self.viewport_height / 2).max(1);
        match (key.code, key.modifiers) {
            (KeyCode::Esc | KeyCode::Char('q'), _) => {
                self.close();
                DiffModalAction::Closed
            }
            (KeyCode::Char('y'), _) => DiffModalAction::CopyDiff,
            (KeyCode::Up | KeyCode::Char('k'), _) => {
                self.scroll = self.scroll.saturating_sub(1);
                DiffModalAction::None
            }
            (KeyCode::Down | KeyCode::Char('j'), _) => {
                self.scroll = self.scroll.saturating_add(1).min(self.max_scroll());
                DiffModalAction::None
            }
            (KeyCode::PageUp, _) => {
                self.scroll = self.scroll.saturating_sub(step);
                DiffModalAction::None
            }
            (KeyCode::PageDown, _) => {
                self.scroll = self.scroll.saturating_add(step).min(self.max_scroll());
                DiffModalAction::None
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.scroll = self.scroll.saturating_sub(half);
                DiffModalAction::None
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.scroll = self.scroll.saturating_add(half).min(self.max_scroll());
                DiffModalAction::None
            }
            (KeyCode::Home | KeyCode::Char('g'), _) => {
                self.scroll = 0;
                DiffModalAction::None
            }
            (KeyCode::End | KeyCode::Char('G'), _) => {
                self.scroll = self.max_scroll();
                DiffModalAction::None
            }
            _ => DiffModalAction::None,
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DiffModalAction {
        if !self.open {
            return DiffModalAction::None;
        }
        let inside = self.modal_area.width > 0
            && self.modal_area.height > 0
            && col >= self.modal_area.x
            && col < self.modal_area.right()
            && row >= self.modal_area.y
            && row < self.modal_area.bottom();
        if !inside {
            self.close();
            return DiffModalAction::Closed;
        }
        DiffModalAction::None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &UiPalette) {
        if !self.open || area.width < 16 || area.height < 8 {
            self.modal_area = Rect::default();
            return;
        }
        let body_band = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(2),
        );
        frame.render_widget(Clear, body_band);
        frame.render_widget(
            Block::default().style(Style::default().bg(theme.bg)),
            body_band,
        );
        let modal = body_band;
        self.modal_area = modal;
        let title = if self.title.is_empty() {
            " Unified Diff ".to_owned()
        } else {
            format!(" {} ", self.title)
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg).fg(theme.fg))
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        let body_area = chunks[0];
        let footer_area = chunks[1];

        let rendered = if self.diff_text.trim().is_empty() {
            vec![Line::from(Span::styled(
                "  No uncommitted or branch changes in this workspace.",
                Style::default().fg(theme.muted),
            ))]
        } else {
            render_unified_diff(&self.diff_text, body_area.width as usize, theme)
        };

        self.total_lines = rendered.len();
        self.viewport_height = body_area.height as usize;
        self.scroll = self.scroll.min(self.max_scroll());

        let end = (self.scroll + self.viewport_height).min(rendered.len());
        let visible = rendered[self.scroll..end].to_vec();
        frame.render_widget(
            Paragraph::new(visible).style(Style::default().bg(theme.bg).fg(theme.fg)),
            body_area,
        );

        let position = if self.total_lines == 0 {
            "0/0".to_owned()
        } else {
            format!("{}-{}/{}", self.scroll + 1, end, self.total_lines)
        };
        let footer = format!(" ↑↓/jk scroll · PgUp/PgDn · y copy diff · Esc close   {position}");
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                footer,
                Style::default()
                    .fg(theme.muted)
                    .bg(theme.surface)
                    .add_modifier(Modifier::BOLD),
            ))),
            footer_area,
        );
    }
}
