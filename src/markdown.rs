use std::collections::BTreeSet;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

use crate::{color, Theme};

const MIN_RENDER_WIDTH: usize = 1;
const COLLAPSED_ERROR_PREVIEW_CHARS: usize = 120;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiPalette {
    pub bg: Color,
    pub surface: Color,
    pub surface_alt: Color,
    pub code_bg: Color,
    pub fg: Color,
    pub muted: Color,
    pub subtle: Color,
    pub border: Color,
    pub accent: Color,
    pub accent_soft: Color,
    pub user_accent: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub info: Color,
    pub diff_add_fg: Color,
    pub diff_add_bg: Color,
    pub diff_del_fg: Color,
    pub diff_del_bg: Color,
    pub diff_hunk_fg: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
}

impl Default for UiPalette {
    fn default() -> Self {
        Self::dark_deck()
    }
}

impl UiPalette {
    pub fn dark_deck() -> Self {
        Self {
            bg: Color::Rgb(0x10, 0x10, 0x11),
            surface: Color::Rgb(0x17, 0x18, 0x1B),
            surface_alt: Color::Rgb(0x1E, 0x20, 0x26),
            code_bg: Color::Rgb(0x19, 0x1B, 0x21),
            fg: Color::Rgb(0xE6, 0xE8, 0xEC),
            muted: Color::Rgb(0x8D, 0x96, 0xA5),
            subtle: Color::Rgb(0x5E, 0x67, 0x76),
            border: Color::Rgb(0x30, 0x35, 0x40),
            accent: Color::Rgb(0x7A, 0xA2, 0xF7),
            accent_soft: Color::Rgb(0xA9, 0xC1, 0xFA),
            user_accent: Color::Rgb(0x73, 0xDA, 0xCA),
            success: Color::Rgb(0x9E, 0xCE, 0x6A),
            warning: Color::Rgb(0xE0, 0xAF, 0x68),
            danger: Color::Rgb(0xF7, 0x76, 0x8E),
            info: Color::Rgb(0x7D, 0xCF, 0xFF),
            diff_add_fg: Color::Rgb(0x9E, 0xCE, 0x6A),
            diff_add_bg: Color::Rgb(0x14, 0x28, 0x1D),
            diff_del_fg: Color::Rgb(0xF7, 0x76, 0x8E),
            diff_del_bg: Color::Rgb(0x2E, 0x16, 0x1C),
            diff_hunk_fg: Color::Rgb(0x7D, 0xCF, 0xFF),
            selection_bg: Color::Rgb(0x25, 0x33, 0x46),
            selection_fg: Color::Rgb(0xF5, 0xF7, 0xFA),
        }
    }

