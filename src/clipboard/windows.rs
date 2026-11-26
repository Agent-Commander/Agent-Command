use std::{
    ffi::OsString,
    io::{self, Write},
    os::windows::{ffi::OsStringExt, process::CommandExt},
    path::PathBuf,
    process::{Command, Stdio},
    ptr, slice, thread,
    time::Duration,
};

use windows_sys::Win32::{
    System::{
        DataExchange::{
            CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
            RegisterClipboardFormatW,
        },
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
    },
    UI::Shell::{DragQueryFileW, HDROP},
};

use super::{
    dib_to_png, image_mime_for_path, ClipboardContent, OpenTarget, MAX_IMAGE_ATTACHMENT_BYTES,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;
const SW_SHOWNORMAL: i32 = 1;
const OPEN_CLIPBOARD_ATTEMPTS: usize = 5;
const OPEN_CLIPBOARD_RETRY_DELAY: Duration = Duration::from_millis(5);

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: *mut core::ffi::c_void,
        lp_operation: *const u16,
        lp_file: *const u16,
        lp_parameters: *const u16,
        lp_directory: *const u16,
        n_show_cmd: i32,
    ) -> isize;
}

fn to_wide_nul(text: &str) -> Vec<u16> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    wide
}

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

struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> io::Result<Self> {
        for attempt in 0..OPEN_CLIPBOARD_ATTEMPTS {
            let opened = unsafe { OpenClipboard(ptr::null_mut()) };
            if opened != 0 {
                return Ok(Self);
            }
            if attempt + 1 < OPEN_CLIPBOARD_ATTEMPTS {
                thread::sleep(OPEN_CLIPBOARD_RETRY_DELAY);
            }
        }
        Err(io::Error::last_os_error())
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}

struct GlobalLockGuard {
    handle: *mut core::ffi::c_void,
    ptr: *const u8,
    len: usize,
}

impl GlobalLockGuard {
    fn lock(handle: *mut core::ffi::c_void) -> Option<Self> {
        if handle.is_null() {
            return None;
        }
        let len = unsafe { GlobalSize(handle) };
        if len == 0 {
            return None;
        }
        let locked = unsafe { GlobalLock(handle) };
        if locked.is_null() {
            return None;
        }
        Some(Self {
            handle,
            ptr: locked.cast::<u8>(),
            len,
        })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        unsafe {
            GlobalUnlock(self.handle);
        }
    }
}

fn read_format_bytes(format: u32) -> Option<Vec<u8>> {
    if format == 0 || unsafe { IsClipboardFormatAvailable(format) } == 0 {
        return None;
    }
    let handle = unsafe { GetClipboardData(format) };
    let guard = GlobalLockGuard::lock(handle)?;
    if guard.len > MAX_IMAGE_ATTACHMENT_BYTES.saturating_mul(2) {
        return None;
    }
    Some(guard.as_slice().to_vec())
}

fn read_hdrop_image_file() -> Option<PathBuf> {
    if unsafe { IsClipboardFormatAvailable(CF_HDROP) } == 0 {
        return None;
    }
    let handle = unsafe { GetClipboardData(CF_HDROP) };
    if handle.is_null() {
        return None;
    }
    let hdrop = handle as HDROP;
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, ptr::null_mut(), 0) };
    for idx in 0..count {
        let len = unsafe { DragQueryFileW(hdrop, idx, ptr::null_mut(), 0) };
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let written = unsafe { DragQueryFileW(hdrop, idx, buf.as_mut_ptr(), buf.len() as u32) };
        if written == 0 {
            continue;
        }
        buf.truncate(written as usize);
        let path = PathBuf::from(OsString::from_wide(&buf));
        if image_mime_for_path(&path).is_some() && path.is_file() {
            return Some(path);
        }
    }
    None
}

fn read_unicode_text() -> Option<String> {
    let bytes = read_format_bytes(CF_UNICODETEXT)?;
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for &[lo, hi] in bytes.as_chunks::<2>().0 {
        let code = u16::from_le_bytes([lo, hi]);
        if code == 0 {
            break;
        }
        units.push(code);
    }
    if units.is_empty() {
        return None;
    }
    Some(String::from_utf16_lossy(&units))
}

pub(super) fn read_native() -> io::Result<ClipboardContent> {
    let _guard = ClipboardGuard::open()?;

    let png_name = to_wide_nul("PNG");
    let png_format = unsafe { RegisterClipboardFormatW(png_name.as_ptr()) };
    if let Some(png) = read_format_bytes(png_format) {
        if png.len() <= MAX_IMAGE_ATTACHMENT_BYTES && super::parse_png_dimensions(&png).is_some() {
            return Ok(ClipboardContent::ImagePng(png));
        }
    }

    for dib_format in [CF_DIBV5, CF_DIB] {
        if let Some(dib) = read_format_bytes(dib_format) {
            if let Ok(png) = dib_to_png(&dib) {
                return Ok(ClipboardContent::ImagePng(png));
            }
        }
    }

    if let Some(path) = read_hdrop_image_file() {
        return Ok(ClipboardContent::ImageFile(path));
    }

    if let Some(text) = read_unicode_text() {
        return Ok(ClipboardContent::Text(text));
    }

    Ok(ClipboardContent::Empty)
}

pub(super) fn open_target(target: &OpenTarget) -> io::Result<()> {
    let wide_verb = to_wide_nul("open");
    let wide_target = match target {
        OpenTarget::Url(url) => to_wide_nul(url),
        OpenTarget::LocalPath(path) => {
            let mut wide: Vec<u16> =
                std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str()).collect();
            wide.push(0);
            wide
        }
    };
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            wide_verb.as_ptr(),
            wide_target.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ShellExecuteW failed with code {result}"
        )))
    }
}
