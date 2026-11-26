#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use uuid::Uuid;

const BASE64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const DISABLE_CLIPBOARD_ENV: &str = "AGENT_COMMAND_NO_CLIPBOARD";
const DISABLE_OPEN_ENV: &str = "AGENT_COMMAND_NO_OPEN";
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const ZLIB_STORED_BLOCK_MAX: usize = 65_535;
const ATTACHMENTS_SUBDIR: &str = "attachments";

pub const MAX_IMAGE_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyBackend {
    NativeAndOsc52 { native: &'static str },
    NativeOnly { native: &'static str },
    Osc52Only,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardContent {
    ImagePng(Vec<u8>),
    ImageFile(PathBuf),
    Text(String),
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedImageAttachment {
    pub path: PathBuf,
    pub mime_type: String,
    pub label: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenTarget {
    Url(String),
    LocalPath(PathBuf),
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

fn read_native_platform() -> io::Result<ClipboardContent> {
    #[cfg(windows)]
    {
        windows::read_native()
    }
    #[cfg(unix)]
    {
        unix::read_native()
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native clipboard read unavailable on this target",
        ))
    }
}

fn open_target_platform(target: &OpenTarget) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::open_target(target)
    }
    #[cfg(unix)]
    {
        unix::open_target(target)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = target;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "opening external targets is unavailable on this platform",
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

pub fn read_clipboard() -> io::Result<ClipboardContent> {
    if env::var_os(DISABLE_CLIPBOARD_ENV).is_some() {
        return Ok(ClipboardContent::Empty);
    }
    read_native_platform()
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65_521;
    let mut s1 = 1u32;
    let mut s2 = 0u32;
    for &byte in bytes {
        s1 = (s1 + u32::from(byte)) % MOD_ADLER;
        s2 = (s2 + s1) % MOD_ADLER;
    }
    (s2 << 16) | s1
}

fn append_png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) -> io::Result<()> {
    let len = u32::try_from(data.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "PNG chunk exceeds u32 length"))?;
    out.extend_from_slice(&len.to_be_bytes());
    let crc_start = out.len();
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    let crc = crc32_ieee(&out[crc_start..]);
    out.extend_from_slice(&crc.to_be_bytes());
    Ok(())
}

pub fn parse_png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || png[..8] != PNG_SIGNATURE || &png[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    (width > 0 && height > 0).then_some((width, height))
}

pub fn encode_rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> io::Result<Vec<u8>> {
    if width == 0 || height == 0 || width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid PNG image dimensions",
        ));
    }
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Image row size overflow"))?;
    let expected = usize::try_from(height)
        .ok()
        .and_then(|h| h.checked_mul(row_bytes))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Image byte size overflow"))?;
    if rgba.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "RGBA pixel buffer length does not match dimensions",
        ));
    }
    let raw_scanlines_len = usize::try_from(height)
        .ok()
        .and_then(|h| h.checked_mul(row_bytes + 1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Scanline size overflow"))?;
    if raw_scanlines_len > MAX_IMAGE_ATTACHMENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Clipboard image exceeds 20 MiB attachment limit",
        ));
    }

    let mut raw = Vec::with_capacity(raw_scanlines_len);
    for row in rgba.chunks_exact(row_bytes) {
        raw.push(0u8);
        raw.extend_from_slice(row);
    }

    let block_count = raw.len().div_ceil(ZLIB_STORED_BLOCK_MAX).max(1);
    let mut zlib = Vec::with_capacity(2 + raw.len() + block_count * 5 + 4);
    zlib.push(0x78);
    zlib.push(0x01);
    if raw.is_empty() {
        zlib.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    } else {
        let mut chunks = raw.chunks(ZLIB_STORED_BLOCK_MAX).peekable();
        while let Some(chunk) = chunks.next() {
            let is_last = chunks.peek().is_none();
            let block_len = chunk.len() as u16;
            zlib.push(u8::from(is_last));
            zlib.extend_from_slice(&block_len.to_le_bytes());
            zlib.extend_from_slice(&(!block_len).to_le_bytes());
            zlib.extend_from_slice(chunk);
        }
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = Vec::with_capacity(64 + zlib.len());
    png.extend_from_slice(&PNG_SIGNATURE);
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = 6;
    ihdr[10] = 0;
    ihdr[11] = 0;
    ihdr[12] = 0;
    append_png_chunk(&mut png, b"IHDR", &ihdr)?;
    append_png_chunk(&mut png, b"IDAT", &zlib)?;
    append_png_chunk(&mut png, b"IEND", &[])?;

    if png.len() > MAX_IMAGE_ATTACHMENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Encoded PNG exceeds 20 MiB attachment limit",
        ));
    }
    Ok(png)
}

