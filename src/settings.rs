//! Settings window built from plain Win32 controls.

use crate::prelude::*;

use core::cell::RefCell;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, MOD_ALT, MOD_CONTROL, MOD_SHIFT, SetFocus};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::app;
use crate::audio::{self, DeviceInfo};
use crate::capture::{self, MonitorInfo};
use crate::config::Config;
use crate::rt::UiCell;
use crate::util::{wide, window_text};

const CLASS: PCWSTR = w!("BufferlessSettings");

const ID_HOTKEY: i32 = 100;
const ID_LENGTH: i32 = 101;
const ID_MONITOR: i32 = 102;
const ID_RESOLUTION: i32 = 103;
const ID_FPS: i32 = 104;
const ID_BITRATE: i32 = 105;
const ID_MEMORY: i32 = 106;
const ID_SYSTEM_AUDIO: i32 = 107;
const ID_MIC: i32 = 108;
const ID_MIC_DEVICE: i32 = 109;
const ID_FOLDER: i32 = 110;
const ID_BROWSE: i32 = 111;
const ID_SOUND: i32 = 112;
const ID_AUTOSTART: i32 = 113;
const ID_MIC_VOLUME: i32 = 114;

const LENGTHS: [(u32, &str); 9] = [
    (15, "15 seconds"),
    (30, "30 seconds"),
    (60, "1 minute"),
    (90, "1.5 minutes"),
    (120, "2 minutes"),
    (180, "3 minutes"),
    (300, "5 minutes"),
    (600, "10 minutes"),
    (1200, "20 minutes"),
];
const RESOLUTIONS: [(u32, &str); 5] = [(0, "Native"), (1440, "1440p"), (1080, "1080p"), (900, "900p"), (720, "720p")];
const FRAME_RATES: [u32; 6] = [30, 60, 90, 120, 144, 165];

struct Settings {
    hwnd: HWND,
    cfg: Config,
    monitors: Vec<MonitorInfo>,
    mics: Vec<DeviceInfo>,
    font: HFONT,
    heading_font: HFONT,
}

static STATE: UiCell<RefCell<Option<Settings>>> = UiCell::new(RefCell::new(None));

pub fn window() -> Option<HWND> {
    STATE.with(|s| s.borrow().as_ref().map(|s| s.hwnd))
}

fn send(hwnd: HWND, id: i32, msg: u32, wparam: usize, lparam: isize) -> isize {
    unsafe {
        let ctl = GetDlgItem(Some(hwnd), id).unwrap_or_default();
        SendMessageW(ctl, msg, Some(WPARAM(wparam)), Some(LPARAM(lparam))).0
    }
}

fn item(hwnd: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(Some(hwnd), id).unwrap_or_default() }
}

fn checked(hwnd: HWND, id: i32) -> bool {
    send(hwnd, id, BM_GETCHECK, 0, 0) == BST_CHECKED.0 as isize
}

fn combo_index(hwnd: HWND, id: i32) -> usize {
    send(hwnd, id, CB_GETCURSEL, 0, 0).max(0) as usize
}

pub fn open(cfg: Config) {
    if let Some(hwnd) = window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        return;
    }
    unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            ..Default::default()
        };
        RegisterClassW(&class); // fails harmlessly if already registered

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            CLASS,
            w!("Bufferless Settings"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            100,
            100,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        let small = crate::util::app_icon(GetSystemMetrics(SM_CXSMICON));
        let big = crate::util::app_icon(GetSystemMetrics(SM_CXICON));
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(small.0 as isize)));
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(big.0 as isize)));

        let dpi = GetDpiForWindow(hwnd);
        let mut ncm = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        let _ =
            SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, ncm.cbSize, Some(&mut ncm as *mut _ as _), 0, dpi);
        let font = CreateFontIndirectW(&ncm.lfMessageFont);
        let mut heading = ncm.lfMessageFont;
        heading.lfWeight = 600;
        let heading_font = CreateFontIndirectW(&heading);

        let mut state = Settings {
            hwnd,
            cfg,
            monitors: capture::list_monitors(),
            mics: audio::list_microphones(),
            font,
            heading_font,
        };
        let (w, h) = state.build(dpi);

        // Size the window around the content and centre it on its monitor.
        let mut rect = RECT { left: 0, top: 0, right: w, bottom: h };
        let _ = AdjustWindowRectExForDpi(&mut rect, style, false, WS_EX_CONTROLPARENT, dpi);
        let (ww, wh) = (rect.right - rect.left, rect.bottom - rect.top);
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut mi);
        let work = mi.rcWork;
        let x = work.left + (work.right - work.left - ww) / 2;
        let y = work.top + (work.bottom - work.top - wh) / 2;
        let _ = SetWindowPos(hwnd, None, x, y, ww, wh, SWP_NOZORDER);

        STATE.with(|s| *s.borrow_mut() = Some(state));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(item(hwnd, ID_HOTKEY)));
    }
}

