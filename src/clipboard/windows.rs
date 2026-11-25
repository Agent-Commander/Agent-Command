use std::{
    io::{self, Write},
    os::windows::process::CommandExt,
    process::{Command, Stdio},
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn encode_utf16_le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + text.len().saturating_mul(2));
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

fn pipe_to_command(program: &str, args: &[&str], payload: &[u8]) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
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
    let utf16 = encode_utf16_le_with_bom(text);
    if pipe_to_command("clip.exe", &[], &utf16).is_ok() {
        return Ok("clip.exe");
    }
    let ps_script = "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())";
    pipe_to_command(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", ps_script],
        text.as_bytes(),
    )?;
    Ok("powershell")
}