fn read_u16_le(bytes: &[u8], offset: usize) -> io::Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Truncated DIB header"))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Truncated DIB header"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_i32_le(bytes: &[u8], offset: usize) -> io::Result<i32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Truncated DIB header"))?;
    Ok(i32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn extract_masked_channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max_val = mask >> shift;
    if max_val == 0 {
        return 0;
    }
    let raw = (pixel & mask) >> shift;
    if max_val == 255 {
        raw as u8
    } else {
        ((u64::from(raw) * 255 + u64::from(max_val) / 2) / u64::from(max_val)) as u8
    }
}

pub fn decode_dib_to_rgba(dib: &[u8]) -> io::Result<(u32, u32, Vec<u8>)> {
    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;

    if dib.len() < 40 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Clipboard DIB is shorter than BITMAPINFOHEADER",
        ));
    }
    let header_size = read_u32_le(dib, 0)? as usize;
    if header_size < 40 || header_size > dib.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Clipboard DIB header size is invalid",
        ));
    }
    let raw_width = read_i32_le(dib, 4)?;
    let raw_height = read_i32_le(dib, 8)?;
    let planes = read_u16_le(dib, 12)?;
    let bit_count = read_u16_le(dib, 14)?;
    let compression = read_u32_le(dib, 16)?;
    let clr_used = read_u32_le(dib, 32)? as usize;

    if raw_width <= 0 || raw_height == 0 || planes != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unsupported clipboard DIB geometry",
        ));
    }
    let width = raw_width as u32;
    let top_down = raw_height < 0;
    let height = raw_height.unsigned_abs();
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Clipboard DIB dimensions exceed limit",
        ));
    }
    if !matches!(bit_count, 24 | 32) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unsupported clipboard DIB bit depth: {bit_count}"),
        ));
    }
    if compression != BI_RGB && !(compression == BI_BITFIELDS && bit_count == 32) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unsupported clipboard DIB compression: {compression}"),
        ));
    }

    let (masks, extra_mask_bytes) = if compression == BI_BITFIELDS {
        if header_size >= 56 {
            (
                Some((
                    read_u32_le(dib, 40)?,
                    read_u32_le(dib, 44)?,
                    read_u32_le(dib, 48)?,
                    read_u32_le(dib, 52)?,
                )),
                0usize,
            )
        } else if header_size == 40 {
            (
                Some((
                    read_u32_le(dib, 40)?,
                    read_u32_le(dib, 44)?,
                    read_u32_le(dib, 48)?,
                    0u32,
                )),
                12usize,
            )
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unsupported DIB bitfield header size",
            ));
        }
    } else {
        (None, 0usize)
    };

    let color_table_bytes = clr_used
        .checked_mul(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "DIB color table overflow"))?;
    let pixel_offset = header_size
        .checked_add(extra_mask_bytes)
        .and_then(|v| v.checked_add(color_table_bytes))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "DIB pixel offset overflow"))?;

    let bytes_per_pixel = usize::from(bit_count / 8);
    let raw_stride = (width as usize)
        .checked_mul(bytes_per_pixel)
        .and_then(|v| v.checked_add(3))
        .map(|v| v & !3)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "DIB stride overflow"))?;
    let required_pixels = raw_stride
        .checked_mul(height as usize)
        .and_then(|v| v.checked_add(pixel_offset))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "DIB buffer size overflow"))?;
    if required_pixels > dib.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Clipboard DIB pixel payload is truncated",
        ));
    }

    let rgba_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|v| v.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "RGBA output overflow"))?;
    if rgba_len > MAX_IMAGE_ATTACHMENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Clipboard image exceeds 20 MiB attachment limit",
        ));
    }

    let mut rgba = vec![0u8; rgba_len];
    let mut saw_nonzero_alpha = false;
    for dst_y in 0..(height as usize) {
        let src_y = if top_down {
            dst_y
        } else {
            (height as usize) - 1 - dst_y
        };
        let src_row_start = pixel_offset + src_y * raw_stride;
        let dst_row_start = dst_y * (width as usize) * 4;
        for x in 0..(width as usize) {
            let src = src_row_start + x * bytes_per_pixel;
            let dst = dst_row_start + x * 4;
            if bit_count == 24 {
                rgba[dst] = dib[src + 2];
                rgba[dst + 1] = dib[src + 1];
                rgba[dst + 2] = dib[src];
                rgba[dst + 3] = 255;
                saw_nonzero_alpha = true;
            } else if let Some((rm, gm, bm, am)) = masks {
                let px = u32::from_le_bytes([dib[src], dib[src + 1], dib[src + 2], dib[src + 3]]);
                let a = if am == 0 {
                    255
                } else {
                    extract_masked_channel(px, am)
                };
                if a != 0 {
                    saw_nonzero_alpha = true;
                }
                rgba[dst] = extract_masked_channel(px, rm);
                rgba[dst + 1] = extract_masked_channel(px, gm);
                rgba[dst + 2] = extract_masked_channel(px, bm);
                rgba[dst + 3] = a;
            } else {
                let a = dib[src + 3];
                if a != 0 {
                    saw_nonzero_alpha = true;
                }
                rgba[dst] = dib[src + 2];
                rgba[dst + 1] = dib[src + 1];
                rgba[dst + 2] = dib[src];
                rgba[dst + 3] = a;
            }
        }
    }

    if !saw_nonzero_alpha {
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }

    Ok((width, height, rgba))
}

