use std::{
    env, fs,
    io::{self, Write},
    path::Path,
    process::{Command, Stdio},
};

use uuid::Uuid;

use super::{parse_png_dimensions, ClipboardContent, OpenTarget, MAX_IMAGE_ATTACHMENT_BYTES};

fn pipe_to_command(program: &str, args: &[&str], payload: &[u8]) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload)?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{program} exited with status {status}"
        )))
    }
}

fn capture_command_stdout(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    if output.stdout.len() > MAX_IMAGE_ATTACHMENT_BYTES {
        return None;
    }
    Some(output.stdout)
}

fn read_macos_clipboard_png() -> Option<Vec<u8>> {
    if let Some(bytes) = capture_command_stdout("pngpaste", &["-"]) {
        if parse_png_dimensions(&bytes).is_some() {
            return Some(bytes);
        }
    }
    let tmp = env::temp_dir().join(format!("ac-clipboard-{}.png", Uuid::new_v4().simple()));
    let path_str = tmp.to_string_lossy().replace('"', "\\\"");
    let script = format!(
        "set imageData to the clipboard as \"PNGf\"\n\
         set fileRef to open for access POSIX file \"{path_str}\" with write permission\n\
         set eof fileRef to 0\n\
         write imageData to fileRef\n\
         close access fileRef"
    );
    let status = Command::new("osascript")
        .args(["-e", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok();
    let result = if status.is_some_and(|s| s.success()) {
        fs::read(&tmp)
            .ok()
            .filter(|b| b.len() <= MAX_IMAGE_ATTACHMENT_BYTES && parse_png_dimensions(b).is_some())
    } else {
        None
    };
    let _ = fs::remove_file(&tmp);
    result
}

pub(super) fn write_native(text: &str) -> io::Result<&'static str> {
    let candidates: &[(&str, &[&str])] = if env::consts::OS == "macos" {
        &[("pbcopy", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    let mut last_error = io::Error::new(io::ErrorKind::NotFound, "no native clipboard utility");
    for (program, args) in candidates {
        match pipe_to_command(program, args, text.as_bytes()) {
            Ok(()) => return Ok(*program),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

pub(super) fn read_native() -> io::Result<ClipboardContent> {
    if env::consts::OS == "macos" {
        if let Some(png) = read_macos_clipboard_png() {
            return Ok(ClipboardContent::ImagePng(png));
        }
        if let Some(bytes) = capture_command_stdout("pbpaste", &[]) {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if !text.is_empty() {
                return Ok(ClipboardContent::Text(text));
            }
        }
        return Ok(ClipboardContent::Empty);
    }

    let wayland = env::var_os("WAYLAND_DISPLAY").is_some();
    if wayland {
        if let Some(png) = capture_command_stdout("wl-paste", &["-t", "image/png"]) {
            if parse_png_dimensions(&png).is_some() {
                return Ok(ClipboardContent::ImagePng(png));
            }
        }
    }
    if let Some(png) = capture_command_stdout(
        "xclip",
        &["-selection", "clipboard", "-t", "image/png", "-o"],
    ) {
        if parse_png_dimensions(&png).is_some() {
            return Ok(ClipboardContent::ImagePng(png));
        }
    }

    let text_candidates: &[(&str, &[&str])] = if wayland {
        &[
            ("wl-paste", &["-n"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("xsel", &["--clipboard", "--output"]),
        ]
    } else {
        &[
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("xsel", &["--clipboard", "--output"]),
            ("wl-paste", &["-n"]),
        ]
    };
    for (program, args) in text_candidates {
        if let Some(bytes) = capture_command_stdout(program, args) {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if !text.is_empty() {
                return Ok(ClipboardContent::Text(text));
            }
        }
    }
    Ok(ClipboardContent::Empty)
}

fn spawn_detached(program: &str, arg: &Path) -> io::Result<()> {
    Command::new(program)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

pub(super) fn open_target(target: &OpenTarget) -> io::Result<()> {
    let program = if env::consts::OS == "macos" {
        "open"
    } else {
        "xdg-open"
    };
    match target {
        OpenTarget::Url(url) => spawn_detached(program, Path::new(url)),
        OpenTarget::LocalPath(path) => spawn_detached(program, path),
    }
}
