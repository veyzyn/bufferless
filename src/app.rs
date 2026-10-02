//! The tray app: hidden window, tray icon, global hotkey, clip saving.

use crate::prelude::*;

use alloc::sync::Arc;
use core::cell::RefCell;

use crate::rt::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::config::Config;
use crate::pipeline::Pipeline;
use crate::ring::Ring;
use crate::rt::{self, UiCell, fs, path};
use crate::util::{copy_wide, hotkey_name, wide};
use crate::{clock, icon, log, mux, settings, util};

const WM_TRAY: u32 = WM_APP + 1;
const WM_CLIP_SAVED: u32 = WM_APP + 2;
/// Posted by a second instance to ask us to show settings.
pub const WM_SHOW_SETTINGS: u32 = WM_APP + 3;
pub const WINDOW_CLASS: PCWSTR = w!("BufferlessTrayWindow");

const HOTKEY_ID: i32 = 1;
const TIMER_STATUS: usize = 1;

const CMD_SAVE: usize = 100;
const CMD_OPEN_FOLDER: usize = 101;
const CMD_SETTINGS: usize = 102;
const CMD_QUIT: usize = 103;

struct App {
    hwnd: HWND,
    cfg: Config,
    ring: Arc<Mutex<Ring>>,
    pipeline: Option<Pipeline>,
    icon_ok: HICON,
    icon_error: HICON,
    showing_error: bool,
    taskbar_created: u32,
    hotkey_ok: bool,
    last_clip: Option<String>,
}

static APP: UiCell<RefCell<Option<App>>> = UiCell::new(RefCell::new(None));

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.borrow_mut().as_mut().map(f))
}

/// Tray icons for the normal and error states, matched to the taskbar theme:
/// white on a dark taskbar, near-black on a light one. Errors dim the mark.
fn tray_icons() -> (HICON, HICON) {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
    let rgb = if util::taskbar_is_light() { [0x1f, 0x1f, 0x1f] } else { [0xff, 0xff, 0xff] };
    (util::icon_from_rgba(size, &icon::glyph(size, rgb, 1.0)), util::icon_from_rgba(size, &icon::glyph(size, rgb, 0.4)))
}

pub fn current_config() -> Config {
    with_app(|a| a.cfg.clone()).unwrap_or_default()
}