    pub fn from_theme(theme: &Theme) -> Self {
        let mut palette = Self::dark_deck();
        palette.bg = color(&theme.background);
        palette.surface = color(&theme.panel);
        palette.fg = color(&theme.foreground);
        palette.muted = color(&theme.muted);
        palette.accent = color(&theme.accent);
        palette.border = color(&theme.border);
        palette.selection_bg = color(&theme.selection);
        palette.warning = color(&theme.amber);
        palette
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Grep,
    Glob,
    Edit,
    Write,
    Bash,
    GitHub,
    Mcp,
    Delegate,
    Other,
}

impl ToolKind {
    pub fn is_exploration(self) -> bool {
        matches!(self, Self::Read | Self::Grep | Self::Glob)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "Read",
            Self::Grep => "Grep",
            Self::Glob => "Glob",
            Self::Edit => "Edit",
            Self::Write => "Write",
            Self::Bash => "Bash",
            Self::GitHub => "GitHub",
            Self::Mcp => "MCP",
            Self::Delegate => "Delegate",
            Self::Other => "Tool",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl DelegationStatus {
    pub fn badge(self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Running => "RUNNING",
            Self::Completed => "READY",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveActivity {
    pub spinner: &'static str,
    pub label: String,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptEntry {
    UserMessage {
        text: String,
        timestamp: Option<String>,
        #[serde(default)]
        attachments: Vec<String>,
    },
    AssistantMarkdown {
        speaker: String,
        markdown: String,
        streaming: bool,
    },
    ThinkingBlock {
        text: String,
        duration_ms: u64,
        collapsed: bool,
    },
    ToolCallCard {
        call_id: String,
        kind: ToolKind,
        name: String,
        summary: String,
        output: String,
        ok: Option<bool>,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
        collapsed: bool,
    },
    DelegationCard {
        worker: String,
        role: String,
        provider: String,
        branch: String,
        status: DelegationStatus,
        summary: String,
    },
    InlineConnectCard {
        id: String,
        title: String,
        detail: String,
        provider_or_tool: String,
        resolved: bool,
    },
    ProdGateCard {
        req_id: String,
        tool: String,
        command: String,
        reason: String,
        resolved: bool,
    },
    SystemNotice {
        level: NoticeLevel,
        text: String,
    },
    TableCard {
        title: String,
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        #[serde(default)]
        alignments: Vec<String>,
        #[serde(default)]
        caption: Option<String>,
    },
    InfoCard {
        title: String,
        #[serde(default)]
        level: String,
        #[serde(default)]
        badge: Option<String>,
        #[serde(default)]
        fields: Vec<(String, String)>,
        #[serde(default)]
        body_markdown: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptHitAction {
    ToggleExpand {
        entry_index: usize,
    },
    Connect {
        card_id: String,
        provider_or_tool: String,
    },
    DismissConnect {
        card_id: String,
    },
    AllowProdGate {
        req_id: String,
    },
    DenyProdGate {
        req_id: String,
    },
    SelectDelegation {
        worker: String,
    },
    OpenLink {
        target: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DocPos {
    pub line: usize,
    pub col: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectionButton {
    #[default]
    Quote,
    Copy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectionPopoverRects {
    pub container: Rect,
    pub quote: Rect,
    pub copy: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: DocPos,
    pub focus: DocPos,
    pub dragging: bool,
    pub popover_open: bool,
    pub focused_button: SelectionButton,
}

impl TextSelection {
    pub fn ordered(&self) -> (DocPos, DocPos) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }

    pub fn toggle_button(&mut self) {
        self.focused_button = match self.focused_button {
            SelectionButton::Quote => SelectionButton::Copy,
            SelectionButton::Copy => SelectionButton::Quote,
        };
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderedCell {
    grapheme: String,
    col_start: usize,
    width: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderedLine {
    cells: Vec<RenderedCell>,
    soft_wrapped: bool,
    copy_prefix_cols: usize,
    copy_end_col: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineLinkSpan {
    col_start: usize,
    col_width: usize,
    target: String,
}

#[derive(Clone, Debug)]
struct BuiltLine {
    line: Line<'static>,
    soft_wrapped: bool,
    copy_prefix_cols: usize,
    copy_end_col: Option<usize>,
    links: Vec<LineLinkSpan>,
}

impl BuiltLine {
    fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            soft_wrapped: false,
            copy_prefix_cols: 0,
            copy_end_col: None,
            links: Vec::new(),
        }
    }

    fn prepend_span(&mut self, span: Span<'static>, count_as_copy_prefix: bool) {
        let shift = span.width();
        self.line.spans.insert(0, span);
        for link in &mut self.links {
            link.col_start = link.col_start.saturating_add(shift);
        }
        if count_as_copy_prefix {
            self.copy_prefix_cols = self.copy_prefix_cols.saturating_add(shift);
        }
        if let Some(end_col) = self.copy_end_col {
            self.copy_end_col = Some(end_col.saturating_add(shift));
        }
    }

    fn width(&self) -> usize {
        self.line.spans.iter().map(Span::width).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HitRect {
    pub area: Rect,
    pub action: TranscriptHitAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineHitRegion {
    line_index: usize,
    col_start: usize,
    col_width: usize,
    action: TranscriptHitAction,
}

#[derive(Clone, Debug)]
pub struct Transcript {
    pub entries: Vec<TranscriptEntry>,
    pub exploration_grouping: bool,
    expanded_exploration_groups: BTreeSet<usize>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            exploration_grouping: true,
            expanded_exploration_groups: BTreeSet::new(),
        }
    }

    pub fn push(&mut self, entry: TranscriptEntry) -> usize {
        self.entries.push(entry);
        self.entries.len() - 1
    }

    pub fn push_user(&mut self, text: impl Into<String>, timestamp: Option<String>) -> usize {
        self.push_user_with_attachments(text, timestamp, Vec::new())
    }

    pub fn push_user_with_attachments(
        &mut self,
        text: impl Into<String>,
        timestamp: Option<String>,
        attachments: Vec<String>,
    ) -> usize {
        self.push(TranscriptEntry::UserMessage {
            text: text.into(),
            timestamp,
            attachments,
        })
    }

    pub fn append_assistant_delta(&mut self, speaker: &str, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(TranscriptEntry::AssistantMarkdown {
            speaker: current_speaker,
            markdown,
            streaming,
        }) = self.entries.last_mut()
        {
            if *streaming && current_speaker == speaker {
                markdown.push_str(delta);
                return;
            }
        }
        self.push(TranscriptEntry::AssistantMarkdown {
            speaker: speaker.to_owned(),
            markdown: delta.to_owned(),
            streaming: true,
        });
    }

    pub fn finish_assistant_stream(&mut self) {
        for entry in self.entries.iter_mut().rev() {
            if let TranscriptEntry::AssistantMarkdown { streaming, .. } = entry {
                *streaming = false;
                break;
            }
        }
    }

    pub fn append_thinking_delta(&mut self, delta: &str, duration_ms: u64) {
        if let Some(TranscriptEntry::ThinkingBlock {
            text,
            duration_ms: current_ms,
            ..
        }) = self.entries.last_mut()
        {
            text.push_str(delta);
            *current_ms = (*current_ms).max(duration_ms);
            return;
        }
        self.push(TranscriptEntry::ThinkingBlock {
            text: delta.to_owned(),
            duration_ms,
            collapsed: true,
        });
    }

    pub fn upsert_tool_call(
        &mut self,
        call_id: String,
        kind: ToolKind,
        name: String,
        summary: String,
    ) -> usize {
        if !call_id.is_empty() {
            for (idx, entry) in self.entries.iter_mut().enumerate().rev() {
                if let TranscriptEntry::ToolCallCard {
                    call_id: existing_id,
                    kind: existing_kind,
                    name: existing_name,
                    summary: existing_summary,
                    ..
                } = entry
                {
                    if existing_id == &call_id {
                        if kind != ToolKind::Other {
                            *existing_kind = kind;
                        }
                        if !name.is_empty() {
                            *existing_name = name;
                        }
                        if !summary.is_empty() {
                            *existing_summary = summary;
                        }
                        return idx;
                    }
                }
            }
        }
        self.push(TranscriptEntry::ToolCallCard {
            call_id,
            kind,
            name,
            summary,
            output: String::new(),
            ok: None,
            exit_code: None,
            duration_ms: None,
            collapsed: true,
        })
    }

    pub fn normalize_loaded(mut entries: Vec<TranscriptEntry>) -> Vec<TranscriptEntry> {
        for entry in &mut entries {
            match entry {
                TranscriptEntry::AssistantMarkdown { streaming, .. } => {
                    *streaming = false;
                }
                TranscriptEntry::ToolCallCard { ok, .. } if ok.is_none() => {
                    *ok = Some(false);
                }
                _ => {}
            }
        }
        entries
    }

    pub fn complete_tool_call(
        &mut self,
        call_id: &str,
        output: impl Into<String>,
        ok: bool,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
    ) -> bool {
        let output_text = output.into();
        for entry in self.entries.iter_mut().rev() {
            if let TranscriptEntry::ToolCallCard {
                call_id: existing_id,
                output: existing_output,
                ok: existing_ok,
                exit_code: existing_exit,
                duration_ms: existing_duration,
                ..
            } = entry
            {
                if existing_id == call_id {
                    *existing_output = output_text;
                    *existing_ok = Some(ok);
                    *existing_exit = exit_code;
                    *existing_duration = duration_ms;
                    return true;
                }
            }
        }
        false
    }

    pub fn resolve_connect_card(&mut self, card_id: &str) -> bool {
        for entry in &mut self.entries {
            if let TranscriptEntry::InlineConnectCard { id, resolved, .. } = entry {
                if id == card_id {
                    *resolved = true;
                    return true;
                }
            }
        }
        false
    }

    pub fn resolve_prod_gate(&mut self, target_req_id: &str) -> bool {
        for entry in &mut self.entries {
            if let TranscriptEntry::ProdGateCard {
                req_id, resolved, ..
            } = entry
            {
                if req_id == target_req_id {
                    *resolved = true;
                    return true;
                }
            }
        }
        false
    }

    pub fn toggle_expand_at_or_latest(&mut self, entry_index: Option<usize>) -> bool {
        if let Some(idx) = entry_index {
            if self.is_exploration_group_start(idx) {
                if !self.expanded_exploration_groups.remove(&idx) {
                    self.expanded_exploration_groups.insert(idx);
                }
                return true;
            }
            if let Some(entry) = self.entries.get_mut(idx) {
                return toggle_entry_collapsed(entry);
            }
            return false;
        }
        for idx in (0..self.entries.len()).rev() {
            if self.is_exploration_group_start(idx) {
                if !self.expanded_exploration_groups.remove(&idx) {
                    self.expanded_exploration_groups.insert(idx);
                }
                return true;
            }
            if let Some(entry) = self.entries.get_mut(idx) {
                if toggle_entry_collapsed(entry) {
                    return true;
                }
            }
        }
        false
    }

    fn is_exploration_group_start(&self, index: usize) -> bool {
        if !self.exploration_grouping {
            return false;
        }
        let is_exp = |idx: usize| {
            matches!(
                self.entries.get(idx),
                Some(TranscriptEntry::ToolCallCard { kind, .. }) if kind.is_exploration()
            )
        };
        if !is_exp(index) {
            return false;
        }
        if index > 0 && is_exp(index - 1) {
            return false;
        }
        is_exp(index + 1)
    }

    pub fn latest_assistant_text(&self) -> Option<&str> {
        self.entries.iter().rev().find_map(|entry| match entry {
            TranscriptEntry::AssistantMarkdown { markdown, .. } => Some(markdown.as_str()),
            _ => None,
        })
    }

    pub fn plain_text(&self) -> String {
        let mut out = Vec::new();
        for entry in &self.entries {
            match entry {
                TranscriptEntry::UserMessage {
                    text, attachments, ..
                } => {
                    if attachments.is_empty() {
                        out.push(format!("You:\n{text}"));
                    } else {
                        out.push(format!("You [{}]:\n{text}", attachments.join(", ")));
                    }
                }
                TranscriptEntry::AssistantMarkdown {
                    speaker, markdown, ..
                } => {
                    out.push(format!("{speaker}:\n{markdown}"));
                }
                TranscriptEntry::ThinkingBlock {
                    text, duration_ms, ..
                } => {
                    out.push(format!(
                        "[Thought {}]\n{}",
                        format_duration(*duration_ms),
                        text
                    ));
                }
                TranscriptEntry::ToolCallCard {
                    name,
                    summary,
                    output,
                    ..
                } => {
                    out.push(format!("[Tool: {name} {summary}]\n{output}"));
                }
                TranscriptEntry::DelegationCard {
                    worker,
                    role,
                    status,
                    summary,
                    ..
                } => {
                    out.push(format!(
                        "[Delegated: {worker} ({role}) {}] {summary}",
                        status.badge()
                    ));
                }
                TranscriptEntry::InlineConnectCard { title, detail, .. } => {
                    out.push(format!("[Connect: {title}] {detail}"));
                }
                TranscriptEntry::ProdGateCard {
                    tool,
                    command,
                    reason,
                    ..
                } => {
                    out.push(format!("[ProdGate: {tool}] {command} ({reason})"));
                }
                TranscriptEntry::SystemNotice { text, .. } => {
                    out.push(format!("[System] {text}"));
                }
                TranscriptEntry::TableCard {
                    title,
                    headers,
                    rows,
                    caption,
                    ..
                } => {
                    let mut block = vec![format!("[Table: {title}]")];
                    if !headers.is_empty() {
                        block.push(headers.join(" | "));
                    }
                    for row in rows {
                        block.push(row.join(" | "));
                    }
                    if let Some(cap) = caption.as_deref().filter(|s| !s.trim().is_empty()) {
                        block.push(cap.to_owned());
                    }
                    out.push(block.join("\n"));
                }
                TranscriptEntry::InfoCard {
                    title,
                    badge,
                    fields,
                    body_markdown,
                    ..
                } => {
                    let badge_part = badge
                        .as_deref()
                        .filter(|b| !b.trim().is_empty())
                        .map(|b| format!(" [{b}]"))
                        .unwrap_or_default();
                    let mut block = vec![format!("[{title}{badge_part}]")];
                    for (k, v) in fields {
                        block.push(format!("{k}: {v}"));
                    }
                    if !body_markdown.trim().is_empty() {
                        block.push(body_markdown.clone());
                    }
                    out.push(block.join("\n"));
                }
            }
        }
        out.join("\n\n")
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.expanded_exploration_groups.clear();
    }
}

fn toggle_entry_collapsed(entry: &mut TranscriptEntry) -> bool {
    match entry {
        TranscriptEntry::ThinkingBlock { collapsed, .. }
        | TranscriptEntry::ToolCallCard { collapsed, .. } => {
            *collapsed = !*collapsed;
            true
        }
        _ => false,
    }
}

#[derive(Clone, Debug)]
pub struct ScrollState {
    offset: usize,
    sticky_bottom: bool,
    total_lines: usize,
    viewport_height: usize,
    message_anchors: Vec<usize>,
    hit_rects: Vec<HitRect>,
    content_area: Rect,
    rendered_lines: Vec<RenderedLine>,
    selection: Option<TextSelection>,
    popover_rects: SelectionPopoverRects,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollState {
    pub fn new() -> Self {
        Self {
            offset: 0,
            sticky_bottom: true,
            total_lines: 0,
            viewport_height: 0,
            message_anchors: Vec::new(),
            hit_rects: Vec::new(),
            content_area: Rect::default(),
            rendered_lines: Vec::new(),
            selection: None,
            popover_rects: SelectionPopoverRects::default(),
        }
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn is_sticky_bottom(&self) -> bool {
        self.sticky_bottom
    }

    pub fn total_lines(&self) -> usize {
        self.total_lines
    }

    pub fn viewport_height(&self) -> usize {
        self.viewport_height
    }

    pub fn max_offset(&self) -> usize {
        self.total_lines.saturating_sub(self.viewport_height)
    }

    pub fn selection(&self) -> Option<&TextSelection> {
        self.selection.as_ref()
    }

    pub fn has_selection(&self) -> bool {
        self.selection.as_ref().is_some_and(|sel| !sel.is_empty())
    }

    pub fn popover_open(&self) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|sel| sel.popover_open && !sel.is_empty())
    }

    pub fn popover_rects(&self) -> SelectionPopoverRects {
        self.popover_rects
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.popover_rects = SelectionPopoverRects::default();
    }

    pub fn focus_popover_button(&mut self, button: SelectionButton) {
        if let Some(sel) = &mut self.selection {
            sel.focused_button = button;
        }
    }

    pub fn toggle_popover_button(&mut self) {
        if let Some(sel) = &mut self.selection {
            sel.toggle_button();
        }
    }

    pub fn set_selection(&mut self, anchor: DocPos, focus: DocPos, open_popover: bool) {
        if self.rendered_lines.is_empty() {
            self.clear_selection();
            return;
        }
        let max_line = self.rendered_lines.len().saturating_sub(1);
        let a = DocPos {
            line: anchor.line.min(max_line),
            col: anchor.col,
        };
        let f = DocPos {
            line: focus.line.min(max_line),
            col: focus.col,
        };
        if a == f {
            self.clear_selection();
            return;
        }
        self.selection = Some(TextSelection {
            anchor: a,
            focus: f,
            dragging: false,
            popover_open: open_popover,
            focused_button: SelectionButton::Quote,
        });
    }

    pub fn screen_to_doc(&self, col: u16, row: u16) -> Option<DocPos> {
        if self.content_area.width == 0
            || self.content_area.height == 0
            || self.rendered_lines.is_empty()
        {
            return None;
        }
        let clamped_row = row.clamp(
            self.content_area.y,
            self.content_area.bottom().saturating_sub(1),
        );
        let clamped_col = col.clamp(
            self.content_area.x,
            self.content_area.right().saturating_sub(1),
        );
        let rel_row = (clamped_row - self.content_area.y) as usize;
        let rel_col = (clamped_col - self.content_area.x) as usize;
        let doc_line = (self.offset + rel_row).min(self.rendered_lines.len().saturating_sub(1));
        Some(DocPos {
            line: doc_line,
            col: rel_col,
        })
    }

    pub fn begin_selection(&mut self, col: u16, row: u16) -> bool {
        if !rect_contains_point(self.content_area, col, row) {
            return false;
        }
        let Some(pos) = self.screen_to_doc(col, row) else {
            return false;
        };
        self.popover_rects = SelectionPopoverRects::default();
        self.selection = Some(TextSelection {
            anchor: pos,
            focus: pos,
            dragging: true,
            popover_open: false,
            focused_button: SelectionButton::Quote,
        });
        true
    }

    pub fn update_selection_drag(&mut self, col: u16, row: u16) -> bool {
        let dragging = self.selection.as_ref().is_some_and(|sel| sel.dragging);
        if !dragging || self.content_area.height == 0 {
            return false;
        }
        if row <= self.content_area.y && self.offset > 0 {
            self.scroll_up(1);
        } else if row >= self.content_area.bottom().saturating_sub(1)
            && self.offset < self.max_offset()
        {
            self.scroll_down(1);
        }
        let Some(pos) = self.screen_to_doc(col, row) else {
            return false;
        };
        if let Some(sel) = &mut self.selection {
            sel.focus = pos;
            return true;
        }
        false
    }

    pub fn finish_selection(&mut self, col: u16, row: u16) -> bool {
        let Some(mut sel) = self.selection.take() else {
            return false;
        };
        if sel.dragging {
            if let Some(pos) = self.screen_to_doc(col, row) {
                sel.focus = pos;
            }
            sel.dragging = false;
        }
        if sel.is_empty() {
            self.popover_rects = SelectionPopoverRects::default();
            return false;
        }
        sel.popover_open = true;
        self.selection = Some(sel);
        if self.extract_selection_text().is_none() {
            self.clear_selection();
            return false;
        }
        true
    }

    pub fn hit_test_popover(&self, col: u16, row: u16) -> Option<SelectionButton> {
        if !self.popover_open() {
            return None;
        }
        if rect_contains_point(self.popover_rects.quote, col, row) {
            return Some(SelectionButton::Quote);
        }
        if rect_contains_point(self.popover_rects.copy, col, row) {
            return Some(SelectionButton::Copy);
        }
        None
    }

    pub fn extract_selection_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?;
        if sel.is_empty() || self.rendered_lines.is_empty() {
            return None;
        }
        let (start, end) = sel.ordered();
        let last_valid = self.rendered_lines.len().saturating_sub(1);
        let first_line = start.line.min(last_valid);
        let last_line = end.line.min(last_valid);

        let mut output = String::new();
        for line_idx in first_line..=last_line {
            let rendered = &self.rendered_lines[line_idx];
            let (min_col, max_col) = selection_line_bounds(
                rendered, line_idx, first_line, last_line, start.col, end.col,
            );

            let mut line_text = String::new();
            for cell in &rendered.cells {
                let cell_end = cell.col_start + cell.width.max(1);
                let overlaps = cell_end > min_col && cell.col_start <= max_col;
                if overlaps {
                    line_text.push_str(&cell.grapheme);
                }
            }
            let normalized = line_text.replace('\u{00a0}', " ");
            let trimmed = normalized.trim_end();
            if line_idx > first_line {
                if rendered.soft_wrapped {
                    if !output.is_empty()
                        && !output.ends_with(char::is_whitespace)
                        && !trimmed.starts_with(char::is_whitespace)
                    {
                        output.push(' ');
                    }
                } else {
                    output.push('\n');
                }
            }
            output.push_str(trimmed);
        }
        let final_text = output.trim_matches('\n').trim_end().to_owned();
        if final_text.trim().is_empty() {
            None
        } else {
            Some(final_text)
        }
    }

    pub fn links_in_viewport(&self) -> Vec<(Rect, String)> {
        self.hit_rects
            .iter()
            .filter_map(|hit| match &hit.action {
                TranscriptHitAction::OpenLink { target } => Some((hit.area, target.clone())),
                _ => None,
            })
            .collect()
    }

    pub fn scroll_up(&mut self, lines: usize) {
        if lines == 0 {
            return;
        }
        self.offset = self.offset.saturating_sub(lines);
        if self.max_offset() > 0 && self.offset < self.max_offset() {
            self.sticky_bottom = false;
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        if lines == 0 {
            return;
        }
        let max_off = self.max_offset();
        self.offset = self.offset.saturating_add(lines).min(max_off);
        if self.offset >= max_off {
            self.sticky_bottom = true;
        }
    }

    pub fn page_up(&mut self) {
        let step = self.viewport_height.saturating_sub(2).max(1);
        self.scroll_up(step);
    }

    pub fn page_down(&mut self) {
        let step = self.viewport_height.saturating_sub(2).max(1);
        self.scroll_down(step);
    }

    pub fn half_page_up(&mut self) {
        let step = (self.viewport_height / 2).max(1);
        self.scroll_up(step);
    }

    pub fn half_page_down(&mut self) {
        let step = (self.viewport_height / 2).max(1);
        self.scroll_down(step);
    }

    pub fn jump_to_bottom(&mut self) {
        self.offset = self.max_offset();
        self.sticky_bottom = true;
    }

    pub fn jump_to_top(&mut self) {
        self.offset = 0;
        self.sticky_bottom = self.max_offset() == 0;
    }

    pub fn jump_prev_message(&mut self) {
        if let Some(&target) = self
            .message_anchors
            .iter()
            .rev()
            .find(|&&anchor| anchor < self.offset)
        {
            self.offset = target.min(self.max_offset());
            self.sticky_bottom = self.offset >= self.max_offset();
        } else {
            self.jump_to_top();
        }
    }

    pub fn jump_next_message(&mut self) {
        if let Some(&target) = self
            .message_anchors
            .iter()
            .find(|&&anchor| anchor > self.offset)
        {
            self.offset = target.min(self.max_offset());
            self.sticky_bottom = self.offset >= self.max_offset();
        } else {
            self.jump_to_bottom();
        }
    }

    pub fn sync_metrics(
        &mut self,
        total_lines: usize,
        viewport_height: usize,
        message_anchors: Vec<usize>,
    ) {
        self.total_lines = total_lines;
        self.viewport_height = viewport_height;
        self.message_anchors = message_anchors;
        let max_off = self.max_offset();
        if self.sticky_bottom {
            self.offset = max_off;
        } else {
            self.offset = self.offset.min(max_off);
            if self.offset >= max_off && max_off == 0 {
                self.sticky_bottom = true;
            }
        }
    }

    pub fn hit_rects(&self) -> &[HitRect] {
        &self.hit_rects
    }

    pub fn hit_test(&self, col: u16, row: u16) -> Option<&TranscriptHitAction> {
        self.hit_rects
            .iter()
            .find(|hit| {
                hit.area.width > 0
                    && hit.area.height > 0
                    && col >= hit.area.x
                    && col < hit.area.right()
                    && row >= hit.area.y
                    && row < hit.area.bottom()
            })
            .map(|hit| &hit.action)
    }

    pub fn toggle_expand_at_or_latest(
        &self,
        transcript: &mut Transcript,
        entry_index: Option<usize>,
    ) -> bool {
        transcript.toggle_expand_at_or_latest(entry_index)
    }
}

fn rect_contains_point(rect: Rect, col: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.right()
        && row >= rect.y
        && row < rect.bottom()
}

fn selection_line_bounds(
    rendered: &RenderedLine,
    line_idx: usize,
    first_line: usize,
    last_line: usize,
    start_col: usize,
    end_col: usize,
) -> (usize, usize) {
    let raw_start = if line_idx == first_line { start_col } else { 0 };
    let min_col = if line_idx > first_line {
        raw_start.max(rendered.copy_prefix_cols)
    } else if first_line != last_line && raw_start < rendered.copy_prefix_cols {
        rendered.copy_prefix_cols
    } else {
        raw_start
    };
    let mut max_col = if line_idx == last_line {
        end_col
    } else {
        usize::MAX
    };
    if let Some(end_limit) = rendered.copy_end_col {
        if end_limit > 0 {
            if line_idx < last_line {
                max_col = max_col.min(end_limit - 1);
            } else if first_line != last_line && max_col >= end_limit {
                max_col = end_limit - 1;
            }
        }
    }
    (min_col, max_col)
}

fn build_rendered_line(built: &BuiltLine) -> RenderedLine {
    let mut cells = Vec::new();
    let mut col = 0usize;
    for span in &built.line.spans {
        for g in span.content.as_ref().graphemes(true) {
            let w = display_width(g);
            cells.push(RenderedCell {
                grapheme: g.to_owned(),
                col_start: col,
                width: w,
            });
            col += w;
        }
    }
    RenderedLine {
        cells,
        soft_wrapped: built.soft_wrapped,
        copy_prefix_cols: built.copy_prefix_cols,
        copy_end_col: built.copy_end_col,
    }
}

fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

fn truncate_str(text: &str, max_width: usize) -> String {
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

fn format_duration(duration_ms: u64) -> String {
    if duration_ms < 1000 {
        format!("{duration_ms}ms")
    } else {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    }
}

#[derive(Clone, Debug)]
struct RichSpan {
    text: String,
    style: Style,
    link_target: Option<String>,
}

impl RichSpan {
    fn plain(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            link_target: None,
        }
    }

    fn linked(text: impl Into<String>, style: Style, target: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style,
            link_target: Some(target.into()),
        }
    }
}

#[derive(Clone, Debug)]
struct StyledPiece {
    text: String,
    style: Style,
    breakable: bool,
    link_target: Option<String>,
}

fn push_piece(
    pieces: &mut Vec<StyledPiece>,
    text: String,
    style: Style,
    breakable: bool,
    link_target: Option<String>,
) {
    if text.is_empty() {
        return;
    }
    pieces.push(StyledPiece {
        text,
        style,
        breakable,
        link_target,
    });
}

fn tokenize_rich_for_wrap(spans: &[RichSpan]) -> Vec<StyledPiece> {
    let mut pieces = Vec::new();
    for span in spans {
        let content = span.text.as_str();
        let style = span.style;
        let mut current = String::new();
        let mut in_space: Option<bool> = None;
        for g in content.graphemes(true) {
            let is_space = g.chars().all(|c| c == ' ' || c == '\t');
            if let Some(prev_space) = in_space {
                if prev_space != is_space {
                    push_piece(
                        &mut pieces,
                        std::mem::take(&mut current),
                        style,
                        prev_space,
                        span.link_target.clone(),
                    );
                }
            }
            if is_space && g == "\t" {
                current.push_str("    ");
            } else {
                current.push_str(g);
            }
            in_space = Some(is_space);
        }
        if let Some(prev_space) = in_space {
            push_piece(
                &mut pieces,
                current,
                style,
                prev_space,
                span.link_target.clone(),
            );
        }
    }
    pieces
}

fn push_span_merged(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.style == style {
            last.content.to_mut().push_str(text);
            return;
        }
    }
    spans.push(Span::styled(text.to_owned(), style));
}

fn record_link_span(
    links: &mut Vec<LineLinkSpan>,
    col_start: usize,
    col_width: usize,
    target: &str,
) {
    if col_width == 0 || target.is_empty() {
        return;
    }
    if let Some(last) = links.last_mut() {
        if last.target == target && last.col_start + last.col_width == col_start {
            last.col_width += col_width;
            return;
        }
    }
    links.push(LineLinkSpan {
        col_start,
        col_width,
        target: target.to_owned(),
    });
}

fn wrap_rich_spans_with_prefix(
    spans: &[RichSpan],
    width: usize,
    first_prefix: &[Span<'static>],
    cont_prefix: &[Span<'static>],
) -> Vec<BuiltLine> {
    let max_width = width.max(MIN_RENDER_WIDTH);
    let first_prefix_w: usize = first_prefix.iter().map(Span::width).sum();
    let cont_prefix_w: usize = cont_prefix.iter().map(Span::width).sum();

    let mut lines: Vec<BuiltLine> = Vec::new();
    let mut current: Vec<Span<'static>> = first_prefix.to_vec();
    let mut current_links: Vec<LineLinkSpan> = Vec::new();
    let mut current_w = first_prefix_w;
    let mut at_line_content_start = true;

    let pieces = tokenize_rich_for_wrap(spans);
    if pieces.is_empty() {
        lines.push(BuiltLine {
            line: Line::from(current),
            soft_wrapped: false,
            copy_prefix_cols: first_prefix_w,
            copy_end_col: None,
            links: Vec::new(),
        });
        return lines;
    }

    let push_finished =
        |lines: &mut Vec<BuiltLine>, current: Vec<Span<'static>>, links: Vec<LineLinkSpan>| {
            let is_cont = !lines.is_empty();
            lines.push(BuiltLine {
                line: Line::from(current),
                soft_wrapped: is_cont,
                copy_prefix_cols: if is_cont {
                    cont_prefix_w
                } else {
                    first_prefix_w
                },
                copy_end_col: None,
                links,
            });
        };

    for piece in pieces {
        let piece_w = display_width(&piece.text);
        if piece.breakable && at_line_content_start {
            continue;
        }
        let avail_for_cont = max_width.saturating_sub(cont_prefix_w).max(1);
        if current_w + piece_w <= max_width {
            if let Some(target) = piece.link_target.as_deref() {
                record_link_span(&mut current_links, current_w, piece_w, target);
            }
            push_span_merged(&mut current, &piece.text, piece.style);
            current_w += piece_w;
            at_line_content_start = false;
            continue;
        }
        if piece.breakable {
            push_finished(
                &mut lines,
                std::mem::replace(&mut current, cont_prefix.to_vec()),
                std::mem::take(&mut current_links),
            );
            current_w = cont_prefix_w;
            at_line_content_start = true;
            continue;
        }
        if !at_line_content_start && piece_w <= avail_for_cont {
            push_finished(
                &mut lines,
                std::mem::replace(&mut current, cont_prefix.to_vec()),
                std::mem::take(&mut current_links),
            );
            current_w = cont_prefix_w;
            if let Some(target) = piece.link_target.as_deref() {
                record_link_span(&mut current_links, current_w, piece_w, target);
            }
            push_span_merged(&mut current, &piece.text, piece.style);
            current_w += piece_w;
            at_line_content_start = false;
            continue;
        }
        for g in piece.text.graphemes(true) {
            let gw = display_width(g);
            if !at_line_content_start && current_w + gw > max_width {
                push_finished(
                    &mut lines,
                    std::mem::replace(&mut current, cont_prefix.to_vec()),
                    std::mem::take(&mut current_links),
                );
                current_w = cont_prefix_w;
            }
            if let Some(target) = piece.link_target.as_deref() {
                record_link_span(&mut current_links, current_w, gw, target);
            }
            push_span_merged(&mut current, g, piece.style);
            current_w += gw;
            at_line_content_start = false;
        }
    }

    if !current.is_empty() || lines.is_empty() {
        push_finished(&mut lines, current, current_links);
    }
    lines
}

fn trim_trailing_link_punct(raw: &str) -> &str {
    let mut end = raw.len();
    loop {
        let slice = &raw[..end];
        let Some(last_ch) = slice.chars().next_back() else {
            break;
        };
        let should_strip = match last_ch {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => slice.matches(')').count() > slice.matches('(').count(),
            ']' => slice.matches(']').count() > slice.matches('[').count(),
            '}' => slice.matches('}').count() > slice.matches('{').count(),
            _ => false,
        };
        if should_strip {
            end -= last_ch.len_utf8();
        } else {
            break;
        }
    }
    &raw[..end]
}

fn strip_location_suffix_for_path_check(candidate: &str) -> &str {
    let without_hash = if let Some((base, frag)) = candidate.rsplit_once("#L") {
        let rest = frag
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == 'C' || c == '-' || c == 'L');
        if rest.is_empty() && !frag.is_empty() && frag.starts_with(|c: char| c.is_ascii_digit()) {
            base
        } else {
            candidate
        }
    } else {
        candidate
    };
    let mut cur = without_hash;
    for _ in 0..2 {
        if let Some((head, tail)) = cur.rsplit_once(':') {
            if !head.is_empty()
                && !tail.is_empty()
                && tail.chars().all(|c| c.is_ascii_digit())
                && !(head.len() == 1 && head.chars().all(|c| c.is_ascii_alphabetic()))
            {
                cur = head;
                continue;
            }
        }
        break;
    }
    cur
}

fn is_known_root_filename(name: &str) -> bool {
    matches!(
        name,
        "Cargo.toml"
            | "Cargo.lock"
            | "README.md"
            | "Makefile"
            | "Dockerfile"
            | "package.json"
            | "tsconfig.json"
            | ".gitignore"
            | "AGENTS.md"
            | "CLAUDE.md"
            | "TEAM.md"
            | "REVIEW.md"
    )
}

fn is_likely_file_path_token(candidate: &str) -> bool {
    if candidate.is_empty()
        || candidate.len() > 260
        || candidate.starts_with("http://")
        || candidate.starts_with("https://")
        || candidate.starts_with("mailto:")
        || candidate.starts_with("//")
        || candidate.starts_with("\\\\")
    {
        return false;
    }
    if candidate
        .chars()
        .any(|c| c.is_control() || matches!(c, '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return false;
    }
    let base = strip_location_suffix_for_path_check(candidate);
    if base.is_empty() {
        return false;
    }
    if is_known_root_filename(base) {
        return true;
    }
    let has_sep = base.contains('/') || base.contains('\\');
    if !has_sep {
        return false;
    }
    let trimmed = base.trim_end_matches(['/', '\\']);
    let file_name = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    if file_name.is_empty() || file_name == "." || file_name == ".." {
        return false;
    }
    if is_known_root_filename(file_name) {
        return true;
    }
    let Some((stem, ext)) = file_name.rsplit_once('.') else {
        return false;
    };
    if stem.is_empty() || ext.is_empty() || ext.len() > 10 {
        return false;
    }
    ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
        && stem
            .chars()
            .any(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_likely_url_token(candidate: &str) -> bool {
    if let Some(rest) = candidate
        .strip_prefix("https://")
        .or_else(|| candidate.strip_prefix("http://"))
    {
        !rest.is_empty() && rest.chars().next().is_some_and(|c| !c.is_whitespace())
    } else {
        false
    }
}

fn is_token_boundary_char(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\'' | '`' | '{' | '}'
        )
}

fn split_autolinks(text: &str, base_style: Style, theme: &UiPalette) -> Vec<RichSpan> {
    if text.is_empty() {
        return Vec::new();
    }
    let link_style = base_style.fg(theme.info).add_modifier(Modifier::UNDERLINED);
    let mut out = Vec::new();
    let mut plain_start = 0usize;
    let mut idx = 0usize;
    let bytes = text.as_bytes();

    while idx < bytes.len() {
        let rest = &text[idx..];
        let ch = match rest.chars().next() {
            Some(c) => c,
            None => break,
        };
        if is_token_boundary_char(ch) {
            idx += ch.len_utf8();
            continue;
        }
        let token_start = idx;
        let mut token_end = idx;
        for (off, c) in rest.char_indices() {
            if c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`') {
                break;
            }
            token_end = token_start + off + c.len_utf8();
        }
        let raw_token = &text[token_start..token_end];
        let leading_trim_bytes: usize = raw_token
            .chars()
            .take_while(|c| matches!(c, '(' | '[' | '{'))
            .map(char::len_utf8)
            .sum();
        let inner_start = token_start + leading_trim_bytes;
        let candidate = trim_trailing_link_punct(&text[inner_start..token_end]);
        let candidate_end = inner_start + candidate.len();

        let is_link = is_likely_url_token(candidate) || is_likely_file_path_token(candidate);
        if is_link && !candidate.is_empty() {
            if inner_start > plain_start {
                out.push(RichSpan::plain(&text[plain_start..inner_start], base_style));
            }
            out.push(RichSpan::linked(candidate, link_style, candidate));
            plain_start = candidate_end;
        }
        idx = token_end;
    }

    if plain_start < text.len() {
        out.push(RichSpan::plain(&text[plain_start..], base_style));
    }
    out
}

fn is_code_keyword(word: &str, lang: &str) -> bool {
    match lang {
        "rs" | "rust" => matches!(
            word,
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        ),
        "py" | "python" => matches!(
            word,
            "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        "js" | "jsx" | "ts" | "tsx" | "javascript" | "typescript" => matches!(
            word,
            "async"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "default"
                | "delete"
                | "do"
                | "else"
                | "enum"
                | "export"
                | "extends"
                | "finally"
                | "for"
                | "from"
                | "function"
                | "if"
                | "implements"
                | "import"
                | "in"
                | "instanceof"
                | "interface"
                | "let"
                | "new"
                | "of"
                | "private"
                | "protected"
                | "public"
                | "readonly"
                | "return"
                | "static"
                | "switch"
                | "throw"
                | "try"
                | "type"
                | "typeof"
                | "var"
                | "void"
                | "while"
                | "yield"
        ),
        "sh" | "bash" | "zsh" | "ps1" | "powershell" | "pwsh" => matches!(
            word,
            "if" | "then"
                | "else"
                | "elif"
                | "fi"
                | "for"
                | "do"
                | "done"
                | "while"
                | "case"
                | "esac"
                | "in"
                | "function"
                | "local"
                | "export"
                | "return"
                | "param"
                | "foreach"
                | "try"
                | "catch"
                | "finally"
                | "throw"
        ),
        "sql" => matches!(
            word.to_ascii_uppercase().as_str(),
            "SELECT"
                | "FROM"
                | "WHERE"
                | "INSERT"
                | "INTO"
                | "VALUES"
                | "UPDATE"
                | "SET"
                | "DELETE"
                | "CREATE"
                | "TABLE"
                | "INDEX"
                | "JOIN"
                | "LEFT"
                | "RIGHT"
                | "INNER"
                | "ON"
                | "GROUP"
                | "ORDER"
                | "BY"
                | "LIMIT"
                | "AND"
                | "OR"
                | "NOT"
                | "AS"
        ),
        "go" => matches!(
            word,
            "break"
                | "case"
                | "chan"
                | "const"
                | "continue"
                | "default"
                | "defer"
                | "else"
                | "for"
                | "func"
                | "go"
                | "if"
                | "import"
                | "interface"
                | "map"
                | "package"
                | "range"
                | "return"
                | "select"
                | "struct"
                | "switch"
                | "type"
                | "var"
        ),
        _ => matches!(
            word,
            "fn" | "let"
                | "const"
                | "var"
                | "if"
                | "else"
                | "for"
                | "while"
                | "return"
                | "struct"
                | "class"
                | "enum"
                | "pub"
                | "def"
                | "function"
                | "import"
                | "export"
                | "async"
                | "await"
                | "match"
                | "use"
                | "mod"
        ),
    }
}

fn is_code_constant(word: &str) -> bool {
    matches!(
        word,
        "true"
            | "false"
            | "True"
            | "False"
            | "null"
            | "nil"
            | "None"
            | "Some"
            | "Ok"
            | "Err"
            | "self"
            | "Self"
            | "this"
            | "undefined"
            | "NULL"
    )
}

fn is_code_type(word: &str, lang: &str) -> bool {
    if matches!(
        word,
        "bool"
            | "char"
            | "str"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f32"
            | "f64"
            | "String"
            | "Vec"
            | "Option"
            | "Result"
            | "Path"
            | "PathBuf"
            | "HashMap"
            | "BTreeMap"
            | "Arc"
            | "Box"
            | "number"
            | "string"
            | "boolean"
            | "any"
            | "unknown"
            | "never"
            | "int"
            | "float"
            | "dict"
            | "list"
    ) {
        return true;
    }
    if matches!(lang, "sql" | "sh" | "bash" | "zsh" | "json") {
        return false;
    }
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_uppercase() && chars.any(|c| c.is_ascii_lowercase())
}

fn highlight_code_line(line: &str, lang: &str, theme: &UiPalette) -> Vec<Span<'static>> {
    let norm_lang = lang.trim().to_ascii_lowercase();
    let base = Style::default().fg(theme.fg).bg(theme.code_bg);
    let comment_style = Style::default()
        .fg(theme.subtle)
        .bg(theme.code_bg)
        .add_modifier(Modifier::ITALIC);
    let string_style = Style::default().fg(theme.success).bg(theme.code_bg);
    let keyword_style = Style::default()
        .fg(theme.accent)
        .bg(theme.code_bg)
        .add_modifier(Modifier::BOLD);
    let type_style = Style::default().fg(theme.info).bg(theme.code_bg);
    let number_style = Style::default().fg(theme.warning).bg(theme.code_bg);
    let macro_style = Style::default().fg(theme.user_accent).bg(theme.code_bg);

    let hash_comments = matches!(
        norm_lang.as_str(),
        "py" | "python"
            | "sh"
            | "bash"
            | "zsh"
            | "ps1"
            | "powershell"
            | "pwsh"
            | "toml"
            | "yaml"
            | "yml"
    );
    let slash_comments = matches!(
        norm_lang.as_str(),
        "" | "rs"
            | "rust"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "javascript"
            | "typescript"
            | "go"
            | "c"
            | "cpp"
            | "h"
            | "java"
            | "jsonc"
    );
    let sql_comments = norm_lang == "sql";
    let is_rust = matches!(norm_lang.as_str(), "rs" | "rust");
    let allow_backtick_str = matches!(
        norm_lang.as_str(),
        "js" | "jsx" | "ts" | "tsx" | "javascript" | "typescript" | "go" | "sh" | "bash"
    );

    let mut spans: Vec<Span<'static>> = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let rest = &line[i..];
        if slash_comments && rest.starts_with("//") {
            push_span_merged(&mut spans, rest, comment_style);
            break;
        }
        if hash_comments && rest.starts_with('#') {
            push_span_merged(&mut spans, rest, comment_style);
            break;
        }
        if sql_comments && rest.starts_with("--") {
            push_span_merged(&mut spans, rest, comment_style);
            break;
        }
        if is_rust && rest.starts_with("#[") {
            let mut end = i + 2;
            let mut depth = 1usize;
            while end < bytes.len() && depth > 0 {
                match bytes[end] {
                    b'[' => depth += 1,
                    b']' => depth -= 1,
                    _ => {}
                }
                end += 1;
            }
            push_span_merged(&mut spans, &line[i..end], macro_style);
            i = end;
            continue;
        }
        if bytes[i] == b'"' || (allow_backtick_str && bytes[i] == b'`') {
            let quote = bytes[i];
            let mut end = i + 1;
            let mut escaped = false;
            while end < bytes.len() {
                let b = bytes[end];
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == quote {
                    end += 1;
                    break;
                }
                end += 1;
            }
            let style = if norm_lang == "json" && line[end..].trim_start().starts_with(':') {
                type_style
            } else {
                string_style
            };
            push_span_merged(&mut spans, &line[i..end], style);
            i = end;
            continue;
        }
        if bytes[i] == b'\'' {
            if is_rust {
                let tail = &line[i + 1..];
                let mut chars = tail.chars();
                let is_char_lit = match chars.next() {
                    Some('\\') => chars.nth(1) == Some('\'') || tail.contains('\''),
                    Some(_) => chars.next() == Some('\''),
                    None => false,
                };
                if !is_char_lit {
                    let mut end = i + 1;
                    while end < bytes.len()
                        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                    {
                        end += 1;
                    }
                    push_span_merged(&mut spans, &line[i..end], macro_style);
                    i = end;
                    continue;
                }
            }
            let mut end = i + 1;
            let mut escaped = false;
            while end < bytes.len() {
                let b = bytes[end];
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'\'' {
                    end += 1;
                    break;
                }
                end += 1;
            }
            push_span_merged(&mut spans, &line[i..end], string_style);
            i = end;
            continue;
        }
        if bytes[i] == b'@' && matches!(norm_lang.as_str(), "py" | "python" | "ts" | "tsx") {
            let mut end = i + 1;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'.')
            {
                end += 1;
            }
            push_span_merged(&mut spans, &line[i..end], macro_style);
            i = end;
            continue;
        }
        if bytes[i].is_ascii_digit() {
            let mut end = i + 1;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'.')
            {
                end += 1;
            }
            push_span_merged(&mut spans, &line[i..end], number_style);
            i = end;
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let mut end = i + 1;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let word = &line[i..end];
            if is_rust && end < bytes.len() && bytes[end] == b'!' {
                push_span_merged(&mut spans, &line[i..end + 1], macro_style);
                i = end + 1;
                continue;
            }
            let style = if is_code_keyword(word, &norm_lang) {
                keyword_style
            } else if is_code_constant(word) || is_code_type(word, &norm_lang) {
                type_style
            } else {
                base
            };
            push_span_merged(&mut spans, word, style);
            i = end;
            continue;
        }
        let ch = match rest.chars().next() {
            Some(c) => c,
            None => break,
        };
        let next = i + ch.len_utf8();
        push_span_merged(&mut spans, &line[i..next], base);
        i = next;
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

fn wrap_styled_code_line(
    spans: &[Span<'static>],
    width: usize,
    prefix: &[Span<'static>],
    pad_bg: Option<Color>,
) -> Vec<BuiltLine> {
    let max_width = width.max(MIN_RENDER_WIDTH);
    let prefix_w: usize = prefix.iter().map(Span::width).sum();
    let content_budget = max_width.saturating_sub(prefix_w).max(1);

    let mut out: Vec<BuiltLine> = Vec::new();
    let mut current: Vec<Span<'static>> = prefix.to_vec();
    let mut used = 0usize;

    let finalize = |out: &mut Vec<BuiltLine>, mut line_spans: Vec<Span<'static>>, used_w: usize| {
        if let Some(bg) = pad_bg {
            let pad = content_budget.saturating_sub(used_w);
            if pad > 0 {
                line_spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
            }
        }
        let is_cont = !out.is_empty();
        out.push(BuiltLine {
            line: Line::from(line_spans),
            soft_wrapped: is_cont,
            copy_prefix_cols: prefix_w,
            copy_end_col: None,
            links: Vec::new(),
        });
    };

    for span in spans {
        let expanded = span.content.replace('\t', "    ");
        for g in expanded.graphemes(true) {
            let gw = display_width(g);
            if used > 0 && used + gw > content_budget {
                finalize(
                    &mut out,
                    std::mem::replace(&mut current, prefix.to_vec()),
                    used,
                );
                used = 0;
            }
            push_span_merged(&mut current, g, span.style);
            used += gw;
        }
    }

    finalize(&mut out, current, used);
    out
}

fn wrap_preserving_indent_built(
    text: &str,
    width: usize,
    style: Style,
    prefix: &[Span<'static>],
    pad_bg: Option<Color>,
) -> Vec<BuiltLine> {
    wrap_styled_code_line(
        &[Span::styled(text.to_owned(), style)],
        width,
        prefix,
        pad_bg,
    )
}

fn rich_spans_width(spans: &[RichSpan]) -> usize {
    spans.iter().map(|s| display_width(&s.text)).sum()
}

fn trim_rich_spans(spans: Vec<RichSpan>) -> Vec<RichSpan> {
    let mut trimmed = spans;
    while let Some(first) = trimmed.first_mut() {
        let stripped = first.text.trim_start();
        if stripped.is_empty() {
            trimmed.remove(0);
        } else {
            if stripped.len() != first.text.len() {
                first.text = stripped.to_owned();
            }
            break;
        }
    }
    while let Some(last) = trimmed.last_mut() {
        let stripped = last.text.trim_end();
        if stripped.is_empty() {
            trimmed.pop();
        } else {
            if stripped.len() != last.text.len() {
                last.text = stripped.to_owned();
            }
            break;
        }
    }
    trimmed
}

fn merge_cell_style(span_style: Style, base_style: Style) -> Style {
    let mut style = base_style;
    if let Some(fg) = span_style.fg {
        style = style.fg(fg);
    }
    if let Some(bg) = span_style.bg {
        style = style.bg(bg);
    }
    style = style.add_modifier(span_style.add_modifier);
    style
}

fn fit_rich_spans_in_cell(
    cell: &[RichSpan],
    width: usize,
    alignment: Alignment,
    base_style: Style,
    col_offset: usize,
) -> (Vec<Span<'static>>, Vec<LineLinkSpan>) {
    if width == 0 {
        return (Vec::new(), Vec::new());
    }
    let full_w = rich_spans_width(cell);
    let mut clipped: Vec<(String, Style, Option<String>)> = Vec::new();
    let used_w = if full_w <= width {
        for sp in cell {
            if !sp.text.is_empty() {
                clipped.push((
                    sp.text.clone(),
                    merge_cell_style(sp.style, base_style),
                    sp.link_target.clone(),
                ));
            }
        }
        full_w
    } else {
        let ellipsis_w = display_width("…");
        let budget = width.saturating_sub(ellipsis_w);
        let mut used = 0usize;
        'outer: for sp in cell {
            let mut chunk = String::new();
            for g in sp.text.graphemes(true) {
                let gw = display_width(g);
                if used + gw > budget {
                    if !chunk.is_empty() {
                        clipped.push((
                            chunk,
                            merge_cell_style(sp.style, base_style),
                            sp.link_target.clone(),
                        ));
                    }
                    break 'outer;
                }
                chunk.push_str(g);
                used += gw;
            }
            if !chunk.is_empty() {
                clipped.push((
                    chunk,
                    merge_cell_style(sp.style, base_style),
                    sp.link_target.clone(),
                ));
            }
        }
        clipped.push(("…".to_owned(), base_style, None));
        used + ellipsis_w
    };

    let pad = width.saturating_sub(used_w);
    let (left_pad, right_pad) = match alignment {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        Alignment::None | Alignment::Left => (0, pad),
    };

    let mut out_spans = Vec::new();
    let mut out_links = Vec::new();
    let mut cursor = col_offset;
    if left_pad > 0 {
        out_spans.push(Span::styled(" ".repeat(left_pad), base_style));
        cursor += left_pad;
    }
    for (text, style, link) in clipped {
        let w = display_width(&text);
        if let Some(target) = link.as_deref() {
            record_link_span(&mut out_links, cursor, w, target);
        }
        push_span_merged(&mut out_spans, &text, style);
        cursor += w;
    }
    if right_pad > 0 {
        out_spans.push(Span::styled(" ".repeat(right_pad), base_style));
    }
    (out_spans, out_links)
}

fn render_rich_table(
    header: &[Vec<RichSpan>],
    rows: &[Vec<Vec<RichSpan>>],
    alignments: &[Alignment],
    max_width: usize,
    theme: &UiPalette,
) -> Vec<BuiltLine> {
    let col_count = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if col_count == 0 {
        return Vec::new();
    }

    let mut widths = vec![1usize; col_count];
    for (i, cell) in header.iter().enumerate() {
        widths[i] = widths[i].max(rich_spans_width(cell));
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(rich_spans_width(cell));
        }
    }

    let separators_w = col_count.saturating_sub(1) * 3 + 2;
    let max_cells_w = max_width.saturating_sub(separators_w).max(col_count);
    let total_cells_w: usize = widths.iter().sum();
    if total_cells_w > max_cells_w {
        let mut remaining = total_cells_w - max_cells_w;
        while remaining > 0 {
            if let Some((idx, &max_w)) = widths.iter().enumerate().max_by_key(|&(_, w)| *w) {
                if max_w <= 3 {
                    break;
                }
                widths[idx] -= 1;
                remaining -= 1;
            } else {
                break;
            }
        }
    }

    let border_style = Style::default().fg(theme.border);
    let render_row = |cells: &[Vec<RichSpan>], row_style: Style| -> BuiltLine {
        let mut spans = vec![Span::styled(" ", row_style)];
        let mut links = Vec::new();
        let mut col = 1usize;
        for (idx, &col_w) in widths.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::styled(" │ ", border_style));
                col += 3;
            }
            let cell = cells.get(idx).map(Vec::as_slice).unwrap_or(&[]);
            let align = alignments.get(idx).copied().unwrap_or(Alignment::Left);
            let (cell_spans, cell_links) =
                fit_rich_spans_in_cell(cell, col_w, align, row_style, col);
            spans.extend(cell_spans);
            links.extend(cell_links);
            col += col_w;
        }
        BuiltLine {
            line: Line::from(spans),
            soft_wrapped: false,
            copy_prefix_cols: 1,
            copy_end_col: None,
            links,
        }
    };

    let mut out = Vec::new();
    if !header.is_empty() {
        out.push(render_row(
            header,
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ));
        let mut sep = String::from("─");
        for (idx, &col_w) in widths.iter().enumerate() {
            if idx > 0 {
                sep.push_str("─┼─");
            }
            sep.push_str(&"─".repeat(col_w));
        }
        out.push(BuiltLine::plain(Line::from(Span::styled(
            sep,
            border_style,
        ))));
    }
    for row in rows {
        out.push(render_row(row, Style::default().fg(theme.fg)));
    }
    out
}

fn parse_inline_markdown_spans(text: &str, base_style: Style, theme: &UiPalette) -> Vec<RichSpan> {
    let options = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(text, options);
    let mut style_stack = vec![base_style];
    let mut link_stack: Vec<String> = Vec::new();
    let mut out: Vec<RichSpan> = Vec::new();

    for event in parser {
        match event {
            Event::Start(Tag::Strong) => {
                let next = style_stack
                    .last()
                    .copied()
                    .unwrap_or(base_style)
                    .add_modifier(Modifier::BOLD);
                style_stack.push(next);
            }
            Event::End(TagEnd::Strong) => {
                if style_stack.len() > 1 {
                    style_stack.pop();
                }
            }
            Event::Start(Tag::Emphasis) => {
                let next = style_stack
                    .last()
                    .copied()
                    .unwrap_or(base_style)
                    .add_modifier(Modifier::ITALIC);
                style_stack.push(next);
            }
            Event::End(TagEnd::Emphasis) => {
                if style_stack.len() > 1 {
                    style_stack.pop();
                }
            }
            Event::Start(Tag::Strikethrough) => {
                let next = style_stack
                    .last()
                    .copied()
                    .unwrap_or(base_style)
                    .fg(theme.muted)
                    .add_modifier(Modifier::CROSSED_OUT);
                style_stack.push(next);
            }
            Event::End(TagEnd::Strikethrough) => {
                if style_stack.len() > 1 {
                    style_stack.pop();
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                link_stack.push(dest_url.to_string());
                let next = style_stack
                    .last()
                    .copied()
                    .unwrap_or(base_style)
                    .fg(theme.info)
                    .add_modifier(Modifier::UNDERLINED);
                style_stack.push(next);
            }
            Event::End(TagEnd::Link) => {
                if style_stack.len() > 1 {
                    style_stack.pop();
                }
                link_stack.pop();
            }
            Event::Code(code) => {
                let style = Style::default().fg(theme.warning).bg(theme.code_bg);
                let link = link_stack.last().cloned().or_else(|| {
                    let trimmed = code.trim();
                    if is_likely_url_token(trimmed) || is_likely_file_path_token(trimmed) {
                        Some(trimmed.to_owned())
                    } else {
                        None
                    }
                });
                out.push(RichSpan {
                    text: code.to_string(),
                    style,
                    link_target: link,
                });
            }
            Event::Text(segment) => {
                let style = style_stack.last().copied().unwrap_or(base_style);
                if let Some(target) = link_stack.last() {
                    out.push(RichSpan::linked(segment.to_string(), style, target.clone()));
                } else {
                    out.extend(split_autolinks(&segment, style, theme));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                let style = style_stack.last().copied().unwrap_or(base_style);
                out.push(RichSpan::plain(" ", style));
            }
            _ => {}
        }
    }
    trim_rich_spans(out)
}

#[derive(Clone, Debug)]
struct ListContext {
    ordered: bool,
    next_index: u64,
}

#[derive(Default)]
struct TableBuffer {
    alignments: Vec<Alignment>,
    header: Vec<Vec<RichSpan>>,
    rows: Vec<Vec<Vec<RichSpan>>>,
    current_row: Vec<Vec<RichSpan>>,
    current_cell: Vec<RichSpan>,
    in_head: bool,
    in_cell: bool,
}

struct MarkdownRenderer<'a> {
    width: usize,
    theme: &'a UiPalette,
    lines: Vec<BuiltLine>,
    pending_spans: Vec<RichSpan>,
    style_stack: Vec<Style>,
    quote_depth: usize,
    list_stack: Vec<ListContext>,
    pending_item_bullet: Option<String>,
    in_code_block: bool,
    code_lang: String,
    code_buffer: String,
    link_url_stack: Vec<String>,
    heading_level: Option<HeadingLevel>,
    in_table: bool,
    table: TableBuffer,
}

impl<'a> MarkdownRenderer<'a> {
    fn new(width: usize, theme: &'a UiPalette) -> Self {
        Self {
            width: width.max(MIN_RENDER_WIDTH),
            theme,
            lines: Vec::new(),
            pending_spans: Vec::new(),
            style_stack: vec![Style::default().fg(theme.fg)],
            quote_depth: 0,
            list_stack: Vec::new(),
            pending_item_bullet: None,
            in_code_block: false,
            code_lang: String::new(),
            code_buffer: String::new(),
            link_url_stack: Vec::new(),
            heading_level: None,
            in_table: false,
            table: TableBuffer::default(),
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack
            .last()
            .copied()
            .unwrap_or_else(|| Style::default().fg(self.theme.fg))
    }

    fn push_style_mod(&mut self, updater: impl FnOnce(Style) -> Style) {
        let next = updater(self.current_style());
        self.style_stack.push(next);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn build_prefixes(&mut self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let mut first = Vec::new();
        let mut cont = Vec::new();
        for _ in 0..self.quote_depth {
            let bar = Span::styled("│ ", Style::default().fg(self.theme.accent));
            first.push(bar.clone());
            cont.push(bar);
        }
        if !self.list_stack.is_empty() {
            let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
            if !indent.is_empty() {
                first.push(Span::raw(indent.clone()));
                cont.push(Span::raw(indent));
            }
            if let Some(bullet) = self.pending_item_bullet.take() {
                let bw = display_width(&bullet);
                first.push(Span::styled(bullet, Style::default().fg(self.theme.accent)));
                cont.push(Span::raw(" ".repeat(bw)));
            } else {
                first.push(Span::raw("  "));
                cont.push(Span::raw("  "));
            }
        }
        (first, cont)
    }

    fn ensure_blank_line(&mut self) {
        if let Some(last) = self.lines.last() {
            let is_empty = last.line.spans.iter().all(|s| s.content.trim().is_empty());
            if !is_empty {
                if self.quote_depth > 0 {
                    let bars: Vec<Span<'static>> = (0..self.quote_depth)
                        .map(|_| Span::styled("│ ", Style::default().fg(self.theme.accent)))
                        .collect();
                    self.lines.push(BuiltLine {
                        line: Line::from(bars),
                        soft_wrapped: false,
                        copy_prefix_cols: self.quote_depth * 2,
                        copy_end_col: None,
                        links: Vec::new(),
                    });
                } else {
                    self.lines.push(BuiltLine::plain(Line::default()));
                }
            }
        }
    }

    fn flush_pending(&mut self) {
        if self.pending_spans.is_empty() && self.pending_item_bullet.is_none() {
            return;
        }
        let spans = std::mem::take(&mut self.pending_spans);
        let (first_prefix, cont_prefix) = self.build_prefixes();
        let wrapped = wrap_rich_spans_with_prefix(&spans, self.width, &first_prefix, &cont_prefix);
        self.lines.extend(wrapped);
    }

    fn flush_code_block(&mut self) {
        let code = std::mem::take(&mut self.code_buffer);
        let lang = std::mem::take(&mut self.code_lang);
        let trimmed_code = code.strip_suffix('\n').unwrap_or(&code);

        let mut quote_prefix = Vec::new();
        for _ in 0..self.quote_depth {
            quote_prefix.push(Span::styled("│ ", Style::default().fg(self.theme.accent)));
        }
        if !self.list_stack.is_empty() {
            let indent = "  ".repeat(self.list_stack.len());
            quote_prefix.push(Span::raw(indent));
        }
        let prefix_w: usize = quote_prefix.iter().map(Span::width).sum();
        let inner_w = self.width.saturating_sub(prefix_w).max(4);

        let label = if lang.trim().is_empty() {
            "code".to_owned()
        } else {
            lang.trim().to_owned()
        };
        let header_prefix = format!("╭─ {label} ");
        let header_fill = inner_w.saturating_sub(display_width(&header_prefix));
        let mut header_spans = quote_prefix.clone();
        header_spans.push(Span::styled(
            format!("{header_prefix}{}", "─".repeat(header_fill)),
            Style::default().fg(self.theme.muted).bg(self.theme.code_bg),
        ));
        self.lines.push(BuiltLine::plain(Line::from(header_spans)));

        let mut code_line_prefix = quote_prefix.clone();
        code_line_prefix.push(Span::styled(
            "│ ",
            Style::default()
                .fg(self.theme.border)
                .bg(self.theme.code_bg),
        ));
        let code_style = Style::default().fg(self.theme.fg).bg(self.theme.code_bg);

        if trimmed_code.is_empty() {
            let wrapped = wrap_preserving_indent_built(
                "",
                self.width,
                code_style,
                &code_line_prefix,
                Some(self.theme.code_bg),
            );
            self.lines.extend(wrapped);
        } else {
            for raw_line in trimmed_code.split('\n') {
                let clean_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
                let highlighted = highlight_code_line(clean_line, &lang, self.theme);
                let wrapped = wrap_styled_code_line(
                    &highlighted,
                    self.width,
                    &code_line_prefix,
                    Some(self.theme.code_bg),
                );
                self.lines.extend(wrapped);
            }
        }

        let mut footer_spans = quote_prefix;
        footer_spans.push(Span::styled(
            format!("╰{}", "─".repeat(inner_w.saturating_sub(1))),
            Style::default()
                .fg(self.theme.border)
                .bg(self.theme.code_bg),
        ));
        self.lines.push(BuiltLine::plain(Line::from(footer_spans)));
    }

    fn flush_table(&mut self) {
        let table = std::mem::take(&mut self.table);
        let built = render_rich_table(
            &table.header,
            &table.rows,
            &table.alignments,
            self.width,
            self.theme,
        );
        self.lines.extend(built);
    }

    fn process(&mut self, event: Event<'_>) {
        if self.in_code_block {
            match event {
                Event::Text(text) => self.code_buffer.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    self.in_code_block = false;
                    self.flush_code_block();
                    self.ensure_blank_line();
                }
                _ => {}
            }
            return;
        }

        if self.in_table {
            match event {
                Event::Start(Tag::TableHead) => {
                    self.table.in_head = true;
                    self.table.current_row.clear();
                }
                Event::End(TagEnd::TableHead) => {
                    self.table.header = std::mem::take(&mut self.table.current_row);
                    self.table.in_head = false;
                }
                Event::Start(Tag::TableRow) => {
                    self.table.current_row.clear();
                }
                Event::End(TagEnd::TableRow) => {
                    let row = std::mem::take(&mut self.table.current_row);
                    if !self.table.in_head {
                        self.table.rows.push(row);
                    }
                }
                Event::Start(Tag::TableCell) => {
                    self.table.in_cell = true;
                    self.table.current_cell.clear();
                    self.style_stack.truncate(1);
                    self.link_url_stack.clear();
                }
                Event::End(TagEnd::TableCell) => {
                    let cell = trim_rich_spans(std::mem::take(&mut self.table.current_cell));
                    self.table.current_row.push(cell);
                    self.table.in_cell = false;
                    self.style_stack.truncate(1);
                    self.link_url_stack.clear();
                }
                Event::Start(Tag::Strong) if self.table.in_cell => {
                    self.push_style_mod(|s| s.add_modifier(Modifier::BOLD));
                }
                Event::End(TagEnd::Strong) if self.table.in_cell => {
                    self.pop_style();
                }
                Event::Start(Tag::Emphasis) if self.table.in_cell => {
                    self.push_style_mod(|s| s.add_modifier(Modifier::ITALIC));
                }
                Event::End(TagEnd::Emphasis) if self.table.in_cell => {
                    self.pop_style();
                }
                Event::Start(Tag::Strikethrough) if self.table.in_cell => {
                    self.push_style_mod(|s| {
                        s.fg(self.theme.muted).add_modifier(Modifier::CROSSED_OUT)
                    });
                }
                Event::End(TagEnd::Strikethrough) if self.table.in_cell => {
                    self.pop_style();
                }
                Event::Start(Tag::Link { dest_url, .. }) if self.table.in_cell => {
                    self.link_url_stack.push(dest_url.to_string());
                    self.push_style_mod(|s| {
                        s.fg(self.theme.info).add_modifier(Modifier::UNDERLINED)
                    });
                }
                Event::End(TagEnd::Link) if self.table.in_cell => {
                    self.pop_style();
                    self.link_url_stack.pop();
                }
                Event::Code(text) if self.table.in_cell => {
                    let style = Style::default()
                        .fg(self.theme.warning)
                        .bg(self.theme.code_bg);
                    let link = self.link_url_stack.last().cloned().or_else(|| {
                        let trimmed = text.trim();
                        if is_likely_url_token(trimmed) || is_likely_file_path_token(trimmed) {
                            Some(trimmed.to_owned())
                        } else {
                            None
                        }
                    });
                    self.table.current_cell.push(RichSpan {
                        text: text.to_string(),
                        style,
                        link_target: link,
                    });
                }
                Event::Text(text) if self.table.in_cell => {
                    let cell_style = if self.style_stack.len() > 1 {
                        self.current_style()
                    } else {
                        Style::default()
                    };
                    if let Some(target) = self.link_url_stack.last() {
                        self.table.current_cell.push(RichSpan::linked(
                            text.to_string(),
                            cell_style,
                            target.clone(),
                        ));
                    } else {
                        self.table
                            .current_cell
                            .extend(split_autolinks(&text, cell_style, self.theme));
                    }
                }
                Event::SoftBreak | Event::HardBreak if self.table.in_cell => {
                    self.table
                        .current_cell
                        .push(RichSpan::plain(" ", Style::default()));
                }
                Event::End(TagEnd::Table) => {
                    self.in_table = false;
                    self.flush_table();
                    self.ensure_blank_line();
                }
                _ => {}
            }
            return;
        }

        match event {
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                self.flush_pending();
                if self.list_stack.is_empty() {
                    self.ensure_blank_line();
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                self.flush_pending();
                self.ensure_blank_line();
                self.heading_level = Some(level);
                let heading_color = match level {
                    HeadingLevel::H1 => self.theme.accent,
                    HeadingLevel::H2 => self.theme.accent_soft,
                    HeadingLevel::H3 => self.theme.user_accent,
                    _ => self.theme.fg,
                };
                self.push_style_mod(|s| s.fg(heading_color).add_modifier(Modifier::BOLD));
                let marker = match level {
                    HeadingLevel::H1 => "▌ ",
                    HeadingLevel::H2 => "▎ ",
                    _ => "",
                };
                if !marker.is_empty() {
                    self.pending_spans
                        .push(RichSpan::plain(marker, self.current_style()));
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                self.flush_pending();
                self.pop_style();
                self.heading_level = None;
                self.ensure_blank_line();
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.flush_pending();
                self.quote_depth += 1;
                self.push_style_mod(|s| s.fg(self.theme.muted).add_modifier(Modifier::ITALIC));
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.flush_pending();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.pop_style();
                self.ensure_blank_line();
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                self.flush_pending();
                self.in_code_block = true;
                self.code_buffer.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => {
                        lang.split_whitespace().next().unwrap_or("").to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Event::Start(Tag::List(start)) => {
                self.flush_pending();
                self.list_stack.push(ListContext {
                    ordered: start.is_some(),
                    next_index: start.unwrap_or(1),
                });
            }
            Event::End(TagEnd::List(_)) => {
                self.flush_pending();
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.ensure_blank_line();
                }
            }
            Event::Start(Tag::Item) => {
                self.flush_pending();
                let bullet = if let Some(ctx) = self.list_stack.last_mut() {
                    if ctx.ordered {
                        let num = ctx.next_index;
                        ctx.next_index += 1;
                        format!("{num}. ")
                    } else {
                        "• ".to_owned()
                    }
                } else {
                    "• ".to_owned()
                };
                self.pending_item_bullet = Some(bullet);
            }
            Event::End(TagEnd::Item) => {
                self.flush_pending();
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                let style = if checked {
                    Style::default().fg(self.theme.success)
                } else {
                    Style::default().fg(self.theme.muted)
                };
                self.pending_spans.push(RichSpan::plain(marker, style));
            }
            Event::Start(Tag::Strong) => {
                self.push_style_mod(|s| s.add_modifier(Modifier::BOLD));
            }
            Event::End(TagEnd::Strong) => {
                self.pop_style();
            }
            Event::Start(Tag::Emphasis) => {
                self.push_style_mod(|s| s.add_modifier(Modifier::ITALIC));
            }
            Event::End(TagEnd::Emphasis) => {
                self.pop_style();
            }
            Event::Start(Tag::Strikethrough) => {
                self.push_style_mod(|s| s.fg(self.theme.muted).add_modifier(Modifier::CROSSED_OUT));
            }
            Event::End(TagEnd::Strikethrough) => {
                self.pop_style();
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.link_url_stack.push(dest_url.to_string());
                self.push_style_mod(|s| s.fg(self.theme.info).add_modifier(Modifier::UNDERLINED));
            }
            Event::End(TagEnd::Link) => {
                self.pop_style();
                if let Some(url) = self.link_url_stack.pop() {
                    let already_shown = self
                        .pending_spans
                        .last()
                        .is_some_and(|s| s.text.as_str() == url);
                    if !url.is_empty() && !already_shown {
                        self.pending_spans.push(RichSpan::linked(
                            format!(" ({url})"),
                            Style::default().fg(self.theme.muted),
                            url,
                        ));
                    }
                }
            }
            Event::Start(Tag::Table(alignments)) => {
                self.flush_pending();
                self.ensure_blank_line();
                self.in_table = true;
                self.table = TableBuffer {
                    alignments,
                    ..TableBuffer::default()
                };
            }
            Event::Text(text) => {
                let style = self.current_style();
                if let Some(target) = self.link_url_stack.last() {
                    self.pending_spans.push(RichSpan::linked(
                        text.to_string(),
                        style,
                        target.clone(),
                    ));
                } else {
                    self.pending_spans
                        .extend(split_autolinks(&text, style, self.theme));
                }
            }
            Event::Code(code) => {
                let style = Style::default()
                    .fg(self.theme.warning)
                    .bg(self.theme.code_bg);
                let link = self.link_url_stack.last().cloned().or_else(|| {
                    let trimmed = code.trim();
                    if is_likely_url_token(trimmed) || is_likely_file_path_token(trimmed) {
                        Some(trimmed.to_owned())
                    } else {
                        None
                    }
                });
                self.pending_spans.push(RichSpan {
                    text: format!(" {code} "),
                    style,
                    link_target: link,
                });
            }
            Event::SoftBreak => {
                let style = self.current_style();
                let link = self.link_url_stack.last().cloned();
                self.pending_spans.push(RichSpan {
                    text: " ".to_owned(),
                    style,
                    link_target: link,
                });
            }
            Event::HardBreak => {
                self.flush_pending();
            }
            Event::Rule => {
                self.flush_pending();
                self.ensure_blank_line();
                self.lines.push(BuiltLine::plain(Line::from(Span::styled(
                    "─".repeat(self.width),
                    Style::default().fg(self.theme.border),
                ))));
                self.ensure_blank_line();
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<BuiltLine> {
        if self.in_code_block {
            self.in_code_block = false;
            self.flush_code_block();
        }
        if self.in_table {
            self.in_table = false;
            self.flush_table();
        }
        self.flush_pending();
        while self
            .lines
            .last()
            .is_some_and(|l| l.line.spans.iter().all(|s| s.content.trim().is_empty()))
        {
            self.lines.pop();
        }
        self.lines
    }
}

fn render_markdown_rich(md: &str, width: usize, theme: &UiPalette) -> Vec<BuiltLine> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(md, options);
    let mut renderer = MarkdownRenderer::new(width, theme);
    for event in parser {
        renderer.process(event);
    }
    renderer.finish()
}

pub fn render_markdown(md: &str, width: usize, theme: &UiPalette) -> Vec<Line<'static>> {
    render_markdown_rich(md, width, theme)
        .into_iter()
        .map(|b| b.line)
        .collect()
}

fn render_unified_diff_built(diff: &str, width: usize, theme: &UiPalette) -> Vec<BuiltLine> {
    let max_width = width.max(MIN_RENDER_WIDTH);
    let mut lines = Vec::new();
    for raw_line in diff.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let (style, pad_bg) = if line.starts_with("diff --git ")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
        {
            (
                Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD),
                None,
            )
        } else if line.starts_with("@@") {
            (
                Style::default()
                    .fg(theme.diff_hunk_fg)
                    .add_modifier(Modifier::BOLD),
                None,
            )
        } else if line.starts_with('+') {
            (
                Style::default().fg(theme.diff_add_fg).bg(theme.diff_add_bg),
                Some(theme.diff_add_bg),
            )
        } else if line.starts_with('-') {
            (
                Style::default().fg(theme.diff_del_fg).bg(theme.diff_del_bg),
                Some(theme.diff_del_bg),
            )
        } else if line.starts_with('\\') {
            (
                Style::default()
                    .fg(theme.subtle)
                    .add_modifier(Modifier::ITALIC),
                None,
            )
        } else {
            (Style::default().fg(theme.muted), None)
        };
        lines.extend(wrap_preserving_indent_built(
            line,
            max_width,
            style,
            &[],
            pad_bg,
        ));
    }
    lines
}

pub fn render_unified_diff(diff: &str, width: usize, theme: &UiPalette) -> Vec<Line<'static>> {
    render_unified_diff_built(diff, width, theme)
        .into_iter()
        .map(|b| b.line)
        .collect()
}

fn looks_like_unified_diff(text: &str) -> bool {
    let has_hunk = text.lines().any(|line| line.starts_with("@@ "));
    let has_file_hdr = text
        .lines()
        .any(|line| line.starts_with("+++ ") || line.starts_with("diff --git "));
    has_hunk && has_file_hdr
}

#[cfg(test)]
fn build_transcript_lines(
    transcript: &Transcript,
    width: usize,
    theme: &UiPalette,
) -> (Vec<Line<'static>>, Vec<usize>, Vec<LineHitRegion>) {
    let (built, anchors, regions) =
        build_transcript_lines_with_activity(transcript, width, theme, None);
    (
        built.into_iter().map(|b| b.line).collect(),
        anchors,
        regions,
    )
}

fn build_transcript_lines_with_activity(
    transcript: &Transcript,
    width: usize,
    theme: &UiPalette,
    activity: Option<&LiveActivity>,
) -> (Vec<BuiltLine>, Vec<usize>, Vec<LineHitRegion>) {
    let max_width = width.max(MIN_RENDER_WIDTH);
    let mut lines: Vec<BuiltLine> = Vec::new();
    let mut message_anchors: Vec<usize> = Vec::new();
    let mut hit_regions: Vec<LineHitRegion> = Vec::new();
    let active_spinner = activity.map(|act| act.spinner);

    let mut idx = 0usize;
    while idx < transcript.entries.len() {
        if idx > 0 {
            let prev_is_compact = matches!(
                transcript.entries.get(idx - 1),
                Some(
                    TranscriptEntry::ThinkingBlock {
                        collapsed: true,
                        ..
                    } | TranscriptEntry::ToolCallCard {
                        collapsed: true,
                        ..
                    } | TranscriptEntry::DelegationCard { .. }
                )
            );
            let curr_is_compact = matches!(
                transcript.entries.get(idx),
                Some(
                    TranscriptEntry::ThinkingBlock {
                        collapsed: true,
                        ..
                    } | TranscriptEntry::ToolCallCard {
                        collapsed: true,
                        ..
                    } | TranscriptEntry::DelegationCard { .. }
                )
            );
            if !(prev_is_compact && curr_is_compact) {
                lines.push(BuiltLine::plain(Line::default()));
            }
        }

        if transcript.exploration_grouping && transcript.is_exploration_group_start(idx) {
            let start = idx;
            while matches!(
                transcript.entries.get(idx),
                Some(TranscriptEntry::ToolCallCard { kind, .. }) if kind.is_exploration()
            ) {
                idx += 1;
            }
            let count = idx - start;
            let expanded = transcript.expanded_exploration_groups.contains(&start);
            let any_running = transcript.entries[start..idx]
                .iter()
                .any(|e| matches!(e, TranscriptEntry::ToolCallCard { ok: None, .. }));
            let targets: Vec<&str> = transcript.entries[start..idx]
                .iter()
                .filter_map(|e| match e {
                    TranscriptEntry::ToolCallCard { summary, .. } if !summary.is_empty() => {
                        Some(summary.as_str())
                    }
                    _ => None,
                })
                .take(4)
                .collect();
            let noun = if count == 1 { "file" } else { "files" };
            let glyph = if any_running {
                active_spinner.unwrap_or("⠋")
            } else if expanded {
                "▾"
            } else {
                "▸"
            };
            let verb = if any_running { "Exploring" } else { "Explored" };
            let detail = if targets.is_empty() {
                String::new()
            } else {
                format!(" ({})", targets.join(", "))
            };
            let header_text = truncate_str(
                &format!("  {glyph} {verb} {count} {noun}{detail}"),
                max_width,
            );
            let line_index = lines.len();
            lines.push(BuiltLine::plain(Line::from(Span::styled(
                header_text,
                Style::default().fg(if any_running {
                    theme.warning
                } else {
                    theme.muted
                }),
            ))));
            hit_regions.push(LineHitRegion {
                line_index,
                col_start: 0,
                col_width: max_width,
                action: TranscriptHitAction::ToggleExpand { entry_index: start },
            });
            if expanded {
                for (sub_idx, entry) in transcript.entries.iter().enumerate().take(idx).skip(start)
                {
                    render_single_entry(
                        sub_idx,
                        entry,
                        max_width,
                        theme,
                        active_spinner,
                        &mut lines,
                        &mut message_anchors,
                        &mut hit_regions,
                    );
                }
            }
            continue;
        }

        render_single_entry(
            idx,
            &transcript.entries[idx],
            max_width,
            theme,
            active_spinner,
            &mut lines,
            &mut message_anchors,
            &mut hit_regions,
        );
        idx += 1;
    }

    if let Some(act) = activity {
        if !lines.is_empty() {
            lines.push(BuiltLine::plain(Line::default()));
        }
        let label = truncate_str(&act.label, max_width.saturating_sub(4).max(1));
        lines.push(BuiltLine::plain(Line::from(vec![
            Span::styled(
                format!("  {} ", act.spinner),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(label, Style::default().fg(theme.accent_soft)),
        ])));
    }

    for (line_index, built) in lines.iter().enumerate() {
        for link in &built.links {
            hit_regions.push(LineHitRegion {
                line_index,
                col_start: link.col_start,
                col_width: link.col_width,
                action: TranscriptHitAction::OpenLink {
                    target: link.target.clone(),
                },
            });
        }
    }

    (lines, message_anchors, hit_regions)
}

fn frame_card_line(mut line: BuiltLine, inner_w: usize, border_color: Color) -> BuiltLine {
    let line_w = line.width();
    let pad = inner_w.saturating_sub(line_w);
    line.prepend_span(
        Span::styled("  │ ", Style::default().fg(border_color)),
        true,
    );
    if pad > 0 {
        line.line.spans.push(Span::raw(" ".repeat(pad)));
    }
    line.copy_end_col = Some(4 + inner_w);
    line.line
        .spans
        .push(Span::styled(" │", Style::default().fg(border_color)));
    line
}

fn attachment_display_label(raw: &str) -> String {
    let trimmed = raw.trim();
    let base = trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed);
    truncate_str(base, 36)
}

#[allow(clippy::too_many_arguments)]
fn render_single_entry(
    entry_index: usize,
    entry: &TranscriptEntry,
    max_width: usize,
    theme: &UiPalette,
    active_spinner: Option<&str>,
    lines: &mut Vec<BuiltLine>,
    message_anchors: &mut Vec<usize>,
    hit_regions: &mut Vec<LineHitRegion>,
) {
    match entry {
        TranscriptEntry::UserMessage {
            text,
            timestamp,
            attachments,
        } => {
            message_anchors.push(lines.len());
            let mut header_spans = vec![Span::styled(
                "  You",
                Style::default()
                    .fg(theme.user_accent)
                    .add_modifier(Modifier::BOLD),
            )];
            if let Some(ts) = timestamp {
                header_spans.push(Span::styled(
                    format!("  {ts}"),
                    Style::default().fg(theme.subtle),
                ));
            }
            lines.push(BuiltLine::plain(Line::from(header_spans)));
            let body_prefix = [Span::styled("  ", Style::default().fg(theme.fg))];
            let quote_prefix = [
                Span::raw("  "),
                Span::styled("│ ", Style::default().fg(theme.accent)),
            ];
            let quote_style = Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::ITALIC);
            let text_style = Style::default().fg(theme.fg);
            for raw in text.split('\n') {
                let clean = raw.strip_suffix('\r').unwrap_or(raw);
                if let Some(quoted) = clean
                    .strip_prefix("> ")
                    .or_else(|| (clean == ">").then_some(""))
                {
                    let spans = split_autolinks(quoted, quote_style, theme);
                    lines.extend(wrap_rich_spans_with_prefix(
                        &spans,
                        max_width,
                        &quote_prefix,
                        &quote_prefix,
                    ));
                } else {
                    let spans = split_autolinks(clean, text_style, theme);
                    lines.extend(wrap_rich_spans_with_prefix(
                        &spans,
                        max_width,
                        &body_prefix,
                        &body_prefix,
                    ));
                }
            }
            if !attachments.is_empty() {
                let pill_style = Style::default()
                    .fg(theme.bg)
                    .bg(theme.info)
                    .add_modifier(Modifier::BOLD);
                let mut pill_spans = Vec::new();
                for (idx, att) in attachments.iter().enumerate() {
                    if idx > 0 {
                        pill_spans.push(RichSpan::plain(" ", Style::default()));
                    }
                    let label = format!(" [img] {} ", attachment_display_label(att));
                    let link_target = if is_likely_url_token(att) || is_likely_file_path_token(att)
                    {
                        Some(att.clone())
                    } else {
                        None
                    };
                    pill_spans.push(RichSpan {
                        text: label,
                        style: pill_style,
                        link_target,
                    });
                }
                lines.extend(wrap_rich_spans_with_prefix(
                    &pill_spans,
                    max_width,
                    &body_prefix,
                    &body_prefix,
                ));
            }
        }
        TranscriptEntry::AssistantMarkdown {
            speaker,
            markdown,
            streaming,
        } => {
            message_anchors.push(lines.len());
            let mut header_spans = vec![Span::styled(
                format!("  {speaker}"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )];
            if *streaming {
                header_spans.push(Span::styled("  ●", Style::default().fg(theme.warning)));
            }
            lines.push(BuiltLine::plain(Line::from(header_spans)));
            let content_w = max_width.saturating_sub(2).max(MIN_RENDER_WIDTH);
            for mut line in render_markdown_rich(markdown, content_w, theme) {
                line.prepend_span(Span::raw("  "), true);
                lines.push(line);
            }
        }
        TranscriptEntry::ThinkingBlock {
            text,
            duration_ms,
            collapsed,
        } => {
            let line_index = lines.len();
            let duration = format_duration(*duration_ms);
            if *collapsed {
                let summary = truncate_str(
                    &format!("  ▸ Thought for {duration} (Ctrl+E to expand)"),
                    max_width,
                );
                lines.push(BuiltLine::plain(Line::from(Span::styled(
                    summary,
                    Style::default()
                        .fg(theme.subtle)
                        .add_modifier(Modifier::ITALIC),
                ))));
            } else {
                let header = truncate_str(&format!("  ▾ Thought for {duration}"), max_width);
                lines.push(BuiltLine::plain(Line::from(Span::styled(
                    header,
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::ITALIC),
                ))));
                let body_w = max_width.saturating_sub(4).max(MIN_RENDER_WIDTH);
                let rendered = if text.trim().is_empty() {
                    Vec::new()
                } else {
                    render_markdown_rich(text, body_w, theme)
                };
                if rendered.is_empty() {
                    lines.push(BuiltLine {
                        line: Line::from(vec![
                            Span::styled("  │ ", Style::default().fg(theme.border)),
                            Span::styled(
                                truncate_str("(No reasoning text streamed by provider)", body_w),
                                Style::default()
                                    .fg(theme.subtle)
                                    .add_modifier(Modifier::ITALIC),
                            ),
                        ]),
                        soft_wrapped: false,
                        copy_prefix_cols: 4,
                        copy_end_col: None,
                        links: Vec::new(),
                    });
                } else {
                    for mut line in rendered {
                        line.prepend_span(
                            Span::styled("  │ ", Style::default().fg(theme.border)),
                            true,
                        );
                        lines.push(line);
                    }
                }
            }
            hit_regions.push(LineHitRegion {
                line_index,
                col_start: 0,
                col_width: max_width,
                action: TranscriptHitAction::ToggleExpand { entry_index },
            });
        }
        TranscriptEntry::ToolCallCard {
            kind,
            name,
            summary,
            output,
            ok,
            exit_code,
            duration_ms,
            collapsed,
            ..
        } => {
            render_tool_call_card(
                entry_index,
                *kind,
                name,
                summary,
                output,
                *ok,
                *exit_code,
                *duration_ms,
                *collapsed,
                max_width,
                theme,
                active_spinner,
                lines,
                hit_regions,
            );
        }
        TranscriptEntry::DelegationCard {
            worker,
            role,
            provider,
            branch,
            status,
            summary,
        } => {
            let line_index = lines.len();
            let status_color = match status {
                DelegationStatus::Queued => theme.muted,
                DelegationStatus::Running => theme.warning,
                DelegationStatus::Completed => theme.success,
                DelegationStatus::Failed => theme.danger,
                DelegationStatus::Cancelled => theme.subtle,
            };
            let meta = if branch.is_empty() {
                format!("{role} · {provider}")
            } else {
                format!("{role} · {provider} · {branch}")
            };
            let badge = status.badge();
            let text = truncate_str(
                &format!("  ▸ Delegated → {worker} [{meta}] {badge}"),
                max_width,
            );
            lines.push(BuiltLine::plain(Line::from(Span::styled(
                text,
                Style::default().fg(status_color),
            ))));
            hit_regions.push(LineHitRegion {
                line_index,
                col_start: 0,
                col_width: max_width,
                action: TranscriptHitAction::SelectDelegation {
                    worker: worker.clone(),
                },
            });
            if !summary.is_empty() {
                let sub = truncate_str(&format!("    └ {summary}"), max_width);
                lines.push(BuiltLine::plain(Line::from(Span::styled(
                    sub,
                    Style::default().fg(theme.muted),
                ))));
            }
        }
        TranscriptEntry::InlineConnectCard {
            id,
            title,
            detail,
            provider_or_tool,
            resolved,
        } => {
            render_inline_connect_card(
                id,
                title,
                detail,
                provider_or_tool,
                *resolved,
                max_width,
                theme,
                lines,
                hit_regions,
            );
        }
        TranscriptEntry::ProdGateCard {
            req_id,
            tool,
            command,
            reason,
            resolved,
        } => {
            render_prod_gate_card(
                req_id,
                tool,
                command,
                reason,
                *resolved,
                max_width,
                theme,
                lines,
                hit_regions,
            );
        }
        TranscriptEntry::SystemNotice { level, text } => {
            let is_routing = *level == NoticeLevel::Info
                && (text.starts_with("Override →")
                    || text.starts_with("Auto →")
                    || text.starts_with("Override "));
            let (glyph, glyph_color, text_color) = match level {
                NoticeLevel::Info if is_routing => ("ℹ", theme.subtle, theme.muted),
                NoticeLevel::Info => ("ℹ", theme.info, theme.info),
                NoticeLevel::Success => ("✓", theme.success, theme.success),
                NoticeLevel::Warning => ("⚠", theme.warning, theme.warning),
                NoticeLevel::Error => ("✗", theme.danger, theme.danger),
            };
            let (prefix_str, cont_str) = if glyph == "ℹ" {
                ("  ℹ  ", "     ")
            } else {
                ("", "    ")
            };
            let prefix_owned = if prefix_str.is_empty() {
                format!("  {glyph} ")
            } else {
                prefix_str.to_owned()
            };
            let prefix = [Span::styled(
                prefix_owned,
                Style::default()
                    .fg(glyph_color)
                    .add_modifier(Modifier::BOLD),
            )];
            let cont = [Span::raw(cont_str)];
            let spans = split_autolinks(text, Style::default().fg(text_color), theme);
            lines.extend(wrap_rich_spans_with_prefix(
                &spans, max_width, &prefix, &cont,
            ));
        }
        TranscriptEntry::TableCard {
            title,
            headers,
            rows,
            alignments,
            caption,
        } => {
            render_table_card(
                title,
                headers,
                rows,
                alignments,
                caption.as_deref(),
                max_width,
                theme,
                lines,
            );
        }
        TranscriptEntry::InfoCard {
            title,
            level,
            badge,
            fields,
            body_markdown,
        } => {
            render_info_card(
                title,
                level,
                badge.as_deref(),
                fields,
                body_markdown,
                max_width,
                theme,
                lines,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_table_card(
    title: &str,
    headers: &[String],
    rows: &[Vec<String>],
    alignments: &[String],
    caption: Option<&str>,
    max_width: usize,
    theme: &UiPalette,
    lines: &mut Vec<BuiltLine>,
) {
    let box_w = max_width.saturating_sub(2).max(16);
    let inner_w = box_w.saturating_sub(4).max(8);
    let header_title = if title.trim().is_empty() {
        "╭─ Table ".to_owned()
    } else {
        format!("╭─ {} ", title.trim())
    };
    let clipped_hdr = truncate_str(&header_title, box_w.saturating_sub(2));
    let fill = box_w.saturating_sub(display_width(&clipped_hdr) + 1);
    lines.push(BuiltLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            clipped_hdr,
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{}╮", "─".repeat(fill)),
            Style::default().fg(theme.border),
        ),
    ])));

    let parsed_alignments: Vec<Alignment> = alignments
        .iter()
        .map(|a| match a.trim().to_ascii_lowercase().as_str() {
            "right" => Alignment::Right,
            "center" => Alignment::Center,
            _ => Alignment::Left,
        })
        .collect();
    let parsed_headers: Vec<Vec<RichSpan>> = headers
        .iter()
        .map(|h| parse_inline_markdown_spans(h, Style::default(), theme))
        .collect();
    let parsed_rows: Vec<Vec<Vec<RichSpan>>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| parse_inline_markdown_spans(c, Style::default(), theme))
                .collect()
        })
        .collect();

    let table_lines = render_rich_table(
        &parsed_headers,
        &parsed_rows,
        &parsed_alignments,
        inner_w,
        theme,
    );
    if table_lines.is_empty() {
        let empty = BuiltLine::plain(Line::from(Span::styled(
            "(empty table)",
            Style::default().fg(theme.subtle),
        )));
        lines.push(frame_card_line(empty, inner_w, theme.border));
    } else {
        for tline in table_lines {
            lines.push(frame_card_line(tline, inner_w, theme.border));
        }
    }

    if let Some(cap) = caption.filter(|c| !c.trim().is_empty()) {
        let cap_spans = parse_inline_markdown_spans(
            cap,
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::ITALIC),
            theme,
        );
        for cline in wrap_rich_spans_with_prefix(&cap_spans, inner_w, &[], &[]) {
            lines.push(frame_card_line(cline, inner_w, theme.border));
        }
    }

    lines.push(BuiltLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))),
            Style::default().fg(theme.border),
        ),
    ])));
}

#[allow(clippy::too_many_arguments)]
fn render_info_card(
    title: &str,
    level: &str,
    badge: Option<&str>,
    fields: &[(String, String)],
    body_markdown: &str,
    max_width: usize,
    theme: &UiPalette,
    lines: &mut Vec<BuiltLine>,
) {
    let accent_color = match level.trim().to_ascii_lowercase().as_str() {
        "success" | "ok" => theme.success,
        "warning" | "warn" => theme.warning,
        "error" | "danger" => theme.danger,
        "accent" => theme.accent,
        _ => theme.info,
    };

    let box_w = max_width.saturating_sub(2).max(16);
    let inner_w = box_w.saturating_sub(4).max(8);
    let badge_suffix = badge
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(|b| format!(" [{b}] ─╮"))
        .unwrap_or_else(|| "╮".to_owned());
    let header_title = format!("╭─ {} ", title.trim());
    let max_left = box_w
        .saturating_sub(display_width(&badge_suffix) + 1)
        .max(4);
    let clipped_hdr = truncate_str(&header_title, max_left);
    let fill = box_w.saturating_sub(display_width(&clipped_hdr) + display_width(&badge_suffix));
    lines.push(BuiltLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            clipped_hdr,
            Style::default()
                .fg(accent_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("─".repeat(fill), Style::default().fg(accent_color)),
        Span::styled(
            badge_suffix,
            Style::default()
                .fg(accent_color)
                .add_modifier(Modifier::BOLD),
        ),
    ])));

    if !fields.is_empty() {
        let key_w = fields
            .iter()
            .map(|(k, _)| display_width(k))
            .max()
            .unwrap_or(0)
            .min(inner_w / 3);
        let cont_indent = " ".repeat(key_w + 2);
        for (key, value) in fields {
            let key_label = format!("{:<width$}: ", truncate_str(key, key_w), width = key_w);
            let mut spans = vec![RichSpan::plain(
                key_label,
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            )];
            spans.extend(parse_inline_markdown_spans(
                value,
                Style::default().fg(theme.fg),
                theme,
            ));
            let cont = [Span::raw(cont_indent.clone())];
            for fline in wrap_rich_spans_with_prefix(&spans, inner_w, &[], &cont) {
                lines.push(frame_card_line(fline, inner_w, accent_color));
            }
        }
    }

    if !body_markdown.trim().is_empty() {
        if !fields.is_empty() {
            let divider = BuiltLine::plain(Line::from(Span::styled(
                "─".repeat(inner_w),
                Style::default().fg(theme.border),
            )));
            lines.push(frame_card_line(divider, inner_w, accent_color));
        }
        for mline in render_markdown_rich(body_markdown, inner_w, theme) {
            lines.push(frame_card_line(mline, inner_w, accent_color));
        }
    }

    lines.push(BuiltLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))),
            Style::default().fg(accent_color),
        ),
    ])));
}

