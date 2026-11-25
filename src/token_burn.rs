use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph, Widget},
};
use serde::Deserialize;

pub const LOW_ALLOWANCE_PERCENT: f64 = 20.0;
pub const FULL_DECK_MIN_WIDTH: u16 = 100;
pub const FULL_DECK_MIN_HEIGHT: u16 = 30;
pub const FULL_CARD_HEIGHT: u16 = 6;
pub const NAME_MAX_WIDTH: usize = 18;

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const GAUGE_LABEL_WIDTH: usize = 5;
const GAUGE_PERCENT_WIDTH: usize = 5;
const COMPACT_BAR_WIDTH: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenBurnPalette {
    pub deck: Color,
    pub panel: Color,
    pub border: Color,
    pub foreground: Color,
    pub muted: Color,
    pub accent: Color,
    pub pill_text: Color,
    pub tier_max: Color,
    pub tier_pro: Color,
    pub tier_plus: Color,
    pub healthy: Color,
    pub depleted: Color,
    pub unknown: Color,
    pub reset_hour: Color,
    pub reset_six_hours: Color,
    pub reset_day: Color,
    pub reset_later: Color,
}

impl Default for TokenBurnPalette {
    fn default() -> Self {
        Self {
            deck: rgb(0x0b0d10),
            panel: rgb(0x15181e),
            border: rgb(0x2a3039),
            foreground: rgb(0xeef1f5),
            muted: rgb(0x7c8591),
            accent: rgb(0xff8a3d),
            pill_text: rgb(0x0b0d10),
            tier_max: rgb(0xc792ff),
            tier_pro: rgb(0x5ab0ff),
            tier_plus: rgb(0x3ddc97),
            healthy: rgb(0x3ddc97),
            depleted: rgb(0xff4d5e),
            unknown: rgb(0x5d6570),
            reset_hour: rgb(0x3ddc97),
            reset_six_hours: rgb(0xffbc26),
            reset_day: rgb(0x44adff),
            reset_later: rgb(0xb680ff),
        }
    }
}

#[derive(Deserialize)]
struct DeckColors {
    background: Option<String>,
    panel: Option<String>,
    border: Option<String>,
    foreground: Option<String>,
    muted: Option<String>,
    accent: Option<String>,
}

impl TokenBurnPalette {
    pub fn from_theme_toml(text: &str) -> Result<Self, String> {
        let colors: DeckColors = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut palette = Self::default();
        for (value, slot) in [
            (colors.background, &mut palette.deck),
            (colors.panel, &mut palette.panel),
            (colors.border, &mut palette.border),
            (colors.foreground, &mut palette.foreground),
            (colors.muted, &mut palette.muted),
            (colors.accent, &mut palette.accent),
        ] {
            if let Some(hex) = value {
                *slot = parse_hex(&hex)?;
            }
        }
        palette.pill_text = palette.deck;
        Ok(palette)
    }

    pub fn allowance_color(&self, remaining_percent: Option<f64>) -> Color {
        match AllowanceState::of(remaining_percent) {
            AllowanceState::Healthy => self.healthy,
            AllowanceState::Low => self.depleted,
            AllowanceState::Unknown => self.unknown,
        }
    }

    pub fn reset_color(&self, bucket: ResetBucket) -> Color {
        match bucket {
            ResetBucket::Missing | ResetBucket::Due => self.muted,
            ResetBucket::WithinHour => self.reset_hour,
            ResetBucket::WithinSixHours => self.reset_six_hours,
            ResetBucket::WithinDay => self.reset_day,
            ResetBucket::Later => self.reset_later,
        }
    }

    pub fn tier_color(&self, tier: &str) -> Color {
        match tier.trim().to_ascii_uppercase().as_str() {
            "MAX" | "ULTRA" | "ENTERPRISE" => self.tier_max,
            "PRO" | "WORKSPACE" | "VERTEX" => self.tier_pro,
            "PLUS" | "GOOGLE" | "BEDROCK" => self.tier_plus,
            "" => self.unknown,
            _ => self.accent,
        }
    }
}

pub fn normalize_tier(raw: Option<&str>) -> String {
    let Some(text) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return String::new();
    };
    let lower = text.to_ascii_lowercase();
    if lower.contains("max") {
        "max".into()
    } else if lower.contains("ultra") {
        "ultra".into()
    } else if lower.contains("enterprise") {
        "enterprise".into()
    } else if lower.contains("team") {
        "team".into()
    } else if lower.contains("workspace") {
        "workspace".into()
    } else if lower.contains("pro") {
        "pro".into()
    } else if lower.contains("plus") {
        "plus".into()
    } else if lower.contains("bedrock") {
        "bedrock".into()
    } else if lower.contains("vertex") {
        "vertex".into()
    } else if lower.contains("foundry") {
        "foundry".into()
    } else if lower.contains("google") || lower.contains("oauth") {
        "google".into()
    } else if lower.contains("api") {
        "api".into()
    } else {
        lower
    }
}