pub fn dib_to_png(dib: &[u8]) -> io::Result<Vec<u8>> {
    let (width, height, rgba) = decode_dib_to_rgba(dib)?;
    encode_rgba_to_png(width, height, &rgba)
}

pub fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

pub fn detect_image_mime_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes[..8] == PNG_SIGNATURE {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

pub fn detect_pasted_image_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') || trimmed.contains('\r') {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    let lower = unquoted.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
    {
        return None;
    }
    let normalized = if cfg!(unix) {
        unquoted.replace("\\ ", " ")
    } else {
        unquoted.to_owned()
    };
    let path = PathBuf::from(&normalized);
    if image_mime_for_path(&path).is_some() && path.is_file() {
        Some(path)
    } else {
        None
    }
}

fn ensure_private_attachments_dir(base_root: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(root) = base_root {
        return crate::auth::private_state_directory(root, ATTACHMENTS_SUBDIR);
    }
    if let Ok(root) = crate::auth::root() {
        if let Ok(dir) = crate::auth::private_state_directory(&root, ATTACHMENTS_SUBDIR) {
            return Ok(dir);
        }
    }
    let fallback_root = env::temp_dir().join("agent-command-state");
    crate::auth::private_state_directory(&fallback_root, ATTACHMENTS_SUBDIR)
}

fn write_private_attachment_atomic(
    dir: &Path,
    filename: &str,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let dest = dir.join(filename);
    let staging = dir.join(format!(".{filename}.{}.tmp", Uuid::new_v4().simple()));
    let mut file = create_private_file(&staging)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&staging, &dest)?;
    Ok(dest)
}

fn create_private_file(path: &Path) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
    }
}