#[allow(clippy::too_many_arguments)]
fn render_tool_call_card(
    entry_index: usize,
    kind: ToolKind,
    name: &str,
    summary: &str,
    output: &str,
    ok: Option<bool>,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    collapsed: bool,
    max_width: usize,
    theme: &UiPalette,
    active_spinner: Option<&str>,
    lines: &mut Vec<BuiltLine>,
    hit_regions: &mut Vec<LineHitRegion>,
) {
    let spinner = active_spinner.unwrap_or("⠋");
    let (status_text, status_color) = match ok {
        Some(true) => {
            let code = exit_code.map(|c| format!(" {c}")).unwrap_or_default();
            let dur = duration_ms
                .map(|d| format!(" ({})", format_duration(d)))
                .unwrap_or_default();
            (format!("✓{code}{dur}"), theme.success)
        }
        Some(false) => {
            let code = exit_code.map(|c| format!(" {c}")).unwrap_or_default();
            let dur = duration_ms
                .map(|d| format!(" ({})", format_duration(d)))
                .unwrap_or_default();
            (format!("✗{code}{dur}"), theme.danger)
        }
        None => ("running…".to_owned(), theme.warning),
    };
    let tool_label = if name.is_empty() { kind.label() } else { name };
    let clean_summary = summary.trim();
    let dedup_summary = if clean_summary.eq_ignore_ascii_case(tool_label) {
        ""
    } else if let Some((prefix, rest)) = clean_summary.split_once(':') {
        if prefix.trim().eq_ignore_ascii_case(tool_label) {
            rest.trim()
        } else {
            clean_summary
        }
    } else {
        clean_summary
    };
    let title = if dedup_summary.is_empty() {
        tool_label.to_owned()
    } else {
        format!("{tool_label} · {dedup_summary}")
    };

    if collapsed {
        let line_index = lines.len();
        let lead = if ok.is_none() { spinner } else { "▸" };
        let left_fg = if ok.is_none() {
            theme.accent_soft
        } else {
            theme.muted
        };
        let left_budget = max_width.saturating_sub(display_width(&status_text) + 5);
        let left = truncate_str(&format!("  {lead} {title}"), left_budget.max(4));
        let gap = max_width
            .saturating_sub(display_width(&left) + display_width(&status_text) + 1)
            .max(1);
        lines.push(BuiltLine::plain(Line::from(vec![
            Span::styled(left, Style::default().fg(left_fg)),
            Span::raw(" ".repeat(gap)),
            Span::styled(status_text, Style::default().fg(status_color)),
        ])));
        hit_regions.push(LineHitRegion {
            line_index,
            col_start: 0,
            col_width: max_width,
            action: TranscriptHitAction::ToggleExpand { entry_index },
        });
        if ok == Some(false) && !output.trim().is_empty() {
            if let Some(err_line) = output.lines().find(|l| !l.trim().is_empty()) {
                let preview = truncate_str(
                    &format!("    └ {}", err_line.trim()),
                    max_width.min(COLLAPSED_ERROR_PREVIEW_CHARS),
                );
                lines.push(BuiltLine::plain(Line::from(Span::styled(
                    preview,
                    Style::default().fg(theme.danger),
                ))));
            }
        }
        return;
    }

    let box_w = max_width.saturating_sub(2).max(12);
    let inner_w = box_w.saturating_sub(4).max(4);
    let line_index = lines.len();

    let header_left = format!("╭─ ▾ {title} ");
    let header_right = format!(" {status_text} ─╮");
    let fill = box_w
        .saturating_sub(display_width(&header_left) + display_width(&header_right))
        .max(1);
    let clipped_left = truncate_str(
        &header_left,
        box_w
            .saturating_sub(display_width(&header_right) + 1)
            .max(4),
    );
    let actual_fill = box_w
        .saturating_sub(display_width(&clipped_left) + display_width(&header_right))
        .max(fill.min(1));
    lines.push(BuiltLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(clipped_left, Style::default().fg(theme.accent_soft)),
        Span::styled("─".repeat(actual_fill), Style::default().fg(theme.border)),
        Span::styled(header_right, Style::default().fg(status_color)),
    ])));
    hit_regions.push(LineHitRegion {
        line_index,
        col_start: 2,
        col_width: box_w,
        action: TranscriptHitAction::ToggleExpand { entry_index },
    });

    let body_lines = if output.is_empty() {
        vec![BuiltLine::plain(Line::from(Span::styled(
            "(no output)",
            Style::default().fg(theme.subtle),
        )))]
    } else if looks_like_unified_diff(output) {
        render_unified_diff_built(output, inner_w, theme)
    } else {
        let mut rendered = Vec::new();
        for raw in output.split('\n') {
            let clean = raw.strip_suffix('\r').unwrap_or(raw);
            rendered.extend(wrap_preserving_indent_built(
                clean,
                inner_w,
                Style::default().fg(theme.fg),
                &[],
                None,
            ));
        }
        rendered
    };

    for bline in body_lines {
        lines.push(frame_card_line(bline, inner_w, theme.border));
    }

    lines.push(BuiltLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))),
            Style::default().fg(theme.border),
        ),
    ])));
}

