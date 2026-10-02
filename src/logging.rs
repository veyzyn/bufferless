use crate::prelude::*;
use crate::rt::Mutex;
use crate::rt::fs::{self, File};

use windows::Win32::System::SystemInformation::GetLocalTime;

static FILE: Mutex<Option<File>> = Mutex::new(None);

pub fn init(path: &str) {
    // Keep the log small: start fresh if it grew past 1 MB.
    let file = if fs::size(path).is_some_and(|len| len > 1 << 20) { File::create(path) } else { File::append(path) };
    *FILE.lock() = file.ok();
}

pub fn write(msg: String) {
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, msg
    );
    if let Some(f) = FILE.lock().as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::logging::write(alloc::format!($($t)*)) };
}
