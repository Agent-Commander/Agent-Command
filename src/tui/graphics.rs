use std::{env, io::IsTerminal, time::Duration};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub const GRAPHICS_ENV: &str = "AGENT_COMMAND_GRAPHICS";
pub const DEFAULT_CELL_PX: (u16, u16) = (10, 20);
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);
const MIN_CELL_WIDTH: u16 = 4;
const MAX_CELL_WIDTH: u16 = 64;
const MIN_CELL_HEIGHT: u16 = 8;
const MAX_CELL_HEIGHT: u16 = 128;
const KITTY_PROBE_ID: &[u8] = b"i=31";
pub(crate) const CAPABILITY_QUERY: &[u8] =
    b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[16t\x1b[14t\x1b[c";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GraphicsMode {
    #[default]
    Auto,
    Kitty,
    Iterm2,
    Sixel,
    Unicode,
    Ascii,
}

impl GraphicsMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "kitty" => Some(Self::Kitty),
            "iterm2" | "iterm" => Some(Self::Iterm2),
            "sixel" => Some(Self::Sixel),
            "unicode" => Some(Self::Unicode),
            "ascii" => Some(Self::Ascii),
            _ => None,
        }
    }

    fn skips_probe(self) -> bool {
        matches!(self, Self::Unicode | Self::Ascii)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InlineProtocol {
    Kitty,
    Iterm2,
    Sixel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicsCaps {
    pub kitty: bool,
    pub iterm2: bool,
    pub sixel: bool,
    pub cell_px: (u16, u16),
    pub cell_px_measured: bool,
    pub octants_safe: bool,
    pub mode: GraphicsMode,
}

impl Default for GraphicsCaps {
    fn default() -> Self {
        Self {
            kitty: false,
            iterm2: false,
            sixel: false,
            cell_px: DEFAULT_CELL_PX,
            cell_px_measured: false,
            octants_safe: false,
            mode: GraphicsMode::Auto,
        }
    }
}

impl GraphicsCaps {
    pub fn inline_protocol(self) -> Option<InlineProtocol> {
        match self.mode {
            GraphicsMode::Kitty => Some(InlineProtocol::Kitty),
            GraphicsMode::Iterm2 => Some(InlineProtocol::Iterm2),
            GraphicsMode::Sixel => Some(InlineProtocol::Sixel),
            GraphicsMode::Unicode | GraphicsMode::Ascii => None,
            GraphicsMode::Auto if self.kitty => Some(InlineProtocol::Kitty),
            GraphicsMode::Auto if self.iterm2 => Some(InlineProtocol::Iterm2),
            GraphicsMode::Auto if self.sixel && self.cell_px_measured => {
                Some(InlineProtocol::Sixel)
            }
            GraphicsMode::Auto => None,
        }
    }

    pub fn use_inline_graphics(self) -> bool {
        self.inline_protocol().is_some()
    }

    pub fn use_sixel(self) -> bool {
        self.inline_protocol() == Some(InlineProtocol::Sixel)
    }

    pub fn use_octants(self) -> bool {
        match self.mode {
            GraphicsMode::Ascii => false,
            _ => self.octants_safe,
        }
    }

    pub fn use_ascii(self) -> bool {
        self.mode == GraphicsMode::Ascii
    }

    pub fn remeasure(&mut self, window: Option<WindowMetrics>) -> bool {
        let Some(cell_px) = window.and_then(WindowMetrics::cell_px) else {
            return false;
        };
        let changed = self.cell_px != cell_px || !self.cell_px_measured;
        self.cell_px = cell_px;
        self.cell_px_measured = true;
        changed
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProbeReply {
    pub sixel: bool,
    pub kitty: bool,
    pub cell_px: Option<(u16, u16)>,
    pub text_area_px: Option<(u16, u16)>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowMetrics {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u16,
    pub height_px: u16,
}

impl WindowMetrics {
    pub fn cell_px(self) -> Option<(u16, u16)> {
        divide_area_into_cells((self.width_px, self.height_px), self.cols, self.rows)
    }
}

fn divide_area_into_cells(area_px: (u16, u16), cols: u16, rows: u16) -> Option<(u16, u16)> {
    if cols == 0 || rows == 0 || area_px.0 == 0 || area_px.1 == 0 {
        return None;
    }
    valid_cell_px((area_px.0 / cols, area_px.1 / rows))
}

fn valid_cell_px((width, height): (u16, u16)) -> Option<(u16, u16)> {
    ((MIN_CELL_WIDTH..=MAX_CELL_WIDTH).contains(&width)
        && (MIN_CELL_HEIGHT..=MAX_CELL_HEIGHT).contains(&height))
    .then_some((width, height))
}

pub fn measure_cell_px(
    probe: Option<ProbeReply>,
    window: Option<WindowMetrics>,
) -> Option<(u16, u16)> {
    let logical = probe.and_then(|reply| reply.cell_px);
    let physical = window.and_then(WindowMetrics::cell_px).or_else(|| {
        let area = probe.and_then(|reply| reply.text_area_px)?;
        let window = window?;
        divide_area_into_cells(area, window.cols, window.rows)
    });
    match (logical, physical) {
        (Some(points), Some(pixels)) if pixels.0 > points.0 || pixels.1 > points.1 => Some(pixels),
        (Some(points), _) => Some(points),
        (None, pixels) => pixels,
    }
}

pub fn parse_terminal_probe_reply(bytes: &[u8]) -> ProbeReply {
    let mut reply = ProbeReply::default();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] != 0x1b {
            index += 1;
            continue;
        }
        match bytes[index + 1] {
            b'[' => {
                let Some((end, body)) = scan_csi(bytes, index + 2) else {
                    index += 2;
                    continue;
                };
                if let Some(sixel) = parse_da1_sixel(body) {
                    reply.sixel |= sixel;
                }
                if let Some(cell_px) = parse_csi_16t_cell_px(body) {
                    reply.cell_px = Some(cell_px);
                }
                if let Some(area) = parse_csi_14t_text_area_px(body) {
                    reply.text_area_px = Some(area);
                }
                index = end;
            }
            b'_' => {
                let Some((end, body)) = scan_apc(bytes, index + 2) else {
                    index += 2;
                    continue;
                };
                reply.kitty |= is_kitty_probe_ok(body);
                index = end;
            }
            _ => index += 1,
        }
    }
    reply
}

pub(crate) fn probe_reply_complete(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }
        if let Some((end, body)) = scan_csi(bytes, index + 2) {
            if parse_da1_sixel(body).is_some() {
                return true;
            }
            index = end;
        } else {
            index += 2;
        }
    }
    false
}

fn scan_csi(bytes: &[u8], start: usize) -> Option<(usize, &[u8])> {
    let mut cursor = start;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if (0x40..=0x7e).contains(&byte) {
            return Some((cursor + 1, &bytes[start..=cursor]));
        }
        if !(0x20..=0x3f).contains(&byte) {
            return None;
        }
        cursor += 1;
    }
    None
}