pub fn run(first_run: bool) {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        RegisterClassW(&class);
        // A real (hidden) top-level window rather than a message-only one, so we
        // receive the TaskbarCreated broadcast when Explorer restarts.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            WINDOW_CLASS,
            w!("Bufferless"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .unwrap();

        let cfg = Config::load();
        if cfg.start_with_windows {
            // Re-point the startup entry at this exe in case it was moved or
            // replaced by a newer build somewhere else.
            util::set_autostart(true);
        }
        if first_run {
            let _ = cfg.save();
        }
        let (icon_ok, icon_error) = tray_icons();
        let ring = Arc::new(Mutex::new(Ring::new(cfg.replay_seconds)));
        let pipeline = Pipeline::start(&cfg, ring.clone());
        let app = App {
            hwnd,
            ring,
            pipeline: Some(pipeline),
            icon_ok,
            icon_error,
            showing_error: false,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            hotkey_ok: false,
            last_clip: None,
            cfg,
        };
        APP.with(|a| *a.borrow_mut() = Some(app));

        with_app(|a| {
            a.add_tray_icon();
            a.register_hotkey();
            if first_run && a.hotkey_ok {
                a.notify(
                    "Bufferless is running",
                    &format!(
                        "Press {} to save the last {} seconds. Right-click the tray icon for settings.",
                        hotkey_name(a.cfg.hotkey_modifiers, a.cfg.hotkey_key),
                        a.cfg.replay_seconds
                    ),
                );
            }
        });
        SetTimer(Some(hwnd), TIMER_STATUS, 2000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if let Some(dlg) = settings::window() {
                if IsDialogMessageW(dlg, &msg).as_bool() {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Dropping the app stops the pipeline and joins its threads.
        if let Some(app) = APP.with(|a| a.borrow_mut().take()) {
            app.remove_tray_icon();
        }
    }
}

impl App {
    fn tray_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: self.hwnd, uID: 1, ..Default::default() }
    }

    fn add_tray_icon(&self) {
        let mut nid = self.tray_data();
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = if self.showing_error { self.icon_error } else { self.icon_ok };
        copy_wide(&mut nid.szTip, &self.tooltip());
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    }

    fn remove_tray_icon(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.tray_data());
        }
    }

    fn tooltip(&self) -> String {
        let status = self.pipeline.as_ref().map(|p| p.status()).unwrap_or_default();
        let (buffered, bytes) = {
            let ring = self.ring.lock();
            (ring.seconds(), ring.bytes())
        };
        // The ring keeps a few extra seconds so clips can start on a keyframe,
        // but a clip is never longer than the replay length.
        let buffered = buffered.min(self.cfg.replay_seconds);
        format!("Bufferless\n{status}\n{buffered}s of {}s buffered ({} MB)", self.cfg.replay_seconds, bytes / 1_000_000)
    }

    fn refresh_tray(&mut self) {
        let status = self.pipeline.as_ref().map(|p| p.status()).unwrap_or_default();
        self.showing_error = status.contains("error");
        let mut nid = self.tray_data();
        nid.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.hIcon = if self.showing_error { self.icon_error } else { self.icon_ok };
        copy_wide(&mut nid.szTip, &self.tooltip());
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn notify(&self, title: &str, text: &str) {
        let mut nid = self.tray_data();
        nid.uFlags = NIF_INFO;
        copy_wide(&mut nid.szInfoTitle, title);
        copy_wide(&mut nid.szInfo, text);
        nid.dwInfoFlags = NIIF_NONE | NIIF_NOSOUND;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn register_hotkey(&mut self) {
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
            let mods = HOT_KEY_MODIFIERS(self.cfg.hotkey_modifiers) | MOD_NOREPEAT;
            self.hotkey_ok = RegisterHotKey(Some(self.hwnd), HOTKEY_ID, mods, self.cfg.hotkey_key).is_ok();
        }
        if !self.hotkey_ok {
            let name = hotkey_name(self.cfg.hotkey_modifiers, self.cfg.hotkey_key);
            log!("hotkey {name} is taken");
            self.notify(
                "Hotkey unavailable",
                &format!("{name} is already used by another app. Pick a different one in Settings."),
            );
        }
    }

    fn save_clip(&mut self) {
        let Some(clip) = self.ring.lock().snapshot(self.cfg.replay_seconds) else {
            util::play_error_sound();
            self.notify("Nothing to save yet", "The replay buffer is still empty.");
            return;
        };
        let folder = self.cfg.save_folder.clone();
        let stem = format!("{} {}", util::foreground_app_name(), util::timestamp());
        let hwnd = self.hwnd.0 as usize;
        rt::thread::spawn("save", move || {
            let start = clock::now();
            // Two saves within the same second shouldn't overwrite each other.
            let path = (1..)
                .map(|n| path::join(&folder, &if n == 1 { format!("{stem}.mp4") } else { format!("{stem} ({n}).mp4") }))
                .find(|p| !fs::exists(p))
                .unwrap();
            let result = fs::create_dir_all(&folder)
                .and_then(|_| mux::write_mp4(&path, &clip))
                .map(|size| (path, size))
                .map_err(|e| e.to_string());
            match &result {
                Ok((p, size)) => log!("saved {p} ({size} bytes) in {} ms", (clock::now() - start) / 10_000),
                Err(e) => log!("save failed: {e}"),
            }
            let boxed = Box::into_raw(Box::new(result));
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as _)), WM_CLIP_SAVED, WPARAM(0), LPARAM(boxed as isize));
            }
        });
    }

    fn on_clip_saved(&mut self, result: Result<(String, u64), String>) {
        match result {
            Ok((path, size)) => {
                if self.cfg.save_sound {
                    util::play_saved_sound();
                }
                let name = path::file_name(&path).to_string();
                // Integer math on purpose: float formatting costs ~5 KB in the tiny build.
                let tenths = (size + 50_000) / 100_000;
                self.notify("Clip saved", &format!("{name} ({}.{} MB)", tenths / 10, tenths % 10));
                self.last_clip = Some(path);
            }
            Err(e) => {
                util::play_error_sound();
                self.notify("Couldn't save clip", &e);
            }
        }
    }
}

/// Open the clips folder, optionally with a file selected. Called without the
/// app borrowed, since ShellExecute can pump messages.
fn open_folder(folder: &str, select: Option<&String>) {
    let _ = fs::create_dir_all(folder);
    let (file, params) = match select {
        Some(p) => (wide("explorer.exe"), wide(&format!("/select,\"{p}\""))),
        None => (wide(folder), vec![0]),
    };
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(file.as_ptr()), PCWSTR(params.as_ptr()), None, SW_SHOWNORMAL);
    }
}