#[allow(clippy::too_many_arguments)]
fn render_inline_connect_card(
    id: &str,
    title: &str,
    detail: &str,
    provider_or_tool: &str,
    resolved: bool,
    max_width: usize,
    theme: &UiPalette,
    lines: &mut Vec<BuiltLine>,
    hit_regions: &mut Vec<LineHitRegion>,
) {
    if resolved {
        let text = truncate_str(&format!("  ✓ Connected: {title}"), max_width);
        lines.push(BuiltLine::plain(Line::from(Span::styled(
            text,
            Style::default().fg(theme.success),
        ))));
        return;
    }

    let box_w = max_width.saturating_sub(2).max(16);
    let inner_w = box_w.saturating_sub(4).max(8);
    let header_title = format!("╭─ Connect: {title} ");
    let clipped_hdr = truncate_str(&header_title, box_w.saturating_sub(2));
    let fill = box_w.saturating_sub(display_width(&clipped_hdr) + 1);
    lines.push(BuiltLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            clipped_hdr,
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{}╮", "─".repeat(fill)),
            Style::default().fg(theme.warning),
        ),
    ])));

    let spans = split_autolinks(detail, Style::default().fg(theme.fg), theme);
    for line in wrap_rich_spans_with_prefix(&spans, inner_w, &[], &[]) {
        lines.push(frame_card_line(line, inner_w, theme.warning));
    }

    let btn_line_index = lines.len();
    let connect_label = "[ Connect ]";
    let dismiss_label = "[ Dismiss ]";
    let connect_col = 4usize;
    let connect_w = display_width(connect_label);
    let dismiss_col = connect_col + connect_w + 2;
    let dismiss_w = display_width(dismiss_label);
    let used_btns = connect_w + 2 + dismiss_w;
    let pad = inner_w.saturating_sub(used_btns);
    lines.push(BuiltLine::plain(Line::from(vec![
        Span::styled("  │ ", Style::default().fg(theme.warning)),
        Span::styled(
            connect_label,
            Style::default()
                .fg(theme.bg)
                .bg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            dismiss_label,
            Style::default().fg(theme.muted).bg(theme.surface_alt),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(" │", Style::default().fg(theme.warning)),
    ])));
    hit_regions.push(LineHitRegion {
        line_index: btn_line_index,
        col_start: connect_col,
        col_width: connect_w,
        action: TranscriptHitAction::Connect {
            card_id: id.to_owned(),
            provider_or_tool: provider_or_tool.to_owned(),
        },
    });
    hit_regions.push(LineHitRegion {
        line_index: btn_line_index,
        col_start: dismiss_col,
        col_width: dismiss_w,
        action: TranscriptHitAction::DismissConnect {
            card_id: id.to_owned(),
        },
    });

    lines.push(BuiltLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))),
            Style::default().fg(theme.warning),
        ),
    ])));
}