fn scan_apc(bytes: &[u8], start: usize) -> Option<(usize, &[u8])> {
    let terminator = bytes[start..]
        .windows(2)
        .position(|pair| pair == b"\x1b\\")?;
    Some((start + terminator + 2, &bytes[start..start + terminator]))
}

fn is_kitty_probe_ok(body: &[u8]) -> bool {
    let Some(rest) = body.strip_prefix(b"G") else {
        return false;
    };
    let Some(split) = rest.iter().position(|&byte| byte == b';') else {
        return false;
    };
    let (keys, message) = (&rest[..split], &rest[split + 1..]);
    keys.split(|&byte| byte == b',')
        .any(|pair| pair == KITTY_PROBE_ID)
        && message.starts_with(b"OK")
}

fn parse_da1_sixel(body: &[u8]) -> Option<bool> {
    let inner = body.strip_prefix(b"?")?.strip_suffix(b"c")?;
    let mut params = parse_u16_params(inner)?;
    let _device_class = params.next()?;
    Some(params.any(|attr| attr == 4))
}

fn parse_csi_size_reply(body: &[u8], selector: u16) -> Option<(u16, u16)> {
    let inner = body.strip_suffix(b"t")?;
    let mut params = parse_u16_params(inner)?;
    if params.next()? != selector {
        return None;
    }
    let height = params.next()?;
    let width = params.next()?;
    if params.next().is_some() {
        return None;
    }
    Some((width, height))
}

fn parse_csi_16t_cell_px(body: &[u8]) -> Option<(u16, u16)> {
    parse_csi_size_reply(body, 6).and_then(valid_cell_px)
}

fn parse_csi_14t_text_area_px(body: &[u8]) -> Option<(u16, u16)> {
    parse_csi_size_reply(body, 4).filter(|&(width, height)| width > 0 && height > 0)
}

fn parse_u16_params(raw: &[u8]) -> Option<impl Iterator<Item = u16> + '_> {
    if raw.is_empty()
        || !raw
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
    {
        return None;
    }
    let text = std::str::from_utf8(raw).ok()?;
    let mut values = Vec::new();
    for part in text.split(';') {
        if part.is_empty() {
            return None;
        }
        values.push(part.parse::<u16>().ok()?);
    }
    Some(values.into_iter())
}