pub fn save_clipboard_png(
    png_bytes: &[u8],
    base_root: Option<&Path>,
) -> io::Result<SavedImageAttachment> {
    if png_bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Clipboard PNG payload is empty",
        ));
    }
    if png_bytes.len() > MAX_IMAGE_ATTACHMENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Clipboard image exceeds 20 MiB attachment limit",
        ));
    }
    let Some((width, height)) = parse_png_dimensions(png_bytes) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Clipboard image payload is not a valid PNG",
        ));
    };
    let dir = ensure_private_attachments_dir(base_root)?;
    let filename = format!("clipboard-{}.png", Uuid::new_v4().simple());
    let path = write_private_attachment_atomic(&dir, &filename, png_bytes)?;
    Ok(SavedImageAttachment {
        path,
        mime_type: "image/png".into(),
        label: format!("{width}×{height} png"),
        width: Some(width),
        height: Some(height),
    })
}

pub fn stage_image_file(
    source: &Path,
    base_root: Option<&Path>,
) -> io::Result<SavedImageAttachment> {
    let ext_mime = image_mime_for_path(source).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unsupported image extension (expected png, jpg, jpeg, webp, or gif)",
        )
    })?;
    let meta = fs::metadata(source)?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Image attachment path is not a regular file",
        ));
    }
    if meta.len() > MAX_IMAGE_ATTACHMENT_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Image file exceeds 20 MiB attachment limit",
        ));
    }
    let bytes = fs::read(source)?;
    if bytes.len() > MAX_IMAGE_ATTACHMENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Image file exceeds 20 MiB attachment limit",
        ));
    }
    let detected_mime = detect_image_mime_from_bytes(&bytes).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "File header does not match a supported raster image format",
        )
    })?;
    let mime_type = detected_mime.to_owned();
    let _ = ext_mime;
    let ext = match detected_mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    };
    let dimensions = if detected_mime == "image/png" {
        parse_png_dimensions(&bytes)
    } else {
        None
    };
    let dir = ensure_private_attachments_dir(base_root)?;
    let filename = format!("image-{}.{ext}", Uuid::new_v4().simple());
    let staged_path = write_private_attachment_atomic(&dir, &filename, &bytes)?;
    let label = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| filename.clone());
    Ok(SavedImageAttachment {
        path: staged_path,
        mime_type,
        label,
        width: dimensions.map(|(w, _)| w),
        height: dimensions.map(|(_, h)| h),
    })
}

fn strip_line_col_suffix(raw: &str) -> &str {
    let without_hash = if let Some(hash_pos) = raw.rfind("#L") {
        let tail = &raw[hash_pos + 2..];
        let valid_hash = !tail.is_empty()
            && tail
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, 'C' | 'c' | '-' | 'L' | 'l'));
        if valid_hash {
            &raw[..hash_pos]
        } else {
            raw
        }
    } else {
        raw
    };
    let mut candidate = without_hash;
    for _ in 0..2 {
        if let Some(colon) = candidate.rfind(':') {
            let tail = &candidate[colon + 1..];
            if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) && colon > 1 {
                candidate = &candidate[..colon];
                continue;
            }
        }
        break;
    }
    candidate
}

fn parse_file_url(url: &str) -> Option<PathBuf> {
    let rest = url
        .strip_prefix("file://")
        .or_else(|| url.strip_prefix("FILE://"))?;
    let without_host = rest
        .strip_prefix("localhost/")
        .map(|tail| format!("/{tail}"))
        .unwrap_or_else(|| rest.to_owned());
    let path_only = without_host
        .split(['#', '?'])
        .next()
        .unwrap_or(&without_host);
    if path_only.is_empty() {
        return None;
    }
    let decoded = percent_decode_path(path_only);
    let cleaned = if decoded.len() >= 3
        && decoded.starts_with('/')
        && decoded.as_bytes()[1].is_ascii_alphabetic()
        && decoded.as_bytes()[2] == b':'
    {
        &decoded[1..]
    } else {
        decoded.as_str()
    };
    Some(PathBuf::from(cleaned))
}