#[allow(clippy::too_many_arguments)]
fn render_prod_gate_card(
    req_id: &str,
    tool: &str,
    command: &str,
    reason: &str,
    resolved: bool,
    max_width: usize,
    theme: &UiPalette,
    lines: &mut Vec<BuiltLine>,
    hit_regions: &mut Vec<LineHitRegion>,
) {
    if resolved {
        let text = truncate_str(&format!("  ✓ ProdGate resolved: {tool}"), max_width);
        lines.push(BuiltLine::plain(Line::from(Span::styled(
            text,
            Style::default().fg(theme.muted),
        ))));
        return;
    }

    let box_w = max_width.saturating_sub(2).max(16);
    let inner_w = box_w.saturating_sub(4).max(8);
    let header_title = format!("╭─ ProdGate Approval: {tool} ");
    let clipped_hdr = truncate_str(&header_title, box_w.saturating_sub(2));
    let fill = box_w.saturating_sub(display_width(&clipped_hdr) + 1);
    lines.push(BuiltLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            clipped_hdr,
            Style::default()
                .fg(theme.danger)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{}╮", "─".repeat(fill)),
            Style::default().fg(theme.danger),
        ),
    ])));

    for (label, value, value_style) in [
        (
            "Command: ",
            command,
            Style::default().fg(theme.warning).bg(theme.code_bg),
        ),
        ("Reason:  ", reason, Style::default().fg(theme.fg)),
    ] {
        let wrapped = wrap_rich_spans_with_prefix(
            &[
                RichSpan::plain(label, Style::default().fg(theme.muted)),
                RichSpan::plain(value, value_style),
            ],
            inner_w,
            &[],
            &[Span::raw("         ")],
        );
        for line in wrapped {
            lines.push(frame_card_line(line, inner_w, theme.danger));
        }
    }

    let btn_line_index = lines.len();
    let allow_label = "[ Allow ]";
    let deny_label = "[ Deny ]";
    let allow_col = 4usize;
    let allow_w = display_width(allow_label);
    let deny_col = allow_col + allow_w + 2;
    let deny_w = display_width(deny_label);
    let used_btns = allow_w + 2 + deny_w;
    let pad = inner_w.saturating_sub(used_btns);
    lines.push(BuiltLine::plain(Line::from(vec![
        Span::styled("  │ ", Style::default().fg(theme.danger)),
        Span::styled(
            allow_label,
            Style::default()
                .fg(theme.bg)
                .bg(theme.success)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            deny_label,
            Style::default()
                .fg(theme.bg)
                .bg(theme.danger)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(" │", Style::default().fg(theme.danger)),
    ])));
    hit_regions.push(LineHitRegion {
        line_index: btn_line_index,
        col_start: allow_col,
        col_width: allow_w,
        action: TranscriptHitAction::AllowProdGate {
            req_id: req_id.to_owned(),
        },
    });
    hit_regions.push(LineHitRegion {
        line_index: btn_line_index,
        col_start: deny_col,
        col_width: deny_w,
        action: TranscriptHitAction::DenyProdGate {
            req_id: req_id.to_owned(),
        },
    });

    lines.push(BuiltLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))),
            Style::default().fg(theme.danger),
        ),
    ])));
}

