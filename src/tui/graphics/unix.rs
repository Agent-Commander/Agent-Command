use std::{
    mem::zeroed,
    time::{Duration, Instant},
};

use super::{
    parse_terminal_probe_reply, probe_reply_complete, ProbeReply, WindowMetrics, CAPABILITY_QUERY,
};

const MAX_PROBE_BYTES: usize = 512;

struct TermiosGuard {
    fd: libc::c_int,
    original: libc::termios,
}

impl Drop for TermiosGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

pub(super) fn probe_da1_and_cell_px(timeout: Duration) -> Option<ProbeReply> {
    let mut original: libc::termios = unsafe { zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
        return None;
    }

    let _guard = TermiosGuard {
        fd: libc::STDIN_FILENO,
        original,
    };

    let mut raw = original;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
        return None;
    }

    let written = unsafe {
        libc::write(
            libc::STDOUT_FILENO,
            CAPABILITY_QUERY.as_ptr().cast(),
            CAPABILITY_QUERY.len(),
        )
    };
    if written < 0 || written as usize != CAPABILITY_QUERY.len() {
        return None;
    }

    let deadline = Instant::now() + timeout;
    let mut response = Vec::with_capacity(64);
    let mut chunk = [0u8; 64];

    while response.len() < MAX_PROBE_BYTES {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait_ms = (deadline - now)
            .as_millis()
            .min(u128::from(libc::c_int::MAX as u32)) as libc::c_int;
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, wait_ms) };
        if ready <= 0 || (pfd.revents & libc::POLLIN) == 0 {
            break;
        }

        let limit = chunk.len().min(MAX_PROBE_BYTES - response.len());
        let count = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), limit) };
        if count <= 0 {
            break;
        }
        response.extend_from_slice(&chunk[..count as usize]);
        if probe_reply_complete(&response) {
            break;
        }
    }

    if response.is_empty() {
        None
    } else {
        Some(parse_terminal_probe_reply(&response))
    }
}

pub(super) fn window_metrics() -> Option<WindowMetrics> {
    let mut size: libc::winsize = unsafe { zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } != 0 {
        return None;
    }
    Some(WindowMetrics {
        cols: size.ws_col,
        rows: size.ws_row,
        width_px: size.ws_xpixel,
        height_px: size.ws_ypixel,
    })
}
