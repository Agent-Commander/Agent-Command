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
    pub reset_due: Color,
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
            reset_due: rgb(0x7c8591),
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
            ResetBucket::Missing => self.muted,
            ResetBucket::Due => self.reset_due,
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

pub const RING_COLS: u16 = 8;
pub const RING_ROWS: u16 = 4;
pub const RING_TILE_MIN_WIDTH: u16 = RING_COLS + 12;
const BRAILLE_BASE: u32 = 0x2800;
const BRAILLE_BITS: [[u32; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
const SINGLE_BAND: f64 = 2.0;
const DUAL_BAND: f64 = 1.6;
const DUAL_GAP: f64 = 1.0;
const RIM_MARGIN: f64 = 0.2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RingLayout {
    Single(Option<f64>),
    Dual { outer: f64, inner: f64 },
}

impl RingLayout {
    pub fn for_windows(primary: Option<f64>, secondary: Option<f64>) -> Self {
        match (clamp_percent(primary), clamp_percent(secondary)) {
            (Some(outer), Some(inner)) => Self::Dual { outer, inner },
            (Some(only), None) | (None, Some(only)) => Self::Single(Some(only)),
            (None, None) => Self::Single(None),
        }
    }

    pub fn headline(self) -> Option<f64> {
        match self {
            Self::Single(value) => value,
            Self::Dual { outer, .. } => Some(outer),
        }
    }

    fn bands(self, radius: f64) -> Vec<RingBand> {
        match self {
            Self::Single(value) => vec![RingBand {
                outer_radius: radius,
                inner_radius: radius - SINGLE_BAND,
                remaining: value,
                kind: DotKind::OuterFill,
            }],
            Self::Dual { outer, inner } => {
                let inner_outer_radius = radius - DUAL_BAND - DUAL_GAP;
                vec![
                    RingBand {
                        outer_radius: radius,
                        inner_radius: radius - DUAL_BAND,
                        remaining: Some(outer),
                        kind: DotKind::OuterFill,
                    },
                    RingBand {
                        outer_radius: inner_outer_radius,
                        inner_radius: inner_outer_radius - DUAL_BAND,
                        remaining: Some(inner),
                        kind: DotKind::InnerFill,
                    },
                ]
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DotKind {
    OuterFill,
    InnerFill,
    Track,
}

#[derive(Clone, Copy, Debug)]
struct RingBand {
    outer_radius: f64,
    inner_radius: f64,
    remaining: Option<f64>,
    kind: DotKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingCell {
    pub symbol: char,
    pub kind: Option<DotKind>,
}

pub fn clockwise_fraction(dx: f64, dy: f64) -> f64 {
    let angle = dx.atan2(-dy);
    let turn = std::f64::consts::TAU;
    ((angle + turn) % turn) / turn
}

fn classify_dot(bands: &[RingBand], dx: f64, dy: f64) -> Option<DotKind> {
    let distance = dx.hypot(dy);
    let band = bands
        .iter()
        .find(|band| distance <= band.outer_radius && distance >= band.inner_radius)?;
    let swept = band
        .remaining
        .is_some_and(|percent| clockwise_fraction(dx, dy) < percent / 100.0);
    Some(if swept { band.kind } else { DotKind::Track })
}

fn dominant_kind(counts: &[(DotKind, usize)]) -> Option<DotKind> {
    counts
        .iter()
        .filter(|(_, count)| *count > 0)
        .max_by(|(a_kind, a_count), (b_kind, b_count)| {
            a_count.cmp(b_count).then_with(|| b_kind.cmp(a_kind))
        })
        .map(|(kind, _)| *kind)
}

pub fn ring_cells(layout: RingLayout, cols: u16, rows: u16) -> Vec<Vec<RingCell>> {
    let dots_wide = f64::from(cols) * 2.0;
    let dots_tall = f64::from(rows) * 4.0;
    let radius = dots_wide.min(dots_tall) / 2.0 - RIM_MARGIN;
    let bands = layout.bands(radius);
    (0..rows)
        .map(|row| {
            (0..cols)
                .map(|col| braille_cell(&bands, col, row, dots_wide, dots_tall))
                .collect()
        })
        .collect()
}

fn braille_cell(bands: &[RingBand], col: u16, row: u16, wide: f64, tall: f64) -> RingCell {
    let mut bits = 0u32;
    let mut counts = [
        (DotKind::OuterFill, 0usize),
        (DotKind::InnerFill, 0usize),
        (DotKind::Track, 0usize),
    ];
    for (sub_x, column_bits) in BRAILLE_BITS.iter().enumerate() {
        for (sub_y, bit) in column_bits.iter().enumerate() {
            let x = f64::from(col) * 2.0 + sub_x as f64 + 0.5;
            let y = f64::from(row) * 4.0 + sub_y as f64 + 0.5;
            if let Some(kind) = classify_dot(bands, x - wide / 2.0, y - tall / 2.0) {
                bits |= bit;
                if let Some(slot) = counts.iter_mut().find(|(k, _)| *k == kind) {
                    slot.1 += 1;
                }
            }
        }
    }
    RingCell {
        symbol: char::from_u32(BRAILLE_BASE + bits).unwrap_or(' '),
        kind: dominant_kind(&counts),
    }
}

impl TokenBurnPalette {
    pub fn ring_dot_color(&self, layout: RingLayout, kind: DotKind) -> Color {
        match (layout, kind) {
            (RingLayout::Single(None), _) => self.unknown,
            (_, DotKind::Track) => self.border,
            (RingLayout::Single(value), _) => self.allowance_color(value),
            (RingLayout::Dual { outer, .. }, DotKind::OuterFill) => {
                self.allowance_color(Some(outer))
            }
            (RingLayout::Dual { inner, .. }, DotKind::InnerFill) => {
                self.allowance_color(Some(inner))
            }
        }
    }
}

pub fn ring_lines(
    layout: RingLayout,
    cols: u16,
    rows: u16,
    palette: &TokenBurnPalette,
) -> Vec<Line<'static>> {
    ring_cells(layout, cols, rows)
        .into_iter()
        .map(|row| {
            Line::from(
                row.into_iter()
                    .map(|cell| match cell.kind {
                        Some(kind) => Span::styled(
                            cell.symbol.to_string(),
                            Style::default().fg(palette.ring_dot_color(layout, kind)),
                        ),
                        None => Span::raw(" "),
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

pub fn compact_ring_glyph(remaining_percent: Option<f64>) -> &'static str {
    match clamp_percent(remaining_percent) {
        None => "○",
        Some(p) if p < 12.5 => "○",
        Some(p) if p < 37.5 => "◔",
        Some(p) if p < 62.5 => "◑",
        Some(p) if p < 87.5 => "◕",
        Some(_) => "●",
    }
}

pub fn compact_ring_span(
    remaining_percent: Option<f64>,
    palette: &TokenBurnPalette,
) -> Span<'static> {
    Span::styled(
        format!(
            "{} {}",
            compact_ring_glyph(remaining_percent),
            percent_label(remaining_percent)
        ),
        Style::default()
            .fg(palette.allowance_color(remaining_percent))
            .add_modifier(Modifier::BOLD),
    )
}

impl AccountCard {
    pub fn ring_layout(&self) -> RingLayout {
        RingLayout::for_windows(self.primary_remaining, self.secondary_remaining)
    }

    pub fn ring_tile_lines(&self, width: u16, palette: &TokenBurnPalette) -> Vec<Line<'static>> {
        let layout = self.ring_layout();
        let text_width = usize::from(width.saturating_sub(RING_COLS + 1));
        let headline = layout.headline();
        let left_label = match headline {
            Some(_) => format!("{} left", percent_label(headline)),
            None => "--".to_owned(),
        };
        let text_rows: [Vec<Span<'static>>; 4] = [
            vec![Span::styled(
                format!("{} · {}", self.provider, self.profile),
                Style::default()
                    .fg(palette.foreground)
                    .add_modifier(Modifier::BOLD),
            )],
            vec![
                Span::styled(
                    left_label,
                    Style::default()
                        .fg(palette.allowance_color(headline))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                tier_pill(&self.tier, palette),
            ],
            vec![reset_badge(self.seconds_until_reset, palette)],
            self.window_caption(layout, palette),
        ];
        ring_lines(layout, RING_COLS, RING_ROWS, palette)
            .into_iter()
            .zip(text_rows)
            .map(|(ring, text)| {
                let mut spans = ring.spans;
                spans.push(Span::raw(" "));
                spans.extend(fit_line(text, text_width).spans);
                Line::from(spans)
            })
            .collect()
    }

    fn window_caption(&self, layout: RingLayout, palette: &TokenBurnPalette) -> Vec<Span<'static>> {
        let caption = |label: &str, value: Option<f64>| {
            Span::styled(
                format!("{label} {}", percent_label(value)),
                Style::default().fg(palette.allowance_color(value)),
            )
        };
        match layout {
            RingLayout::Dual { outer, inner } => vec![
                caption("5h", Some(outer)),
                Span::styled(" · ", Style::default().fg(palette.muted)),
                caption("7d", Some(inner)),
            ],
            RingLayout::Single(Some(_)) if self.primary_remaining.is_some() => {
                vec![caption("5h", self.primary_remaining)]
            }
            RingLayout::Single(Some(_)) => vec![caption("7d", self.secondary_remaining)],
            RingLayout::Single(None) => vec![Span::styled(
                "unreported",
                Style::default().fg(palette.unknown),
            )],
        }
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
            (Some(-90), ResetBucket::Due, p.reset_due, "due"),
            (Some(0), ResetBucket::Due, p.reset_due, "due"),
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

    fn dot_grid(layout: RingLayout) -> Vec<Vec<bool>> {
        let cells = ring_cells(layout, RING_COLS, RING_ROWS);
        let (wide, tall) = (usize::from(RING_COLS) * 2, usize::from(RING_ROWS) * 4);
        let mut grid = vec![vec![false; wide]; tall];
        for (row, line) in cells.iter().enumerate() {
            for (col, cell) in line.iter().enumerate() {
                let bits = cell.symbol as u32 - BRAILLE_BASE;
                for (sub_x, column) in BRAILLE_BITS.iter().enumerate() {
                    for (sub_y, bit) in column.iter().enumerate() {
                        grid[row * 4 + sub_y][col * 2 + sub_x] = bits & bit != 0;
                    }
                }
            }
        }
        grid
    }

    fn kinds(layout: RingLayout) -> Vec<DotKind> {
        ring_cells(layout, RING_COLS, RING_ROWS)
            .into_iter()
            .flatten()
            .filter_map(|cell| cell.kind)
            .collect()
    }

    #[test]
    fn ring_layout_uses_concentric_rings_only_when_both_windows_report() {
        assert_eq!(
            RingLayout::for_windows(Some(64.0), Some(12.0)),
            RingLayout::Dual {
                outer: 64.0,
                inner: 12.0
            }
        );
        assert_eq!(
            RingLayout::for_windows(None, Some(150.0)),
            RingLayout::Single(Some(100.0))
        );
        assert_eq!(
            RingLayout::for_windows(Some(f64::NAN), None),
            RingLayout::Single(None)
        );
        assert_eq!(RingLayout::Single(None).headline(), None);
        assert_eq!(
            RingLayout::Dual {
                outer: 30.0,
                inner: 90.0
            }
            .headline(),
            Some(30.0)
        );
    }

    #[test]
    fn sweep_starts_at_twelve_o_clock_and_runs_clockwise() {
        for (dx, dy, fraction) in [
            (0.0, -1.0, 0.0),
            (1.0, 0.0, 0.25),
            (0.0, 1.0, 0.5),
            (-1.0, 0.0, 0.75),
        ] {
            assert!((clockwise_fraction(dx, dy) - fraction).abs() < 1e-9);
        }
        let quarter = ring_cells(RingLayout::Single(Some(25.0)), RING_COLS, RING_ROWS);
        let right_of_top = usize::from(RING_COLS) / 2;
        assert_eq!(quarter[0][right_of_top].kind, Some(DotKind::OuterFill));
        assert_eq!(quarter[0][right_of_top - 1].kind, Some(DotKind::Track));
        let bottom = usize::from(RING_ROWS) - 1;
        assert!(quarter[bottom]
            .iter()
            .all(|cell| cell.kind != Some(DotKind::OuterFill)));
    }

    #[test]
    fn ring_silhouette_is_a_symmetric_hollow_circle_in_braille() {
        for layout in [
            RingLayout::Single(Some(40.0)),
            RingLayout::Single(None),
            RingLayout::Dual {
                outer: 70.0,
                inner: 10.0,
            },
        ] {
            let grid = dot_grid(layout);
            let (tall, wide) = (grid.len(), grid[0].len());
            for y in 0..tall {
                for x in 0..wide {
                    assert_eq!(grid[y][x], grid[y][wide - 1 - x], "{layout:?}");
                    assert_eq!(grid[y][x], grid[tall - 1 - y][x], "{layout:?}");
                }
            }
            assert!(!grid[0][0] && !grid[tall - 1][wide - 1]);
            assert!(!grid[tall / 2][wide / 2], "ring must be hollow");
            assert!(grid[0][wide / 2] && grid[tall / 2][0]);
            for cell in ring_cells(layout, RING_COLS, RING_ROWS)
                .into_iter()
                .flatten()
            {
                assert!(('\u{2800}'..='\u{28FF}').contains(&cell.symbol));
            }
        }
        let single = dot_grid(RingLayout::Single(Some(50.0)));
        let dual = dot_grid(RingLayout::Dual {
            outer: 50.0,
            inner: 50.0,
        });
        let count = |grid: &Vec<Vec<bool>>| grid.iter().flatten().filter(|d| **d).count();
        assert!(count(&dual) > count(&single));
    }

    #[test]
    fn fill_tracks_remaining_and_unknown_shows_only_muted_track() {
        assert!(kinds(RingLayout::Single(Some(100.0)))
            .iter()
            .all(|kind| *kind == DotKind::OuterFill));
        assert!(kinds(RingLayout::Single(Some(0.0)))
            .iter()
            .all(|kind| *kind == DotKind::Track));
        assert!(kinds(RingLayout::Single(None))
            .iter()
            .all(|kind| *kind == DotKind::Track));
        let half = kinds(RingLayout::Single(Some(50.0)));
        let filled = half.iter().filter(|k| **k == DotKind::OuterFill).count();
        assert!(filled * 3 > half.len() && filled * 3 < half.len() * 2);
        let dual = kinds(RingLayout::Dual {
            outer: 80.0,
            inner: 60.0,
        });
        assert!(dual.contains(&DotKind::OuterFill) && dual.contains(&DotKind::InnerFill));

        let p = TokenBurnPalette::default();
        let low = RingLayout::Single(Some(19.0));
        assert_eq!(p.ring_dot_color(low, DotKind::OuterFill), p.depleted);
        assert_eq!(p.ring_dot_color(low, DotKind::Track), p.border);
        let healthy = RingLayout::Single(Some(20.0));
        assert_eq!(p.ring_dot_color(healthy, DotKind::OuterFill), p.healthy);
        let unknown = RingLayout::Single(None);
        assert_eq!(p.ring_dot_color(unknown, DotKind::Track), p.unknown);
        let dual = RingLayout::Dual {
            outer: 64.0,
            inner: 12.0,
        };
        assert_eq!(p.ring_dot_color(dual, DotKind::OuterFill), p.healthy);
        assert_eq!(p.ring_dot_color(dual, DotKind::InnerFill), p.depleted);
        let line_fg: Vec<Option<Color>> = ring_lines(unknown, RING_COLS, RING_ROWS, &p)
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.style.fg))
            .filter(Option::is_some)
            .collect();
        assert!(line_fg.iter().all(|fg| *fg == Some(p.unknown)));
    }

    #[test]
    fn compact_glyphs_quantize_remaining_and_color_by_state() {
        for (remaining, glyph) in [
            (None, "○"),
            (Some(5.0), "○"),
            (Some(25.0), "◔"),
            (Some(50.0), "◑"),
            (Some(75.0), "◕"),
            (Some(95.0), "●"),
        ] {
            assert_eq!(compact_ring_glyph(remaining), glyph, "{remaining:?}");
        }
        let p = TokenBurnPalette::default();
        let unknown = compact_ring_span(None, &p);
        assert_eq!(unknown.content, "○ --");
        assert_eq!(unknown.style.fg, Some(p.unknown));
        assert_eq!(compact_ring_span(Some(12.0), &p).style.fg, Some(p.depleted));
        assert_eq!(compact_ring_span(Some(64.0), &p).content, "◕ 64%");
    }

    #[test]
    fn ring_tile_lines_fit_and_show_name_percent_tier_and_reset() {
        let p = TokenBurnPalette::default();
        let all = cards();
        let lines = all[0].ring_tile_lines(28, &p);
        assert_eq!(lines.len(), usize::from(RING_ROWS));
        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert!(lines.iter().all(|line| line.width() <= 28), "{text:?}");
        assert!(text[0].contains("claude · work"));
        assert!(text[1].contains("64% left") && text[1].contains(" MAX "));
        assert!(text[2].contains("↻ 3h 20m"));
        assert!(text[3].contains("5h 64% · 7d 12%"));
        let single = all[1].ring_tile_lines(28, &p);
        let text: Vec<String> = single.iter().map(ToString::to_string).collect();
        assert!(text[1].contains("20% left"), "{text:?}");
        assert!(text[3].contains("7d 20%"));
        let unknown = AccountCard {
            secondary_remaining: None,
            ..all[1].clone()
        };
        let text: Vec<String> = unknown
            .ring_tile_lines(28, &p)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(text[1].contains("--") && !text[1].contains("0%"));
        assert!(text[2].contains("↻ -") && text[3].contains("unreported"));
    }

    #[test]
    fn reset_palette_matches_token_burn_hues() {
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