impl App {
    /// Apply settings from the settings window.
    fn apply(&mut self, new: Config) {
        let old = core::mem::replace(&mut self.cfg, new);
        if let Err(e) = self.cfg.save() {
            log!("couldn't save config: {e}");
        }
        if old.capture_settings_differ(&self.cfg) {
            log!("capture settings changed, restarting pipeline");
            self.pipeline = None; // joins the old threads first
            self.ring.lock().clear();
            self.pipeline = Some(Pipeline::start(&self.cfg, self.ring.clone()));
        } else if old.replay_seconds != self.cfg.replay_seconds {
            self.ring.lock().set_keep(self.cfg.replay_seconds);
        }
        if (old.hotkey_modifiers, old.hotkey_key) != (self.cfg.hotkey_modifiers, self.cfg.hotkey_key) || !self.hotkey_ok
        {
            self.register_hotkey();
        }
        if old.start_with_windows != self.cfg.start_with_windows {
            util::set_autostart(self.cfg.start_with_windows);
        }
        self.refresh_tray();
    }
}

/// Called by the settings window when the user hits Save.
pub fn apply_config(cfg: Config) {
    with_app(|a| a.apply(cfg));
}

fn handle_command(cmd: usize) {
    match cmd {
        CMD_SAVE => {
            with_app(|a| a.save_clip());
        }
        CMD_OPEN_FOLDER => {
            if let Some(folder) = with_app(|a| a.cfg.save_folder.clone()) {
                open_folder(&folder, None);
            }
        }
        CMD_SETTINGS => settings::open(current_config()),
        CMD_QUIT => unsafe { PostQuitMessage(0) },
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => {
            with_app(|a| a.save_clip());
            LRESULT(0)
        }
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in the low word of lparam.
            match (lparam.0 & 0xffff) as u32 {
                WM_CONTEXTMENU => {
                    // The menu runs a modal loop that dispatches messages back
                    // into this wndproc, so the app must not be borrowed meanwhile.
                    if let Some((hwnd, cfg)) = with_app(|a| (a.hwnd, a.cfg.clone())) {
                        handle_command(show_menu(hwnd, &cfg));
                    }
                }
                WM_LBUTTONDBLCLK => settings::open(current_config()),
                NIN_BALLOONUSERCLICK => {
                    if let Some((folder, clip)) = with_app(|a| (a.cfg.save_folder.clone(), a.last_clip.clone())) {
                        open_folder(&folder, clip.as_ref());
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLIP_SAVED => {
            let result = unsafe { Box::from_raw(lparam.0 as *mut Result<(String, u64), String>) };
            with_app(|a| a.on_clip_saved(*result));
            LRESULT(0)
        }
        WM_SHOW_SETTINGS => {
            settings::open(current_config());
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_STATUS => {
            with_app(|a| a.refresh_tray());
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // Light/dark switches arrive as "ImmersiveColorSet"; just redraw.
            with_app(|a| {
                let (ok, error) = tray_icons();
                unsafe {
                    let _ = DestroyIcon(a.icon_ok);
                    let _ = DestroyIcon(a.icon_error);
                }
                (a.icon_ok, a.icon_error) = (ok, error);
                a.refresh_tray();
            });
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            let recreated = with_app(|a| msg == a.taskbar_created && msg != 0).unwrap_or(false);
            if recreated {
                with_app(|a| a.add_tray_icon());
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

fn show_menu(hwnd: HWND, cfg: &Config) -> usize {
    unsafe {
        let menu = CreatePopupMenu().unwrap();
        let save_label = format!("Save clip	{}", hotkey_name(cfg.hotkey_modifiers, cfg.hotkey_key));
        let items = [
            (CMD_SAVE, save_label.as_str()),
            (CMD_OPEN_FOLDER, "Open clips folder"),
            (CMD_SETTINGS, "Settings..."),
            (0, ""),
            (CMD_QUIT, "Quit"),
        ];
        for (id, label) in items {
            if id == 0 {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            } else {
                let text = wide(label);
                let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(text.as_ptr()));
            }
        }
        let _ = SetMenuDefaultItem(menu, CMD_SAVE as u32, 0);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    }
}