fn paint_selection_highlight(
    buffer: &mut Buffer,
    content_area: Rect,
    scroll: &ScrollState,
    theme: &UiPalette,
) {
    let Some(sel) = scroll.selection() else {
        return;
    };
    if sel.is_empty() || scroll.rendered_lines.is_empty() {
        return;
    }
    let (start, end) = sel.ordered();
    let last_valid = scroll.rendered_lines.len().saturating_sub(1);
    let first_line = start.line.min(last_valid);
    let last_line = end.line.min(last_valid);

    for rel_y in 0..content_area.height as usize {
        let doc_line = scroll.offset() + rel_y;
        if doc_line < first_line || doc_line > last_line {
            continue;
        }
        let Some(rendered) = scroll.rendered_lines.get(doc_line) else {
            continue;
        };
        let (min_col, max_col) = selection_line_bounds(
            rendered, doc_line, first_line, last_line, start.col, end.col,
        );
        let y = content_area.y + rel_y as u16;
        for cell in &rendered.cells {
            let cell_w = cell.width.max(1);
            let cell_end = cell.col_start + cell_w;
            if cell_end <= min_col || cell.col_start > max_col {
                continue;
            }
            for dx in 0..cell_w {
                let rel_col = cell.col_start + dx;
                if rel_col < content_area.width as usize {
                    let x = content_area.x + rel_col as u16;
                    buffer[(x, y)]
                        .set_bg(theme.selection_bg)
                        .set_fg(theme.selection_fg);
                }
            }
        }
    }
}

