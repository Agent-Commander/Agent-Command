#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::{
    env,
    io::{self, IsTerminal, Write},
};

const BASE64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const DISABLE_CLIPBOARD_ENV: &str = "AGENT_COMMAND_NO_CLIPBOARD";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyBackend {
    NativeAndOsc52 { native: &'static str },
    NativeOnly { native: &'static str },
    Osc52Only,
}

pub fn sanitize_clipboard_text(text: &str) -> String {
    text.replace('\0', "")
}

pub fn base64_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let triple = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        output.push(BASE64_TABLE[((triple >> 18) & 0x3F) as usize] as char);
        output.push(BASE64_TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            output.push(BASE64_TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(BASE64_TABLE[(triple & 0x3F) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

pub fn encode_osc52(text: &str) -> String {
    let clean = sanitize_clipboard_text(text);
    let encoded = base64_encode(clean.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

pub fn encode_osc52_for_terminal(text: &str, in_tmux: bool, in_screen: bool) -> String {
    let base = encode_osc52(text);
    if in_tmux {
        let escaped = base.replace('\x1b', "\x1b\x1b");
        format!("\x1bPtmux;{escaped}\x1b\\")
    } else if in_screen {
        format!("\x1bP{base}\x1b\\")
    } else {
        base
    }
}

pub fn write_osc52<W: Write>(writer: &mut W, text: &str) -> io::Result<()> {
    let in_tmux = env::var_os("TMUX").is_some();
    let in_screen = !in_tmux
        && env::var("TERM")
            .map(|term| term.starts_with("screen"))
            .unwrap_or(false);
    let seq = encode_osc52_for_terminal(text, in_tmux, in_screen);
    writer.write_all(seq.as_bytes())?;
    writer.flush()
}

fn write_native_platform(text: &str) -> io::Result<&'static str> {
    #[cfg(windows)]
    {
        windows::write_native(text)
    }
    #[cfg(unix)]
    {
        unix::write_native(text)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = text;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native clipboard unavailable on this target",
        ))
    }
}

pub fn copy_text_with<W, F>(
    text: &str,
    osc52_writer: Option<&mut W>,
    native_writer: F,
) -> io::Result<CopyBackend>
where
    W: Write,
    F: FnOnce(&str) -> io::Result<&'static str>,
{
    let clean = sanitize_clipboard_text(text);
    let osc52_ok = match osc52_writer {
        Some(writer) => {
            let seq = encode_osc52(&clean);
            writer
                .write_all(seq.as_bytes())
                .and_then(|()| writer.flush())
                .is_ok()
        }
        None => false,
    };
    let native_result = native_writer(&clean);
    match (native_result, osc52_ok) {
        (Ok(native), true) => Ok(CopyBackend::NativeAndOsc52 { native }),
        (Ok(native), false) => Ok(CopyBackend::NativeOnly { native }),
        (Err(_), true) => Ok(CopyBackend::Osc52Only),
        (Err(error), false) => Err(error),
    }
}

fn osc52_supported_on_stdout() -> bool {
    if !io::stdout().is_terminal() {
        return false;
    }
    env::var("TERM").map(|term| term != "dumb").unwrap_or(true)
}

pub fn copy_text(text: &str) -> io::Result<CopyBackend> {
    let clean = sanitize_clipboard_text(text);
    if env::var_os(DISABLE_CLIPBOARD_ENV).is_some() {
        return Ok(CopyBackend::Osc52Only);
    }
    let osc52_ok = if osc52_supported_on_stdout() {
        let mut stdout = io::stdout().lock();
        write_osc52(&mut stdout, &clean).is_ok()
    } else {
        false
    };
    match (write_native_platform(&clean), osc52_ok) {
        (Ok(native), true) => Ok(CopyBackend::NativeAndOsc52 { native }),
        (Ok(native), false) => Ok(CopyBackend::NativeOnly { native }),
        (Err(_), true) => Ok(CopyBackend::Osc52Only),
        (Err(error), false) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_strips_nul_bytes_and_encodes_utf8() {
        let seq = encode_osc52("hello\0 café λ");
        assert_eq!(
            seq,
            format!("\x1b]52;c;{}\x07", base64_encode("hello café λ".as_bytes()))
        );
        assert!(!sanitize_clipboard_text("a\0b\0c").contains('\0'));
    }

    #[test]
    fn osc52_wraps_for_tmux_and_screen() {
        let tmux = encode_osc52_for_terminal("hi", true, false);
        assert!(tmux.starts_with("\x1bPtmux;\x1b\x1b]52;c;"));
        assert!(tmux.ends_with("\x07\x1b\\"));

        let screen = encode_osc52_for_terminal("hi", false, true);
        assert!(screen.starts_with("\x1bP\x1b]52;c;"));
        assert!(screen.ends_with("\x07\x1b\\"));
    }

    #[test]
    fn dual_copy_writer_handles_all_backend_combinations() {
        let mut osc_buf = Vec::new();
        let mut native_seen = String::new();
        let backend = copy_text_with("a\0b", Some(&mut osc_buf), |clean| {
            native_seen = clean.to_owned();
            Ok("mock-native")
        })
        .unwrap();
        assert_eq!(
            backend,
            CopyBackend::NativeAndOsc52 {
                native: "mock-native"
            }
        );
        assert_eq!(native_seen, "ab");
        assert_eq!(String::from_utf8(osc_buf).unwrap(), encode_osc52("ab"));

        let mut osc_only_buf = Vec::new();
        let backend = copy_text_with("only-osc", Some(&mut osc_only_buf), |_| {
            Err(io::Error::new(io::ErrorKind::NotFound, "missing"))
        })
        .unwrap();
        assert_eq!(backend, CopyBackend::Osc52Only);
        assert_eq!(
            String::from_utf8(osc_only_buf).unwrap(),
            encode_osc52("only-osc")
        );

        let backend =
            copy_text_with::<Vec<u8>, _>("native-only", None, |_| Ok("clip.exe")).unwrap();
        assert_eq!(backend, CopyBackend::NativeOnly { native: "clip.exe" });

        let err = copy_text_with::<Vec<u8>, _>("headless", None, |_| {
            Err(io::Error::new(io::ErrorKind::Unsupported, "headless"))
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
