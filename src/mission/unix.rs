use std::{os::unix::process::CommandExt, process::Command};

pub(super) fn own_group(command: &mut Command) -> &mut Command {
    command.process_group(0)
}

pub(super) fn kill_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    if let Ok(raw) = i32::try_from(pid) {
        unsafe {
            libc::kill(-raw, libc::SIGKILL);
            libc::kill(raw, libc::SIGKILL);
        }
    }
}