pub fn render_selection_popover(
    frame: &mut Frame,
    bounds: Rect,
    scroll: &mut ScrollState,
    theme: &UiPalette,
) {
    scroll.popover_rects = SelectionPopoverRects::default();
    let Some(sel) = scroll.selection().cloned() else {
        return;
    };
    if !sel.popover_open || sel.is_empty() || bounds.width < 12 || bounds.height == 0 {
        return;
    }

    let (start, end) = sel.ordered();
    let view_start = scroll.offset();
    let view_end = view_start + scroll.viewport_height();
    if end.line < view_start || start.line >= view_end {
        return;
    }

    let vis_start_rel = start.line.saturating_sub(view_start);
    let vis_end_rel = end
        .line
        .saturating_sub(view_start)
        .min(bounds.height.saturating_sub(1) as usize);
    let vis_start_row = bounds.y + vis_start_rel as u16;
    let vis_end_row = bounds.y + vis_end_rel as u16;

    let boxed = bounds.height >= 4 && bounds.width >= 27;
    let (quote_label, copy_label, pop_w, pop_h) = if boxed {
        (" Quote (q) ", " Copy (c) ", 27u16, 3u16)
    } else {
        let q = " Quote ";
        let c = " Copy ";
        let w = (display_width(q) + 1 + display_width(c)) as u16;
        (q, c, w.min(bounds.width), 1u16)
    };
    if pop_w > bounds.width || pop_h > bounds.height {
        return;
    }

    let below_y = vis_end_row.saturating_add(1);
    let pop_y = if below_y.saturating_add(pop_h) <= bounds.bottom() {
        below_y
    } else if vis_start_row >= bounds.y.saturating_add(pop_h) {
        vis_start_row - pop_h
    } else {
        bounds.bottom().saturating_sub(pop_h).max(bounds.y)
    };

    let anchor_col = bounds
        .x
        .saturating_add((end.col as u16).min(bounds.width.saturating_sub(1)));
    let ideal_x = anchor_col.saturating_sub(pop_w / 2);
    let max_x = bounds.right().saturating_sub(pop_w);
    let pop_x = ideal_x.clamp(bounds.x, max_x.max(bounds.x));

    let container = Rect::new(pop_x, pop_y, pop_w, pop_h);
    frame.render_widget(Clear, container);

    let focused_style = Style::default()
        .fg(theme.bg)
        .bg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let unfocused_style = Style::default().fg(theme.fg).bg(theme.surface_alt);
    let (quote_style, copy_style) = match sel.focused_button {
        SelectionButton::Quote => (focused_style, unfocused_style),
        SelectionButton::Copy => (unfocused_style, focused_style),
    };

    let quote_w = display_width(quote_label) as u16;
    let copy_w = display_width(copy_label) as u16;

    if boxed {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.surface));
        let inner = block.inner(container);
        frame.render_widget(block, container);
        let row = Line::from(vec![
            Span::styled(" ", Style::default().bg(theme.surface)),
            Span::styled(quote_label, quote_style),
            Span::styled("  ", Style::default().bg(theme.surface)),
            Span::styled(copy_label, copy_style),
            Span::styled(" ", Style::default().bg(theme.surface)),
        ]);
        frame.render_widget(
            Paragraph::new(row).style(Style::default().bg(theme.surface)),
            inner,
        );
        scroll.popover_rects = SelectionPopoverRects {
            container,
            quote: Rect::new(inner.x + 1, inner.y, quote_w.min(inner.width), 1),
            copy: Rect::new(
                inner.x + 1 + quote_w + 2,
                inner.y,
                copy_w.min(inner.width.saturating_sub(1 + quote_w + 2)),
                1,
            ),
        };
    } else {
        let row = Line::from(vec![
            Span::styled(quote_label, quote_style),
            Span::styled(" ", Style::default().bg(theme.surface)),
            Span::styled(copy_label, copy_style),
        ]);
        frame.render_widget(
            Paragraph::new(row).style(Style::default().bg(theme.surface)),
            container,
        );
        scroll.popover_rects = SelectionPopoverRects {
            container,
            quote: Rect::new(container.x, container.y, quote_w.min(container.width), 1),
            copy: Rect::new(
                container.x + quote_w + 1,
                container.y,
                copy_w.min(container.width.saturating_sub(quote_w + 1)),
                1,
            ),
        };
    }
}

pub fn render_transcript(
    frame: &mut Frame,
    area: Rect,
    transcript: &Transcript,
    scroll: &mut ScrollState,
    theme: &UiPalette,
) {
    render_transcript_with_activity(frame, area, transcript, scroll, theme, None);
}