fn rgb(value: u32) -> Color {
    Color::Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

fn parse_hex(hex: &str) -> Result<Color, String> {
    let digits = hex.trim_start_matches('#');
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "Theme color {hex:?} must contain six hexadecimal digits"
        ));
    }
    u32::from_str_radix(digits, 16)
        .map(rgb)
        .map_err(|e| e.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllowanceState {
    Healthy,
    Low,
    Unknown,
}

impl AllowanceState {
    pub fn of(remaining_percent: Option<f64>) -> Self {
        match remaining_percent.filter(|p| p.is_finite()) {
            Some(p) if p >= LOW_ALLOWANCE_PERCENT => Self::Healthy,
            Some(_) => Self::Low,
            None => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetBucket {
    Missing,
    Due,
    WithinHour,
    WithinSixHours,
    WithinDay,
    Later,
}

impl ResetBucket {
    pub fn of(seconds_until_reset: Option<i64>) -> Self {
        match seconds_until_reset {
            None => Self::Missing,
            Some(s) if s <= 0 => Self::Due,
            Some(s) if s <= HOUR => Self::WithinHour,
            Some(s) if s <= 6 * HOUR => Self::WithinSixHours,
            Some(s) if s <= DAY => Self::WithinDay,
            Some(_) => Self::Later,
        }
    }
}

pub fn countdown_label(seconds_until_reset: Option<i64>) -> String {
    match (ResetBucket::of(seconds_until_reset), seconds_until_reset) {
        (ResetBucket::Missing, _) | (_, None) => "-".into(),
        (ResetBucket::Due, _) => "due".into(),
        (_, Some(s)) if s < HOUR => format!("{}m", (s + MINUTE - 1) / MINUTE),
        (_, Some(s)) if s < DAY => format!("{}h {:02}m", s / HOUR, s % HOUR / MINUTE),
        (_, Some(s)) => format!("{}d {}h", s / DAY, s % DAY / HOUR),
    }
}

pub fn clamp_percent(remaining_percent: Option<f64>) -> Option<f64> {
    remaining_percent
        .filter(|p| p.is_finite())
        .map(|p| p.clamp(0.0, 100.0))
}

pub fn percent_label(remaining_percent: Option<f64>) -> String {
    clamp_percent(remaining_percent).map_or_else(|| "--".into(), |p| format!("{:.0}%", p.floor()))
}

pub fn tier_pill(tier: &str, palette: &TokenBurnPalette) -> Span<'static> {
    let label = match tier.trim() {
        "" => "-".to_string(),
        named => named.to_ascii_uppercase(),
    };
    Span::styled(
        format!(" {label} "),
        Style::default()
            .fg(palette.pill_text)
            .bg(palette.tier_color(tier))
            .add_modifier(Modifier::BOLD),
    )
}

pub fn reset_badge(seconds_until_reset: Option<i64>, palette: &TokenBurnPalette) -> Span<'static> {
    let color = palette.reset_color(ResetBucket::of(seconds_until_reset));
    Span::styled(
        format!("↻ {}", countdown_label(seconds_until_reset)),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

pub fn allowance_gauge(
    label: &str,
    remaining_percent: Option<f64>,
    bar_width: usize,
    palette: &TokenBurnPalette,
) -> Vec<Span<'static>> {
    let color = palette.allowance_color(remaining_percent);
    let filled = clamp_percent(remaining_percent)
        .map_or(0, |p| ((p / 100.0) * bar_width as f64).round() as usize)
        .min(bar_width);
    vec![
        Span::styled(
            format!("{label:<GAUGE_LABEL_WIDTH$}"),
            Style::default().fg(palette.muted),
        ),
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "░".repeat(bar_width - filled),
            Style::default().fg(palette.border),
        ),
        Span::styled(
            format!("{:>GAUGE_PERCENT_WIDTH$}", percent_label(remaining_percent)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountCard {
    pub provider: String,
    pub profile: String,
    pub tier: String,
    pub primary_remaining: Option<f64>,
    pub secondary_remaining: Option<f64>,
    pub seconds_until_reset: Option<i64>,
    pub route_model: Option<String>,
    pub route_metric: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeckMode {
    Full,
    Compact,
}

impl DeckMode {
    pub fn for_area(area: Rect) -> Self {
        if area.width >= FULL_DECK_MIN_WIDTH && area.height >= FULL_DECK_MIN_HEIGHT {
            Self::Full
        } else {
            Self::Compact
        }
    }
}

pub fn clip(text: &str, max_width: usize) -> String {
    if Span::raw(text).width() <= max_width {
        return text.to_string();
    }
    let mut clipped = String::new();
    for ch in text.chars() {
        let next = format!("{clipped}{ch}…");
        if Span::raw(next.as_str()).width() > max_width {
            break;
        }
        clipped.push(ch);
    }
    if max_width > 0 {
        clipped.push('…');
    }
    clipped
}

pub fn fit_line(spans: Vec<Span<'static>>, max_width: usize) -> Line<'static> {
    let mut used = 0;
    let mut fitted = Vec::new();
    for span in spans {
        let room = max_width.saturating_sub(used);
        if room == 0 {
            break;
        }
        let span = if span.width() <= room {
            span
        } else {
            Span::styled(clip(&span.content, room), span.style)
        };
        used += span.width();
        fitted.push(span);
    }
    Line::from(fitted)
}

impl AccountCard {
    fn title(&self) -> String {
        clip(
            &format!("{} · {}", self.provider, self.profile),
            NAME_MAX_WIDTH,
        )
    }

    fn route_badge(&self, palette: &TokenBurnPalette) -> Vec<Span<'static>> {
        let Some(model) = &self.route_model else {
            let reason = self.route_metric.as_deref().unwrap_or("unrouted");
            return vec![Span::styled(
                format!("▸ {reason}"),
                Style::default().fg(palette.muted),
            )];
        };
        let mut spans = vec![Span::styled(
            format!("▸ {model}"),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )];
        if let Some(metric) = &self.route_metric {
            spans.push(Span::styled(
                format!(" · {metric}"),
                Style::default().fg(palette.muted),
            ));
        }
        spans
    }

    pub fn full_lines(&self, inner_width: u16, palette: &TokenBurnPalette) -> Vec<Line<'static>> {
        let width = inner_width as usize;
        let bar_width = width.saturating_sub(GAUGE_LABEL_WIDTH + GAUGE_PERCENT_WIDTH);
        let gap = Span::raw("  ");
        let header = vec![
            tier_pill(&self.tier, palette),
            gap,
            reset_badge(self.seconds_until_reset, palette),
        ];
        vec![
            fit_line(header, width),
            fit_line(
                allowance_gauge("5h", self.primary_remaining, bar_width, palette),
                width,
            ),
            fit_line(
                allowance_gauge("week", self.secondary_remaining, bar_width, palette),
                width,
            ),
            fit_line(self.route_badge(palette), width),
        ]
    }

    pub fn compact_line(&self, width: u16, palette: &TokenBurnPalette) -> Line<'static> {
        let gap = || Span::raw(" ");
        let mut spans = vec![
            Span::styled(
                format!("{:<NAME_MAX_WIDTH$}", self.title()),
                Style::default().fg(palette.foreground),
            ),
            gap(),
            tier_pill(&self.tier, palette),
            gap(),
        ];
        spans.extend(allowance_gauge(
            "5h",
            self.primary_remaining,
            COMPACT_BAR_WIDTH,
            palette,
        ));
        spans.push(gap());
        spans.extend(allowance_gauge(
            "wk",
            self.secondary_remaining,
            COMPACT_BAR_WIDTH,
            palette,
        ));
        spans.push(gap());
        spans.push(reset_badge(self.seconds_until_reset, palette));
        spans.push(gap());
        spans.extend(self.route_badge(palette));
        fit_line(spans, width as usize)
    }

    pub fn render_full(&self, area: Rect, buf: &mut Buffer, palette: &TokenBurnPalette) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette.border))
            .style(Style::default().bg(palette.panel).fg(palette.foreground))
            .title(Span::styled(
                format!(" {} ", self.title()),
                Style::default()
                    .fg(palette.foreground)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        block.render(area, buf);
        Paragraph::new(self.full_lines(inner.width, palette)).render(inner, buf);
    }
}

pub fn render_deck(
    cards: &[AccountCard],
    area: Rect,
    buf: &mut Buffer,
    palette: &TokenBurnPalette,
) {
    render_deck_as(DeckMode::for_area(area), cards, area, buf, palette);
}

pub fn render_deck_as(
    mode: DeckMode,
    cards: &[AccountCard],
    area: Rect,
    buf: &mut Buffer,
    palette: &TokenBurnPalette,
) {
    buf.set_style(
        area,
        Style::default().bg(palette.deck).fg(palette.foreground),
    );
    let row_height = match mode {
        DeckMode::Full => FULL_CARD_HEIGHT,
        DeckMode::Compact => 1,
    };
    let capacity = (area.height / row_height) as usize;
    for (index, card) in cards.iter().take(capacity).enumerate() {
        let row = Rect {
            y: area.y + index as u16 * row_height,
            height: row_height,
            ..area
        };
        match mode {
            DeckMode::Full => card.render_full(row, buf, palette),
            DeckMode::Compact => Paragraph::new(card.compact_line(row.width, palette))
                .style(Style::default().bg(palette.panel))
                .render(row, buf),
        }
    }
}

pub const RING_CELL_WIDTH: u16 = 8;
pub const RING_CELL_HEIGHT: u16 = 4;
pub const RING_TILE_HEIGHT: u16 = RING_CELL_HEIGHT;
const BRAILLE_BASE: u32 = 0x2800;
const BRAILLE_DOT_BITS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
const RING_STROKE: f64 = 0.62;
const RING_SPACING: f64 = 2.6;
const RING_TEXT_GAP: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingDot {
    Empty,
    Track(usize),
    Fill(usize),
}

pub fn ring_dot_grid(
    width_cells: u16,
    height_cells: u16,
    rings: &[Option<f64>],
) -> Vec<Vec<RingDot>> {
    let columns = usize::from(width_cells) * 2;
    let rows = usize::from(height_cells) * 4;
    let center_x = (columns as f64 - 1.0) / 2.0;
    let center_y = (rows as f64 - 1.0) / 2.0;
    let outer_radius = (columns.min(rows) as f64) / 2.0 - 0.5;
    (0..rows)
        .map(|row| {
            (0..columns)
                .map(|column| {
                    let dx = column as f64 - center_x;
                    let dy = row as f64 - center_y;
                    classify_dot(dx, dy, outer_radius, rings)
                })
                .collect()
        })
        .collect()
}

fn classify_dot(dx: f64, dy: f64, outer_radius: f64, rings: &[Option<f64>]) -> RingDot {
    let distance = dx.hypot(dy);
    let Some(index) = (0..rings.len()).find(|index| {
        let radius = outer_radius - *index as f64 * RING_SPACING;
        radius > 0.0 && (distance - radius).abs() <= RING_STROKE
    }) else {
        return RingDot::Empty;
    };
    let sweep = clockwise_fraction_from_top(dx, dy);
    match clamp_percent(rings[index]) {
        Some(percent) if sweep < percent / 100.0 => RingDot::Fill(index),
        _ => RingDot::Track(index),
    }
}

pub fn clockwise_fraction_from_top(dx: f64, dy: f64) -> f64 {
    let degrees = dx.atan2(-dy).to_degrees();
    degrees.rem_euclid(360.0) / 360.0
}

fn ring_track_color(rings: &[Option<f64>], index: usize, palette: &TokenBurnPalette) -> Color {
    match clamp_percent(rings[index]) {
        Some(_) => palette.border,
        None => palette.unknown,
    }
}

fn braille_cell(
    dots: &[Vec<RingDot>],
    cell_x: usize,
    cell_y: usize,
    rings: &[Option<f64>],
    palette: &TokenBurnPalette,
) -> Span<'static> {
    let mut members = Vec::new();
    for (row_offset, bits) in BRAILLE_DOT_BITS.iter().enumerate() {
        for (column_offset, bit) in bits.iter().enumerate() {
            members.push((
                dots[cell_y * 4 + row_offset][cell_x * 2 + column_offset],
                *bit,
            ));
        }
    }
    let ring = members
        .iter()
        .filter_map(|(dot, _)| match dot {
            RingDot::Fill(index) | RingDot::Track(index) => Some(*index),
            RingDot::Empty => None,
        })
        .min();
    let (pattern, color) = match ring {
        Some(ring) if members.iter().any(|(dot, _)| *dot == RingDot::Fill(ring)) => (
            dot_pattern(&members, |dot| dot == RingDot::Fill(ring)),
            palette.allowance_color(rings[ring]),
        ),
        Some(ring) => (
            dot_pattern(&members, |dot| dot == RingDot::Track(ring)),
            ring_track_color(rings, ring, palette),
        ),
        None => (0, palette.muted),
    };
    if pattern == 0 {
        return Span::raw(" ");
    }
    let symbol = char::from_u32(BRAILLE_BASE + u32::from(pattern)).unwrap_or(' ');
    Span::styled(symbol.to_string(), Style::default().fg(color))
}

fn dot_pattern(members: &[(RingDot, u8)], include: impl Fn(RingDot) -> bool) -> u8 {
    members
        .iter()
        .filter(|(dot, _)| include(*dot))
        .fold(0, |pattern, (_, bit)| pattern | bit)
}

pub fn braille_ring_lines(
    rings: &[Option<f64>],
    width_cells: u16,
    height_cells: u16,
    palette: &TokenBurnPalette,
) -> Vec<Line<'static>> {
    let rings = if rings.is_empty() { &[None][..] } else { rings };
    let dots = ring_dot_grid(width_cells, height_cells, rings);
    let mut lines: Vec<Vec<Span<'static>>> = (0..usize::from(height_cells))
        .map(|cell_y| {
            (0..usize::from(width_cells))
                .map(|cell_x| braille_cell(&dots, cell_x, cell_y, rings, palette))
                .collect()
        })
        .collect();
    if rings.len() == 1 {
        overlay_center_label(&mut lines, rings[0], palette);
    }
    lines.into_iter().map(Line::from).collect()
}

fn overlay_center_label(
    lines: &mut [Vec<Span<'static>>],
    remaining: Option<f64>,
    palette: &TokenBurnPalette,
) {
    let label =
        clamp_percent(remaining).map_or_else(|| "--".to_owned(), |p| format!("{:.0}", p.floor()));
    let Some(row) = lines.get_mut(lines.len() / 2) else {
        return;
    };
    let chars: Vec<char> = label.chars().collect();
    let start = (row.len().saturating_sub(chars.len())) / 2;
    let fits = row
        .get(start..start + chars.len())
        .is_some_and(|cells| cells.iter().all(|cell| cell.content == " "));
    if !fits {
        return;
    }
    let style = Style::default()
        .fg(palette.allowance_color(remaining))
        .add_modifier(Modifier::BOLD);
    for (offset, ch) in chars.into_iter().enumerate() {
        row[start + offset] = Span::styled(ch.to_string(), style);
    }
}

pub fn ring_glyph(remaining_percent: Option<f64>) -> &'static str {
    match clamp_percent(remaining_percent) {
        None => "○",
        Some(p) if p < 12.5 => "○",
        Some(p) if p < 37.5 => "◔",
        Some(p) if p < 62.5 => "◑",
        Some(p) if p < 87.5 => "◕",
        Some(_) => "●",
    }
}

pub fn compact_ring(remaining_percent: Option<f64>, palette: &TokenBurnPalette) -> Span<'static> {
    Span::styled(
        ring_glyph(remaining_percent),
        Style::default()
            .fg(palette.allowance_color(remaining_percent))
            .add_modifier(Modifier::BOLD),
    )
}

impl AccountCard {
    pub fn ring_windows(&self) -> Vec<Option<f64>> {
        match (
            clamp_percent(self.primary_remaining),
            clamp_percent(self.secondary_remaining),
        ) {
            (Some(primary), Some(secondary)) => vec![Some(primary), Some(secondary)],
            (Some(only), None) | (None, Some(only)) => vec![Some(only)],
            (None, None) => vec![None],
        }
    }

    pub fn headline_remaining(&self) -> Option<f64> {
        clamp_percent(self.primary_remaining).or(clamp_percent(self.secondary_remaining))
    }

    pub fn ring_tile_lines(&self, width: u16, palette: &TokenBurnPalette) -> Vec<Line<'static>> {
        let rings = self.ring_windows();
        let ring_lines = braille_ring_lines(&rings, RING_CELL_WIDTH, RING_CELL_HEIGHT, palette);
        let text_width =
            usize::from(width).saturating_sub(usize::from(RING_CELL_WIDTH) + RING_TEXT_GAP);
        let text_lines = self.ring_text_lines(rings.len() > 1, palette);
        ring_lines
            .into_iter()
            .zip(text_lines)
            .map(|(ring, text)| {
                let mut spans = ring.spans;
                spans.push(Span::raw(" ".repeat(RING_TEXT_GAP)));
                spans.extend(fit_line(text, text_width).spans);
                Line::from(spans)
            })
            .collect()
    }

    fn ring_text_lines(&self, dual: bool, palette: &TokenBurnPalette) -> [Vec<Span<'static>>; 4] {
        let headline = self.headline_remaining();
        let title = vec![Span::styled(
            format!("{} · {}", self.provider, self.profile),
            Style::default()
                .fg(palette.foreground)
                .add_modifier(Modifier::BOLD),
        )];
        let allowance = vec![
            Span::styled(
                format!("{} left", percent_label(headline)),
                Style::default()
                    .fg(palette.allowance_color(headline))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            tier_pill(&self.tier, palette),
        ];
        let reset = vec![reset_badge(self.seconds_until_reset, palette)];
        if !dual {
            return [title, allowance, reset, Vec::new()];
        }
        let weekly = vec![Span::styled(
            format!("7d {}", percent_label(self.secondary_remaining)),
            Style::default().fg(palette.allowance_color(self.secondary_remaining)),
        )];
        [title, allowance, weekly, reset]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn cards() -> Vec<AccountCard> {
        vec![
            AccountCard {
                provider: "claude".into(),
                profile: "work".into(),
                tier: "max".into(),
                primary_remaining: Some(64.0),
                secondary_remaining: Some(12.0),
                seconds_until_reset: Some(3 * HOUR + 20 * MINUTE),
                route_model: Some("claude-opus-5-5".into()),
                route_metric: Some("coding 72".into()),
            },
            AccountCard {
                provider: "codex".into(),
                profile: "a-very-long-personal-profile-name".into(),
                tier: "plus".into(),
                primary_remaining: None,
                secondary_remaining: Some(20.0),
                seconds_until_reset: None,
                route_model: None,
                route_metric: None,
            },
        ]
    }

    fn render(width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_deck(
                    &cards(),
                    frame.area(),
                    frame.buffer_mut(),
                    &TokenBurnPalette::default(),
                )
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    fn find(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
        (0..buf.area.height).find_map(|y| {
            let cells: Vec<String> = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            let first = needle.chars().next()?.to_string();
            (0..cells.len()).find_map(|x| {
                let matches = needle
                    .chars()
                    .enumerate()
                    .all(|(i, ch)| cells.get(x + i) == Some(&ch.to_string()));
                (cells[x] == first && matches).then_some((x as u16, y))
            })
        })
    }

    #[test]
    fn allowance_color_switches_at_twenty_percent() {
        let p = TokenBurnPalette::default();
        assert_eq!(p.allowance_color(Some(100.0)), p.healthy);
        assert_eq!(p.allowance_color(Some(20.0)), p.healthy);
        assert_eq!(p.allowance_color(Some(19.99)), p.depleted);
        assert_eq!(p.allowance_color(Some(0.0)), p.depleted);
        assert_eq!(p.allowance_color(None), p.unknown);
        assert_eq!(p.allowance_color(Some(f64::NAN)), p.unknown);
        assert_eq!(percent_label(Some(150.0)), "100%");
        assert_eq!(percent_label(Some(-4.0)), "0%");
        assert_eq!(percent_label(Some(19.99)), "19%");
        assert_eq!(percent_label(None), "--");
    }

    #[test]
    fn reset_countdown_buckets_and_labels() {
        let p = TokenBurnPalette::default();
        for (seconds, bucket, color, label) in [
            (None, ResetBucket::Missing, p.muted, "-"),
            (Some(-90), ResetBucket::Due, p.muted, "due"),
            (Some(0), ResetBucket::Due, p.muted, "due"),
            (Some(30), ResetBucket::WithinHour, p.reset_hour, "1m"),
            (Some(HOUR), ResetBucket::WithinHour, p.reset_hour, "1h 00m"),
            (
                Some(HOUR + 1),
                ResetBucket::WithinSixHours,
                p.reset_six_hours,
                "1h 00m",
            ),
            (
                Some(6 * HOUR),
                ResetBucket::WithinSixHours,
                p.reset_six_hours,
                "6h 00m",
            ),
            (
                Some(6 * HOUR + 1),
                ResetBucket::WithinDay,
                p.reset_day,
                "6h 00m",
            ),
            (Some(DAY), ResetBucket::WithinDay, p.reset_day, "1d 0h"),
            (
                Some(DAY + 5 * HOUR),
                ResetBucket::Later,
                p.reset_later,
                "1d 5h",
            ),
        ] {
            assert_eq!(ResetBucket::of(seconds), bucket, "{seconds:?}");
            assert_eq!(p.reset_color(bucket), color, "{seconds:?}");
            assert_eq!(countdown_label(seconds), label, "{seconds:?}");
        }
        assert_eq!(reset_badge(Some(-1), &p).content, "↻ due");
    }

    #[test]
    fn tier_pills_are_uppercase_and_colored_by_tier() {
        let p = TokenBurnPalette::default();
        for (tier, label, color) in [
            ("max", " MAX ", p.tier_max),
            ("Pro", " PRO ", p.tier_pro),
            (" plus ", " PLUS ", p.tier_plus),
            ("team", " TEAM ", p.accent),
            ("", " - ", p.unknown),
        ] {
            let pill = tier_pill(tier, &p);
            assert_eq!(pill.content, label);
            assert_eq!(pill.style.bg, Some(color));
            assert_eq!(pill.style.fg, Some(p.pill_text));
        }
    }

    #[test]
    fn gauge_fills_in_proportion_and_keeps_its_width() {
        let p = TokenBurnPalette::default();
        for remaining in [
            None,
            Some(0.0),
            Some(19.0),
            Some(64.0),
            Some(100.0),
            Some(400.0),
        ] {
            let line = Line::from(allowance_gauge("5h", remaining, 10, &p));
            assert_eq!(line.width(), GAUGE_LABEL_WIDTH + 10 + GAUGE_PERCENT_WIDTH);
        }
        let spans = allowance_gauge("5h", Some(64.0), 10, &p);
        assert_eq!(spans[1].content, "██████");
        assert_eq!(spans[1].style.fg, Some(p.healthy));
        assert_eq!(spans[2].content, "░░░░");
    }

    #[test]
    fn palette_reads_the_shipped_theme_without_breaking_the_app_theme() {
        let shipped = include_str!("../theme.toml");
        let palette = TokenBurnPalette::from_theme_toml(shipped).unwrap();
        assert_eq!(palette.deck, rgb(0x0b0d10));
        assert_eq!(palette.pill_text, palette.deck);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("theme.toml");
        assert!(crate::Theme::load(&path).is_ok());
        assert!(TokenBurnPalette::from_theme_toml("accent = \"xyz\"").is_err());
        assert_eq!(
            TokenBurnPalette::from_theme_toml("").unwrap(),
            TokenBurnPalette::default()
        );
    }

    #[test]
    fn clipping_respects_display_width() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
        assert_eq!(Span::raw(clip("工作工作工作", 5)).width(), 5);
        assert_eq!(
            fit_line(vec![Span::raw("abc"), Span::raw("defg")], 5).width(),
            5
        );
    }

    #[test]
    fn compact_strip_fits_80_by_24() {
        let buf = render(80, 24);
        assert_eq!(DeckMode::for_area(buf.area), DeckMode::Compact);
        let p = TokenBurnPalette::default();
        for card in cards() {
            assert!(card.compact_line(80, &p).width() <= 80);
        }
        let first = row_text(&buf, 0);
        assert!(first.starts_with("claude · work"), "{first}");
        assert!(first.contains(" MAX "), "{first}");
        assert!(first.contains("64%") && first.contains("12%"), "{first}");
        assert!(first.contains("↻ 3h 20m"), "{first}");
        let second = row_text(&buf, 1);
        assert!(second.contains("codex · a-very-lo…"), "{second}");
        assert!(second.contains("--") && second.contains("↻ -"), "{second}");
        let (x, y) = find(&buf, "12%").unwrap();
        assert_eq!(buf[(x, y)].fg, p.depleted);
        let (x, y) = find(&buf, "64%").unwrap();
        assert_eq!(buf[(x, y)].fg, p.healthy);
        assert_eq!(buf[(0, 23)].bg, p.deck);
    }

    #[test]
    fn full_cards_fit_120_by_40() {
        let buf = render(120, 40);
        assert_eq!(DeckMode::for_area(buf.area), DeckMode::Full);
        let p = TokenBurnPalette::default();
        assert_eq!(buf[(0, 0)].symbol(), "╭");
        assert_eq!(buf[(119, 0)].symbol(), "╮");
        assert_eq!(buf[(119, 5)].symbol(), "╯");
        assert_eq!(buf[(0, FULL_CARD_HEIGHT)].symbol(), "╭");
        assert!(row_text(&buf, 0).contains(" claude · work "));
        assert!(row_text(&buf, 1).contains(" MAX   ↻ 3h 20m"));
        assert!(row_text(&buf, 2)
            .trim_end_matches(['│', ' '])
            .ends_with("64%"));
        assert!(row_text(&buf, 4).contains("▸ claude-opus-5-5 · coding 72"));
        assert!(row_text(&buf, 10).contains("▸ unrouted"));
        let (x, y) = find(&buf, "↻ 3h").unwrap();
        assert_eq!(buf[(x, y)].fg, p.reset_six_hours);
        let (x, y) = find(&buf, " PLUS ").unwrap();
        assert_eq!(buf[(x, y)].bg, p.tier_plus);
        for card in cards() {
            for line in card.full_lines(118, &p) {
                assert!(line.width() <= 118);
            }
        }
    }

    fn dots(rings: &[Option<f64>]) -> Vec<RingDot> {
        ring_dot_grid(RING_CELL_WIDTH, RING_CELL_HEIGHT, rings)
            .into_iter()
            .flatten()
            .collect()
    }

    fn ring_text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn braille_ring_sweeps_clockwise_from_twelve_oclock() {
        assert_eq!(clockwise_fraction_from_top(0.0, -1.0), 0.0);
        assert!((clockwise_fraction_from_top(1.0, 0.0) - 0.25).abs() < 1e-9);
        assert!((clockwise_fraction_from_top(0.0, 1.0) - 0.5).abs() < 1e-9);
        assert!((clockwise_fraction_from_top(-1.0, 0.0) - 0.75).abs() < 1e-9);
        let full = dots(&[Some(100.0)]);
        assert!(full.contains(&RingDot::Fill(0)));
        assert!(!full.contains(&RingDot::Track(0)));
        let empty = dots(&[Some(0.0)]);
        assert!(empty.contains(&RingDot::Track(0)));
        assert!(!empty.contains(&RingDot::Fill(0)));
        let grid = ring_dot_grid(RING_CELL_WIDTH, RING_CELL_HEIGHT, &[Some(50.0)]);
        let half = usize::from(RING_CELL_WIDTH);
        for row in &grid {
            for (column, dot) in row.iter().enumerate() {
                if *dot == RingDot::Fill(0) {
                    assert!(column >= half, "fill must stay on the right half");
                }
            }
        }
        let ring_cells = full.iter().filter(|dot| **dot != RingDot::Empty).count();
        assert_eq!(
            ring_cells,
            empty.iter().filter(|dot| **dot != RingDot::Empty).count()
        );
    }

    #[test]
    fn single_ring_is_a_closed_braille_circle_with_a_center_label() {
        let p = TokenBurnPalette::default();
        let lines = braille_ring_lines(&[Some(64.0)], RING_CELL_WIDTH, RING_CELL_HEIGHT, &p);
        assert_eq!(lines.len(), usize::from(RING_CELL_HEIGHT));
        let text = ring_text(&lines);
        assert!(text[2].contains("64"), "{text:?}");
        for line in &lines {
            assert_eq!(line.width(), usize::from(RING_CELL_WIDTH));
        }
        let symbols: Vec<char> = text.iter().flat_map(|row| row.chars()).collect();
        assert!(symbols
            .iter()
            .all(|ch| *ch == ' ' || ch.is_ascii_digit() || ('\u{2800}'..='\u{28FF}').contains(ch)));
        let first = &lines[0].spans;
        assert!(first.iter().any(|span| span.style.fg == Some(p.healthy)));
        assert!(lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.style.fg == Some(p.border)));
        let low = braille_ring_lines(&[Some(12.0)], RING_CELL_WIDTH, RING_CELL_HEIGHT, &p);
        assert!(low
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.style.fg == Some(p.depleted)));
    }

    #[test]
    fn unknown_ring_is_muted_and_labelled_dashes_never_zero_or_full() {
        let p = TokenBurnPalette::default();
        let lines = braille_ring_lines(&[None], RING_CELL_WIDTH, RING_CELL_HEIGHT, &p);
        let text = ring_text(&lines).join("\n");
        assert!(text.contains("--"), "{text}");
        assert!(!text.contains('0'));
        let colors: Vec<_> = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.content != " ")
            .map(|span| span.style.fg)
            .collect();
        assert!(colors.iter().all(|fg| *fg == Some(p.unknown)), "{colors:?}");
        assert!(!dots(&[None]).contains(&RingDot::Fill(0)));
        assert_eq!(
            braille_ring_lines(&[], RING_CELL_WIDTH, RING_CELL_HEIGHT, &p),
            lines
        );
    }

    #[test]
    fn dual_windows_render_concentric_rings_and_single_window_one_ring() {
        let both = dots(&[Some(64.0), Some(30.0)]);
        for index in [0, 1] {
            assert!(both
                .iter()
                .any(|dot| matches!(dot, RingDot::Fill(i) | RingDot::Track(i) if *i == index)));
        }
        let card = |primary, secondary| AccountCard {
            primary_remaining: primary,
            secondary_remaining: secondary,
            ..AccountCard::default()
        };
        assert_eq!(
            card(Some(64.0), Some(30.0)).ring_windows(),
            [Some(64.0), Some(30.0)]
        );
        assert_eq!(card(None, Some(30.0)).ring_windows(), [Some(30.0)]);
        assert_eq!(card(Some(150.0), None).ring_windows(), [Some(100.0)]);
        assert_eq!(card(None, None).ring_windows(), [None]);
        assert_eq!(card(None, Some(30.0)).headline_remaining(), Some(30.0));
        assert_eq!(card(Some(f64::NAN), None).headline_remaining(), None);
        let p = TokenBurnPalette::default();
        let lines = braille_ring_lines(
            &[Some(64.0), Some(30.0)],
            RING_CELL_WIDTH,
            RING_CELL_HEIGHT,
            &p,
        );
        let colors: Vec<_> = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter_map(|span| span.style.fg)
            .collect();
        assert!(colors.contains(&p.healthy));
    }

    #[test]
    fn compact_ring_glyphs_follow_remaining_allowance() {
        let p = TokenBurnPalette::default();
        for (remaining, glyph, color) in [
            (None, "○", p.unknown),
            (Some(5.0), "○", p.depleted),
            (Some(19.0), "◔", p.depleted),
            (Some(50.0), "◑", p.healthy),
            (Some(70.0), "◕", p.healthy),
            (Some(100.0), "●", p.healthy),
        ] {
            let span = compact_ring(remaining, &p);
            assert_eq!(span.content, glyph, "{remaining:?}");
            assert_eq!(span.style.fg, Some(color), "{remaining:?}");
        }
    }

    #[test]
    fn ring_tiles_show_name_percent_tier_and_reset_within_width() {
        let p = TokenBurnPalette::default();
        for card in cards() {
            for width in [20u16, 24, 29] {
                let lines = card.ring_tile_lines(width, &p);
                assert_eq!(lines.len(), usize::from(RING_TILE_HEIGHT));
                for line in &lines {
                    assert!(line.width() <= usize::from(width), "{width}");
                }
            }
        }
        let text = ring_text(&cards()[0].ring_tile_lines(29, &p)).join("\n");
        assert!(text.contains("claude · work"), "{text}");
        assert!(
            text.contains("64% left") && text.contains(" MAX "),
            "{text}"
        );
        assert!(
            text.contains("7d 12%") && text.contains("↻ 3h 20m"),
            "{text}"
        );
        let unknown = ring_text(&cards()[1].ring_tile_lines(29, &p)).join("\n");
        assert!(
            unknown.contains("20% left") && unknown.contains("↻ -"),
            "{unknown}"
        );
        let missing = AccountCard {
            provider: "gemini".into(),
            profile: "x".into(),
            ..AccountCard::default()
        };
        let none = ring_text(&missing.ring_tile_lines(29, &p)).join("\n");
        assert!(none.contains("-- left") && !none.contains("0%"), "{none}");
    }

    #[test]
    fn reset_proximity_uses_token_burn_colors() {
        let p = TokenBurnPalette::default();
        assert_eq!(p.reset_hour, rgb(0x3ddc97));
        assert_eq!(p.reset_six_hours, rgb(0xffbc26));
        assert_eq!(p.reset_day, rgb(0x44adff));
        assert_eq!(p.reset_later, rgb(0xb680ff));
        assert_eq!(p.reset_color(ResetBucket::Due), p.muted);
        assert_eq!(p.reset_color(ResetBucket::Missing), p.muted);
        assert_eq!(p.healthy, rgb(0x3ddc97));
        assert_eq!(p.depleted, rgb(0xff4d5e));
        assert_eq!(p.unknown, rgb(0x5d6570));
    }
}
