use std::collections::BTreeSet;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
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
        self.push(TranscriptEntry::UserMessage {
            text: text.into(),
            timestamp,
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
                TranscriptEntry::UserMessage { text, .. } => {
                    out.push(format!("You:\n{text}"));
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

fn pad_to_width(text: &str, width: usize, alignment: Alignment) -> String {
    let clipped = truncate_str(text, width);
    let w = display_width(&clipped);
    let pad = width.saturating_sub(w);
    match alignment {
        Alignment::Right => format!("{}{}", " ".repeat(pad), clipped),
        Alignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), clipped, " ".repeat(right))
        }
        Alignment::None | Alignment::Left => format!("{}{}", clipped, " ".repeat(pad)),
    }
}

fn format_duration(duration_ms: u64) -> String {
    if duration_ms < 1000 {
        format!("{duration_ms}ms")
    } else {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    }
}

#[derive(Clone, Debug)]
struct StyledPiece {
    text: String,
    style: Style,
    breakable: bool,
}

fn push_piece(pieces: &mut Vec<StyledPiece>, text: String, style: Style, breakable: bool) {
    if text.is_empty() {
        return;
    }
    pieces.push(StyledPiece {
        text,
        style,
        breakable,
    });
}

fn tokenize_for_wrap(spans: &[Span<'static>]) -> Vec<StyledPiece> {
    let mut pieces = Vec::new();
    for span in spans {
        let content = span.content.as_ref();
        let style = span.style;
        let mut current = String::new();
        let mut in_space: Option<bool> = None;
        for g in content.graphemes(true) {
            let is_space = g.chars().all(|c| c == ' ' || c == '\t');
            if let Some(prev_space) = in_space {
                if prev_space != is_space {
                    push_piece(&mut pieces, std::mem::take(&mut current), style, prev_space);
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
            push_piece(&mut pieces, current, style, prev_space);
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

fn wrap_spans_with_prefix(
    spans: &[Span<'static>],
    width: usize,
    first_prefix: &[Span<'static>],
    cont_prefix: &[Span<'static>],
) -> Vec<Line<'static>> {
    let max_width = width.max(MIN_RENDER_WIDTH);
    let first_prefix_w: usize = first_prefix.iter().map(Span::width).sum();
    let cont_prefix_w: usize = cont_prefix.iter().map(Span::width).sum();

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = first_prefix.to_vec();
    let mut current_w = first_prefix_w;
    let mut at_line_content_start = true;

    let pieces = tokenize_for_wrap(spans);
    if pieces.is_empty() {
        lines.push(Line::from(current));
        return lines;
    }

    for piece in pieces {
        let piece_w = display_width(&piece.text);
        if piece.breakable && at_line_content_start {
            continue;
        }
        let avail_for_cont = max_width.saturating_sub(cont_prefix_w).max(1);
        if current_w + piece_w <= max_width {
            push_span_merged(&mut current, &piece.text, piece.style);
            current_w += piece_w;
            at_line_content_start = false;
            continue;
        }
        if piece.breakable {
            lines.push(Line::from(current));
            current = cont_prefix.to_vec();
            current_w = cont_prefix_w;
            at_line_content_start = true;
            continue;
        }
        if !at_line_content_start && piece_w <= avail_for_cont {
            lines.push(Line::from(current));
            current = cont_prefix.to_vec();
            current_w = cont_prefix_w;
            push_span_merged(&mut current, &piece.text, piece.style);
            current_w += piece_w;
            at_line_content_start = false;
            continue;
        }
        for g in piece.text.graphemes(true) {
            let gw = display_width(g);
            if !at_line_content_start && current_w + gw > max_width {
                lines.push(Line::from(current));
                current = cont_prefix.to_vec();
                current_w = cont_prefix_w;
            }
            push_span_merged(&mut current, g, piece.style);
            current_w += gw;
            at_line_content_start = false;
        }
    }

    if !current.is_empty() || lines.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

fn wrap_preserving_indent(
    text: &str,
    width: usize,
    style: Style,
    prefix: &[Span<'static>],
    pad_bg: Option<Color>,
) -> Vec<Line<'static>> {
    let max_width = width.max(MIN_RENDER_WIDTH);
    let prefix_w: usize = prefix.iter().map(Span::width).sum();
    let content_budget = max_width.saturating_sub(prefix_w).max(1);
    let expanded = text.replace('\t', "    ");

    let mut out = Vec::new();
    let mut line_buf = String::new();
    let mut used = 0usize;

    for g in expanded.graphemes(true) {
        let gw = display_width(g);
        if used > 0 && used + gw > content_budget {
            let mut spans = prefix.to_vec();
            spans.push(Span::styled(std::mem::take(&mut line_buf), style));
            if let Some(bg) = pad_bg {
                let pad = content_budget.saturating_sub(used);
                if pad > 0 {
                    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
                }
            }
            out.push(Line::from(spans));
            used = 0;
        }
        line_buf.push_str(g);
        used += gw;
    }

    let mut spans = prefix.to_vec();
    spans.push(Span::styled(line_buf, style));
    if let Some(bg) = pad_bg {
        let pad = content_budget.saturating_sub(used);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
        }
    }
    out.push(Line::from(spans));
    out
}

#[derive(Clone, Debug)]
struct ListContext {
    ordered: bool,
    next_index: u64,
}

#[derive(Default)]
struct TableBuffer {
    alignments: Vec<Alignment>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_head: bool,
    in_cell: bool,
}

struct MarkdownRenderer<'a> {
    width: usize,
    theme: &'a UiPalette,
    lines: Vec<Line<'static>>,
    pending_spans: Vec<Span<'static>>,
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
            let is_empty = last.spans.iter().all(|s| s.content.trim().is_empty());
            if !is_empty {
                if self.quote_depth > 0 {
                    let bars: Vec<Span<'static>> = (0..self.quote_depth)
                        .map(|_| Span::styled("│ ", Style::default().fg(self.theme.accent)))
                        .collect();
                    self.lines.push(Line::from(bars));
                } else {
                    self.lines.push(Line::default());
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
        let wrapped = wrap_spans_with_prefix(&spans, self.width, &first_prefix, &cont_prefix);
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
        self.lines.push(Line::from(header_spans));

        let mut code_line_prefix = quote_prefix.clone();
        code_line_prefix.push(Span::styled(
            "│ ",
            Style::default()
                .fg(self.theme.border)
                .bg(self.theme.code_bg),
        ));
        let code_style = Style::default().fg(self.theme.fg).bg(self.theme.code_bg);

        if trimmed_code.is_empty() {
            let wrapped = wrap_preserving_indent(
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
                let wrapped = wrap_preserving_indent(
                    clean_line,
                    self.width,
                    code_style,
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
        self.lines.push(Line::from(footer_spans));
    }

    fn flush_table(&mut self) {
        let table = std::mem::take(&mut self.table);
        let col_count = table
            .header
            .len()
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if col_count == 0 {
            return;
        }
        let mut widths = vec![1usize; col_count];
        for (i, cell) in table.header.iter().enumerate() {
            widths[i] = widths[i].max(display_width(cell));
        }
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(display_width(cell));
            }
        }

        let separators_w = col_count.saturating_sub(1) * 3 + 2;
        let max_cells_w = self.width.saturating_sub(separators_w).max(col_count);
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

        let render_row = |cells: &[String], style: Style, border_style: Style| -> Line<'static> {
            let mut spans = vec![Span::styled(" ", style)];
            for (idx, &col_w) in widths.iter().enumerate() {
                if idx > 0 {
                    spans.push(Span::styled(" │ ", border_style));
                }
                let text = cells.get(idx).map(String::as_str).unwrap_or("");
                let align = table
                    .alignments
                    .get(idx)
                    .copied()
                    .unwrap_or(Alignment::Left);
                spans.push(Span::styled(pad_to_width(text, col_w, align), style));
            }
            Line::from(spans)
        };

        let border_style = Style::default().fg(self.theme.border);
        if !table.header.is_empty() {
            self.lines.push(render_row(
                &table.header,
                Style::default()
                    .fg(self.theme.accent_soft)
                    .add_modifier(Modifier::BOLD),
                border_style,
            ));
            let mut sep = String::from("─");
            for (idx, &col_w) in widths.iter().enumerate() {
                if idx > 0 {
                    sep.push_str("─┼─");
                }
                sep.push_str(&"─".repeat(col_w));
            }
            self.lines.push(Line::from(Span::styled(sep, border_style)));
        }
        for row in &table.rows {
            self.lines.push(render_row(
                row,
                Style::default().fg(self.theme.fg),
                border_style,
            ));
        }
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
                }
                Event::End(TagEnd::TableCell) => {
                    let cell = std::mem::take(&mut self.table.current_cell);
                    self.table.current_row.push(cell.trim().to_owned());
                    self.table.in_cell = false;
                }
                Event::Text(text) | Event::Code(text) if self.table.in_cell => {
                    self.table.current_cell.push_str(&text);
                }
                Event::SoftBreak | Event::HardBreak if self.table.in_cell => {
                    self.table.current_cell.push(' ');
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
                        .push(Span::styled(marker.to_owned(), self.current_style()));
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
                self.pending_spans
                    .push(Span::styled(marker.to_owned(), style));
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
                        .is_some_and(|s| s.content.as_ref() == url);
                    if !url.is_empty() && !already_shown {
                        self.pending_spans.push(Span::styled(
                            format!(" ({url})"),
                            Style::default().fg(self.theme.muted),
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
                self.pending_spans
                    .push(Span::styled(text.to_string(), style));
            }
            Event::Code(code) => {
                let style = Style::default()
                    .fg(self.theme.warning)
                    .bg(self.theme.code_bg);
                self.pending_spans
                    .push(Span::styled(format!(" {code} "), style));
            }
            Event::SoftBreak => {
                let style = self.current_style();
                self.pending_spans.push(Span::styled(" ".to_owned(), style));
            }
            Event::HardBreak => {
                self.flush_pending();
            }
            Event::Rule => {
                self.flush_pending();
                self.ensure_blank_line();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(self.width),
                    Style::default().fg(self.theme.border),
                )));
                self.ensure_blank_line();
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
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
            .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
        {
            self.lines.pop();
        }
        self.lines
    }
}

pub fn render_markdown(md: &str, width: usize, theme: &UiPalette) -> Vec<Line<'static>> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(md, options);
    let mut renderer = MarkdownRenderer::new(width, theme);
    for event in parser {
        renderer.process(event);
    }
    renderer.finish()
}

pub fn render_unified_diff(diff: &str, width: usize, theme: &UiPalette) -> Vec<Line<'static>> {
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
        lines.extend(wrap_preserving_indent(line, max_width, style, &[], pad_bg));
    }
    lines
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
    build_transcript_lines_with_activity(transcript, width, theme, None)
}

fn build_transcript_lines_with_activity(
    transcript: &Transcript,
    width: usize,
    theme: &UiPalette,
    activity: Option<&LiveActivity>,
) -> (Vec<Line<'static>>, Vec<usize>, Vec<LineHitRegion>) {
    let max_width = width.max(MIN_RENDER_WIDTH);
    let mut lines: Vec<Line<'static>> = Vec::new();
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
                lines.push(Line::default());
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
            lines.push(Line::from(Span::styled(
                header_text,
                Style::default().fg(if any_running {
                    theme.warning
                } else {
                    theme.muted
                }),
            )));
            hit_regions.push(LineHitRegion {
                line_index,
                col_start: 0,
                col_width: max_width,
                action: TranscriptHitAction::ToggleExpand { entry_index: start },
            });
            if expanded {
                for sub_idx in start..idx {
                    render_single_entry(
                        sub_idx,
                        &transcript.entries[sub_idx],
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
            lines.push(Line::default());
        }
        let label = truncate_str(&act.label, max_width.saturating_sub(4).max(1));
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", act.spinner),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(label, Style::default().fg(theme.accent_soft)),
        ]));
    }

    (lines, message_anchors, hit_regions)
}

#[allow(clippy::too_many_arguments)]
fn render_single_entry(
    entry_index: usize,
    entry: &TranscriptEntry,
    max_width: usize,
    theme: &UiPalette,
    active_spinner: Option<&str>,
    lines: &mut Vec<Line<'static>>,
    message_anchors: &mut Vec<usize>,
    hit_regions: &mut Vec<LineHitRegion>,
) {
    match entry {
        TranscriptEntry::UserMessage { text, timestamp } => {
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
            lines.push(Line::from(header_spans));
            let body_prefix = [Span::styled("  ", Style::default().fg(theme.fg))];
            for raw in text.split('\n') {
                let wrapped = wrap_spans_with_prefix(
                    &[Span::styled(raw.to_owned(), Style::default().fg(theme.fg))],
                    max_width,
                    &body_prefix,
                    &body_prefix,
                );
                lines.extend(wrapped);
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
            lines.push(Line::from(header_spans));
            let content_w = max_width.saturating_sub(2).max(MIN_RENDER_WIDTH);
            for mut line in render_markdown(markdown, content_w, theme) {
                line.spans.insert(0, Span::raw("  "));
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
                lines.push(Line::from(Span::styled(
                    summary,
                    Style::default()
                        .fg(theme.subtle)
                        .add_modifier(Modifier::ITALIC),
                )));
            } else {
                let header = truncate_str(&format!("  ▾ Thought for {duration}"), max_width);
                lines.push(Line::from(Span::styled(
                    header,
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::ITALIC),
                )));
                let body_w = max_width.saturating_sub(4).max(MIN_RENDER_WIDTH);
                let rendered = if text.trim().is_empty() {
                    Vec::new()
                } else {
                    render_markdown(text, body_w, theme)
                };
                if rendered.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(theme.border)),
                        Span::styled(
                            truncate_str("(No reasoning text streamed by provider)", body_w),
                            Style::default()
                                .fg(theme.subtle)
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                } else {
                    for mut line in rendered {
                        line.spans
                            .insert(0, Span::styled("  │ ", Style::default().fg(theme.border)));
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
            lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(status_color),
            )));
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
                lines.push(Line::from(Span::styled(
                    sub,
                    Style::default().fg(theme.muted),
                )));
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
            lines.extend(wrap_spans_with_prefix(
                &[Span::styled(text.clone(), Style::default().fg(text_color))],
                max_width,
                &prefix,
                &cont,
            ));
        }
    }
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
    lines: &mut Vec<Line<'static>>,
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
        lines.push(Line::from(vec![
            Span::styled(left, Style::default().fg(left_fg)),
            Span::raw(" ".repeat(gap)),
            Span::styled(status_text, Style::default().fg(status_color)),
        ]));
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
                lines.push(Line::from(Span::styled(
                    preview,
                    Style::default().fg(theme.danger),
                )));
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
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(clipped_left, Style::default().fg(theme.accent_soft)),
        Span::styled("─".repeat(actual_fill), Style::default().fg(theme.border)),
        Span::styled(header_right, Style::default().fg(status_color)),
    ]));
    hit_regions.push(LineHitRegion {
        line_index,
        col_start: 2,
        col_width: box_w,
        action: TranscriptHitAction::ToggleExpand { entry_index },
    });

    let body_lines = if output.is_empty() {
        vec![Line::from(Span::styled(
            "(no output)",
            Style::default().fg(theme.subtle),
        ))]
    } else if looks_like_unified_diff(output) {
        render_unified_diff(output, inner_w, theme)
    } else {
        let mut rendered = Vec::new();
        for raw in output.split('\n') {
            let clean = raw.strip_suffix('\r').unwrap_or(raw);
            rendered.extend(wrap_preserving_indent(
                clean,
                inner_w,
                Style::default().fg(theme.fg),
                &[],
                None,
            ));
        }
        rendered
    };

    for mut bline in body_lines {
        let line_w: usize = bline.spans.iter().map(Span::width).sum();
        let pad = inner_w.saturating_sub(line_w);
        bline
            .spans
            .insert(0, Span::styled("  │ ", Style::default().fg(theme.border)));
        if pad > 0 {
            bline.spans.push(Span::raw(" ".repeat(pad)));
        }
        bline
            .spans
            .push(Span::styled(" │", Style::default().fg(theme.border)));
        lines.push(bline);
    }

    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))),
            Style::default().fg(theme.border),
        ),
    ]));
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
    lines: &mut Vec<Line<'static>>,
    hit_regions: &mut Vec<LineHitRegion>,
) {
    if resolved {
        let text = truncate_str(&format!("  ✓ Connected: {title}"), max_width);
        lines.push(Line::from(Span::styled(
            text,
            Style::default().fg(theme.success),
        )));
        return;
    }

    let box_w = max_width.saturating_sub(2).max(16);
    let inner_w = box_w.saturating_sub(4).max(8);
    let header_title = format!("╭─ Connect: {title} ");
    let clipped_hdr = truncate_str(&header_title, box_w.saturating_sub(2));
    let fill = box_w.saturating_sub(display_width(&clipped_hdr) + 1);
    lines.push(Line::from(vec![
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
    ]));

    let wrapped = wrap_spans_with_prefix(
        &[Span::styled(
            detail.to_owned(),
            Style::default().fg(theme.fg),
        )],
        inner_w,
        &[],
        &[],
    );
    for mut line in wrapped {
        let lw: usize = line.spans.iter().map(Span::width).sum();
        let pad = inner_w.saturating_sub(lw);
        line.spans
            .insert(0, Span::styled("  │ ", Style::default().fg(theme.warning)));
        if pad > 0 {
            line.spans.push(Span::raw(" ".repeat(pad)));
        }
        line.spans
            .push(Span::styled(" │", Style::default().fg(theme.warning)));
        lines.push(line);
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
    lines.push(Line::from(vec![
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
    ]));
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

    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))),
            Style::default().fg(theme.warning),
        ),
    ]));
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
    lines: &mut Vec<Line<'static>>,
    hit_regions: &mut Vec<LineHitRegion>,
) {
    if resolved {
        let text = truncate_str(&format!("  ✓ ProdGate resolved: {tool}"), max_width);
        lines.push(Line::from(Span::styled(
            text,
            Style::default().fg(theme.muted),
        )));
        return;
    }

    let box_w = max_width.saturating_sub(2).max(16);
    let inner_w = box_w.saturating_sub(4).max(8);
    let header_title = format!("╭─ ProdGate Approval: {tool} ");
    let clipped_hdr = truncate_str(&header_title, box_w.saturating_sub(2));
    let fill = box_w.saturating_sub(display_width(&clipped_hdr) + 1);
    lines.push(Line::from(vec![
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
    ]));

    for (label, value, value_style) in [
        (
            "Command: ",
            command,
            Style::default().fg(theme.warning).bg(theme.code_bg),
        ),
        ("Reason:  ", reason, Style::default().fg(theme.fg)),
    ] {
        let wrapped = wrap_spans_with_prefix(
            &[
                Span::styled(label.to_owned(), Style::default().fg(theme.muted)),
                Span::styled(value.to_owned(), value_style),
            ],
            inner_w,
            &[],
            &[Span::raw("         ")],
        );
        for mut line in wrapped {
            let lw: usize = line.spans.iter().map(Span::width).sum();
            let pad = inner_w.saturating_sub(lw);
            line.spans
                .insert(0, Span::styled("  │ ", Style::default().fg(theme.danger)));
            if pad > 0 {
                line.spans.push(Span::raw(" ".repeat(pad)));
            }
            line.spans
                .push(Span::styled(" │", Style::default().fg(theme.danger)));
            lines.push(line);
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
    lines.push(Line::from(vec![
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
    ]));
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

    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("╰{}╯", "─".repeat(box_w.saturating_sub(2))),
            Style::default().fg(theme.danger),
        ),
    ]));
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
    if area.width == 0 || area.height == 0 {
        scroll.sync_metrics(0, 0, Vec::new());
        return;
    }

    let content_width = area.width.saturating_sub(1).max(1) as usize;
    let (mut all_lines, anchors, regions) =
        build_transcript_lines_with_activity(transcript, content_width, theme, activity);
    if !all_lines.is_empty() {
        all_lines.push(Line::default());
    }
    let viewport_h = area.height as usize;
    scroll.sync_metrics(all_lines.len(), viewport_h, anchors);

    let start = scroll.offset();
    let end = (start + viewport_h).min(all_lines.len());
    let visible: Vec<Line<'static>> = all_lines[start..end].to_vec();

    let content_area = Rect::new(area.x, area.y, content_width as u16, area.height);
    frame.render_widget(
        Paragraph::new(visible).style(Style::default().bg(theme.bg).fg(theme.fg)),
        content_area,
    );

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
        let (live_lines, _, _) =
            build_transcript_lines_with_activity(&transcript, 80, &theme, Some(&activity));
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
}