fn percent_decode_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn sanitize_open_target(target: &str, workspace: Option<&Path>) -> io::Result<OpenTarget> {
    let trimmed = target.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|c| c == '\0' || c == '\r' || c == '\n' || c.is_control())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Target contains control characters or is empty",
        ));
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
    {
        if trimmed.chars().any(char::is_whitespace) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "URL target cannot contain whitespace",
            ));
        }
        return Ok(OpenTarget::Url(trimmed.to_owned()));
    }
    if lower.starts_with("file://") {
        let local = parse_file_url(trimmed)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Malformed file:// link"))?;
        let canonical = fs::canonicalize(&local)?;
        return Ok(OpenTarget::LocalPath(canonical));
    }
    if lower.contains("://")
        || lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("vbscript:")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unsupported link scheme",
        ));
    }

    let raw_path = strip_line_col_suffix(trimmed);
    let candidate = if let Some(rest) = raw_path
        .strip_prefix("~/")
        .or_else(|| raw_path.strip_prefix("~\\"))
    {
        let home = env::home_dir().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Cannot resolve home directory")
        })?;
        home.join(rest)
    } else {
        let p = PathBuf::from(raw_path);
        if p.is_absolute() {
            p
        } else if let Some(ws) = workspace {
            ws.join(p)
        } else {
            p
        }
    };
    let canonical = fs::canonicalize(&candidate)?;
    Ok(OpenTarget::LocalPath(canonical))
}

