use std::{
    io::{self, Write},
    process::{Command, Stdio},
};

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

pub(super) fn write_native(text: &str) -> io::Result<&'static str> {
    let candidates: &[(&str, &[&str])] = if std::env::consts::OS == "macos" {
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
