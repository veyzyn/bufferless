//! A tiny runtime replacing the parts of Rust's std this app used, built
//! directly on Win32. Together with `#![no_std]` this lets the exe drop std
//! and the C runtime entirely. The process entry point, allocator and panic
//! handler live in `start` and are left out of test builds, which use std.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicPtr, Ordering};

use windows::Win32::Foundation::{CloseHandle, E_FAIL, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Threading::{
    AcquireSRWLockExclusive, CreateThread, INFINITE, ReleaseSRWLockExclusive, SRWLOCK, Sleep, THREAD_CREATION_FLAGS,
    WaitForSingleObject,
};
use windows::core::PCWSTR;

use crate::util::wide;

pub type Result<T> = core::result::Result<T, windows::core::Error>;

pub fn error(msg: &str) -> windows::core::Error {
    windows::core::Error::new(E_FAIL, msg)
}

// ---------------------------------------------------------------------------
// Sync

/// A mutex on top of an SRW lock. Unlike std's there's no poisoning: a panic
/// aborts the process anyway.
pub struct Mutex<T> {
    lock: UnsafeCell<SRWLOCK>,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self { lock: UnsafeCell::new(SRWLOCK { Ptr: core::ptr::null_mut() }), value: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        unsafe { AcquireSRWLockExclusive(self.lock.get()) };
        MutexGuard { mutex: self }
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        unsafe { ReleaseSRWLockExclusive(self.mutex.lock.get()) };
    }
}

/// Lazily initialised value. Racing initialisers are fine: the loser's value
/// is dropped and everyone sees the winner's.
pub struct OnceLock<T> {
    ptr: AtomicPtr<T>,
}

impl<T> OnceLock<T> {
    pub const fn new() -> Self {
        Self { ptr: AtomicPtr::new(core::ptr::null_mut()) }
    }

    pub fn get_or_init(&self, init: impl FnOnce() -> T) -> &T {
        let current = self.ptr.load(Ordering::Acquire);
        if !current.is_null() {
            return unsafe { &*current };
        }
        let new = Box::into_raw(Box::new(init()));
        match self.ptr.compare_exchange(core::ptr::null_mut(), new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => unsafe { &*new },
            Err(winner) => {
                drop(unsafe { Box::from_raw(new) });
                unsafe { &*winner }
            }
        }
    }
}

/// Global state that is only ever touched from the UI thread (the stand-in
/// for `thread_local!`, which needs std). Same `with` API as `LocalKey`.
pub struct UiCell<T>(UnsafeCell<T>);

unsafe impl<T> Sync for UiCell<T> {}

impl<T> UiCell<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    pub fn with<R>(&'static self, f: impl FnOnce(&T) -> R) -> R {
        f(unsafe { &*self.0.get() })
    }
}

// ---------------------------------------------------------------------------
// Threads

pub mod thread {
    use super::*;

    pub struct JoinHandle {
        handle: HANDLE,
        name: &'static str,
    }

    unsafe impl Send for JoinHandle {}

    impl JoinHandle {
        pub fn name(&self) -> &'static str {
            self.name
        }

        pub fn join(self) {
            unsafe { WaitForSingleObject(self.handle, INFINITE) };
            // Drop closes the handle.
        }
    }

    impl Drop for JoinHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    pub fn spawn(name: &'static str, f: impl FnOnce() + Send + 'static) -> JoinHandle {
        unsafe extern "system" fn start(param: *mut c_void) -> u32 {
            let f = unsafe { Box::from_raw(param as *mut Box<dyn FnOnce() + Send>) };
            f();
            0
        }
        let boxed: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(f));
        let param = Box::into_raw(boxed) as *const c_void;
        let handle = unsafe { CreateThread(None, 0, Some(start), Some(param), THREAD_CREATION_FLAGS(0), None) }
            .expect("CreateThread failed");
        JoinHandle { handle, name }
    }

    pub fn sleep_ms(ms: u32) {
        unsafe { Sleep(ms) };
    }
}

// ---------------------------------------------------------------------------
// Files and paths. Paths are plain strings; this app only builds them by
// joining known folders with file names.

pub mod fs {
    use super::*;

    fn open(path: &str, access: u32, disposition: FILE_CREATION_DISPOSITION) -> Result<HANDLE> {
        let p = wide(path);
        unsafe {
            CreateFileW(PCWSTR(p.as_ptr()), access, FILE_SHARE_READ, None, disposition, FILE_ATTRIBUTE_NORMAL, None)
        }
    }