pub fn open_external(target: &str, workspace: Option<&Path>) -> io::Result<OpenTarget> {
    let validated = sanitize_open_target(target, workspace)?;
    if env::var_os(DISABLE_OPEN_ENV).is_some() {
        return Ok(validated);
    }
    open_target_platform(&validated)?;
    Ok(validated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_dib_24(width: u32, height: i32, bgr_pixels: &[(u8, u8, u8)]) -> Vec<u8> {
        let abs_h = height.unsigned_abs() as usize;
        let stride = (width as usize * 3 + 3) & !3;
        let mut dib = vec![0u8; 40 + stride * abs_h];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&(width as i32).to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        dib[16..20].copy_from_slice(&0u32.to_le_bytes());
        for y in 0..abs_h {
            for x in 0..(width as usize) {
                let (b, g, r) = bgr_pixels[y * (width as usize) + x];
                let off = 40 + y * stride + x * 3;
                dib[off] = b;
                dib[off + 1] = g;
                dib[off + 2] = r;
            }
        }
        dib
    }

    fn build_test_dib_32_bitfields(width: u32, height: i32, pixels: &[u32]) -> Vec<u8> {
        let abs_h = height.unsigned_abs() as usize;
        let stride = width as usize * 4;
        let mut dib = vec![0u8; 124 + stride * abs_h];
        dib[0..4].copy_from_slice(&124u32.to_le_bytes());
        dib[4..8].copy_from_slice(&(width as i32).to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&3u32.to_le_bytes());
        dib[40..44].copy_from_slice(&0x00FF_0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x0000_FF00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
        dib[52..56].copy_from_slice(&0xFF00_0000u32.to_le_bytes());
        for y in 0..abs_h {
            for x in 0..(width as usize) {
                let off = 124 + y * stride + x * 4;
                dib[off..off + 4].copy_from_slice(&pixels[y * (width as usize) + x].to_le_bytes());
            }
        }
        dib
    }

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

    #[test]
    fn dib_24_and_dibv5_bitfields_decode_and_encode_valid_png() {
        let dib24 = build_test_dib_24(
            2,
            2,
            &[(10, 20, 30), (40, 50, 60), (70, 80, 90), (100, 110, 120)],
        );
        let (w, h, rgba) = decode_dib_to_rgba(&dib24).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&rgba[0..4], &[90, 80, 70, 255]);
        assert_eq!(&rgba[4..8], &[120, 110, 100, 255]);
        assert_eq!(&rgba[8..12], &[30, 20, 10, 255]);
        assert_eq!(&rgba[12..16], &[60, 50, 40, 255]);

        let png = dib_to_png(&dib24).unwrap();
        assert_eq!(&png[..8], &PNG_SIGNATURE);
        assert_eq!(parse_png_dimensions(&png), Some((2, 2)));
        assert_eq!(detect_image_mime_from_bytes(&png), Some("image/png"));
        assert_eq!(
            &png[png.len() - 12..png.len() - 4],
            &[0, 0, 0, 0, b'I', b'E', b'N', b'D']
        );

        let dib32 = build_test_dib_32_bitfields(2, -1, &[0x0011_2233, 0x0044_5566]);
        let (w32, h32, rgba32) = decode_dib_to_rgba(&dib32).unwrap();
        assert_eq!((w32, h32), (2, 1));
        assert_eq!(&rgba32[0..4], &[0x11, 0x22, 0x33, 255]);
        assert_eq!(&rgba32[4..8], &[0x44, 0x55, 0x66, 255]);
    }

    #[test]
    fn save_clipboard_png_and_stage_image_file_persist_privately_and_reject_bad_inputs() {
        let base = env::temp_dir().join(format!("ac-clip-test-{}", Uuid::new_v4().simple()));
        let png = encode_rgba_to_png(3, 2, &[0xAA; 3 * 2 * 4]).unwrap();
        let saved = save_clipboard_png(&png, Some(&base)).unwrap();
        assert!(saved.path.is_file());
        assert_eq!(saved.mime_type, "image/png");
        assert_eq!(saved.width, Some(3));
        assert_eq!(saved.height, Some(2));
        assert_eq!(saved.label, "3×2 png");

        let staged = stage_image_file(&saved.path, Some(&base)).unwrap();
        assert!(staged.path.is_file());
        assert_eq!(staged.mime_type, "image/png");
        assert_eq!(staged.width, Some(3));

        let fake_png = base.join("fake.png");
        fs::write(&fake_png, b"not a real png").unwrap();
        assert!(stage_image_file(&fake_png, Some(&base)).is_err());
        assert!(save_clipboard_png(b"not a png", Some(&base)).is_err());

        let quoted = format!("\"{}\"", saved.path.display());
        assert_eq!(detect_pasted_image_path(&quoted), Some(saved.path.clone()));
        assert_eq!(detect_pasted_image_path("https://example.com/a.png"), None);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn sanitize_open_target_validates_urls_and_workspace_paths_with_line_suffixes() {
        let ws = env::temp_dir().join(format!("ac-open-test-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(ws.join("src")).unwrap();
        let file = ws.join("src").join("demo.rs");
        fs::write(&file, "fn main() {}\n").unwrap();

        assert_eq!(
            sanitize_open_target("https://example.com/docs?q=1", Some(&ws)).unwrap(),
            OpenTarget::Url("https://example.com/docs?q=1".into())
        );
        assert_eq!(
            sanitize_open_target("mailto:dev@example.com", Some(&ws)).unwrap(),
            OpenTarget::Url("mailto:dev@example.com".into())
        );
        assert!(sanitize_open_target("javascript:alert(1)", Some(&ws)).is_err());
        assert!(sanitize_open_target("https://example.com\ncalc.exe", Some(&ws)).is_err());

        let resolved = sanitize_open_target("src/demo.rs:42:7", Some(&ws)).unwrap();
        assert_eq!(
            resolved,
            OpenTarget::LocalPath(fs::canonicalize(&file).unwrap())
        );

        let resolved_hash = sanitize_open_target("src/demo.rs#L10C2", Some(&ws)).unwrap();
        assert_eq!(
            resolved_hash,
            OpenTarget::LocalPath(fs::canonicalize(&file).unwrap())
        );

        let _ = fs::remove_dir_all(&ws);
    }
}
