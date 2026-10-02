use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn init(path: &Path) {
    // Keep the log small: start fresh if it grew past 1 MB.
    let truncate = std::fs::metadata(path).map(|m| m.len() > 1 << 20).unwrap_or(false);
    let file =
        std::fs::OpenOptions::new().create(true).append(!truncate).write(true).truncate(truncate).open(path).ok();
    *FILE.lock().unwrap() = file;
}

pub fn write(msg: String) {
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, msg
    );
    // Release builds have no console, so don't drag in stdio for nothing.
    #[cfg(debug_assertions)]
    eprint!("{line}");
    if let Some(f) = FILE.lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::logging::write(format!($($t)*)) };
}
