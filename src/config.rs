use std::path::PathBuf;

use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_ALT, VK_F10};
use windows::Win32::UI::Shell::{FOLDERID_RoamingAppData, FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

#[derive(Clone, PartialEq, Debug)]
pub struct Config {
    /// RegisterHotKey modifier flags (MOD_ALT, MOD_CONTROL, ...).
    pub hotkey_modifiers: u32,
    /// Virtual key code.
    pub hotkey_key: u32,
    pub replay_seconds: u32,
    pub monitor: u32,
    /// Output height in pixels, 0 = native.
    pub resolution: u32,
    pub fps: u32,
    pub bitrate_mbps: u32,
    pub system_audio: bool,
    pub microphone: bool,
    /// Endpoint id, empty = default microphone.
    pub microphone_device: String,
    pub microphone_volume: u32,
    pub save_folder: String,
    pub save_sound: bool,
    pub start_with_windows: bool,
}

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
        s.map(PathBuf::from)
    }
}

impl Default for Config {
    fn default() -> Self {
        let videos = known_folder(&FOLDERID_Videos).unwrap_or_else(|| PathBuf::from("."));
        Self {
            hotkey_modifiers: MOD_ALT.0,
            hotkey_key: VK_F10.0 as u32,
            replay_seconds: 60,
            monitor: 0,
            resolution: 0,
            fps: 60,
            bitrate_mbps: 20,
            system_audio: true,
            microphone: false,
            microphone_device: String::new(),
            microphone_volume: 100,
            save_folder: videos.join("Bufferless").to_string_lossy().into_owned(),
            save_sound: true,
            start_with_windows: false,
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        known_folder(&FOLDERID_RoamingAppData).unwrap_or_else(|| PathBuf::from(".")).join("bufferless")
    }

    fn path() -> PathBuf {
        Self::dir().join("config.toml")
    }

    pub fn load() -> Self {
        let mut cfg = Self::default();
        if let Ok(text) = std::fs::read_to_string(Self::path()) {
            cfg.parse(&text);
        }
        cfg.sanitize();
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(Self::dir())?;
        std::fs::write(Self::path(), self.to_toml())
    }

    /// Read `key = value` lines (the flat subset of TOML this file uses).
    /// Unknown keys and malformed values are ignored and keep their defaults.
    fn parse(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            let num = || value.parse::<u32>().ok();
            let flag = || match value {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            let text = || parse_string(value);
            match key.trim() {
                "hotkey_modifiers" => set(&mut self.hotkey_modifiers, num()),
                "hotkey_key" => set(&mut self.hotkey_key, num()),
                "replay_seconds" => set(&mut self.replay_seconds, num()),
                "monitor" => set(&mut self.monitor, num()),
                "resolution" => set(&mut self.resolution, num()),
                "fps" => set(&mut self.fps, num()),
                "bitrate_mbps" => set(&mut self.bitrate_mbps, num()),
                "system_audio" => set(&mut self.system_audio, flag()),
                "microphone" => set(&mut self.microphone, flag()),
                "microphone_device" => set(&mut self.microphone_device, text()),
                "microphone_volume" => set(&mut self.microphone_volume, num()),
                "save_folder" => set(&mut self.save_folder, text()),
                "save_sound" => set(&mut self.save_sound, flag()),
                "start_with_windows" => set(&mut self.start_with_windows, flag()),
                _ => {}
            }
        }
    }

    fn to_toml(&self) -> String {
        let s = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
        format!(
            "hotkey_modifiers = {}\nhotkey_key = {}\nreplay_seconds = {}\nmonitor = {}\nresolution = {}\nfps = {}\n\
             bitrate_mbps = {}\nsystem_audio = {}\nmicrophone = {}\nmicrophone_device = {}\nmicrophone_volume = {}\n\
             save_folder = {}\nsave_sound = {}\nstart_with_windows = {}\n",
            self.hotkey_modifiers,
            self.hotkey_key,
            self.replay_seconds,
            self.monitor,
            self.resolution,
            self.fps,
            self.bitrate_mbps,
            self.system_audio,
            self.microphone,
            s(&self.microphone_device),
            self.microphone_volume,
            s(&self.save_folder),
            self.save_sound,
            self.start_with_windows,
        )
    }

    fn sanitize(&mut self) {
        self.replay_seconds = self.replay_seconds.clamp(5, 30 * 60);
        self.fps = self.fps.clamp(10, 240);
        self.bitrate_mbps = self.bitrate_mbps.clamp(1, 150);
        self.microphone_volume = self.microphone_volume.min(400);
    }

    /// Settings that require restarting the capture pipeline when changed.
    pub fn capture_settings_differ(&self, other: &Config) -> bool {
        (self.monitor, self.resolution, self.fps, self.bitrate_mbps)
            != (other.monitor, other.resolution, other.fps, other.bitrate_mbps)
            || (self.system_audio, self.microphone, &self.microphone_device, self.microphone_volume)
                != (other.system_audio, other.microphone, &other.microphone_device, other.microphone_volume)
    }

    /// Rough RAM needed for the replay buffer.
    pub fn estimated_memory_mb(&self) -> u32 {
        let bytes_per_sec = self.bitrate_mbps as u64 * 1_000_000 / 8 + crate::mux::AUDIO_BITRATE as u64 / 8;
        (bytes_per_sec * (self.replay_seconds as u64 + 3) / 1_000_000) as u32
    }
}

fn set<T>(field: &mut T, value: Option<T>) {
    if let Some(v) = value {
        *field = v;
    }
}

/// A TOML basic ("...") or literal ('...') string.
fn parse_string(value: &str) -> Option<String> {
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return Some(inner.to_string());
    }
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                other => out.push(other),
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut cfg = Config::default();
        cfg.save_folder = r#"C:\Users\x\Vids "quoted""#.into();
        cfg.microphone = true;
        cfg.fps = 144;
        let mut back = Config::default();
        back.parse(&cfg.to_toml());
        assert_eq!(back, cfg);
    }

    #[test]
    fn reads_files_written_by_the_toml_crate() {
        let mut cfg = Config::default();
        cfg.parse(
            r"hotkey_modifiers = 3
save_folder = 'C:\Users\you\Videos\Bufferless'
bogus = 1
fps = nope
",
        );
        assert_eq!(cfg.hotkey_modifiers, 3);
        assert_eq!(cfg.save_folder, r"C:\Users\you\Videos\Bufferless");
        assert_eq!(cfg.fps, Config::default().fps);
    }
}