pub fn render_transcript_with_activity(
    frame: &mut Frame,
    area: Rect,
    transcript: &Transcript,
    scroll: &mut ScrollState,
    theme: &UiPalette,
    activity: Option<&LiveActivity>,
) {
    scroll.hit_rects.clear();
    scroll.popover_rects = SelectionPopoverRects::default();
    if area.width == 0 || area.height == 0 {
        scroll.content_area = Rect::default();
        scroll.rendered_lines.clear();
        scroll.sync_metrics(0, 0, Vec::new());
        return;
    }

    let content_width = area.width.saturating_sub(1).max(1) as usize;
    let (mut all_lines, anchors, regions) =
        build_transcript_lines_with_activity(transcript, content_width, theme, activity);
    if !all_lines.is_empty() {
        all_lines.push(BuiltLine::plain(Line::default()));
    }
    let viewport_h = area.height as usize;
    scroll.sync_metrics(all_lines.len(), viewport_h, anchors);

    let content_area = Rect::new(area.x, area.y, content_width as u16, area.height);
    scroll.content_area = content_area;
    scroll.rendered_lines = all_lines.iter().map(build_rendered_line).collect();

    let start = scroll.offset();
    let end = (start + viewport_h).min(all_lines.len());
    let visible: Vec<Line<'static>> = all_lines[start..end]
        .iter()
        .map(|b| b.line.clone())
        .collect();

    frame.render_widget(
        Paragraph::new(visible).style(Style::default().bg(theme.bg).fg(theme.fg)),
        content_area,
    );

    paint_selection_highlight(frame.buffer_mut(), content_area, scroll, theme);

    for region in regions {
        if region.line_index >= start && region.line_index < end {
            let rel_y = (region.line_index - start) as u16;
            let col_start = (region.col_start as u16).min(content_area.width);
            let avail_w = content_area.width.saturating_sub(col_start);
            let col_w = (region.col_width as u16).min(avail_w);
            if col_w > 0 {
                scroll.hit_rects.push(HitRect {
                    area: Rect::new(content_area.x + col_start, content_area.y + rel_y, col_w, 1),
                    action: region.action,
                });
            }
        }
    }

    if area.width >= 2 {
        let bar_x = area.right() - 1;
        let total = all_lines.len();
        let bar_area = Rect::new(bar_x, area.y, 1, area.height);
        let mut bar_lines = Vec::with_capacity(viewport_h);
        if total > viewport_h {
            let thumb_h = ((viewport_h * viewport_h) / total).clamp(1, viewport_h);
            let max_off = total - viewport_h;
            let track_span = viewport_h.saturating_sub(thumb_h);
            let thumb_top = (start * track_span).checked_div(max_off).unwrap_or(0);
            for row in 0..viewport_h {
                let in_thumb = row >= thumb_top && row < thumb_top + thumb_h;
                let (glyph, color) = if in_thumb {
                    ("┃", theme.muted)
                } else {
                    ("│", theme.border)
                };
                bar_lines.push(Line::from(Span::styled(
                    glyph,
                    Style::default().fg(color).bg(theme.bg),
                )));
            }
        } else {
            for _ in 0..viewport_h {
                bar_lines.push(Line::from(Span::styled(" ", Style::default().bg(theme.bg))));
            }
        }
        frame.render_widget(Paragraph::new(bar_lines), bar_area);
    }

    if !scroll.is_sticky_bottom()
        && scroll.offset() < scroll.max_offset()
        && content_area.width >= 16
    {
        let pill = " ↓ Bottom (End) ";
        let pill_w = display_width(pill) as u16;
        if pill_w <= content_area.width {
            let pill_area = Rect::new(
                content_area.right() - pill_w,
                content_area.bottom() - 1,
                pill_w,
                1,
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    pill,
                    Style::default()
                        .fg(theme.bg)
                        .bg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ))),
                pill_area,
            );
        }
    }

    render_selection_popover(frame, content_area, scroll, theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    fn buffer_text(buffer: &Buffer, area: Rect) -> String {
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn lines_to_string(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn markdown_renders_headings_styles_code_lists_quotes_tables_and_links() {
        let theme = UiPalette::dark_deck();
        let md = "\
# Main Title

Hello **bold** *italic* ~~gone~~ `inline_fn()` and [docs](https://example.com).

> Quoted insight

- First item
- [x] Checked task

```rust
fn main() {
    let indent = 4;
}
```

---

| File | Status |
| --- | --- |
| src/editor.rs | ready |
";
        let lines = render_markdown(md, 72, &theme);
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("Main Title"));
        assert!(rendered.contains("bold"));
        assert!(rendered.contains("italic"));
        assert!(rendered.contains("gone"));
        assert!(rendered.contains("inline_fn()"));
        assert!(rendered.contains("https://example.com"));
        assert!(rendered.contains("│ Quoted insight"));
        assert!(rendered.contains("• First item"));
        assert!(rendered.contains("[x] Checked task"));
        assert!(rendered.contains("╭─ rust "));
        assert!(rendered.contains("    let indent = 4;"));
        assert!(rendered.contains("src/editor.rs"));
        assert!(rendered.contains("ready"));
    }

    #[test]
    fn unified_diff_colors_additions_deletions_and_hunks() {
        let theme = UiPalette::dark_deck();
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 mod auth;
-mod old;
+pub mod editor;
+pub mod markdown;
";
        let lines = render_unified_diff(diff, 80, &theme);
        let hunk = lines
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.contains("@@ -1,3 +1,4 @@"))
            })
            .unwrap();
        assert_eq!(hunk.spans[0].style.fg, Some(theme.diff_hunk_fg));

        let added = lines
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.contains("+pub mod editor;"))
            })
            .unwrap();
        assert_eq!(added.spans[0].style.fg, Some(theme.diff_add_fg));
        assert_eq!(added.spans[0].style.bg, Some(theme.diff_add_bg));

        let deleted = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("-mod old;")))
            .unwrap();
        assert_eq!(deleted.spans[0].style.fg, Some(theme.diff_del_fg));
        assert_eq!(deleted.spans[0].style.bg, Some(theme.diff_del_bg));
    }

    #[test]
    fn exploration_grouping_collapses_and_expands_consecutive_reads() {
        let theme = UiPalette::dark_deck();
        let mut transcript = Transcript::new();
        for (id, kind, path) in [
            ("c1", ToolKind::Read, "src/lib.rs"),
            ("c2", ToolKind::Grep, "src/auth.rs"),
            ("c3", ToolKind::Glob, "Cargo.toml"),
        ] {
            transcript.push(TranscriptEntry::ToolCallCard {
                call_id: id.into(),
                kind,
                name: kind.label().into(),
                summary: path.into(),
                output: "contents".into(),
                ok: Some(true),
                exit_code: Some(0),
                duration_ms: Some(12),
                collapsed: true,
            });
        }

        let (collapsed_lines, _, regions) = build_transcript_lines(&transcript, 80, &theme);
        assert_eq!(collapsed_lines.len(), 1);
        let summary_text = lines_to_string(&collapsed_lines);
        assert!(summary_text.contains("▸ Explored 3 files"));
        assert!(summary_text.contains("src/lib.rs"));
        assert_eq!(regions.len(), 1);

        assert!(transcript.toggle_expand_at_or_latest(Some(0)));
        let (expanded_lines, _, _) = build_transcript_lines(&transcript, 80, &theme);
        assert!(expanded_lines.len() > 1);
        assert!(lines_to_string(&expanded_lines).contains("▾ Explored 3 files"));
    }

    #[test]
    fn thinking_block_is_single_line_when_collapsed_and_expands_cleanly() {
        let theme = UiPalette::dark_deck();
        let mut transcript = Transcript::new();
        transcript.push(TranscriptEntry::ThinkingBlock {
            text: "Line one\n\nLine two".into(),
            duration_ms: 3200,
            collapsed: true,
        });
        let (collapsed_lines, _, _) = build_transcript_lines(&transcript, 80, &theme);
        assert_eq!(collapsed_lines.len(), 1);
        assert!(lines_to_string(&collapsed_lines).contains("▸ Thought for 3.2s"));

        assert!(transcript.toggle_expand_at_or_latest(None));
        let (expanded_lines, _, _) = build_transcript_lines(&transcript, 80, &theme);
        assert!(expanded_lines.len() >= 3);
        assert!(lines_to_string(&expanded_lines).contains("Line two"));
    }

    #[test]
    fn scroll_state_sticky_follow_message_jumps_and_hit_rects_work() {
        let theme = UiPalette::dark_deck();
        let mut transcript = Transcript::new();
        for idx in 0..12 {
            transcript.push_user(format!("Prompt {idx}"), None);
            transcript.push(TranscriptEntry::AssistantMarkdown {
                speaker: "Commander".into(),
                markdown: format!(
                    "Response paragraph {idx} with **markdown** details.\n\nSecond line {idx}."
                ),
                streaming: false,
            });
        }
        transcript.push(TranscriptEntry::InlineConnectCard {
            id: "conn-1".into(),
            title: "GitHub".into(),
            detail: "Sign in to open pull requests.".into(),
            provider_or_tool: "github".into(),
            resolved: false,
        });
        transcript.push(TranscriptEntry::ProdGateCard {
            req_id: "gate-1".into(),
            tool: "Bash".into(),
            command: "git push --force".into(),
            reason: "Force push rewrites remote history".into(),
            resolved: false,
        });

        let mut scroll = ScrollState::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_transcript(f, f.area(), &transcript, &mut scroll, &theme))
            .unwrap();
        assert!(scroll.is_sticky_bottom());
        assert_eq!(scroll.offset(), scroll.max_offset());

        let connect_hit = scroll
            .hit_rects()
            .iter()
            .find(|h| matches!(h.action, TranscriptHitAction::Connect { .. }))
            .cloned()
            .unwrap();
        assert_eq!(
            scroll.hit_test(connect_hit.area.x + 1, connect_hit.area.y),
            Some(&connect_hit.action)
        );
        let allow_hit = scroll
            .hit_rects()
            .iter()
            .find(|h| matches!(h.action, TranscriptHitAction::AllowProdGate { .. }))
            .cloned()
            .unwrap();
        assert_eq!(
            scroll.hit_test(allow_hit.area.x + 1, allow_hit.area.y),
            Some(&allow_hit.action)
        );

        let before = scroll.offset();
        scroll.scroll_up(5);
        assert!(!scroll.is_sticky_bottom());
        assert_eq!(scroll.offset(), before - 5);

        transcript.push_user("New streaming tail", None);
        terminal
            .draw(|f| render_transcript(f, f.area(), &transcript, &mut scroll, &theme))
            .unwrap();
        assert!(!scroll.is_sticky_bottom());
        assert_eq!(scroll.offset(), before - 5);

        scroll.jump_prev_message();
        assert!(scroll.offset() < before - 5);
        scroll.jump_next_message();
        scroll.page_up();
        assert!(!scroll.is_sticky_bottom());
        scroll.page_down();
        scroll.scroll_down(10_000);
        assert!(scroll.is_sticky_bottom());
        assert_eq!(scroll.offset(), scroll.max_offset());
        scroll.jump_to_top();
        assert_eq!(scroll.offset(), 0);
        assert!(!scroll.is_sticky_bottom());
        scroll.jump_to_bottom();
        assert!(scroll.is_sticky_bottom());
        assert_eq!(scroll.offset(), scroll.max_offset());

        terminal
            .draw(|f| render_transcript(f, f.area(), &transcript, &mut scroll, &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let text = buffer_text(&buf, Rect::new(0, 0, 80, 24));
        assert!(text.contains("ProdGate"));
        assert!(text.contains("New streaming tail"));
    }

    #[test]
    fn renders_cleanly_at_supported_and_tiny_sizes() {
        let theme = UiPalette::dark_deck();
        let mut transcript = Transcript::new();
        transcript.push_user("café 日本語 🚀", Some("12:00".into()));
        transcript.push(TranscriptEntry::AssistantMarkdown {
            speaker: "Commander".into(),
            markdown: "```rust\nprintln!(\"hello\");\n```".into(),
            streaming: true,
        });
        let mut scroll = ScrollState::new();
        for (w, h) in [(20, 6), (80, 24), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| render_transcript(f, f.area(), &transcript, &mut scroll, &theme))
                .unwrap();
        }
    }

    #[test]
    fn empty_thinking_block_expands_with_fallback_and_tool_upsert_updates_in_place() {
        let theme = UiPalette::dark_deck();
        let mut transcript = Transcript::new();
        transcript.append_thinking_delta("", 4200);
        assert!(transcript.toggle_expand_at_or_latest(None));
        let (lines, _, _) = build_transcript_lines(&transcript, 80, &theme);
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("▾ Thought for 4.2s"), "{rendered}");
        assert!(
            rendered.contains("(No reasoning text streamed by provider)"),
            "{rendered}"
        );

        let idx1 = transcript.upsert_tool_call(
            "call-1".into(),
            ToolKind::Write,
            "Write".into(),
            String::new(),
        );
        let idx2 = transcript.upsert_tool_call(
            "call-1".into(),
            ToolKind::Write,
            "Write".into(),
            "C:\\Users\\j\\gridfall.html".into(),
        );
        assert_eq!(idx1, idx2);
        assert_eq!(transcript.entries.len(), 2);

        let activity = LiveActivity {
            spinner: "⠙",
            label: "Working… 4.2s · Write C:\\Users\\j\\gridfall.html".into(),
            elapsed_ms: 4200,
        };
        let (live_built, _, _) =
            build_transcript_lines_with_activity(&transcript, 80, &theme, Some(&activity));
        let live_lines: Vec<Line<'static>> = live_built.into_iter().map(|b| b.line).collect();
        let live_text = lines_to_string(&live_lines);
        assert!(live_text.contains("⠙ Write · C:\\Users\\j\\gridfall.html"));
        assert!(live_text.contains("running…"));
        assert!(live_text.contains("⠙ Working… 4.2s · Write C:\\Users\\j\\gridfall.html"));

        assert!(transcript.complete_tool_call("call-1", "Wrote 12 KB", true, Some(0), Some(180)));
        let (done_lines, _, _) = build_transcript_lines(&transcript, 80, &theme);
        assert!(lines_to_string(&done_lines).contains("✓ 0 (180ms)"));
    }

    #[test]
    fn structured_transcript_entries_serialize_and_normalize_cleanly() {
        let entries = [
            TranscriptEntry::UserMessage {
                text: "Build gridfall".into(),
                timestamp: Some("01:20:00".into()),
                attachments: Vec::new(),
            },
            TranscriptEntry::ThinkingBlock {
                text: "Plan HTML5 canvas layout".into(),
                duration_ms: 1400,
                collapsed: true,
            },
            TranscriptEntry::ToolCallCard {
                call_id: "t1".into(),
                kind: ToolKind::Write,
                name: "Write".into(),
                summary: "gridfall.html".into(),
                output: "ok".into(),
                ok: None,
                exit_code: None,
                duration_ms: None,
                collapsed: true,
            },
            TranscriptEntry::AssistantMarkdown {
                speaker: "Commander".into(),
                markdown: "| Key | Action |\n|---|---|\n| ← → | Move |".into(),
                streaming: true,
            },
            TranscriptEntry::SystemNotice {
                level: NoticeLevel::Info,
                text: "Override → claude/default".into(),
            },
        ];
        let jsonl: String = entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: Vec<TranscriptEntry> = jsonl
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let normalized = Transcript::normalize_loaded(parsed);
        assert!(matches!(
            &normalized[2],
            TranscriptEntry::ToolCallCard {
                ok: Some(false),
                ..
            }
        ));
        assert!(matches!(
            &normalized[3],
            TranscriptEntry::AssistantMarkdown {
                streaming: false,
                ..
            }
        ));

        let legacy_user_json =
            r#"{"type":"user_message","text":"Legacy prompt","timestamp":"01:00:00"}"#;
        let legacy: TranscriptEntry = serde_json::from_str(legacy_user_json).unwrap();
        assert!(matches!(
            legacy,
            TranscriptEntry::UserMessage {
                ref text,
                ref attachments,
                ..
            } if text == "Legacy prompt" && attachments.is_empty()
        ));

        let theme = UiPalette::dark_deck();
        let mut transcript = Transcript::new();
        transcript.entries = normalized;
        let (lines, _, _) = build_transcript_lines(&transcript, 80, &theme);
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("─┼─"), "{rendered}");
        assert!(!rendered.contains("|---|"), "{rendered}");
        assert!(
            rendered.contains("ℹ  Override → claude/default"),
            "{rendered}"
        );
    }

    #[test]
    fn character_selection_highlight_popover_flip_and_hit_testing_at_80x24_and_120x40() {
        let theme = UiPalette::dark_deck();
        let mut transcript = Transcript::new();
        transcript.push_user("Inspect chat selection behavior", None);
        transcript.push(TranscriptEntry::AssistantMarkdown {
            speaker: "Commander".into(),
            markdown: "Alpha bravo charlie delta echo foxtrot.".into(),
            streaming: false,
        });

        for (w, h) in [(80, 24), (120, 40)] {
            let mut scroll = ScrollState::new();
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| render_transcript(f, f.area(), &transcript, &mut scroll, &theme))
                .unwrap();

            let buf = terminal.backend().buffer().clone();
            let target_row = (0..h)
                .find(|&y| {
                    let row_text: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
                    row_text.contains("Alpha bravo charlie")
                })
                .expect("assistant line visible");
            let row_text: String = (0..w).map(|x| buf[(x, target_row)].symbol()).collect();
            let bravo_col = row_text.find("bravo").expect("bravo col") as u16;
            let end_col = bravo_col + "bravo charlie".len() as u16 - 1;

            assert!(scroll.begin_selection(bravo_col, target_row));
            assert!(scroll.update_selection_drag(end_col, target_row));
            assert!(scroll.finish_selection(end_col, target_row));
            assert_eq!(
                scroll.extract_selection_text().as_deref(),
                Some("bravo charlie")
            );
            assert!(scroll.popover_open());

            terminal
                .draw(|f| render_transcript(f, f.area(), &transcript, &mut scroll, &theme))
                .unwrap();
            let highlighted = terminal.backend().buffer().clone();
            assert_eq!(highlighted[(bravo_col, target_row)].bg, theme.selection_bg);
            assert_eq!(highlighted[(bravo_col, target_row)].fg, theme.selection_fg);

            let rects = scroll.popover_rects();
            assert!(rects.container.width > 0);
            assert!(rects.container.right() <= w);
            assert!(rects.container.bottom() <= h);
            assert_eq!(
                scroll.hit_test_popover(rects.quote.x + 1, rects.quote.y),
                Some(SelectionButton::Quote)
            );
            assert_eq!(
                scroll.hit_test_popover(rects.copy.x + 1, rects.copy.y),
                Some(SelectionButton::Copy)
            );
            scroll.toggle_popover_button();
            assert_eq!(
                scroll.selection().map(|s| s.focused_button),
                Some(SelectionButton::Copy)
            );
        }

        let mut tall = Transcript::new();
        for idx in 0..25 {
            tall.push_user(format!("Line item {idx} near viewport edge"), None);
        }
        let mut scroll = ScrollState::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_transcript(f, f.area(), &tall, &mut scroll, &theme))
            .unwrap();
        let bottom_text_row = 22u16;
        assert!(scroll.begin_selection(2, bottom_text_row));
        assert!(scroll.finish_selection(15, bottom_text_row));
        terminal
            .draw(|f| render_transcript(f, f.area(), &tall, &mut scroll, &theme))
            .unwrap();
        let rects = scroll.popover_rects();
        assert!(rects.container.width > 0);
        assert!(
            rects.container.bottom() <= bottom_text_row,
            "expected popover to flip above bottom row {bottom_text_row}, got {:?}",
            rects.container
        );
    }

    #[test]
    fn table_markdown_and_table_card_support_selection_quoting_inline_styles_and_links() {
        let theme = UiPalette::dark_deck();
        let mut transcript = Transcript::new();
        transcript.push(TranscriptEntry::AssistantMarkdown {
            speaker: "Commander".into(),
            markdown: "| File | Status | Link |\n| --- | --- | --- |\n| src/editor.rs:42 | **ready** | [docs](https://example.com) |".into(),
            streaming: false,
        });
        transcript.push(TranscriptEntry::TableCard {
            title: "Build Matrix".into(),
            headers: vec!["Target".into(), "Owner".into(), "Source".into()],
            rows: vec![
                vec![
                    "Windows".into(),
                    "**green**".into(),
                    "src/clipboard/windows.rs:12".into(),
                ],
                vec![
                    "Linux".into(),
                    "`passing`".into(),
                    "src/clipboard/unix.rs:8".into(),
                ],
            ],
            alignments: vec!["left".into(), "center".into(), "left".into()],
            caption: Some("All targets verified".into()),
        });

        let mut scroll = ScrollState::new();
        let mut terminal = Terminal::new(TestBackend::new(96, 28)).unwrap();
        terminal
            .draw(|f| render_transcript(f, f.area(), &transcript, &mut scroll, &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        let md_row = (0..28u16)
            .find(|&y| {
                let text: String = (0..96u16).map(|x| buf[(x, y)].symbol()).collect();
                text.contains("src/editor.rs:42")
            })
            .expect("markdown table row");
        let ready_col = (0..90u16)
            .find(|&x| {
                let slice: String = (0..5u16).map(|d| buf[(x + d, md_row)].symbol()).collect();
                slice == "ready"
            })
            .expect("ready col");
        assert!(buf[(ready_col, md_row)].modifier.contains(Modifier::BOLD));

        let links = scroll.links_in_viewport();
        assert!(
            links.iter().any(|(_, target)| target == "src/editor.rs:42"),
            "{links:?}"
        );
        assert!(
            links
                .iter()
                .any(|(_, target)| target == "https://example.com"),
            "{links:?}"
        );
        assert!(
            links
                .iter()
                .any(|(_, target)| target == "src/clipboard/windows.rs:12"),
            "{links:?}"
        );

        let card_row1 = (0..28u16)
            .find(|&y| {
                let text: String = (0..96u16).map(|x| buf[(x, y)].symbol()).collect();
                text.contains("Windows") && text.contains("green")
            })
            .expect("table card row 1");
        let card_row2 = card_row1 + 1;
        let win_col = (0..88u16)
            .find(|&x| {
                let slice: String = (0..7u16)
                    .map(|d| buf[(x + d, card_row1)].symbol())
                    .collect();
                slice == "Windows"
            })
            .expect("Windows col");
        assert!(scroll.begin_selection(win_col, card_row1));
        assert!(scroll.finish_selection(win_col + 6, card_row1));
        assert_eq!(scroll.extract_selection_text().as_deref(), Some("Windows"));

        assert!(scroll.begin_selection(win_col, card_row1));
        assert!(scroll.finish_selection(win_col + 14, card_row2));
        let multi = scroll
            .extract_selection_text()
            .expect("multi-row table card selection");
        assert!(multi.contains("Windows"), "{multi}");
        assert!(multi.contains("Linux"), "{multi}");
        assert!(!multi.ends_with('│'), "right box border leaked: {multi}");
    }

    #[test]
    fn soft_wrapped_paragraph_selection_joins_lines_without_spurious_newlines() {
        let theme = UiPalette::dark_deck();
        let mut transcript = Transcript::new();
        transcript.push(TranscriptEntry::AssistantMarkdown {
            speaker: "Commander".into(),
            markdown: "One two three four five six seven eight nine ten eleven twelve.\n\nSecond paragraph starts here.".into(),
            streaming: false,
        });
        let mut scroll = ScrollState::new();
        let mut terminal = Terminal::new(TestBackend::new(36, 16)).unwrap();
        terminal
            .draw(|f| render_transcript(f, f.area(), &transcript, &mut scroll, &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let first_row = (0..16u16)
            .find(|&y| {
                let text: String = (0..36u16).map(|x| buf[(x, y)].symbol()).collect();
                text.contains("One two three")
            })
            .expect("first wrapped row");
        assert!(scroll.begin_selection(2, first_row));
        assert!(scroll.finish_selection(30, first_row + 1));
        let selected = scroll.extract_selection_text().expect("wrapped selection");
        assert!(
            !selected.contains('\n'),
            "soft wrap inserted hard newline: {selected:?}"
        );
        assert!(selected.contains("One two three"), "{selected:?}");
        assert!(
            selected.contains("eight") || selected.contains("seven"),
            "{selected:?}"
        );
    }

    #[test]
    fn clickable_links_track_markdown_bare_urls_and_file_paths_across_wraps() {
        let theme = UiPalette::dark_deck();
        let mut transcript = Transcript::new();
        transcript.push(TranscriptEntry::AssistantMarkdown {
            speaker: "Commander".into(),
            markdown: "Visit https://example.com/very/long/path/that/wraps/across/lines and check src/markdown.rs#L10-L20 plus `src/mission.rs:88`.".into(),
            streaming: false,
        });
        let mut scroll = ScrollState::new();
        let mut terminal = Terminal::new(TestBackend::new(40, 14)).unwrap();
        terminal
            .draw(|f| render_transcript(f, f.area(), &transcript, &mut scroll, &theme))
            .unwrap();

        let links = scroll.links_in_viewport();
        let url_hits: Vec<_> = links
            .iter()
            .filter(|(_, t)| t == "https://example.com/very/long/path/that/wraps/across/lines")
            .collect();
        assert!(
            url_hits.len() >= 2,
            "expected wrapped URL hit regions across >=2 rows, got {url_hits:?}"
        );
        assert!(
            links.iter().any(|(_, t)| t == "src/markdown.rs#L10-L20"),
            "{links:?}"
        );
        assert!(
            links.iter().any(|(_, t)| t == "src/mission.rs:88"),
            "{links:?}"
        );

        let (rect, target) = &url_hits[0];
        assert_eq!(
            scroll.hit_test(rect.x, rect.y),
            Some(&TranscriptHitAction::OpenLink {
                target: target.clone(),
            })
        );
    }

    #[test]
    fn code_blocks_syntax_highlight_and_info_card_and_user_quotes_attachments_render_cleanly() {
        let theme = UiPalette::dark_deck();
        let code_lines = render_markdown(
            "```rust\npub fn compute(x: usize) -> usize {\n    let msg = \"ok\";\n    x + 42\n}\n```",
            72,
            &theme,
        );
        let fn_line = code_lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "fn"))
            .expect("fn keyword span");
        let fn_span = fn_line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "fn")
            .unwrap();
        assert_eq!(fn_span.style.fg, Some(theme.accent));
        assert!(fn_span.style.add_modifier.contains(Modifier::BOLD));

        let str_line = code_lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("\"ok\"")))
            .expect("string literal span");
        let str_span = str_line
            .spans
            .iter()
            .find(|s| s.content.contains("\"ok\""))
            .unwrap();
        assert_eq!(str_span.style.fg, Some(theme.success));

        let num_line = code_lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "42"))
            .expect("number literal span");
        let num_span = num_line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "42")
            .unwrap();
        assert_eq!(num_span.style.fg, Some(theme.warning));

        let mut transcript = Transcript::new();
        transcript.push_user_with_attachments(
            "> Quoted excerpt from assistant\n\nPlease inspect src/editor.rs",
            Some("12:34".into()),
            vec!["C:\\tmp\\shot.png".into()],
        );
        transcript.push(TranscriptEntry::InfoCard {
            title: "Vault Status".into(),
            level: "success".into(),
            badge: Some("CONNECTED".into()),
            fields: vec![
                ("Endpoint".into(), "https://api.example.com".into()),
                ("Config".into(), "src/mission.rs:12".into()),
            ],
            body_markdown: "All **credentials** sealed cleanly.".into(),
        });

        let mut scroll = ScrollState::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_transcript(f, f.area(), &transcript, &mut scroll, &theme))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer(), Rect::new(0, 0, 80, 24));
        assert!(text.contains("│ Quoted excerpt from assistant"), "{text}");
        assert!(text.contains("[img] shot.png"), "{text}");
        assert!(text.contains("Vault Status"), "{text}");
        assert!(text.contains("[CONNECTED]"), "{text}");
        assert!(text.contains("credentials"), "{text}");
        let links = scroll.links_in_viewport();
        assert!(
            links.iter().any(|(_, t)| t == "https://api.example.com"),
            "{links:?}"
        );
        assert!(
            links.iter().any(|(_, t)| t == "src/mission.rs:12"),
            "{links:?}"
        );
        assert!(links.iter().any(|(_, t)| t == "src/editor.rs"), "{links:?}");
    }
}
