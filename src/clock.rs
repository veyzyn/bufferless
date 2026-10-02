//! Shared QPC-based clock. All timestamps in the app are in 100ns units on this
//! timeline, which is the same timeline WASAPI reports device positions on.

use std::sync::OnceLock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

pub const SECOND: i64 = 10_000_000;

fn freq() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0;
        unsafe { QueryPerformanceFrequency(&mut f).ok() };
        f
    })
}

/// Current time in 100ns units.
pub fn now() -> i64 {
    let mut c = 0;
    unsafe { QueryPerformanceCounter(&mut c).ok() };
    (c as i128 * SECOND as i128 / freq() as i128) as i64
}
