//! The server's own log: every line it prints, also kept in memory so the
//! System page can show what happened since it started.

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// Lines kept in memory; older ones only remain in the terminal or log file.
const KEEP: usize = 3000;

#[derive(Clone, Debug, Serialize)]
pub struct Line {
    /// Counts up from 1, so a page can ask for lines after the last it saw.
    pub seq: u64,
    pub time: String,
    pub level: &'static str,
    pub message: String,
}

struct Buffer {
    next: u64,
    lines: VecDeque<Line>,
}

fn buffer() -> &'static Mutex<Buffer> {
    static BUFFER: OnceLock<Mutex<Buffer>> = OnceLock::new();
    BUFFER.get_or_init(|| Mutex::new(Buffer { next: 1, lines: VecDeque::with_capacity(KEEP) }))
}

/// When this process started, for the System page.
pub fn started() -> chrono::DateTime<chrono::Local> {
    static STARTED: OnceLock<chrono::DateTime<chrono::Local>> = OnceLock::new();
    *STARTED.get_or_init(chrono::Local::now)
}

/// Records a line and prints it to stderr with its time and level.
pub fn record(level: &'static str, message: impl Into<String>) {
    let message = message.into();
    let now = chrono::Local::now();
    eprintln!("{}  {:<5}  {message}", now.format("%Y-%m-%d %H:%M:%S"), level.to_uppercase());
    remember(level, message, now);
}

/// Keeps a line without printing it (the caller printed it its own way).
pub fn remember(level: &'static str, message: String, at: chrono::DateTime<chrono::Local>) {
    let Ok(mut buffer) = buffer().lock() else { return };
    let seq = buffer.next;
    buffer.next += 1;
    if buffer.lines.len() == KEEP {
        buffer.lines.pop_front();
    }
    buffer.lines.push_back(Line {
        seq,
        time: at.format("%Y-%m-%d %H:%M:%S").to_string(),
        level,
        message,
    });
}

/// Lines after `after`, oldest first, at most `limit` of the newest.
pub fn lines_after(after: u64, limit: usize) -> Vec<Line> {
    let Ok(buffer) = buffer().lock() else { return Vec::new() };
    let newer: Vec<&Line> = buffer.lines.iter().filter(|line| line.seq > after).collect();
    let skip = newer.len().saturating_sub(limit);
    newer.into_iter().skip(skip).cloned().collect()
}

/// How many lines were dropped from memory because the buffer was full.
pub fn dropped() -> u64 {
    let Ok(buffer) = buffer().lock() else { return 0 };
    buffer.lines.front().map_or(0, |line| line.seq - 1)
}

/// Records panics too, so a crash in a background task shows on the page.
pub fn catch_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let place = info.location().map(|at| format!(" at {}:{}", at.file(), at.line())).unwrap_or_default();
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        remember("error", format!("panic{place}: {what}"), chrono::Local::now());
        previous(info);
    }));
}

macro_rules! info {
    ($($arg:tt)*) => { $crate::serverlog::record("info", format!($($arg)*)) };
}
macro_rules! warning {
    ($($arg:tt)*) => { $crate::serverlog::record("warn", format!($($arg)*)) };
}
macro_rules! error {
    ($($arg:tt)*) => { $crate::serverlog::record("error", format!($($arg)*)) };
}
pub(crate) use {error, info, warning};