impl Settings {
    /// Create all controls. Returns the client size.
    fn build(&mut self, dpi: u32) -> (i32, i32) {
        let s = |v: i32| v * dpi as i32 / 96;
        let hwnd = self.hwnd;
        let (label_x, ctl_x, ctl_w, row_h, row) = (s(20), s(170), s(270), s(24), s(32));
        let width = ctl_x + ctl_w + s(20);
        let mut y = s(14);

        let create = |class: PCWSTR, text: &str, style: u32, ex: WINDOW_EX_STYLE, x, y, w, h, id: i32, font: HFONT| unsafe {
            let text = wide(text);
            let ctl = CreateWindowExW(
                ex,
                class,
                PCWSTR(text.as_ptr()),
                WINDOW_STYLE(style) | WS_CHILD | WS_VISIBLE,
                x,
                y,
                w,
                h,
                Some(hwnd),
                Some(HMENU(id as isize as _)),
                None,
                None,
            )
            .unwrap_or_default();
            SendMessageW(ctl, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            ctl
        };
        let font = self.font;
        let heading = |text: &str, y: i32| {
            create(w!("STATIC"), text, 0, WINDOW_EX_STYLE::default(), label_x, y, ctl_w, row_h, -1, self.heading_font);
        };
        let label = |text: &str, y: i32| {
            create(
                w!("STATIC"),
                text,
                0,
                WINDOW_EX_STYLE::default(),
                label_x,
                y + s(4),
                ctl_x - label_x,
                row_h,
                -1,
                font,
            );
        };
        let combo = |id: i32, y: i32, w: i32, items: &[String], selected: usize| {
            let ctl = create(
                w!("COMBOBOX"),
                "",
                (CBS_DROPDOWNLIST | WS_VSCROLL.0 as i32 | WS_TABSTOP.0 as i32) as u32,
                WINDOW_EX_STYLE::default(),
                ctl_x,
                y,
                w,
                s(300),
                id,
                font,
            );
            for it in items {
                let t = wide(it);
                unsafe { SendMessageW(ctl, CB_ADDSTRING, None, Some(LPARAM(t.as_ptr() as isize))) };
            }
            unsafe { SendMessageW(ctl, CB_SETCURSEL, Some(WPARAM(selected)), None) };
        };
        let checkbox = |id: i32, text: &str, x: i32, y: i32, w: i32, on: bool| {
            let ctl = create(
                w!("BUTTON"),
                text,
                (BS_AUTOCHECKBOX | WS_TABSTOP.0 as i32) as u32,
                WINDOW_EX_STYLE::default(),
                x,
                y,
                w,
                row_h,
                id,
                font,
            );
            unsafe { SendMessageW(ctl, BM_SETCHECK, Some(WPARAM(on as usize)), None) };
        };
        let cfg = &self.cfg;

        // --- Recording
        heading("Recording", y);
        y += row;
        label("Save clip hotkey", y);
        let hk =
            create(w!("msctls_hotkey32"), "", WS_TABSTOP.0, WS_EX_CLIENTEDGE, ctl_x, y, ctl_w, row_h, ID_HOTKEY, font);
        unsafe {
            let mut f = 0;
            if cfg.hotkey_modifiers & MOD_ALT.0 != 0 {
                f |= HOTKEYF_ALT;
            }
            if cfg.hotkey_modifiers & MOD_CONTROL.0 != 0 {
                f |= HOTKEYF_CONTROL;
            }
            if cfg.hotkey_modifiers & MOD_SHIFT.0 != 0 {
                f |= HOTKEYF_SHIFT;
            }
            SendMessageW(hk, HKM_SETHOTKEY, Some(WPARAM((cfg.hotkey_key | f << 8) as usize)), None);
        }
        y += row;

        label("Replay length", y);
        let lengths: Vec<String> = LENGTHS.iter().map(|l| l.1.to_string()).collect();
        let sel = LENGTHS.iter().position(|l| l.0 >= cfg.replay_seconds).unwrap_or(2);
        combo(ID_LENGTH, y, ctl_w, &lengths, sel);
        y += row;

        label("Display", y);
        let monitors: Vec<String> =
            self.monitors.iter().map(|m| format!("{} - {}x{}", m.name, m.width, m.height)).collect();
        combo(ID_MONITOR, y, ctl_w, &monitors, (cfg.monitor as usize).min(monitors.len().saturating_sub(1)));
        y += row;

        label("Resolution", y);
        let res: Vec<String> = RESOLUTIONS.iter().map(|r| r.1.to_string()).collect();
        let sel = RESOLUTIONS.iter().position(|r| r.0 == cfg.resolution).unwrap_or(0);
        combo(ID_RESOLUTION, y, ctl_w, &res, sel);
        y += row;

        label("Frame rate", y);
        let rates: Vec<String> = FRAME_RATES.iter().map(|r| format!("{r} fps")).collect();
        let sel = FRAME_RATES.iter().position(|&r| r >= cfg.fps).unwrap_or(1);
        combo(ID_FPS, y, ctl_w, &rates, sel);
        y += row;

        label("Bitrate (Mbps)", y);
        create(
            w!("EDIT"),
            &cfg.bitrate_mbps.to_string(),
            (ES_NUMBER | ES_AUTOHSCROLL | WS_TABSTOP.0 as i32) as u32,
            WS_EX_CLIENTEDGE,
            ctl_x,
            y,
            s(60),
            row_h,
            ID_BITRATE,
            font,
        );
        create(
            w!("STATIC"),
            "",
            0,
            WINDOW_EX_STYLE::default(),
            ctl_x + s(72),
            y + s(4),
            ctl_w - s(72),
            row_h,
            ID_MEMORY,
            font,
        );
        y += row + s(10);

        // --- Audio
        heading("Audio", y);
        y += row;
        checkbox(ID_SYSTEM_AUDIO, "Record system audio", label_x, y, ctl_w, cfg.system_audio);
        y += row;
        checkbox(ID_MIC, "Record microphone", label_x, y, ctl_x - label_x, cfg.microphone);
        let mut mics = vec!["Default microphone".to_string()];
        mics.extend(self.mics.iter().map(|m| m.name.clone()));
        let sel = self.mics.iter().position(|m| m.id == cfg.microphone_device).map_or(0, |i| i + 1);
        combo(ID_MIC_DEVICE, y, ctl_w, &mics, sel);
        y += row;
        label("Mic volume (%)", y);
        create(
            w!("EDIT"),
            &cfg.microphone_volume.to_string(),
            (ES_NUMBER | ES_AUTOHSCROLL | WS_TABSTOP.0 as i32) as u32,
            WS_EX_CLIENTEDGE,
            ctl_x,
            y,
            s(60),
            row_h,
            ID_MIC_VOLUME,
            font,
        );
        y += row + s(10);

        // --- General
        heading("General", y);
        y += row;
        label("Save clips to", y);
        create(
            w!("EDIT"),
            &cfg.save_folder,
            (ES_AUTOHSCROLL | WS_TABSTOP.0 as i32) as u32,
            WS_EX_CLIENTEDGE,
            ctl_x,
            y,
            ctl_w - s(84),
            row_h,
            ID_FOLDER,
            font,
        );
        create(
            w!("BUTTON"),
            "Browse...",
            (BS_PUSHBUTTON | WS_TABSTOP.0 as i32) as u32,
            WINDOW_EX_STYLE::default(),
            ctl_x + ctl_w - s(76),
            y - s(1),
            s(76),
            row_h + s(2),
            ID_BROWSE,
            font,
        );
        y += row;
        checkbox(ID_SOUND, "Play a sound when a clip is saved", label_x, y, width - 2 * label_x, cfg.save_sound);
        y += row;
        checkbox(ID_AUTOSTART, "Start with Windows", label_x, y, width - 2 * label_x, cfg.start_with_windows);
        y += row + s(14);

        // --- Buttons (IDOK/IDCANCEL so Enter and Esc work via IsDialogMessage)
        let (bw, bh) = (s(88), s(28));
        create(
            w!("BUTTON"),
            "Save",
            (BS_DEFPUSHBUTTON | WS_TABSTOP.0 as i32) as u32,
            WINDOW_EX_STYLE::default(),
            width - s(20) - 2 * bw - s(8),
            y,
            bw,
            bh,
            IDOK.0,
            font,
        );
        create(
            w!("BUTTON"),
            "Cancel",
            (BS_PUSHBUTTON | WS_TABSTOP.0 as i32) as u32,
            WINDOW_EX_STYLE::default(),
            width - s(20) - bw,
            y,
            bw,
            bh,
            IDCANCEL.0,
            font,
        );
        y += bh + s(16);

        self.update_dependent();
        (width, y)
    }

    /// Read the form into a Config.
    fn read(&self) -> Config {
        let h = self.hwnd;
        let mut cfg = self.cfg.clone();

        let hk = send(h, ID_HOTKEY, HKM_GETHOTKEY, 0, 0) as u32;
        let (vk, f) = (hk & 0xff, (hk >> 8) & 0xff);
        if vk != 0 {
            cfg.hotkey_key = vk;
            cfg.hotkey_modifiers = 0;
            if f & HOTKEYF_ALT != 0 {
                cfg.hotkey_modifiers |= MOD_ALT.0;
            }
            if f & HOTKEYF_CONTROL != 0 {
                cfg.hotkey_modifiers |= MOD_CONTROL.0;
            }
            if f & HOTKEYF_SHIFT != 0 {
                cfg.hotkey_modifiers |= MOD_SHIFT.0;
            }
        }
        cfg.replay_seconds = LENGTHS[combo_index(h, ID_LENGTH).min(LENGTHS.len() - 1)].0;
        cfg.monitor = combo_index(h, ID_MONITOR) as u32;
        cfg.resolution = RESOLUTIONS[combo_index(h, ID_RESOLUTION).min(RESOLUTIONS.len() - 1)].0;
        cfg.fps = FRAME_RATES[combo_index(h, ID_FPS).min(FRAME_RATES.len() - 1)];
        cfg.bitrate_mbps = window_text(item(h, ID_BITRATE)).parse::<u32>().unwrap_or(cfg.bitrate_mbps).clamp(1, 150);
        cfg.system_audio = checked(h, ID_SYSTEM_AUDIO);
        cfg.microphone = checked(h, ID_MIC);
        cfg.microphone_device = match combo_index(h, ID_MIC_DEVICE) {
            0 => String::new(),
            i => self.mics.get(i - 1).map(|m| m.id.clone()).unwrap_or_default(),
        };
        cfg.microphone_volume =
            window_text(item(h, ID_MIC_VOLUME)).parse::<u32>().unwrap_or(cfg.microphone_volume).min(400);
        let folder = window_text(item(h, ID_FOLDER));
        if !folder.trim().is_empty() {
            cfg.save_folder = folder.trim().to_string();
        }
        cfg.save_sound = checked(h, ID_SOUND);
        cfg.start_with_windows = checked(h, ID_AUTOSTART);
        cfg
    }

    /// Refresh the RAM estimate and enable/disable mic controls.
    fn update_dependent(&self) {
        let cfg = self.read();
        let text = wide(&format!("uses ~{} MB of RAM", cfg.estimated_memory_mb()));
        unsafe {
            let _ = SetWindowTextW(item(self.hwnd, ID_MEMORY), PCWSTR(text.as_ptr()));
            let _ = EnableWindow(item(self.hwnd, ID_MIC_DEVICE), cfg.microphone);
            let _ = EnableWindow(item(self.hwnd, ID_MIC_VOLUME), cfg.microphone);
        }
    }
}

fn browse_folder(owner: HWND, current: &str) -> Option<String> {
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dialog.GetOptions().ok()?;
        dialog.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        if let Ok(item) = windows::Win32::UI::Shell::SHCreateItemFromParsingName::<
            _,
            _,
            windows::Win32::UI::Shell::IShellItem,
        >(PCWSTR(wide(current).as_ptr()), None)
        {
            let _ = dialog.SetFolder(&item);
        }
        dialog.Show(Some(owner)).ok()?;
        let result = dialog.GetResult().ok()?;
        let path = result.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as _));
        s
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as i32;
            let code = ((wparam.0 >> 16) & 0xffff) as u32;
            match id {
                x if x == IDOK.0 => {
                    let cfg = STATE.with(|s| s.borrow().as_ref().map(|s| s.read()));
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                    if let Some(cfg) = cfg {
                        app::apply_config(cfg);
                    }
                }
                x if x == IDCANCEL.0 => unsafe {
                    let _ = DestroyWindow(hwnd);
                },
                ID_BROWSE => {
                    let current = window_text(item(hwnd, ID_FOLDER));
                    // Modal dialog: no STATE borrow held here.
                    if let Some(path) = browse_folder(hwnd, &current) {
                        let t = wide(&path);
                        unsafe {
                            let _ = SetWindowTextW(item(hwnd, ID_FOLDER), PCWSTR(t.as_ptr()));
                        }
                    }
                }
                ID_BITRATE | ID_LENGTH | ID_MIC if matches!(code, EN_CHANGE | CBN_SELCHANGE | BN_CLICKED) => {
                    STATE.with(|s| {
                        if let Ok(s) = s.try_borrow() {
                            if let Some(s) = s.as_ref() {
                                s.update_dependent();
                            }
                        }
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => unsafe {
            // Make labels and checkboxes blend with the window background.
            let hdc = HDC(wparam.0 as _);
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
            LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize)
        },
        WM_DESTROY => {
            if let Some(s) = STATE.with(|s| s.borrow_mut().take()) {
                unsafe {
                    let _ = DeleteObject(s.font.into());
                    let _ = DeleteObject(s.heading_font.into());
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
