//! Small Win32 helpers shared by the tray app and the settings window.

use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyNameTextW, MAPVK_VK_TO_VSC, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, MapVirtualKeyW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};

/// NUL-terminated UTF-16 string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Copy a string into a fixed-size UTF-16 buffer, truncating if needed.
pub fn copy_wide(dst: &mut [u16], s: &str) {
    let src: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..src.len()].copy_from_slice(&src);
    dst[src.len()] = 0;
}

pub fn window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// The classic "record" glyph: a ring with a dot, drawn with anti-aliasing.
pub fn make_icon(size: i32, rgb: (u8, u8, u8)) -> HICON {
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
        };
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        let s = size as f32;
        let c = s / 2.0;
        let (ring_outer, ring_inner, dot) = (s * 0.47, s * 0.36, s * 0.25);
        for y in 0..size {
            for x in 0..size {
                let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
                let cover = |r: f32| (r - d + 0.5).clamp(0.0, 1.0);
                let ring = cover(ring_outer) * (1.0 - cover(ring_inner));
                let a = (ring + cover(dot)).min(1.0);
                let a8 = (a * 255.0) as u32;
                pixels[(y * size + x) as usize] = a8 << 24 | (rgb.0 as u32) << 16 | (rgb.1 as u32) << 8 | rgb.2 as u32;
            }
        }
        let mask = CreateBitmap(size, size, 1, 1, None);
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// Short two-note blip, synthesized once and played from memory.
pub fn play_saved_sound() {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    let wav = WAV.get_or_init(|| {
        const RATE: u32 = 44_100;
        let mut pcm: Vec<i16> = Vec::new();
        for (freq, ms) in [(880.0f32, 70), (1320.0, 90)] {
            let n = RATE * ms / 1000;
            for i in 0..n {
                let t = i as f32 / RATE as f32;
                // Quick fade in/out so it doesn't click.
                let env = (i.min(n - i) as f32 / (RATE as f32 * 0.008)).min(1.0);
                let v = (t * freq * std::f32::consts::TAU).sin() * env * 0.25;
                pcm.push((v * i16::MAX as f32) as i16);
            }
        }
        let data_len = pcm.len() as u32 * 2;
        let mut w = Vec::with_capacity(44 + data_len as usize);
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&1u16.to_le_bytes()); // mono
        w.extend_from_slice(&RATE.to_le_bytes());
        w.extend_from_slice(&(RATE * 2).to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        for s in pcm {
            w.extend_from_slice(&s.to_le_bytes());
        }
        w
    });
    unsafe {
        let _ = PlaySoundW(PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

pub fn play_error_sound() {
    unsafe {
        let _ = PlaySoundW(w!("SystemHand"), None, SND_ALIAS | SND_ASYNC);
    }
}

/// "2026-10-02 13-45-12"
pub fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// Name of the app in the foreground (usually the game), for clip file names.
pub fn foreground_app_name() -> String {
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let name = (|| {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let r = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
            let _ = CloseHandle(process);
            r.ok()?;
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            let stem = std::path::Path::new(&path).file_stem()?.to_string_lossy().into_owned();
            Some(stem)
        })();
        match name.as_deref() {
            None | Some("explorer") | Some("bufferless") => "Desktop".into(),
            Some(n) => n.chars().filter(|c| !r#"<>:"/\|?*"#.contains(*c)).collect(),
        }
    }
}

pub fn hotkey_name(modifiers: u32, vk: u32) -> String {
    let mut parts = Vec::new();
    for (flag, name) in [(MOD_CONTROL, "Ctrl"), (MOD_ALT, "Alt"), (MOD_SHIFT, "Shift"), (MOD_WIN, "Win")] {
        if modifiers & flag.0 != 0 {
            parts.push(name.to_string());
        }
    }
    let key = match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6f),
        0x30..=0x39 | 0x41..=0x5a => char::from_u32(vk).unwrap().to_string(),
        _ => unsafe {
            let scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
            let mut buf = [0u16; 64];
            let n = GetKeyNameTextW((scan << 16) as i32, &mut buf);
            if n > 0 { String::from_utf16_lossy(&buf[..n as usize]) } else { format!("Key {vk}") }
        },
    };
    parts.push(key);
    parts.join("+")
}

pub fn set_autostart(enable: bool) {
    let key = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    unsafe {
        if enable {
            let Ok(exe) = std::env::current_exe() else { return };
            let value = wide(&format!("\"{}\"", exe.display()));
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key,
                w!("Bufferless"),
                REG_SZ.0,
                Some(value.as_ptr() as _),
                (value.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, key, w!("Bufferless"));
        }
    }
}
