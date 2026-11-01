//! Keeps the receiver's log from growing without end.
//!
//! The log is simply this process's stdout, which pcmlink-ctl appends to a file. A
//! receiver runs for days and nothing else ever trims the file, so it trims itself:
//! past the limit it keeps the newest half. It looks only when it has just printed,
//! on the main thread, which is also where nearly all of its printing happens.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::mem::ManuallyDrop;
use std::path::PathBuf;

/// Each receiver's share of the budget for all of pcmlink's logs.
const LIMIT: u64 = 20 * 1024 * 1024;

pub fn keep_within_budget() {
    trim(std::io::stdout().as_raw_fd(), LIMIT);
}

fn trim(fd: RawFd, limit: u64) {
    // Only a file opened for appending: anything else (a terminal, a pipe) is not
    // ours to cut, and a plain write offset would leave a hole after the cut.
    // SAFETY: fcntl on a descriptor we only ask about.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || flags & libc::O_APPEND == 0 {
        return;
    }
    // SAFETY: fd stays open, and ManuallyDrop keeps this File from closing it.
    let log = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    let Ok(metadata) = log.metadata() else { return };
    if !metadata.is_file() || metadata.len() <= limit {
        return;
    }
    // stdout is write-only, so read through a second descriptor on the same file.
    let Some(path) = path_of(fd) else { return };
    let Ok(mut reader) = File::open(path) else { return };
    let keep = limit / 2;
    if reader.seek(SeekFrom::End(-(keep as i64))).is_err() {
        return;
    }
    let mut tail = Vec::with_capacity(keep as usize);
    if reader.read_to_end(&mut tail).is_err() {
        return;
    }
    // Start on a whole line.
    let start = tail.iter().position(|&b| b == b'\n').map_or(0, |at| at + 1);
    if log.set_len(0).is_err() {
        return;
    }
    let mut writer: &File = &log;
    let _ = std::io::Write::write_all(&mut writer, &tail[start..]);
}

fn path_of(fd: RawFd) -> Option<PathBuf> {
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes, NUL-terminated.
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buffer.as_mut_ptr()) } < 0 {
        return None;
    }
    let end = buffer.iter().position(|&b| b == 0)?;
    buffer.truncate(end);
    String::from_utf8(buffer).ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pcmlink-rx-log-{}-{name}", std::process::id()))
    }

    #[test]
    fn past_the_limit_it_keeps_the_newest_half_on_a_line_boundary() {
        let path = scratch("trim");
        let mut text = String::new();
        for i in 0..2000 {
            text += &format!("line {i:05}\n");
        }
        fs::write(&path, &text).unwrap();
        let mut log = OpenOptions::new().append(true).open(&path).unwrap();
        trim(log.as_raw_fd(), 8000);
        let kept = fs::read_to_string(&path).unwrap();
        assert!(kept.len() <= 4000);
        assert!(kept.starts_with("line "));
        assert!(kept.ends_with("line 01999\n"));
        // What is written next lands right after, with no hole.
        writeln!(log, "after").unwrap();
        let kept = fs::read_to_string(&path).unwrap();
        assert!(kept.ends_with("line 01999\nafter\n"));
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn under_the_limit_nothing_changes() {
        let path = scratch("under");
        fs::write(&path, "short\n").unwrap();
        let log = OpenOptions::new().append(true).open(&path).unwrap();
        trim(log.as_raw_fd(), 8000);
        assert_eq!(fs::read_to_string(&path).unwrap(), "short\n");
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_file_not_opened_for_appending_is_left_alone() {
        let path = scratch("plain");
        fs::write(&path, "x".repeat(10_000)).unwrap();
        let log = OpenOptions::new().write(true).open(&path).unwrap();
        trim(log.as_raw_fd(), 100);
        assert_eq!(fs::metadata(&path).unwrap().len(), 10_000);
        fs::remove_file(&path).unwrap();
    }
}