    pub fn read(path: &str) -> Result<Vec<u8>> {
        let file = File(open(path, FILE_GENERIC_READ.0, OPEN_EXISTING)?);
        let mut size = 0i64;
        unsafe { GetFileSizeEx(file.0, &mut size)? };
        let mut buf = alloc::vec![0u8; size as usize];
        let mut done = 0usize;
        while done < buf.len() {
            let mut n = 0u32;
            unsafe { ReadFile(file.0, Some(&mut buf[done..]), Some(&mut n), None)? };
            if n == 0 {
                break;
            }
            done += n as usize;
        }
        buf.truncate(done);
        Ok(buf)
    }

    pub fn read_to_string(path: &str) -> Option<String> {
        String::from_utf8(read(path).ok()?).ok()
    }

    pub fn write(path: &str, data: &[u8]) -> Result<()> {
        File::create(path)?.write_all(data)
    }

    fn attributes(path: &str) -> Option<WIN32_FILE_ATTRIBUTE_DATA> {
        let p = wide(path);
        let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
        unsafe { GetFileAttributesExW(PCWSTR(p.as_ptr()), GetFileExInfoStandard, &mut data as *mut _ as _).ok()? };
        Some(data)
    }

    pub fn exists(path: &str) -> bool {
        attributes(path).is_some()
    }

    pub fn size(path: &str) -> Option<u64> {
        attributes(path).map(|a| (a.nFileSizeHigh as u64) << 32 | a.nFileSizeLow as u64)
    }

    pub fn create_dir_all(path: &str) -> Result<()> {
        // Create each prefix in turn; existing ones fail harmlessly.
        let mut prefix = String::new();
        for (i, part) in path.split(['\\', '/']).enumerate() {
            if i > 0 {
                prefix.push('\\');
            }
            prefix.push_str(part);
            // Skip the drive ("C:") and the empty parts of UNC prefixes.
            if part.is_empty() || part.ends_with(':') {
                continue;
            }
            let p = wide(&prefix);
            unsafe {
                let _ = CreateDirectoryW(PCWSTR(p.as_ptr()), None);
            }
        }
        if exists(path) { Ok(()) } else { Err(windows::core::Error::from_thread()) }
    }

    pub struct File(HANDLE);

    impl File {
        pub fn create(path: &str) -> Result<Self> {
            open(path, GENERIC_WRITE.0, CREATE_ALWAYS).map(Self)
        }

        pub fn append(path: &str) -> Result<Self> {
            open(path, FILE_APPEND_DATA.0, OPEN_ALWAYS).map(Self)
        }

        pub fn write_all(&mut self, mut data: &[u8]) -> Result<()> {
            while !data.is_empty() {
                let mut n = 0u32;
                let chunk = &data[..data.len().min(1 << 30)];
                unsafe { WriteFile(self.0, Some(chunk), Some(&mut n), None)? };
                data = &data[n as usize..];
            }
            Ok(())
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    unsafe impl Send for File {}

    pub struct BufWriter {
        file: File,
        buf: Vec<u8>,
    }

    impl BufWriter {
        pub fn new(file: File, capacity: usize) -> Self {
            Self { file, buf: Vec::with_capacity(capacity) }
        }

        pub fn write_all(&mut self, data: &[u8]) -> Result<()> {
            if self.buf.len() + data.len() > self.buf.capacity() {
                self.flush()?;
            }
            if data.len() >= self.buf.capacity() {
                return self.file.write_all(data);
            }
            self.buf.extend_from_slice(data);
            Ok(())
        }

        pub fn flush(&mut self) -> Result<()> {
            self.file.write_all(&self.buf)?;
            self.buf.clear();
            Ok(())
        }
    }
}

pub mod path {
    use super::*;

    pub fn join(dir: &str, name: &str) -> String {
        let mut s = String::from(dir.trim_end_matches(['\\', '/']));
        s.push('\\');
        s.push_str(name);
        s
    }

    pub fn file_name(path: &str) -> &str {
        path.rsplit(['\\', '/']).next().unwrap_or(path)
    }

    pub fn file_stem(path: &str) -> &str {
        let name = file_name(path);
        name.rsplit_once('.').map_or(name, |(stem, _)| stem)
    }

    pub fn current_exe() -> String {
        let mut buf = [0u16; 1024];
        let n = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
        String::from_utf16_lossy(&buf[..n])
    }
}

// ---------------------------------------------------------------------------
// Float helpers. These are std-only (they lower to C runtime calls), and
// only need to be good enough for icons and a beep.

pub mod math {
    pub fn round(x: f32) -> f32 {
        if x >= 0.0 { (x + 0.5) as i64 as f32 } else { (x - 0.5) as i64 as f32 }
    }