pub fn detect_octants_safe_from_env(
    wt_session: bool,
    term_program: Option<&str>,
    term: Option<&str>,
) -> bool {
    if wt_session {
        return true;
    }
    if let Some(program) = term_program.map(str::trim) {
        let lower = program.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "wezterm" | "ghostty" | "kitty" | "contour" | "rio"
        ) {
            return true;
        }
    }
    if let Some(term_name) = term.map(str::trim) {
        let lower = term_name.to_ascii_lowercase();
        if lower.starts_with("xterm-kitty")
            || lower.starts_with("foot")
            || lower.starts_with("wezterm")
            || lower.starts_with("xterm-ghostty")
            || lower.starts_with("contour")
            || lower.starts_with("rio")
        {
            return true;
        }
    }
    false
}

pub fn detect_iterm2(term_program: Option<&str>, multiplexed: bool) -> bool {
    !multiplexed && term_program.is_some_and(|program| program.trim() == "iTerm.app")
}

pub fn detect_multiplexer(tmux: bool, term: Option<&str>) -> bool {
    tmux || term.is_some_and(|name| {
        let lower = name.trim().to_ascii_lowercase();
        lower.starts_with("screen") || lower.starts_with("tmux")
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TerminalEnv<'a> {
    pub graphics_override: Option<&'a str>,
    pub wt_session: bool,
    pub term_program: Option<&'a str>,
    pub term: Option<&'a str>,
    pub multiplexed: bool,
}

impl TerminalEnv<'_> {
    fn mode(&self) -> GraphicsMode {
        self.graphics_override
            .and_then(GraphicsMode::parse)
            .unwrap_or_default()
    }
}

pub fn resolve_graphics_caps(
    terminal: &TerminalEnv<'_>,
    probe: Option<ProbeReply>,
    window: Option<WindowMetrics>,
) -> GraphicsCaps {
    let mode = terminal.mode();
    let probed = !mode.skips_probe();
    let measured = measure_cell_px(probe.filter(|_| probed), window.filter(|_| probed));
    GraphicsCaps {
        kitty: probed && probe.is_some_and(|reply| reply.kitty),
        iterm2: probed && detect_iterm2(terminal.term_program, terminal.multiplexed),
        sixel: probed && probe.is_some_and(|reply| reply.sixel),
        cell_px: measured.unwrap_or(DEFAULT_CELL_PX),
        cell_px_measured: measured.is_some(),
        octants_safe: detect_octants_safe_from_env(
            terminal.wt_session,
            terminal.term_program,
            terminal.term,
        ),
        mode,
    }
}

pub fn probe_terminal_graphics() -> GraphicsCaps {
    let graphics_override = env::var(GRAPHICS_ENV).ok();
    let term_program = env::var("TERM_PROGRAM").ok();
    let term = env::var("TERM").ok();
    let terminal = TerminalEnv {
        graphics_override: graphics_override.as_deref(),
        wt_session: env::var_os("WT_SESSION").is_some_and(|value| !value.is_empty()),
        term_program: term_program.as_deref(),
        term: term.as_deref(),
        multiplexed: detect_multiplexer(
            env::var_os("TMUX").is_some_and(|value| !value.is_empty()),
            term.as_deref(),
        ),
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let probe = (interactive && !terminal.mode().skips_probe())
        .then(|| probe_platform(PROBE_TIMEOUT))
        .flatten();
    resolve_graphics_caps(&terminal, probe, window_metrics())
}

#[cfg(windows)]
fn probe_platform(timeout: Duration) -> Option<ProbeReply> {
    windows::probe_da1_and_cell_px(timeout)
}

#[cfg(unix)]
fn probe_platform(timeout: Duration) -> Option<ProbeReply> {
    unix::probe_da1_and_cell_px(timeout)
}

#[cfg(not(any(windows, unix)))]
fn probe_platform(_timeout: Duration) -> Option<ProbeReply> {
    None
}

#[cfg(unix)]
pub fn window_metrics() -> Option<WindowMetrics> {
    unix::window_metrics()
}

#[cfg(not(unix))]
pub fn window_metrics() -> Option<WindowMetrics> {
    let (cols, rows) = crossterm::terminal::size().ok()?;
    Some(WindowMetrics {
        cols,
        rows,
        width_px: 0,
        height_px: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(sixel: bool, cell_px: Option<(u16, u16)>) -> Option<ProbeReply> {
        Some(ProbeReply {
            sixel,
            cell_px,
            ..ProbeReply::default()
        })
    }

    fn window(cols: u16, rows: u16, width_px: u16, height_px: u16) -> Option<WindowMetrics> {
        Some(WindowMetrics {
            cols,
            rows,
            width_px,
            height_px,
        })
    }

    fn env_with(graphics_override: Option<&str>, wt_session: bool) -> TerminalEnv<'_> {
        TerminalEnv {
            graphics_override,
            wt_session,
            ..TerminalEnv::default()
        }
    }

    #[test]
    fn parses_da1_sixel_and_csi_16t_cell_dimensions() {
        let reply =
            parse_terminal_probe_reply(b"\x1b[6;20;10t\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c");
        assert!(reply.sixel);
        assert!(!reply.kitty);
        assert_eq!(reply.cell_px, Some((10, 20)));

        let no_sixel = parse_terminal_probe_reply(b"\x1b[6;18;9t\x1b[?62;1;2;6;22c");
        assert!(!no_sixel.sixel);
        assert_eq!(no_sixel.cell_px, Some((9, 18)));

        let only_da1 = parse_terminal_probe_reply(b"\x1b[?62;4c");
        assert!(only_da1.sixel);
        assert_eq!(only_da1.cell_px, None);
    }

    #[test]
    fn parses_kitty_graphics_reply_and_csi_14t_text_area() {
        let reply = parse_terminal_probe_reply(
            b"\x1b_Gi=31;OK\x1b\\\x1b[6;18;9t\x1b[4;1440;1800t\x1b[?62;22c",
        );
        assert!(reply.kitty);
        assert!(!reply.sixel);
        assert_eq!(reply.cell_px, Some((9, 18)));
        assert_eq!(reply.text_area_px, Some((1800, 1440)));

        let refused = parse_terminal_probe_reply(b"\x1b_Gi=31;ENOTSUPPORTED\x1b\\\x1b[?62c");
        assert!(!refused.kitty);
        let other_id = parse_terminal_probe_reply(b"\x1b_Gi=7;OK\x1b\\\x1b[?62c");
        assert!(!other_id.kitty);
        let truncated = parse_terminal_probe_reply(b"\x1b_Gi=31;OK");
        assert!(!truncated.kitty);
    }

    #[test]
    fn rejects_malformed_or_out_of_range_probe_replies() {
        let garbage = parse_terminal_probe_reply(b"hello\x1b[6;0;0t\x1b[6;999;999t\x1b[?4c");
        assert!(!garbage.sixel);
        assert_eq!(garbage.cell_px, None);

        let truncated = parse_terminal_probe_reply(b"\x1b[6;20;10\x1b[?61;4");
        assert!(!truncated.sixel);
        assert_eq!(truncated.cell_px, None);
        assert!(!probe_reply_complete(b"\x1b[6;20;10t"));
        assert!(probe_reply_complete(b"\x1b[6;20;10t\x1b[?61;4c"));
        assert!(!probe_reply_complete(b"\x1b_Gi=31;OK\x1b\\"));
    }

    #[test]
    fn measures_cell_pixels_preferring_physical_hidpi_over_logical_points() {
        let logical = Some(ProbeReply {
            cell_px: Some((9, 18)),
            ..ProbeReply::default()
        });
        assert_eq!(
            measure_cell_px(logical, window(200, 50, 3600, 1800)),
            Some((18, 36))
        );
        assert_eq!(
            measure_cell_px(logical, window(200, 50, 0, 0)),
            Some((9, 18))
        );
        assert_eq!(
            measure_cell_px(None, window(100, 40, 1000, 800)),
            Some((10, 20))
        );

        let text_area = Some(ProbeReply {
            text_area_px: Some((2000, 1600)),
            ..ProbeReply::default()
        });
        assert_eq!(
            measure_cell_px(text_area, window(100, 40, 0, 0)),
            Some((20, 40))
        );
        assert_eq!(measure_cell_px(text_area, None), None);
        assert_eq!(measure_cell_px(None, window(0, 0, 100, 100)), None);
        assert_eq!(measure_cell_px(None, None), None);
    }

    #[test]
    fn auto_refuses_sixel_without_measured_cell_pixels() {
        let unmeasured = resolve_graphics_caps(&env_with(None, true), probe(true, None), None);
        assert!(unmeasured.sixel);
        assert!(!unmeasured.cell_px_measured);
        assert_eq!(unmeasured.cell_px, DEFAULT_CELL_PX);
        assert!(!unmeasured.use_sixel());
        assert!(!unmeasured.use_inline_graphics());
        assert!(unmeasured.use_octants());

        let from_ioctl = resolve_graphics_caps(
            &env_with(None, true),
            probe(true, None),
            window(120, 40, 1200, 800),
        );
        assert!(from_ioctl.cell_px_measured);
        assert_eq!(from_ioctl.cell_px, (10, 20));
        assert_eq!(from_ioctl.inline_protocol(), Some(InlineProtocol::Sixel));

        let forced = resolve_graphics_caps(&env_with(Some("sixel"), false), None, None);
        assert_eq!(forced.inline_protocol(), Some(InlineProtocol::Sixel));
        assert_eq!(forced.cell_px, DEFAULT_CELL_PX);
    }

    #[test]
    fn selects_kitty_then_iterm2_then_sixel_in_auto() {
        let kitty_and_sixel = Some(ProbeReply {
            kitty: true,
            sixel: true,
            cell_px: Some((10, 20)),
            ..ProbeReply::default()
        });
        let kitty = resolve_graphics_caps(&env_with(None, false), kitty_and_sixel, None);
        assert_eq!(kitty.inline_protocol(), Some(InlineProtocol::Kitty));

        let iterm = TerminalEnv {
            term_program: Some("iTerm.app"),
            ..TerminalEnv::default()
        };
        let iterm_caps = resolve_graphics_caps(&iterm, probe(true, Some((10, 20))), None);
        assert!(iterm_caps.iterm2);
        assert_eq!(iterm_caps.inline_protocol(), Some(InlineProtocol::Iterm2));

        let iterm_in_tmux = TerminalEnv {
            multiplexed: detect_multiplexer(true, Some("xterm-256color")),
            ..iterm
        };
        let tmux_caps = resolve_graphics_caps(&iterm_in_tmux, None, None);
        assert!(!tmux_caps.iterm2);
        assert_eq!(tmux_caps.inline_protocol(), None);
        assert!(detect_multiplexer(false, Some("screen-256color")));
        assert!(!detect_iterm2(Some("Apple_Terminal"), false));

        let sixel = resolve_graphics_caps(&env_with(None, true), probe(true, Some((10, 20))), None);
        assert_eq!(sixel.inline_protocol(), Some(InlineProtocol::Sixel));

        let forced_kitty = resolve_graphics_caps(&env_with(Some("kitty"), false), None, None);
        assert_eq!(forced_kitty.inline_protocol(), Some(InlineProtocol::Kitty));
        let forced_iterm = resolve_graphics_caps(&env_with(Some("iterm2"), false), None, None);
        assert_eq!(forced_iterm.inline_protocol(), Some(InlineProtocol::Iterm2));
    }

    #[test]
    fn resolves_env_override_and_terminal_capabilities() {
        let forced_unicode = resolve_graphics_caps(
            &env_with(Some("UNICODE"), true),
            probe(true, Some((12, 24))),
            None,
        );
        assert!(!forced_unicode.use_inline_graphics());
        assert!(forced_unicode.use_octants());
        assert_eq!(forced_unicode.mode, GraphicsMode::Unicode);

        let forced_ascii = resolve_graphics_caps(&env_with(Some("ascii"), true), None, None);
        assert!(!forced_ascii.use_inline_graphics());
        assert!(!forced_ascii.use_octants());
        assert!(forced_ascii.use_ascii());

        let classic_conhost = resolve_graphics_caps(&env_with(None, false), None, None);
        assert!(!classic_conhost.use_inline_graphics());
        assert!(!classic_conhost.use_octants());
        assert!(!classic_conhost.use_ascii());
    }

    #[test]
    fn remeasure_updates_cell_pixels_on_resize() {
        let mut caps = GraphicsCaps::default();
        assert!(!caps.remeasure(window(80, 24, 0, 0)));
        assert!(!caps.cell_px_measured);
        assert!(caps.remeasure(window(80, 24, 1440, 864)));
        assert_eq!(caps.cell_px, (18, 36));
        assert!(caps.cell_px_measured);
        assert!(!caps.remeasure(window(100, 30, 1800, 1080)));
    }

    #[test]
    fn detects_modern_terminals_for_octants() {
        assert!(detect_octants_safe_from_env(true, None, None));
        assert!(detect_octants_safe_from_env(false, Some("WezTerm"), None));
        assert!(detect_octants_safe_from_env(false, Some("ghostty"), None));
        assert!(detect_octants_safe_from_env(
            false,
            None,
            Some("foot-extra")
        ));
        assert!(detect_octants_safe_from_env(
            false,
            None,
            Some("xterm-kitty")
        ));
        assert!(!detect_octants_safe_from_env(
            false,
            Some("Apple_Terminal"),
            Some("xterm-256color")
        ));
        assert!(!detect_octants_safe_from_env(false, None, Some("dumb")));
    }
}
