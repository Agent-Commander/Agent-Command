use std::{env, io::IsTerminal, time::Duration};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub const GRAPHICS_ENV: &str = "AGENT_COMMAND_GRAPHICS";
pub const DEFAULT_CELL_PX: (u16, u16) = (10, 20);
const PROBE_TIMEOUT: Duration = Duration::from_millis(60);
const MIN_CELL_WIDTH: u16 = 4;
const MAX_CELL_WIDTH: u16 = 64;
const MIN_CELL_HEIGHT: u16 = 8;
const MAX_CELL_HEIGHT: u16 = 128;
pub(crate) const CAPABILITY_QUERY: &[u8] = b"\x1b[16t\x1b[c";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GraphicsMode {
    #[default]
    Auto,
    Sixel,
    Unicode,
    Ascii,
}

impl GraphicsMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "sixel" => Some(Self::Sixel),
            "unicode" => Some(Self::Unicode),
            "ascii" => Some(Self::Ascii),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphicsCaps {
    pub sixel: bool,
    pub cell_px: (u16, u16),
    pub octants_safe: bool,
    pub mode: GraphicsMode,
}

impl Default for GraphicsCaps {
    fn default() -> Self {
        Self {
            sixel: false,
            cell_px: DEFAULT_CELL_PX,
            octants_safe: false,
            mode: GraphicsMode::Auto,
        }
    }
}

impl GraphicsCaps {
    pub fn use_sixel(self) -> bool {
        match self.mode {
            GraphicsMode::Auto => self.sixel,
            GraphicsMode::Sixel => true,
            GraphicsMode::Unicode | GraphicsMode::Ascii => false,
        }
    }

    pub fn use_octants(self) -> bool {
        match self.mode {
            GraphicsMode::Ascii => false,
            GraphicsMode::Auto | GraphicsMode::Sixel | GraphicsMode::Unicode => self.octants_safe,
        }
    }

    pub fn use_ascii(self) -> bool {
        self.mode == GraphicsMode::Ascii
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProbeReply {
    pub sixel: bool,
    pub cell_px: Option<(u16, u16)>,
}

pub fn parse_terminal_probe_reply(bytes: &[u8]) -> ProbeReply {
    let mut reply = ProbeReply::default();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let body_start = index + 2;
        if let Some((end, body)) = scan_csi(bytes, body_start) {
            if let Some(sixel) = parse_da1_sixel(body) {
                reply.sixel |= sixel;
            }
            if let Some(cell_px) = parse_csi_16t_cell_px(body) {
                reply.cell_px = Some(cell_px);
            }
            index = end;
        } else {
            index += 2;
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

fn parse_da1_sixel(body: &[u8]) -> Option<bool> {
    let inner = body.strip_prefix(b"?")?.strip_suffix(b"c")?;
    let mut params = parse_u16_params(inner)?;
    let _device_class = params.next()?;
    Some(params.any(|attr| attr == 4))
}

fn parse_csi_16t_cell_px(body: &[u8]) -> Option<(u16, u16)> {
    let inner = body.strip_suffix(b"t")?;
    let mut params = parse_u16_params(inner)?;
    if params.next()? != 6 {
        return None;
    }
    let height = params.next()?;
    let width = params.next()?;
    if params.next().is_some() {
        return None;
    }
    if !(MIN_CELL_WIDTH..=MAX_CELL_WIDTH).contains(&width)
        || !(MIN_CELL_HEIGHT..=MAX_CELL_HEIGHT).contains(&height)
    {
        return None;
    }
    Some((width, height))
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

pub fn resolve_graphics_caps(
    env_override: Option<&str>,
    wt_session: bool,
    term_program: Option<&str>,
    term: Option<&str>,
    probe: Option<ProbeReply>,
) -> GraphicsCaps {
    let mode = env_override
        .and_then(GraphicsMode::parse)
        .unwrap_or(GraphicsMode::Auto);
    let octants_safe = detect_octants_safe_from_env(wt_session, term_program, term);
    let sixel = match mode {
        GraphicsMode::Sixel => true,
        GraphicsMode::Unicode | GraphicsMode::Ascii => false,
        GraphicsMode::Auto => probe.is_some_and(|reply| reply.sixel),
    };
    let cell_px = probe
        .and_then(|reply| reply.cell_px)
        .unwrap_or(DEFAULT_CELL_PX);
    GraphicsCaps {
        sixel,
        cell_px,
        octants_safe,
        mode,
    }
}

pub fn probe_terminal_graphics() -> GraphicsCaps {
    let env_override = env::var(GRAPHICS_ENV).ok();
    let wt_session = env::var_os("WT_SESSION").is_some_and(|value| !value.is_empty());
    let term_program = env::var("TERM_PROGRAM").ok();
    let term = env::var("TERM").ok();
    let mode = env_override
        .as_deref()
        .and_then(GraphicsMode::parse)
        .unwrap_or(GraphicsMode::Auto);
    let probe = if matches!(mode, GraphicsMode::Unicode | GraphicsMode::Ascii)
        || !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
    {
        None
    } else {
        probe_platform(PROBE_TIMEOUT)
    };
    resolve_graphics_caps(
        env_override.as_deref(),
        wt_session,
        term_program.as_deref(),
        term.as_deref(),
        probe,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_da1_sixel_and_csi_16t_cell_dimensions() {
        let reply =
            parse_terminal_probe_reply(b"\x1b[6;20;10t\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c");
        assert!(reply.sixel);
        assert_eq!(reply.cell_px, Some((10, 20)));

        let no_sixel = parse_terminal_probe_reply(b"\x1b[6;18;9t\x1b[?62;1;2;6;22c");
        assert!(!no_sixel.sixel);
        assert_eq!(no_sixel.cell_px, Some((9, 18)));

        let only_da1 = parse_terminal_probe_reply(b"\x1b[?62;4c");
        assert!(only_da1.sixel);
        assert_eq!(only_da1.cell_px, None);
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
    }

    #[test]
    fn resolves_env_override_and_terminal_capabilities() {
        let wt_sixel = resolve_graphics_caps(
            None,
            true,
            None,
            Some("dumb"),
            Some(ProbeReply {
                sixel: true,
                cell_px: Some((10, 20)),
            }),
        );
        assert!(wt_sixel.use_sixel());
        assert!(wt_sixel.use_octants());
        assert_eq!(wt_sixel.cell_px, (10, 20));
        assert_eq!(wt_sixel.mode, GraphicsMode::Auto);

        let forced_unicode = resolve_graphics_caps(
            Some("UNICODE"),
            true,
            None,
            None,
            Some(ProbeReply {
                sixel: true,
                cell_px: Some((12, 24)),
            }),
        );
        assert!(!forced_unicode.use_sixel());
        assert!(forced_unicode.use_octants());
        assert_eq!(forced_unicode.mode, GraphicsMode::Unicode);

        let forced_ascii = resolve_graphics_caps(Some("ascii"), true, None, None, None);
        assert!(!forced_ascii.use_sixel());
        assert!(!forced_ascii.use_octants());
        assert!(forced_ascii.use_ascii());

        let forced_sixel = resolve_graphics_caps(Some("sixel"), false, None, None, None);
        assert!(forced_sixel.use_sixel());
        assert!(!forced_sixel.use_octants());
        assert_eq!(forced_sixel.cell_px, DEFAULT_CELL_PX);

        let classic_conhost = resolve_graphics_caps(None, false, None, None, None);
        assert!(!classic_conhost.use_sixel());
        assert!(!classic_conhost.use_octants());
        assert!(!classic_conhost.use_ascii());
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