    pub fn sin(x: f32) -> f32 {
        use core::f32::consts::{PI, TAU};
        // Reduce to [-pi, pi], then a Taylor series is accurate to ~1e-6.
        let x = x - TAU * round(x / TAU);
        let x = x.clamp(-PI, PI);
        let x2 = x * x;
        x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0 * (1.0 - x2 / 72.0 * (1.0 - x2 / 110.0)))))
    }
}

// ---------------------------------------------------------------------------
// Process entry, allocator, panic handler and the bits the C runtime used to
// provide to compiled code.

#[cfg(not(test))]
mod start {
    use core::alloc::{GlobalAlloc, Layout};

    use windows::Win32::System::Memory::{
        GetProcessHeap, HEAP_FLAGS, HEAP_ZERO_MEMORY, HeapAlloc, HeapFree, HeapReAlloc,
    };
    use windows::Win32::System::Threading::ExitProcess;

    /// The Windows heap hands out 16-byte aligned blocks on x64.
    const HEAP_ALIGN: usize = 16;

    struct WinHeap;

    unsafe impl GlobalAlloc for WinHeap {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            unsafe { self.alloc_with(layout, HEAP_FLAGS(0)) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            unsafe { self.alloc_with(layout, HEAP_ZERO_MEMORY) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe {
                let base = if layout.align() <= HEAP_ALIGN { ptr } else { *(ptr as *mut *mut u8).sub(1) };
                let _ = HeapFree(GetProcessHeap().unwrap_or_default(), HEAP_FLAGS(0), Some(base as _));
            }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            unsafe {
                if layout.align() <= HEAP_ALIGN {
                    let heap = GetProcessHeap().unwrap_or_default();
                    return HeapReAlloc(heap, HEAP_FLAGS(0), Some(ptr as _), new_size) as *mut u8;
                }
                let new = self.alloc(Layout::from_size_align_unchecked(new_size, layout.align()));
                if !new.is_null() {
                    core::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                    self.dealloc(ptr, layout);
                }
                new
            }
        }
    }

    impl WinHeap {
        unsafe fn alloc_with(&self, layout: Layout, flags: HEAP_FLAGS) -> *mut u8 {
            unsafe {
                let heap = GetProcessHeap().unwrap_or_default();
                if layout.align() <= HEAP_ALIGN {
                    return HeapAlloc(heap, flags, layout.size()) as *mut u8;
                }
                // Over-allocate and stash the real block pointer just before
                // the aligned one.
                let base = HeapAlloc(heap, flags, layout.size() + layout.align()) as *mut u8;
                if base.is_null() {
                    return base;
                }
                let aligned = (base as usize + size_of::<usize>()).next_multiple_of(layout.align()) as *mut u8;
                *(aligned as *mut *mut u8).sub(1) = base;
                aligned
            }
        }
    }

    #[global_allocator]
    static HEAP: WinHeap = WinHeap;

    #[panic_handler]
    fn panic(_: &core::panic::PanicInfo) -> ! {
        unsafe { ExitProcess(101) }
    }

    /// Referenced by any code using floating point when there's no CRT.
    #[unsafe(no_mangle)]
    static _fltused: i32 = 0;

    // Stack probe for frames over a page. Touches each page between the
    // current stack pointer and the new frame so the guard page grows the
    // stack in order. Same contract as MSVC's: size in rax, all registers
    // preserved, rsp not adjusted.
    core::arch::global_asm!(
        ".globl __chkstk",
        "__chkstk:",
        "    push rcx",
        "    push rax",
        "    lea rcx, [rsp + 24]",
        "    cmp rax, 0x1000",
        "    jb 2f",
        "1:",
        "    sub rcx, 0x1000",
        "    test [rcx], rcx",
        "    sub rax, 0x1000",
        "    cmp rax, 0x1000",
        "    ja 1b",
        "2:",
        "    sub rcx, rax",
        "    test [rcx], rcx",
        "    pop rax",
        "    pop rcx",
        "    ret",
    );

    /// The windows crate measures wide C strings with this. The volatile read
    /// stops LLVM from recognising the loop and turning it into a call to
    /// `wcslen` itself.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn wcslen(s: *const u16) -> usize {
        let mut n = 0;
        while unsafe { core::ptr::read_volatile(s.add(n)) } != 0 {
            n += 1;
        }
        n
    }

    /// Entry for normal builds, which still link the C runtime: its startup
    /// code initialises itself and then calls `main`.
    #[unsafe(no_mangle)]
    extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
        crate::main();
        0
    }

    /// Entry for the tiny build, which links no C runtime at all (passed to
    /// the linker with /ENTRY by build-tiny.ps1).
    #[unsafe(no_mangle)]
    extern "system" fn bufferless_start() -> ! {
        crate::main();
        unsafe { ExitProcess(0) }
    }
}
