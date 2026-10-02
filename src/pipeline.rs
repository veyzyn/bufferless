//! Owns the capture threads. Stopping joins them, so a restart with new
//! settings never has two encoders fighting over the GPU.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::audio::{self, AudioConfig};
use crate::capture::{self, VideoConfig};
use crate::config::Config;
use crate::ring::Ring;

pub struct Pipeline {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub status: Arc<Mutex<String>>,
}

impl Pipeline {
    pub fn start(cfg: &Config, ring: Arc<Mutex<Ring>>) -> Self {
        ring.lock().unwrap().set_keep(cfg.replay_seconds);
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new("Starting...".to_string()));
        let mut threads = Vec::new();

        let video = VideoConfig {
            monitor: cfg.monitor as usize,
            fps: cfg.fps,
            bitrate: cfg.bitrate_mbps * 1_000_000,
            height: cfg.resolution,
        };
        {
            let (ring, stop, status) = (ring.clone(), stop.clone(), status.clone());
            threads.push(
                std::thread::Builder::new()
                    .name("video".into())
                    .spawn(move || capture::run(video, ring, stop, status))
                    .unwrap(),
            );
        }

        if cfg.system_audio || cfg.microphone {
            let audio = AudioConfig {
                system: cfg.system_audio,
                mic: cfg.microphone,
                mic_device: cfg.microphone_device.clone(),
                mic_volume: cfg.microphone_volume as f32 / 100.0,
            };
            let (ring, stop) = (ring.clone(), stop.clone());
            threads.push(
                std::thread::Builder::new().name("audio".into()).spawn(move || audio::run(audio, ring, stop)).unwrap(),
            );
        }

        Self { stop, threads, status }
    }

    pub fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let name = t.thread().name().unwrap_or("?").to_string();
            let _ = t.join();
            crate::log!("pipeline: {name} thread stopped");
        }
    }
}
