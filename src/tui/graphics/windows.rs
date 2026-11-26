use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    System::{
        Console::{
            GetConsoleMode, GetNumberOfConsoleInputEvents, GetStdHandle, ReadConsoleInputW,
            SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
            ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE,
        },
        Threading::WaitForSingleObject,
    },
};

use super::{parse_terminal_probe_reply, probe_reply_complete, ProbeReply, CAPABILITY_QUERY};

const MAX_PROBE_BYTES: usize = 512;

struct ConsoleModeGuard {
    input: HANDLE,
    input_mode: CONSOLE_MODE,
    output: HANDLE,
    output_mode: CONSOLE_MODE,
}

impl Drop for ConsoleModeGuard {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.input, self.input_mode);
            SetConsoleMode(self.output, self.output_mode);
        }
    }
}

fn valid_handle(handle: HANDLE) -> bool {
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

pub(super) fn probe_da1_and_cell_px(timeout: Duration) -> Option<ProbeReply> {
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if !valid_handle(input) || !valid_handle(output) {
        return None;
    }

    let mut original_in: CONSOLE_MODE = 0;
    let mut original_out: CONSOLE_MODE = 0;
    if unsafe { GetConsoleMode(input, &mut original_in) } == 0
        || unsafe { GetConsoleMode(output, &mut original_out) } == 0
    {
        return None;
    }

    let _guard = ConsoleModeGuard {
        input,
        input_mode: original_in,
        output,
        output_mode: original_out,
    };

    let raw_in = (original_in & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
        | ENABLE_VIRTUAL_TERMINAL_INPUT;
    let vt_out = original_out | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    if unsafe { SetConsoleMode(input, raw_in) } == 0
        || unsafe { SetConsoleMode(output, vt_out) } == 0
    {
        return None;
    }

    let mut stdout = io::stdout().lock();
    if stdout.write_all(CAPABILITY_QUERY).is_err() || stdout.flush().is_err() {
        return None;
    }
    drop(stdout);

    let deadline = Instant::now() + timeout;
    let mut response = Vec::with_capacity(64);
    let mut records = [INPUT_RECORD::default(); 32];

    while response.len() < MAX_PROBE_BYTES {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait_ms = (deadline - now).as_millis().min(u128::from(u32::MAX)) as u32;
        if unsafe { WaitForSingleObject(input, wait_ms) } != WAIT_OBJECT_0 {
            break;
        }

        let mut pending: u32 = 0;
        if unsafe { GetNumberOfConsoleInputEvents(input, &mut pending) } == 0 || pending == 0 {
            continue;
        }

        let to_read = pending.min(records.len() as u32);
        let mut read_count: u32 = 0;
        if unsafe { ReadConsoleInputW(input, records.as_mut_ptr(), to_read, &mut read_count) } == 0
        {
            break;
        }

        for record in &records[..read_count as usize] {
            if u32::from(record.EventType) != KEY_EVENT {
                continue;
            }
            let key = unsafe { record.Event.KeyEvent };
            if key.bKeyDown == 0 {
                continue;
            }
            let code = unsafe { key.uChar.UnicodeChar };
            if code != 0 && code <= 0x7f {
                response.push(code as u8);
            }
        }

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
