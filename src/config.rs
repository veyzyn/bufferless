use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_ALT, VK_F10};
use windows::Win32::UI::Shell::{FOLDERID_RoamingAppData, FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
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
        let mut cfg: Self =
            std::fs::read_to_string(Self::path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default();
        cfg.sanitize();
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(Self::dir())?;
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(Self::path(), text)
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
