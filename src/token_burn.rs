use crate::tui::graphics::{GraphicsCaps, InlineProtocol};
use ratatui::{
    buffer::{Buffer, CellDiffOption},
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
const SUPERSAMPLE_GRID: usize = 4;
const SINGLE_RING_RADIUS: f64 = 0.83;
const SINGLE_RING_HALF_STROKE: f64 = 0.115;
const DUAL_OUTER_RADIUS: f64 = 0.84;
const DUAL_OUTER_HALF_STROKE: f64 = 0.11;
const DUAL_INNER_RADIUS: f64 = 0.50;
const DUAL_INNER_HALF_STROKE: f64 = 0.105;
const SIXEL_GLOW_WIDTH: f64 = 0.085;
const SIXEL_GLOW_STRENGTH: f64 = 0.24;
const SIXEL_BAND_HEIGHT: usize = 6;
const SIXEL_RLE_MIN_RUN: usize = 4;
const UNKNOWN_DASH_SEGMENTS: f64 = 8.0;
const UNKNOWN_DASH_DUTY: f64 = 0.5;
const UNKNOWN_TRACK_DIM: f64 = 0.6;
const PNG_MIN_SIDE_PX: usize = 160;
const LABEL_SCALES: [usize; 8] = [8, 7, 6, 5, 4, 3, 2, 1];
const KITTY_IMAGE_ID_BASE: u32 = 7100;
const KITTY_CHUNK_BYTES: usize = 4096;
const SIXEL_TRANSPARENT_REGISTER: usize = 0;

const OCTANT_EXCEPTIONS: [(u8, char); 26] = [
    (0x00, ' '),
    (0x01, '\u{1CEA8}'),
    (0x02, '\u{1CEAB}'),
    (0x03, '\u{1FB82}'),
    (0x05, '\u{2598}'),
    (0x0A, '\u{259D}'),
    (0x0F, '\u{2580}'),
    (0x14, '\u{1FBE6}'),
    (0x28, '\u{1FBE7}'),
    (0x3F, '\u{1FB85}'),
    (0x40, '\u{1CEA3}'),
    (0x50, '\u{2596}'),
    (0x55, '\u{258C}'),
    (0x5A, '\u{259E}'),
    (0x5F, '\u{259B}'),
    (0x80, '\u{1CEA0}'),
    (0xA0, '\u{2597}'),
    (0xA5, '\u{259A}'),
    (0xAA, '\u{2590}'),
    (0xAF, '\u{259C}'),
    (0xC0, '\u{2582}'),
    (0xF0, '\u{2584}'),
    (0xF5, '\u{2599}'),
    (0xFA, '\u{259F}'),
    (0xFC, '\u{2586}'),
    (0xFF, '\u{2588}'),
];

const BITMAP_FONT_5X7: [(char, [u8; 7]); 12] = [
    ('0', [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E]),
    ('1', [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E]),
    ('2', [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F]),
    ('3', [0x1E, 0x01, 0x01, 0x0E, 0x01, 0x01, 0x1E]),
    ('4', [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02]),
    ('5', [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E]),
    ('6', [0x0E, 0x10, 0x1E, 0x11, 0x11, 0x11, 0x0E]),
    ('7', [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08]),
    ('8', [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E]),
    ('9', [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x01, 0x0E]),
    ('%', [0x19, 0x19, 0x02, 0x04, 0x08, 0x13, 0x13]),
    ('-', [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00]),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

impl Rgb {
    const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    fn from_color(color: Color, fallback: Self) -> Self {
        match color {
            Color::Rgb(r, g, b) => Self::new(r, g, b),
            Color::Black => Self::new(0x00, 0x00, 0x00),
            Color::White => Self::new(0xee, 0xf1, 0xf5),
            Color::Red | Color::LightRed => Self::new(0xff, 0x4d, 0x5e),
            Color::Green | Color::LightGreen => Self::new(0x3d, 0xdc, 0x97),
            Color::Yellow | Color::LightYellow => Self::new(0xff, 0xbc, 0x26),
            Color::Blue | Color::LightBlue => Self::new(0x5a, 0xb0, 0xff),
            Color::Magenta | Color::LightMagenta => Self::new(0xc7, 0x92, 0xff),
            Color::Cyan | Color::LightCyan => Self::new(0x73, 0xda, 0xca),
            Color::Gray => Self::new(0x7c, 0x85, 0x91),
            Color::DarkGray => Self::new(0x2a, 0x30, 0x39),
            _ => fallback,
        }
    }

    const fn to_color(self) -> Color {
        Color::Rgb(self.r, self.g, self.b)
    }

    fn lerp(self, other: Self, t: f64) -> Self {
        let clamped = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| -> u8 {
            ((f64::from(a) + (f64::from(b) - f64::from(a)) * clamped).round()).clamp(0.0, 255.0)
                as u8
        };
        Self::new(
            mix(self.r, other.r),
            mix(self.g, other.g),
            mix(self.b, other.b),
        )
    }

    fn brighten(self, amount: f64) -> Self {
        self.lerp(Self::new(255, 255, 255), amount)
    }

    fn dist_sq(self, other: Self) -> u32 {
        let dr = i32::from(self.r) - i32::from(other.r);
        let dg = i32::from(self.g) - i32::from(other.g);
        let db = i32::from(self.b) - i32::from(other.b);
        (dr * dr + dg * dg + db * db) as u32
    }

    fn sixel_percent(self) -> (u8, u8, u8) {
        let scale = |channel: u8| ((u16::from(channel) * 100 + 127) / 255) as u8;
        (scale(self.r), scale(self.g), scale(self.b))
    }
}

#[derive(Clone, Copy)]
struct RingBand {
    radius: f64,
    half_stroke: f64,
    remaining: Option<f64>,
    fill_rgb: Rgb,
    track_rgb: Rgb,
    dashed: bool,
}

fn unknown_track_rgb(bg: Rgb, palette: &TokenBurnPalette) -> Rgb {
    bg.lerp(
        Rgb::from_color(palette.unknown, Rgb::new(0x5d, 0x65, 0x70)),
        UNKNOWN_TRACK_DIM,
    )
}

fn on_unknown_dash(x: f64, y: f64) -> bool {
    (clockwise_fraction_from_top(x, y) * UNKNOWN_DASH_SEGMENTS).fract() < UNKNOWN_DASH_DUTY
}

fn ring_bands(rings: &[Option<f64>], bg: Rgb, palette: &TokenBurnPalette) -> Vec<RingBand> {
    let normalized = if rings.is_empty() { &[None][..] } else { rings };
    let border_rgb = Rgb::from_color(palette.border, Rgb::new(0x2a, 0x30, 0x39));
    let unknown_rgb = Rgb::from_color(palette.unknown, Rgb::new(0x5d, 0x65, 0x70));
    let dim_unknown = unknown_track_rgb(bg, palette);
    let recessed_track = bg.lerp(border_rgb, 0.78);
    let specs: &[(f64, f64)] = if normalized.len() == 1 {
        &[(SINGLE_RING_RADIUS, SINGLE_RING_HALF_STROKE)]
    } else {
        &[
            (DUAL_OUTER_RADIUS, DUAL_OUTER_HALF_STROKE),
            (DUAL_INNER_RADIUS, DUAL_INNER_HALF_STROKE),
        ]
    };
    normalized
        .iter()
        .zip(specs.iter())
        .map(|(&raw, &(radius, half_stroke))| {
            let remaining = clamp_percent(raw);
            let fill_rgb = Rgb::from_color(palette.allowance_color(remaining), unknown_rgb);
            let track_rgb = if remaining.is_some() {
                recessed_track
            } else {
                dim_unknown
            };
            RingBand {
                radius,
                half_stroke,
                remaining,
                fill_rgb,
                track_rgb,
                dashed: remaining.is_none(),
            }
        })
        .collect()
}

fn smoothstep(edge0: f64, edge1: f64, x: f64) -> f64 {
    let span = edge1 - edge0;
    if span.abs() < f64::EPSILON {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / span).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn arc_signed_distance(x: f64, y: f64, radius: f64, half_stroke: f64, fraction: f64) -> f64 {
    let dist = x.hypot(y);
    let ring_dist = (dist - radius).abs() - half_stroke;
    if fraction >= 0.9999 {
        return ring_dist;
    }
    let sweep = clockwise_fraction_from_top(x, y);
    if sweep <= fraction {
        return ring_dist;
    }
    let start_cap = x.hypot(y + radius) - half_stroke;
    let end_angle = fraction * std::f64::consts::TAU;
    let end_x = radius * end_angle.sin();
    let end_y = -radius * end_angle.cos();
    let end_cap = (x - end_x).hypot(y - end_y) - half_stroke;
    start_cap.min(end_cap)
}

fn sample_ring_sdf(x: f64, y: f64, bands: &[RingBand], bg: Rgb, glow: bool) -> (Rgb, f64) {
    let dist = x.hypot(y);
    let mut color = bg;
    let mut coverage = 0.0_f64;

    for band in bands {
        let d_track = (dist - band.radius).abs() - band.half_stroke;
        if d_track <= 0.0 && (!band.dashed || on_unknown_dash(x, y)) {
            let radial_falloff = ((dist - band.radius).abs() / band.half_stroke).clamp(0.0, 1.0);
            let shaded_track = if band.remaining.is_some() {
                band.track_rgb.lerp(bg, radial_falloff * 0.25)
            } else {
                band.track_rgb
            };
            color = shaded_track;
            coverage = 1.0;
        }
        if let Some(percent) = band.remaining {
            if percent > 0.0 {
                let frac = percent / 100.0;
                let d_fill = arc_signed_distance(x, y, band.radius, band.half_stroke, frac);
                if d_fill <= 0.0 {
                    let sweep = clockwise_fraction_from_top(x, y);
                    let progress = (sweep / frac.max(0.01)).clamp(0.0, 1.0);
                    let radial = ((dist - band.radius).abs() / band.half_stroke).clamp(0.0, 1.0);
                    let base = bg.lerp(band.fill_rgb, 0.84 + 0.16 * progress);
                    let sheen = (1.0 - radial * radial) * (0.10 + 0.18 * progress);
                    color = base.brighten(sheen);
                    coverage = 1.0;
                } else if glow && d_fill < SIXEL_GLOW_WIDTH {
                    let halo =
                        (1.0 - smoothstep(0.0, SIXEL_GLOW_WIDTH, d_fill)) * SIXEL_GLOW_STRENGTH;
                    color = color.lerp(band.fill_rgb, halo);
                    coverage = coverage.max(halo);
                }
            }
        }
    }

    (color, coverage)
}

fn supersample_region(
    u0: f64,
    v0: f64,
    u1: f64,
    v1: f64,
    bands: &[RingBand],
    bg: Rgb,
    glow: bool,
) -> (Rgb, f64) {
    let mut sum_r = 0.0_f64;
    let mut sum_g = 0.0_f64;
    let mut sum_b = 0.0_f64;
    let mut sum_cov = 0.0_f64;
    let n = SUPERSAMPLE_GRID as f64;
    let total = n * n;
    for sy in 0..SUPERSAMPLE_GRID {
        let v = v0 + ((sy as f64 + 0.5) / n) * (v1 - v0);
        for sx in 0..SUPERSAMPLE_GRID {
            let u = u0 + ((sx as f64 + 0.5) / n) * (u1 - u0);
            let (rgb, cov) = sample_ring_sdf(u, v, bands, bg, glow);
            sum_r += f64::from(rgb.r);
            sum_g += f64::from(rgb.g);
            sum_b += f64::from(rgb.b);
            sum_cov += cov;
        }
    }
    let avg = Rgb::new(
        (sum_r / total).round().clamp(0.0, 255.0) as u8,
        (sum_g / total).round().clamp(0.0, 255.0) as u8,
        (sum_b / total).round().clamp(0.0, 255.0) as u8,
    );
    (avg, sum_cov / total)
}

fn octant_char(mask: u8) -> char {
    if let Ok(index) = OCTANT_EXCEPTIONS.binary_search_by_key(&mask, |&(bits, _)| bits) {
        return OCTANT_EXCEPTIONS[index].1;
    }
    let skipped = OCTANT_EXCEPTIONS
        .iter()
        .take_while(|&&(bits, _)| bits < mask)
        .count() as u32;
    char::from_u32(0x1CD00 + u32::from(mask) - skipped).unwrap_or(' ')
}

fn glyph_5x7(ch: char) -> Option<[u8; 7]> {
    BITMAP_FONT_5X7
        .iter()
        .find_map(|&(glyph, rows)| (glyph == ch).then_some(rows))
}

fn all_rings_unknown(rings: &[Option<f64>]) -> bool {
    rings.is_empty() || rings.iter().all(|ring| clamp_percent(*ring).is_none())
}

fn headline_from_slice(rings: &[Option<f64>]) -> Option<f64> {
    rings
        .iter()
        .filter_map(|&ring| clamp_percent(ring))
        .reduce(f64::min)
}

fn ring_center_label(rings: &[Option<f64>]) -> String {
    let headline = headline_from_slice(rings);
    match (headline, rings.len() > 1) {
        (None, _) => "--".to_owned(),
        (Some(value), false) => format!("{:.0}%", value.floor()),
        (Some(value), true) => format!("{:.0}", value.floor()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rgba {
    rgb: Rgb,
    alpha: u8,
}

impl Rgba {
    const TRANSPARENT: Self = Self {
        rgb: Rgb::new(0, 0, 0),
        alpha: 0,
    };

    fn over(self, bg: Rgb) -> Rgb {
        bg.lerp(self.rgb, f64::from(self.alpha) / 255.0)
    }
}

struct RingRaster {
    width: usize,
    height: usize,
    pixels: Vec<Rgba>,
}

impl RingRaster {
    fn render(
        rings: &[Option<f64>],
        width: usize,
        height: usize,
        bg: Rgb,
        palette: &TokenBurnPalette,
    ) -> Self {
        let rings = if rings.is_empty() { &[None][..] } else { rings };
        let bands = ring_bands(rings, bg, palette);
        let glow = !all_rings_unknown(rings);
        let side = width.min(height).max(1) as f64;
        let half_w = width as f64 * 0.5;
        let half_h = height as f64 * 0.5;
        let inv_half = 2.0 / side;
        let mut pixels = Vec::with_capacity(width * height);
        for py in 0..height {
            let v0 = (py as f64 - half_h) * inv_half;
            let v1 = ((py + 1) as f64 - half_h) * inv_half;
            for px in 0..width {
                let u0 = (px as f64 - half_w) * inv_half;
                let u1 = ((px + 1) as f64 - half_w) * inv_half;
                pixels.push(supersample_rgba(u0, v0, u1, v1, &bands, bg, glow));
            }
        }
        let mut raster = Self {
            width,
            height,
            pixels,
        };
        raster.stamp_center_label(rings, bg, palette);
        raster
    }

    fn rgba_bytes(&self) -> Vec<u8> {
        self.pixels
            .iter()
            .flat_map(|pixel| [pixel.rgb.r, pixel.rgb.g, pixel.rgb.b, pixel.alpha])
            .collect()
    }

    fn stamp_center_label(&mut self, rings: &[Option<f64>], bg: Rgb, palette: &TokenBurnPalette) {
        let glyphs: Vec<[u8; 7]> = ring_center_label(rings)
            .chars()
            .filter_map(glyph_5x7)
            .collect();
        if glyphs.is_empty() {
            return;
        }
        let hole_norm = if rings.len() > 1 {
            DUAL_INNER_RADIUS - DUAL_INNER_HALF_STROKE
        } else {
            SINGLE_RING_RADIUS - SINGLE_RING_HALF_STROKE
        };
        let hole_radius_px = (self.width.min(self.height) as f64 * 0.5) * hole_norm;
        let base_w = glyphs.len() * 5 + glyphs.len().saturating_sub(1);
        let base_h = 7usize;
        let Some(scale) = LABEL_SCALES.into_iter().find(|&candidate| {
            let half_w = (base_w * candidate) as f64 * 0.5;
            let half_h = (base_h * candidate) as f64 * 0.5;
            half_w.hypot(half_h) + 2.0 < hole_radius_px
        }) else {
            return;
        };
        let mask = label_mask(&glyphs, scale);
        let total_w = base_w * scale;
        let total_h = base_h * scale;
        let origin_x = (self.width.saturating_sub(total_w)) / 2;
        let origin_y = (self.height.saturating_sub(total_h)) / 2;
        let label = Rgba {
            rgb: center_label_rgb(rings, palette),
            alpha: 255,
        };
        for (my, mx) in (0..total_h).flat_map(|my| (0..total_w).map(move |mx| (my, mx))) {
            if !mask[my * total_w + mx] {
                continue;
            }
            for (oy, ox) in (-1isize..=1).flat_map(|oy| (-1isize..=1).map(move |ox| (oy, ox))) {
                let px = origin_x as isize + mx as isize + ox;
                let py = origin_y as isize + my as isize + oy;
                if px >= 0 && py >= 0 && (px as usize) < self.width && (py as usize) < self.height {
                    let pixel = &mut self.pixels[py as usize * self.width + px as usize];
                    pixel.rgb = pixel.rgb.lerp(bg, 0.72);
                }
            }
        }
        for (my, mx) in (0..total_h).flat_map(|my| (0..total_w).map(move |mx| (my, mx))) {
            let (px, py) = (origin_x + mx, origin_y + my);
            if mask[my * total_w + mx] && px < self.width && py < self.height {
                self.pixels[py * self.width + px] = label;
            }
        }
    }
}

fn label_mask(glyphs: &[[u8; 7]], scale: usize) -> Vec<bool> {
    let total_w = (glyphs.len() * 5 + glyphs.len().saturating_sub(1)) * scale;
    let total_h = 7 * scale;
    let mut mask = vec![false; total_w * total_h];
    for (glyph_index, rows) in glyphs.iter().enumerate() {
        let gx = glyph_index * 6 * scale;
        for (row_idx, &bits) in rows.iter().enumerate() {
            for col_idx in (0..5usize).filter(|col| bits & (1 << (4 - col)) != 0) {
                for (dy, dx) in (0..scale).flat_map(|dy| (0..scale).map(move |dx| (dy, dx))) {
                    let mx = gx + col_idx * scale + dx;
                    let my = row_idx * scale + dy;
                    if mx < total_w && my < total_h {
                        mask[my * total_w + mx] = true;
                    }
                }
            }
        }
    }
    mask
}

fn center_label_rgb(rings: &[Option<f64>], palette: &TokenBurnPalette) -> Rgb {
    match headline_from_slice(rings) {
        Some(headline) => Rgb::from_color(
            palette.allowance_color(Some(headline)),
            Rgb::new(0xee, 0xf1, 0xf5),
        )
        .brighten(0.18),
        None => Rgb::from_color(palette.unknown, Rgb::new(0x5d, 0x65, 0x70)),
    }
}

fn supersample_rgba(
    u0: f64,
    v0: f64,
    u1: f64,
    v1: f64,
    bands: &[RingBand],
    bg: Rgb,
    glow: bool,
) -> Rgba {
    let mut sum = [0.0_f64; 3];
    let mut sum_cov = 0.0_f64;
    let n = SUPERSAMPLE_GRID as f64;
    for sy in 0..SUPERSAMPLE_GRID {
        let v = v0 + ((sy as f64 + 0.5) / n) * (v1 - v0);
        for sx in 0..SUPERSAMPLE_GRID {
            let u = u0 + ((sx as f64 + 0.5) / n) * (u1 - u0);
            let (rgb, cov) = sample_ring_sdf(u, v, bands, bg, glow);
            sum[0] += f64::from(rgb.r) * cov;
            sum[1] += f64::from(rgb.g) * cov;
            sum[2] += f64::from(rgb.b) * cov;
            sum_cov += cov;
        }
    }
    if sum_cov <= f64::EPSILON {
        return Rgba::TRANSPARENT;
    }
    let channel = |value: f64| (value / sum_cov).round().clamp(0.0, 255.0) as u8;
    Rgba {
        rgb: Rgb::new(channel(sum[0]), channel(sum[1]), channel(sum[2])),
        alpha: ((sum_cov / (n * n)) * 255.0).round().clamp(0.0, 255.0) as u8,
    }
}

fn push_unique_rgb(palette: &mut Vec<Rgb>, color: Rgb) {
    let target = color.sixel_percent();
    if !palette
        .iter()
        .any(|existing| existing.sixel_percent() == target)
    {
        palette.push(color);
    }
}

fn build_sixel_palette(
    bands: &[RingBand],
    rings: &[Option<f64>],
    bg: Rgb,
    palette: &TokenBurnPalette,
) -> Vec<Rgb> {
    let mut table = Vec::with_capacity(32);
    table.push(bg);
    if all_rings_unknown(rings) {
        let unknown = Rgb::from_color(palette.unknown, Rgb::new(0x5d, 0x65, 0x70));
        for step in 1..=6 {
            push_unique_rgb(&mut table, bg.lerp(unknown, f64::from(step) / 6.0));
        }
        return table;
    }

    let border_rgb = Rgb::from_color(palette.border, Rgb::new(0x2a, 0x30, 0x39));
    let track = bg.lerp(border_rgb, 0.78);
    for step in 1..=4 {
        push_unique_rgb(&mut table, bg.lerp(track, f64::from(step) / 4.0));
    }

    for band in bands {
        let steps = if bands.len() == 1 { 7 } else { 6 };
        for step in 1..=steps {
            push_unique_rgb(
                &mut table,
                bg.lerp(band.fill_rgb, f64::from(step) / f64::from(steps)),
            );
        }
        push_unique_rgb(&mut table, band.fill_rgb.brighten(0.26));
    }

    let label = center_label_rgb(rings, palette);
    for step in [0.45, 0.75, 1.0] {
        push_unique_rgb(&mut table, bg.lerp(label, step));
    }
    table
}

fn nearest_palette_index(color: Rgb, table: &[Rgb]) -> usize {
    let mut best_index = 0usize;
    let mut best_dist = u32::MAX;
    for (index, &entry) in table.iter().enumerate() {
        let dist = color.dist_sq(entry);
        if dist < best_dist {
            best_dist = dist;
            best_index = index;
            if dist == 0 {
                break;
            }
        }
    }
    best_index
}

fn push_sixel_rle(out: &mut String, ch: char, count: usize) {
    if count >= SIXEL_RLE_MIN_RUN {
        out.push('!');
        out.push_str(&count.to_string());
        out.push(ch);
    } else {
        for _ in 0..count {
            out.push(ch);
        }
    }
}

pub fn sixel_pixel_dimensions(
    width_cells: u16,
    height_cells: u16,
    cell_px: (u16, u16),
) -> (usize, usize) {
    let cell_w = usize::from(cell_px.0.max(1));
    let cell_h = usize::from(cell_px.1.max(1));
    let px_w = usize::from(width_cells).saturating_mul(cell_w);
    let px_h =
        (usize::from(height_cells).saturating_mul(cell_h) / SIXEL_BAND_HEIGHT) * SIXEL_BAND_HEIGHT;
    (px_w, px_h)
}

pub fn png_pixel_dimensions(
    width_cells: u16,
    height_cells: u16,
    cell_px: (u16, u16),
) -> (usize, usize) {
    let base_w = usize::from(width_cells) * usize::from(cell_px.0.max(1));
    let base_h = usize::from(height_cells) * usize::from(cell_px.1.max(1));
    let longest = base_w.max(base_h).max(1);
    let scale = ((PNG_MIN_SIDE_PX + longest / 2) / longest).max(1);
    (base_w * scale, base_h * scale)
}

fn surface_rgb(bg: Color, palette: &TokenBurnPalette) -> Rgb {
    Rgb::from_color(
        bg,
        Rgb::from_color(palette.panel, Rgb::new(0x15, 0x18, 0x1e)),
    )
}

pub fn encode_ring_png(
    rings: &[Option<f64>],
    width_cells: u16,
    height_cells: u16,
    cell_px: (u16, u16),
    bg: Color,
    palette: &TokenBurnPalette,
) -> Vec<u8> {
    let (px_w, px_h) = png_pixel_dimensions(width_cells, height_cells, cell_px);
    if px_w == 0 || px_h == 0 {
        return Vec::new();
    }
    let raster = RingRaster::render(rings, px_w, px_h, surface_rgb(bg, palette), palette);
    crate::clipboard::encode_rgba_to_png(px_w as u32, px_h as u32, &raster.rgba_bytes())
        .unwrap_or_default()
}

pub fn encode_ring_sixel(
    rings: &[Option<f64>],
    width_cells: u16,
    height_cells: u16,
    cell_px: (u16, u16),
    bg: Color,
    palette: &TokenBurnPalette,
) -> String {
    let (px_w, px_h) = sixel_pixel_dimensions(width_cells, height_cells, cell_px);
    if px_w == 0 || px_h == 0 {
        return String::new();
    }
    let rings = if rings.is_empty() { &[None][..] } else { rings };
    let bg_rgb = surface_rgb(bg, palette);
    let raster = RingRaster::render(rings, px_w, px_h, bg_rgb, palette);
    let table = build_sixel_palette(&ring_bands(rings, bg_rgb, palette), rings, bg_rgb, palette);
    let indexed: Vec<usize> = raster
        .pixels
        .iter()
        .map(|&pixel| match pixel.alpha {
            0 => SIXEL_TRANSPARENT_REGISTER,
            _ => nearest_palette_index(pixel.over(bg_rgb), &table),
        })
        .collect();

    let mut out = String::with_capacity(px_w * (px_h / SIXEL_BAND_HEIGHT) + 256);
    out.push_str(&format!("\x1bP0;1;0q\"1;1;{px_w};{px_h}"));
    for (register, &rgb) in table.iter().enumerate().skip(1) {
        let (r, g, b) = rgb.sixel_percent();
        out.push_str(&format!("#{register};2;{r};{g};{b}"));
    }

    let band_count = px_h / SIXEL_BAND_HEIGHT;
    let mut row_masks = vec![0u8; px_w];
    for band_idx in 0..band_count {
        if band_idx > 0 {
            out.push('-');
        }
        let y_base = band_idx * SIXEL_BAND_HEIGHT;
        let mut first_register = true;
        for register in 1..table.len() {
            let mut last_non_zero = None;
            for (x, slot) in row_masks.iter_mut().enumerate() {
                let mut mask = 0u8;
                for bit in 0..SIXEL_BAND_HEIGHT {
                    if indexed[(y_base + bit) * px_w + x] == register {
                        mask |= 1 << bit;
                    }
                }
                *slot = mask;
                if mask != 0 {
                    last_non_zero = Some(x);
                }
            }
            let Some(end_x) = last_non_zero else {
                continue;
            };
            if !first_register {
                out.push('$');
            }
            first_register = false;
            out.push('#');
            out.push_str(&register.to_string());
            let mut x = 0usize;
            while x <= end_x {
                let value = row_masks[x];
                let start = x;
                while x <= end_x && row_masks[x] == value {
                    x += 1;
                }
                push_sixel_rle(&mut out, char::from(63 + value), x - start);
            }
        }
    }
    out.push_str("\x1b\\");
    out
}

pub fn kitty_transmit_sequence(image_id: u32, png: &[u8], cols: u16, rows: u16) -> String {
    let encoded = crate::clipboard::base64_encode(png);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(KITTY_CHUNK_BYTES).collect();
    let mut out = String::with_capacity(encoded.len() + chunks.len() * 16 + 64);
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        if index == 0 {
            out.push_str(&format!(
                "\x1b_Ga=T,f=100,t=d,i={image_id},c={cols},r={rows},C=1,q=2,m={more};"
            ));
        } else {
            out.push_str(&format!("\x1b_Gm={more};"));
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\x1b\\");
    }
    out
}

pub fn kitty_delete_sequence(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

pub fn iterm2_inline_sequence(png: &[u8], cols: u16, rows: u16) -> String {
    format!(
        "\x1b]1337;File=inline=1;width={cols};height={rows};preserveAspectRatio=1:{}\x07",
        crate::clipboard::base64_encode(png)
    )
}

fn partition_octant_cell(
    samples: &[(Rgb, f64); 8],
    bg_rgb: Rgb,
    unknown_rgb: Option<Rgb>,
) -> (u8, Rgb, Rgb) {
    if let Some(muted) = unknown_rgb {
        let mut mask = 0u8;
        for (bit, &(_, cov)) in samples.iter().enumerate() {
            if cov >= 0.35 {
                mask |= 1 << bit;
            }
        }
        return (mask, muted, bg_rgb);
    }

    let mut farthest_a = 0usize;
    let mut farthest_b = 0usize;
    let mut max_dist = 0u32;
    for i in 0..samples.len() {
        for j in (i + 1)..samples.len() {
            let d = samples[i].0.dist_sq(samples[j].0);
            if d > max_dist {
                max_dist = d;
                farthest_a = i;
                farthest_b = j;
            }
        }
    }

    if max_dist <= 24 {
        let avg = average_rgbs(samples.iter().map(|&(rgb, _)| rgb));
        if avg.dist_sq(bg_rgb) <= 24 {
            return (0, bg_rgb, bg_rgb);
        }
        return (0xFF, avg, bg_rgb);
    }

    let mut seed_bg = samples[farthest_a].0;
    let mut seed_fg = samples[farthest_b].0;
    if seed_fg.dist_sq(bg_rgb) < seed_bg.dist_sq(bg_rgb) {
        std::mem::swap(&mut seed_bg, &mut seed_fg);
    }

    let mut mask = 0u8;
    for _ in 0..2 {
        mask = 0;
        let mut fg_group = Vec::with_capacity(8);
        let mut bg_group = Vec::with_capacity(8);
        for (bit, &(rgb, _)) in samples.iter().enumerate() {
            if rgb.dist_sq(seed_fg) < rgb.dist_sq(seed_bg) {
                mask |= 1 << bit;
                fg_group.push(rgb);
            } else {
                bg_group.push(rgb);
            }
        }
        if !fg_group.is_empty() {
            seed_fg = average_rgbs(fg_group.into_iter());
        }
        if !bg_group.is_empty() {
            seed_bg = average_rgbs(bg_group.into_iter());
        }
    }

    (mask, seed_fg, seed_bg)
}

fn average_rgbs(iter: impl Iterator<Item = Rgb>) -> Rgb {
    let mut r = 0u32;
    let mut g = 0u32;
    let mut b = 0u32;
    let mut count = 0u32;
    for rgb in iter {
        r += u32::from(rgb.r);
        g += u32::from(rgb.g);
        b += u32::from(rgb.b);
        count += 1;
    }
    if count == 0 {
        return Rgb::new(0, 0, 0);
    }
    Rgb::new((r / count) as u8, (g / count) as u8, (b / count) as u8)
}

pub fn octant_ring_lines(
    rings: &[Option<f64>],
    width_cells: u16,
    height_cells: u16,
    bg: Color,
    palette: &TokenBurnPalette,
) -> Vec<Line<'static>> {
    let rings = if rings.is_empty() { &[None][..] } else { rings };
    let cols = usize::from(width_cells);
    let rows = usize::from(height_cells);
    let bg_rgb = Rgb::from_color(
        bg,
        Rgb::from_color(palette.panel, Rgb::new(0x15, 0x18, 0x1e)),
    );
    let bands = ring_bands(rings, bg_rgb, palette);
    let unknown_rgb = all_rings_unknown(rings).then(|| unknown_track_rgb(bg_rgb, palette));

    let sub_cols = (cols * 2) as f64;
    let sub_rows = (rows * 4) as f64;
    let mut lines: Vec<Vec<Span<'static>>> = (0..rows)
        .map(|cell_y| {
            (0..cols)
                .map(|cell_x| {
                    let mut samples = [(bg_rgb, 0.0); 8];
                    for sr in 0..4usize {
                        let v0 = ((cell_y * 4 + sr) as f64 / sub_rows) * 2.0 - 1.0;
                        let v1 = ((cell_y * 4 + sr + 1) as f64 / sub_rows) * 2.0 - 1.0;
                        for sc in 0..2usize {
                            let u0 = ((cell_x * 2 + sc) as f64 / sub_cols) * 2.0 - 1.0;
                            let u1 = ((cell_x * 2 + sc + 1) as f64 / sub_cols) * 2.0 - 1.0;
                            samples[sr * 2 + sc] =
                                supersample_region(u0, v0, u1, v1, &bands, bg_rgb, false);
                        }
                    }
                    let (mask, fg_rgb, cell_bg_rgb) =
                        partition_octant_cell(&samples, bg_rgb, unknown_rgb);
                    if mask == 0 {
                        Span::styled(" ", Style::default().bg(bg))
                    } else {
                        let ch = octant_char(mask);
                        let style = if let Some(muted) = unknown_rgb {
                            Style::default().fg(muted.to_color()).bg(bg)
                        } else {
                            Style::default()
                                .fg(fg_rgb.to_color())
                                .bg(cell_bg_rgb.to_color())
                        };
                        Span::styled(ch.to_string(), style)
                    }
                })
                .collect()
        })
        .collect();

    if rings.len() == 1 {
        overlay_center_label_with_bg(&mut lines, rings[0], Some(bg), palette);
    }
    lines.into_iter().map(Line::from).collect()
}

pub fn halfblock_ring_lines(
    rings: &[Option<f64>],
    width_cells: u16,
    height_cells: u16,
    bg: Color,
    palette: &TokenBurnPalette,
) -> Vec<Line<'static>> {
    let rings = if rings.is_empty() { &[None][..] } else { rings };
    let cols = usize::from(width_cells);
    let rows = usize::from(height_cells);
    let bg_rgb = Rgb::from_color(
        bg,
        Rgb::from_color(palette.panel, Rgb::new(0x15, 0x18, 0x1e)),
    );
    let bands = ring_bands(rings, bg_rgb, palette);
    let unknown_only = all_rings_unknown(rings);
    let muted = unknown_track_rgb(bg_rgb, palette).to_color();

    let sub_cols = cols as f64;
    let sub_rows = (rows * 2) as f64;
    let mut lines: Vec<Vec<Span<'static>>> = (0..rows)
        .map(|cell_y| {
            (0..cols)
                .map(|cell_x| {
                    let u0 = (cell_x as f64 / sub_cols) * 2.0 - 1.0;
                    let u1 = ((cell_x + 1) as f64 / sub_cols) * 2.0 - 1.0;
                    let top_v0 = ((cell_y * 2) as f64 / sub_rows) * 2.0 - 1.0;
                    let top_v1 = ((cell_y * 2 + 1) as f64 / sub_rows) * 2.0 - 1.0;
                    let bot_v0 = top_v1;
                    let bot_v1 = ((cell_y * 2 + 2) as f64 / sub_rows) * 2.0 - 1.0;
                    let (top_rgb, top_cov) =
                        supersample_region(u0, top_v0, u1, top_v1, &bands, bg_rgb, false);
                    let (bot_rgb, bot_cov) =
                        supersample_region(u0, bot_v0, u1, bot_v1, &bands, bg_rgb, false);
                    if unknown_only {
                        let top_on = top_cov >= 0.35;
                        let bot_on = bot_cov >= 0.35;
                        return match (top_on, bot_on) {
                            (false, false) => Span::styled(" ", Style::default().bg(bg)),
                            (true, false) => Span::styled("▀", Style::default().fg(muted).bg(bg)),
                            (false, true) => Span::styled("▄", Style::default().fg(muted).bg(bg)),
                            (true, true) => Span::styled("█", Style::default().fg(muted).bg(bg)),
                        };
                    }
                    let top_empty = top_rgb.dist_sq(bg_rgb) <= 18;
                    let bot_empty = bot_rgb.dist_sq(bg_rgb) <= 18;
                    if top_empty && bot_empty {
                        Span::styled(" ", Style::default().bg(bg))
                    } else if top_rgb.dist_sq(bot_rgb) <= 18 {
                        Span::styled("█", Style::default().fg(top_rgb.to_color()).bg(bg))
                    } else if top_empty {
                        Span::styled("▄", Style::default().fg(bot_rgb.to_color()).bg(bg))
                    } else if bot_empty {
                        Span::styled("▀", Style::default().fg(top_rgb.to_color()).bg(bg))
                    } else {
                        Span::styled(
                            "▀",
                            Style::default()
                                .fg(top_rgb.to_color())
                                .bg(bot_rgb.to_color()),
                        )
                    }
                })
                .collect()
        })
        .collect();

    if rings.len() == 1 {
        overlay_center_label_with_bg(&mut lines, rings[0], Some(bg), palette);
    }
    lines.into_iter().map(Line::from).collect()
}

pub fn ascii_ring_lines(
    rings: &[Option<f64>],
    width_cells: u16,
    height_cells: u16,
    bg: Color,
    palette: &TokenBurnPalette,
) -> Vec<Line<'static>> {
    let rings = if rings.is_empty() { &[None][..] } else { rings };
    let cols = usize::from(width_cells);
    let rows = usize::from(height_cells);
    let bg_rgb = Rgb::from_color(
        bg,
        Rgb::from_color(palette.panel, Rgb::new(0x15, 0x18, 0x1e)),
    );
    let bands = ring_bands(rings, bg_rgb, palette);
    let unknown_only = all_rings_unknown(rings);
    let muted = unknown_track_rgb(bg_rgb, palette).to_color();

    let mut lines: Vec<Vec<Span<'static>>> = (0..rows)
        .map(|cell_y| {
            (0..cols)
                .map(|cell_x| {
                    let u = ((cell_x as f64 + 0.5) / cols as f64) * 2.0 - 1.0;
                    let v = ((cell_y as f64 + 0.5) / rows as f64) * 2.0 - 1.0;
                    let dist = u.hypot(v);
                    let hit = bands
                        .iter()
                        .enumerate()
                        .find(|(_, band)| (dist - band.radius).abs() <= band.half_stroke * 1.15);
                    let Some((index, band)) = hit else {
                        return Span::styled(" ", Style::default().bg(bg));
                    };
                    if unknown_only {
                        return if on_unknown_dash(u, v) {
                            Span::styled(".", Style::default().fg(muted).bg(bg))
                        } else {
                            Span::styled(" ", Style::default().bg(bg))
                        };
                    }
                    let sweep = clockwise_fraction_from_top(u, v);
                    match band.remaining {
                        Some(percent) if sweep < percent / 100.0 => {
                            let symbol = if index == 0 { "#" } else { "o" };
                            Span::styled(
                                symbol,
                                Style::default()
                                    .fg(palette.allowance_color(Some(percent)))
                                    .bg(bg),
                            )
                        }
                        Some(_) => Span::styled(".", Style::default().fg(palette.border).bg(bg)),
                        None => Span::styled(".", Style::default().fg(palette.unknown).bg(bg)),
                    }
                })
                .collect()
        })
        .collect();

    if rings.len() == 1 {
        overlay_center_label_with_bg(&mut lines, rings[0], Some(bg), palette);
    }
    lines.into_iter().map(Line::from).collect()
}

pub fn ring_lines_for_caps(
    rings: &[Option<f64>],
    width_cells: u16,
    height_cells: u16,
    bg: Color,
    palette: &TokenBurnPalette,
    caps: GraphicsCaps,
) -> Vec<Line<'static>> {
    if caps.use_ascii() {
        ascii_ring_lines(rings, width_cells, height_cells, bg, palette)
    } else if caps.use_octants() {
        octant_ring_lines(rings, width_cells, height_cells, bg, palette)
    } else {
        halfblock_ring_lines(rings, width_cells, height_cells, bg, palette)
    }
}

pub fn is_ring_cell_symbol(symbol: &str) -> bool {
    let Some(ch) = symbol.chars().next() else {
        return false;
    };
    ('\u{2801}'..='\u{28FF}').contains(&ch)
        || ('\u{2580}'..='\u{259F}').contains(&ch)
        || ('\u{1CD00}'..='\u{1CDE5}').contains(&ch)
        || matches!(
            ch,
            '\u{1CEA0}'
                | '\u{1CEA3}'
                | '\u{1CEA8}'
                | '\u{1CEAB}'
                | '\u{1FB82}'
                | '\u{1FB85}'
                | '\u{1FBE6}'
                | '\u{1FBE7}'
                | '#'
                | 'o'
                | '.'
        )
}

#[derive(Clone, Debug, PartialEq)]
pub struct RingSlot {
    pub rect: Rect,
    pub rings: Vec<Option<f64>>,
    pub bg: Color,
    pub palette: TokenBurnPalette,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingCacheKey {
    pub rect: Rect,
    pub quantized_tenths: Vec<Option<u16>>,
    pub bg: Color,
    pub palette: TokenBurnPalette,
}

impl RingSlot {
    pub fn cache_key(&self) -> RingCacheKey {
        RingCacheKey {
            rect: self.rect,
            quantized_tenths: self
                .rings
                .iter()
                .map(|&value| clamp_percent(value).map(|pct| (pct * 10.0).round() as u16))
                .collect(),
            bg: self.bg,
            palette: self.palette,
        }
    }

    pub fn encode_sixel(&self, cell_px: (u16, u16)) -> String {
        encode_ring_sixel(
            &self.rings,
            self.rect.width,
            self.rect.height,
            cell_px,
            self.bg,
            &self.palette,
        )
    }

    pub fn encode_png(&self, cell_px: (u16, u16)) -> Vec<u8> {
        encode_ring_png(
            &self.rings,
            self.rect.width,
            self.rect.height,
            cell_px,
            self.bg,
            &self.palette,
        )
    }

    pub fn encode_inline(
        &self,
        protocol: InlineProtocol,
        cell_px: (u16, u16),
        image_id: u32,
    ) -> String {
        let (cols, rows) = (self.rect.width, self.rect.height);
        match protocol {
            InlineProtocol::Sixel => self.encode_sixel(cell_px),
            InlineProtocol::Kitty => {
                let png = self.encode_png(cell_px);
                if png.is_empty() {
                    return String::new();
                }
                kitty_transmit_sequence(image_id, &png, cols, rows)
            }
            InlineProtocol::Iterm2 => {
                let png = self.encode_png(cell_px);
                if png.is_empty() {
                    return String::new();
                }
                iterm2_inline_sequence(&png, cols, rows)
            }
        }
    }

    pub fn fits_without_scroll(&self, frame_area: Rect) -> bool {
        self.rect.width > 0
            && self.rect.height > 0
            && self.rect.right() <= frame_area.right()
            && self.rect.bottom() < frame_area.bottom()
    }

    pub fn prepare_cells(&self, buf: &mut Buffer, displayed: bool) {
        let area = self.rect.intersection(buf.area);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let cell = &mut buf[(x, y)];
                cell.reset();
                cell.set_symbol(" ").set_bg(self.bg);
                if displayed {
                    cell.set_diff_option(CellDiffOption::Skip);
                }
            }
        }
    }
}

pub fn rect_intersects_any(rect: Rect, overlays: &[Rect]) -> bool {
    overlays.iter().any(|overlay| {
        overlay.width > 0
            && overlay.height > 0
            && rect.x < overlay.right()
            && rect.right() > overlay.x
            && rect.y < overlay.bottom()
            && rect.bottom() > overlay.y
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineFrameUpdate {
    pub deletes: Vec<String>,
    pub sequences: Vec<(u16, u16, String)>,
}

impl InlineFrameUpdate {
    pub fn is_empty(&self) -> bool {
        self.deletes.is_empty() && self.sequences.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct InlineRingCache {
    displayed: Vec<Option<RingCacheKey>>,
    placed_ids: Vec<u32>,
    protocol: Option<InlineProtocol>,
    cell_px: (u16, u16),
}

impl InlineRingCache {
    pub fn image_id(index: usize) -> u32 {
        KITTY_IMAGE_ID_BASE + u32::try_from(index).unwrap_or(u32::MAX - KITTY_IMAGE_ID_BASE)
    }

    pub fn clear(&mut self) {
        self.displayed.clear();
    }

    pub fn is_displayed(&self, index: usize, key: &RingCacheKey) -> bool {
        self.displayed
            .get(index)
            .is_some_and(|shown| shown.as_ref() == Some(key))
    }

    pub fn release(&mut self) -> InlineFrameUpdate {
        self.displayed.clear();
        InlineFrameUpdate {
            deletes: self.take_deletes(|_| true),
            sequences: Vec::new(),
        }
    }

    pub fn diff_and_encode(
        &mut self,
        slots: &[RingSlot],
        protocol: InlineProtocol,
        cell_px: (u16, u16),
    ) -> InlineFrameUpdate {
        if self.protocol != Some(protocol) || self.cell_px != cell_px {
            self.clear();
            self.protocol = Some(protocol);
            self.cell_px = cell_px;
        }
        let mut next = Vec::with_capacity(slots.len());
        let mut sequences = Vec::new();
        let mut retained_ids = Vec::new();
        for (index, slot) in slots.iter().enumerate() {
            let key = slot.cache_key();
            if self.is_displayed(index, &key) {
                retained_ids.push(Self::image_id(index));
                next.push(Some(key));
                continue;
            }
            let payload = slot.encode_inline(protocol, cell_px, Self::image_id(index));
            if payload.is_empty() {
                next.push(None);
                continue;
            }
            sequences.push((slot.rect.x, slot.rect.y, payload));
            next.push(Some(key));
        }
        self.displayed = next;
        let deletes = self.take_deletes(|id| !retained_ids.contains(&id));
        if protocol == InlineProtocol::Kitty {
            self.placed_ids = self
                .displayed
                .iter()
                .enumerate()
                .filter(|(_, shown)| shown.is_some())
                .map(|(index, _)| Self::image_id(index))
                .collect();
        }
        InlineFrameUpdate { deletes, sequences }
    }

    fn take_deletes(&mut self, removable: impl Fn(u32) -> bool) -> Vec<String> {
        let (removed, kept): (Vec<u32>, Vec<u32>) =
            self.placed_ids.drain(..).partition(|&id| removable(id));
        self.placed_ids = kept;
        removed.into_iter().map(kitty_delete_sequence).collect()
    }
}

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
    overlay_center_label_with_bg(lines, remaining, None, palette);
}

fn overlay_center_label_with_bg(
    lines: &mut [Vec<Span<'static>>],
    remaining: Option<f64>,
    bg: Option<Color>,
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
    let mut style = Style::default()
        .fg(palette.allowance_color(remaining))
        .add_modifier(Modifier::BOLD);
    if let Some(background) = bg {
        style = style.bg(background);
    }
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
        match (
            clamp_percent(self.primary_remaining),
            clamp_percent(self.secondary_remaining),
        ) {
            (Some(primary), Some(secondary)) => Some(primary.min(secondary)),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        }
    }

    pub fn ring_tile_lines(&self, width: u16, palette: &TokenBurnPalette) -> Vec<Line<'static>> {
        let rings = self.ring_windows();
        let ring_lines = braille_ring_lines(&rings, RING_CELL_WIDTH, RING_CELL_HEIGHT, palette);
        self.combine_ring_and_text(ring_lines, rings.len() > 1, width, palette)
    }

    pub fn render_ring_tile(&self, width: u16, palette: &TokenBurnPalette) -> Vec<Line<'static>> {
        self.ring_tile_lines(width, palette)
    }

    pub fn ring_tile_lines_for_caps(
        &self,
        width: u16,
        bg: Color,
        palette: &TokenBurnPalette,
        caps: GraphicsCaps,
    ) -> Vec<Line<'static>> {
        let rings = self.ring_windows();
        let ring_lines =
            ring_lines_for_caps(&rings, RING_CELL_WIDTH, RING_CELL_HEIGHT, bg, palette, caps);
        self.combine_ring_and_text(ring_lines, rings.len() > 1, width, palette)
    }

    pub fn ring_slot(&self, tile: Rect, bg: Color, palette: TokenBurnPalette) -> RingSlot {
        RingSlot {
            rect: Rect::new(
                tile.x,
                tile.y,
                RING_CELL_WIDTH.min(tile.width),
                RING_CELL_HEIGHT.min(tile.height),
            ),
            rings: self.ring_windows(),
            bg,
            palette,
        }
    }

    fn combine_ring_and_text(
        &self,
        ring_lines: Vec<Line<'static>>,
        dual: bool,
        width: u16,
        palette: &TokenBurnPalette,
    ) -> Vec<Line<'static>> {
        let text_width =
            usize::from(width).saturating_sub(usize::from(RING_CELL_WIDTH) + RING_TEXT_GAP);
        let text_lines = self.ring_text_lines(dual, palette);
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
            text.contains("12% left") && text.contains(" MAX "),
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
    fn headline_remaining_uses_tightest_window_when_both_exist() {
        let card = |primary, secondary| AccountCard {
            primary_remaining: primary,
            secondary_remaining: secondary,
            ..AccountCard::default()
        };
        assert_eq!(card(Some(80.0), Some(0.0)).headline_remaining(), Some(0.0));
        assert_eq!(
            card(Some(64.0), Some(12.0)).headline_remaining(),
            Some(12.0)
        );
        assert_eq!(
            card(Some(15.0), Some(90.0)).headline_remaining(),
            Some(15.0)
        );
        assert_eq!(card(Some(50.0), None).headline_remaining(), Some(50.0));
        assert_eq!(card(None, Some(30.0)).headline_remaining(), Some(30.0));
        assert_eq!(
            card(Some(f64::NAN), Some(30.0)).headline_remaining(),
            Some(30.0)
        );
        assert_eq!(card(None, None).headline_remaining(), None);

        let p = TokenBurnPalette::default();
        let glyph = compact_ring(card(Some(80.0), Some(0.0)).headline_remaining(), &p);
        assert_eq!(glyph.content, "○");
        assert_eq!(glyph.style.fg, Some(p.depleted));
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

    #[test]
    fn sixel_band_height_guard_never_overflows_cell_rect() {
        for cell_px in [(10, 20), (9, 19), (8, 16), (14, 29), (12, 25), (7, 13)] {
            let (px_w, px_h) = sixel_pixel_dimensions(RING_CELL_WIDTH, RING_CELL_HEIGHT, cell_px);
            assert_eq!(px_w, usize::from(RING_CELL_WIDTH) * usize::from(cell_px.0));
            assert_eq!(px_h % 6, 0, "{cell_px:?}: {px_h}");
            assert!(
                px_h <= usize::from(RING_CELL_HEIGHT) * usize::from(cell_px.1),
                "{cell_px:?}: {px_h}"
            );
            assert!(px_h + 6 > usize::from(RING_CELL_HEIGHT) * usize::from(cell_px.1));
        }
        assert_eq!(
            sixel_pixel_dimensions(RING_CELL_WIDTH, RING_CELL_HEIGHT, (10, 20)),
            (80, 78)
        );
    }

    #[test]
    fn sixel_encoder_emits_valid_rle_frame_and_keeps_unknown_muted() {
        let p = TokenBurnPalette::default();
        let bg = p.panel;
        let header = "\x1bP0;1;0q\"1;1;80;78#1;2;";
        let encode = |rings: &[Option<f64>]| {
            encode_ring_sixel(rings, RING_CELL_WIDTH, RING_CELL_HEIGHT, (10, 20), bg, &p)
        };
        for rings in [&[Some(64.0)][..], &[Some(64.0), Some(12.0)], &[None]] {
            let sixel = encode(rings);
            assert!(sixel.starts_with(header), "{sixel}");
            assert!(sixel.ends_with("\x1b\\"));
            assert!(sixel.contains('!'));
            assert_eq!(sixel.matches('-').count(), 12);
            assert!(
                !sixel.contains("#0"),
                "register 0 is the transparent background"
            );
        }

        let unknown = encode(&[None]);
        let healthy_pct = Rgb::from_color(p.healthy, Rgb::new(0, 0, 0)).sixel_percent();
        let depleted_pct = Rgb::from_color(p.depleted, Rgb::new(0, 0, 0)).sixel_percent();
        assert!(!unknown.contains(&format!(
            ";2;{};{};{}",
            healthy_pct.0, healthy_pct.1, healthy_pct.2
        )));
        assert!(!unknown.contains(&format!(
            ";2;{};{};{}",
            depleted_pct.0, depleted_pct.1, depleted_pct.2
        )));
        assert_eq!(encode(&[]), unknown);
        assert_ne!(unknown, encode(&[Some(100.0)]));
    }

    #[test]
    fn octant_mapping_covers_all_256_unique_masks_with_exact_unicode_16_entries() {
        let mut seen = std::collections::HashSet::new();
        for mask in 0..=u8::MAX {
            let ch = octant_char(mask);
            assert!(
                seen.insert(ch),
                "duplicate octant char for mask 0x{mask:02X}"
            );
            if mask > 0 {
                assert!(is_ring_cell_symbol(&ch.to_string()), "0x{mask:02X} {ch:?}");
            }
        }
        assert_eq!(seen.len(), 256);
        assert_eq!(octant_char(0x00), ' ');
        assert_eq!(octant_char(0x04), '\u{1CD00}');
        assert_eq!(octant_char(0x0F), '▀');
        assert_eq!(octant_char(0x14), '\u{1FBE6}');
        assert_eq!(octant_char(0x28), '\u{1FBE7}');
        assert_eq!(octant_char(0x55), '▌');
        assert_eq!(octant_char(0xAA), '▐');
        assert_eq!(octant_char(0xF0), '▄');
        assert_eq!(octant_char(0xFE), '\u{1CDE5}');
        assert_eq!(octant_char(0xFF), '█');
    }

    #[test]
    fn octant_halfblock_and_ascii_rings_respect_width_labels_and_unknown_muted() {
        use crate::tui::graphics::{GraphicsCaps, GraphicsMode};

        let p = TokenBurnPalette::default();
        let bg = p.panel;
        let configs = [
            GraphicsCaps {
                sixel: false,
                cell_px: (10, 20),
                octants_safe: true,
                mode: GraphicsMode::Unicode,
                ..GraphicsCaps::default()
            },
            GraphicsCaps {
                sixel: false,
                cell_px: (10, 20),
                octants_safe: false,
                mode: GraphicsMode::Unicode,
                ..GraphicsCaps::default()
            },
            GraphicsCaps {
                sixel: false,
                cell_px: (10, 20),
                octants_safe: false,
                mode: GraphicsMode::Ascii,
                ..GraphicsCaps::default()
            },
        ];

        for caps in configs {
            let single = ring_lines_for_caps(
                &[Some(64.0)],
                RING_CELL_WIDTH,
                RING_CELL_HEIGHT,
                bg,
                &p,
                caps,
            );
            assert_eq!(single.len(), usize::from(RING_CELL_HEIGHT));
            for line in &single {
                assert_eq!(line.width(), usize::from(RING_CELL_WIDTH), "{caps:?}");
            }
            let text = ring_text(&single);
            assert!(text[2].contains("64"), "{caps:?}: {text:?}");

            let full_hundred = ring_lines_for_caps(
                &[Some(100.0)],
                RING_CELL_WIDTH,
                RING_CELL_HEIGHT,
                bg,
                &p,
                caps,
            );
            assert!(
                ring_text(&full_hundred)[2].contains("100"),
                "{caps:?}: {:?}",
                ring_text(&full_hundred)
            );

            let unknown =
                ring_lines_for_caps(&[None], RING_CELL_WIDTH, RING_CELL_HEIGHT, bg, &p, caps);
            let unknown_text = ring_text(&unknown).join("\n");
            assert!(unknown_text.contains("--"), "{caps:?}: {unknown_text}");
            assert!(!unknown_text.contains('0'), "{caps:?}: {unknown_text}");
            let dim_track =
                unknown_track_rgb(Rgb::from_color(bg, Rgb::new(0, 0, 0)), &p).to_color();
            for span in unknown.iter().flat_map(|line| &line.spans) {
                match span.content.as_ref() {
                    " " => {}
                    "-" => assert_eq!(span.style.fg, Some(p.unknown), "{caps:?}: {span:?}"),
                    _ => assert_eq!(span.style.fg, Some(dim_track), "{caps:?}: {span:?}"),
                }
            }
            let painted = |lines: &[Line<'static>]| {
                lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .filter(|span| is_ring_cell_symbol(&span.content))
                    .count()
            };
            assert!(
                painted(&unknown) < painted(&full_hundred),
                "{caps:?}: unknown must be a segmented track, not a full ring"
            );

            let dual = ring_lines_for_caps(
                &[Some(64.0), Some(12.0)],
                RING_CELL_WIDTH,
                RING_CELL_HEIGHT,
                bg,
                &p,
                caps,
            );
            assert_eq!(dual.len(), usize::from(RING_CELL_HEIGHT));
            for line in &dual {
                assert_eq!(line.width(), usize::from(RING_CELL_WIDTH), "{caps:?}");
            }
            let ring_cells = dual
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| is_ring_cell_symbol(&span.content))
                .count();
            assert!(ring_cells >= 16, "{caps:?}: {ring_cells}");
        }

        let halfblock_symbols: Vec<char> = ring_text(&halfblock_ring_lines(
            &[Some(64.0)],
            RING_CELL_WIDTH,
            RING_CELL_HEIGHT,
            bg,
            &p,
        ))
        .into_iter()
        .flat_map(|row| row.chars().collect::<Vec<_>>())
        .collect();
        assert!(halfblock_symbols
            .iter()
            .all(|&ch| ch == ' ' || ch.is_ascii_digit() || matches!(ch, '▀' | '▄' | '█')));
    }

    #[test]
    fn ring_tile_caps_helper_and_sixel_cache_diff_and_occlusion_work() {
        use crate::tui::graphics::{GraphicsCaps, GraphicsMode};

        let p = TokenBurnPalette::default();
        let card = &cards()[0];
        assert_eq!(card.render_ring_tile(29, &p), card.ring_tile_lines(29, &p));

        let caps = GraphicsCaps {
            sixel: true,
            cell_px: (10, 20),
            cell_px_measured: true,
            octants_safe: true,
            mode: GraphicsMode::Auto,
            ..GraphicsCaps::default()
        };
        for width in [20u16, 24, 29] {
            let lines = card.ring_tile_lines_for_caps(width, p.panel, &p, caps);
            assert_eq!(lines.len(), usize::from(RING_TILE_HEIGHT));
            for line in &lines {
                assert!(line.width() <= usize::from(width));
            }
        }

        let tile = Rect::new(1, 6, 28, RING_TILE_HEIGHT);
        let slot = card.ring_slot(tile, p.panel, p);
        assert_eq!(
            slot.rect,
            Rect::new(1, 6, RING_CELL_WIDTH, RING_CELL_HEIGHT)
        );
        assert!(slot.fits_without_scroll(Rect::new(0, 0, 140, 40)));
        assert!(!slot.fits_without_scroll(Rect::new(0, 0, 140, 10)));
        assert!(!rect_intersects_any(slot.rect, &[Rect::new(40, 5, 60, 20)]));
        assert!(rect_intersects_any(slot.rect, &[Rect::new(5, 8, 40, 10)]));

        let mut cache = InlineRingCache::default();
        assert!(!cache.is_displayed(0, &slot.cache_key()));
        let first =
            cache.diff_and_encode(std::slice::from_ref(&slot), InlineProtocol::Sixel, (10, 20));
        assert_eq!(first.sequences.len(), 1);
        assert_eq!(first.sequences[0].0, 1);
        assert_eq!(first.sequences[0].1, 6);
        assert!(first.deletes.is_empty());
        assert!(cache.is_displayed(0, &slot.cache_key()));

        let idle =
            cache.diff_and_encode(std::slice::from_ref(&slot), InlineProtocol::Sixel, (10, 20));
        assert!(idle.is_empty());

        cache.clear();
        assert!(!cache.is_displayed(0, &slot.cache_key()));
        let forced =
            cache.diff_and_encode(std::slice::from_ref(&slot), InlineProtocol::Sixel, (10, 20));
        assert_eq!(forced.sequences.len(), 1);

        let remeasured =
            cache.diff_and_encode(std::slice::from_ref(&slot), InlineProtocol::Sixel, (20, 40));
        assert_eq!(remeasured.sequences.len(), 1);

        let hidden = cache.diff_and_encode(&[], InlineProtocol::Sixel, (20, 40));
        assert!(hidden.is_empty());
        assert!(!cache.is_displayed(0, &slot.cache_key()));
    }

    fn decode_stored_png(png: &[u8]) -> (usize, usize, Vec<u8>) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let mut offset = 8;
        let mut size = (0usize, 0usize);
        let mut zlib = Vec::new();
        while offset + 8 <= png.len() {
            let len = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = &png[offset + 4..offset + 8];
            let data = &png[offset + 8..offset + 8 + len];
            if kind == b"IHDR" {
                size = (
                    u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize,
                    u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize,
                );
                assert_eq!(data[8], 8, "bit depth");
                assert_eq!(data[9], 6, "RGBA color type");
            }
            if kind == b"IDAT" {
                zlib.extend_from_slice(data);
            }
            offset += 12 + len;
        }
        let mut raw = Vec::new();
        let mut cursor = 2;
        loop {
            let last = zlib[cursor] & 1 == 1;
            let len = u16::from_le_bytes([zlib[cursor + 1], zlib[cursor + 2]]) as usize;
            raw.extend_from_slice(&zlib[cursor + 5..cursor + 5 + len]);
            cursor += 5 + len;
            if last {
                break;
            }
        }
        let rgba = raw
            .chunks_exact(size.0 * 4 + 1)
            .flat_map(|row| {
                assert_eq!(row[0], 0);
                row[1..].to_vec()
            })
            .collect();
        (size.0, size.1, rgba)
    }

    fn ring_pixel_at(raster: &RingRaster, radius_norm: f64, fraction: f64) -> Rgba {
        let side = raster.width.min(raster.height) as f64;
        let radius = radius_norm * side * 0.5;
        let angle = fraction * std::f64::consts::TAU;
        let x = raster.width as f64 * 0.5 + radius * angle.sin();
        let y = raster.height as f64 * 0.5 - radius * angle.cos();
        raster.pixels[y as usize * raster.width + x as usize]
    }

    #[test]
    fn png_ring_is_transparent_rgba_sized_for_hidpi_cells() {
        let p = TokenBurnPalette::default();
        assert_eq!(png_pixel_dimensions(8, 4, (10, 20)), (160, 160));
        assert_eq!(png_pixel_dimensions(8, 4, (18, 36)), (144, 144));
        assert_eq!(png_pixel_dimensions(8, 4, (9, 18)), (144, 144));

        let png = encode_ring_png(&[Some(64.0)], 8, 4, (10, 20), p.panel, &p);
        let (width, height, rgba) = decode_stored_png(&png);
        assert_eq!((width, height), (160, 160));
        let alpha = |x: usize, y: usize| rgba[(y * width + x) * 4 + 3];
        for (x, y) in [
            (0, 0),
            (width - 1, 0),
            (0, height - 1),
            (width - 1, height - 1),
        ] {
            assert_eq!(alpha(x, y), 0, "corner ({x},{y}) must be transparent");
        }
        assert!(rgba.iter().skip(3).step_by(4).any(|&alpha| alpha == 255));
        assert!(rgba
            .iter()
            .skip(3)
            .step_by(4)
            .any(|&alpha| alpha > 0 && alpha < 255));
        assert!(encode_ring_png(&[Some(64.0)], 0, 4, (10, 20), p.panel, &p).is_empty());
    }

    #[test]
    fn raster_ring_sweeps_clockwise_from_twelve_with_accurate_coverage() {
        let p = TokenBurnPalette::default();
        let bg = Rgb::from_color(p.panel, Rgb::new(0, 0, 0));
        let probes = [0.125, 0.375, 0.625, 0.875];
        for (percent, filled) in [(0.0, 0), (25.0, 1), (50.0, 2), (75.0, 3), (100.0, 4)] {
            let raster = RingRaster::render(&[Some(percent)], 160, 160, bg, &p);
            let fill = Rgb::from_color(p.allowance_color(Some(percent)), bg);
            let track = bg.lerp(Rgb::from_color(p.border, bg), 0.78);
            for (index, fraction) in probes.into_iter().enumerate() {
                let pixel = ring_pixel_at(&raster, SINGLE_RING_RADIUS, fraction);
                assert_eq!(pixel.alpha, 255, "{percent}% at {fraction}");
                let is_fill = pixel.rgb.dist_sq(fill) < pixel.rgb.dist_sq(track);
                assert_eq!(
                    is_fill,
                    index < filled,
                    "{percent}% at {fraction}: {pixel:?}"
                );
            }
        }

        let dual = RingRaster::render(&[Some(75.0), Some(25.0)], 160, 160, bg, &p);
        let healthy = Rgb::from_color(p.healthy, bg);
        let outer_q3 = ring_pixel_at(&dual, DUAL_OUTER_RADIUS, 0.625);
        let inner_q3 = ring_pixel_at(&dual, DUAL_INNER_RADIUS, 0.625);
        let inner_q1 = ring_pixel_at(&dual, DUAL_INNER_RADIUS, 0.125);
        assert!(outer_q3.rgb.dist_sq(healthy) < 6000, "{outer_q3:?}");
        assert!(inner_q1.rgb.dist_sq(healthy) < 6000, "{inner_q1:?}");
        assert!(inner_q3.rgb.dist_sq(healthy) > 6000, "{inner_q3:?}");
    }

    #[test]
    fn unknown_raster_is_a_dim_segmented_track_distinct_from_full() {
        let p = TokenBurnPalette::default();
        let bg = Rgb::from_color(p.panel, Rgb::new(0, 0, 0));
        let unknown = RingRaster::render(&[None], 160, 160, bg, &p);
        let full = RingRaster::render(&[Some(100.0)], 160, 160, bg, &p);
        let opaque = |raster: &RingRaster| raster.pixels.iter().filter(|px| px.alpha > 0).count();
        assert!(opaque(&unknown) * 10 < opaque(&full) * 7);

        let samples: Vec<u8> = (0..64)
            .map(|step| {
                ring_pixel_at(&unknown, SINGLE_RING_RADIUS, (f64::from(step) + 0.5) / 64.0).alpha
            })
            .collect();
        assert!(samples.contains(&0), "dashes need gaps");
        assert!(samples.contains(&255), "dashes need segments");
        let dim = unknown_track_rgb(bg, &p);
        let bright = Rgb::from_color(p.unknown, bg);
        let segment = (0..64)
            .map(|step| ring_pixel_at(&unknown, SINGLE_RING_RADIUS, (f64::from(step) + 0.5) / 64.0))
            .find(|px| px.alpha == 255)
            .unwrap();
        assert!(segment.rgb.dist_sq(dim) < segment.rgb.dist_sq(bright));
        let center = unknown.pixels[80 * 160 + 80 - 6];
        assert_eq!(center.rgb, Rgb::from_color(p.unknown, bg), "muted -- label");
    }

    #[test]
    fn kitty_and_iterm2_sequences_fit_exact_cells_and_kitty_deletes_hidden_ids() {
        let p = TokenBurnPalette::default();
        let card = &cards()[0];
        let slot = card.ring_slot(Rect::new(1, 6, 28, RING_TILE_HEIGHT), p.panel, p);
        let kitty = slot.encode_inline(InlineProtocol::Kitty, (10, 20), 7100);
        assert!(
            kitty.starts_with("\x1b_Ga=T,f=100,t=d,i=7100,c=8,r=4,C=1,q=2,m=1;"),
            "{}",
            &kitty[..60]
        );
        assert!(kitty.ends_with("\x1b\\"));
        assert!(kitty.contains("\x1b_Gm=0;"));
        let iterm = slot.encode_inline(InlineProtocol::Iterm2, (10, 20), 7100);
        assert!(iterm.starts_with(
            "\x1b]1337;File=inline=1;width=8;height=4;preserveAspectRatio=1:iVBORw0KGgo"
        ));
        assert!(iterm.ends_with('\x07'));

        let mut cache = InlineRingCache::default();
        let placed =
            cache.diff_and_encode(std::slice::from_ref(&slot), InlineProtocol::Kitty, (10, 20));
        assert_eq!(placed.sequences.len(), 1);
        assert!(placed.deletes.is_empty());
        assert!(cache
            .diff_and_encode(std::slice::from_ref(&slot), InlineProtocol::Kitty, (10, 20))
            .is_empty());

        let mut changed = slot.clone();
        changed.rings = vec![Some(10.0)];
        let replaced = cache.diff_and_encode(
            std::slice::from_ref(&changed),
            InlineProtocol::Kitty,
            (10, 20),
        );
        assert_eq!(replaced.deletes, vec![kitty_delete_sequence(7100)]);
        assert_eq!(replaced.sequences.len(), 1);

        let hidden = cache.diff_and_encode(&[], InlineProtocol::Kitty, (10, 20));
        assert_eq!(
            hidden.deletes,
            vec!["\x1b_Ga=d,d=I,i=7100,q=2\x1b\\".to_owned()]
        );
        assert!(hidden.sequences.is_empty());
        assert!(cache.release().is_empty());

        cache.diff_and_encode(std::slice::from_ref(&slot), InlineProtocol::Kitty, (10, 20));
        assert_eq!(cache.release().deletes, vec![kitty_delete_sequence(7100)]);
    }
}
