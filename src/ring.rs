//! In-memory replay buffer of *encoded* packets. Saving a clip never re-encodes,
//! it just copies packets from the last keyframe before the requested start.

use crate::prelude::*;

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use crate::clock::SECOND;

pub struct Packet {
    /// Presentation time on the shared clock, 100ns units.
    pub pts: i64,
    pub key: bool,
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Annex B SPS/PPS from the encoder's output type, if it reported one.
    pub seq_header: Vec<u8>,
}

pub struct Clip {
    pub format: VideoFormat,
    pub video: Vec<Arc<Packet>>,
    pub audio: Vec<Arc<Packet>>,
}

#[derive(Default)]
pub struct Ring {
    video: VecDeque<Arc<Packet>>,
    audio: VecDeque<Arc<Packet>>,
    format: Option<VideoFormat>,
    /// How much history to keep, 100ns units.
    keep: i64,
    bytes: usize,
}

impl Ring {
    pub fn new(seconds: u32) -> Self {
        // A couple of seconds of slack so we can always start on a keyframe.
        Self { keep: (seconds as i64 + 3) * SECOND, ..Default::default() }
    }

    pub fn set_keep(&mut self, seconds: u32) {
        self.keep = (seconds as i64 + 3) * SECOND;
        self.trim();
    }

    /// Called when a capture session (re)starts. Packets from an encoder with a
    /// different format can't share a file, so the buffer is dropped in that case.
    pub fn set_format(&mut self, format: VideoFormat) {
        if self.format.as_ref() != Some(&format) {
            self.video.clear();
            self.audio.clear();
            self.bytes = 0;
        }
        self.format = Some(format);
    }

    pub fn update_seq_header(&mut self, seq: Vec<u8>) {
        if let Some(f) = self.format.as_mut() {
            f.seq_header = seq;
        }
    }

    pub fn push_video(&mut self, p: Packet) {
        self.bytes += p.data.len();
        self.video.push_back(Arc::new(p));
        self.trim();
    }

    pub fn push_audio(&mut self, p: Packet) {
        self.bytes += p.data.len();
        self.audio.push_back(Arc::new(p));
        self.trim();
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// How many seconds of video are buffered right now.
    pub fn seconds(&self) -> u32 {
        match (self.video.front(), self.video.back()) {
            (Some(first), Some(last)) => ((last.pts - first.pts) / SECOND) as u32,
            _ => 0,
        }
    }

    pub fn clear(&mut self) {
        self.video.clear();
        self.audio.clear();
        self.bytes = 0;
    }

    fn trim(&mut self) {
        let newest = self.video.back().map(|p| p.pts).max(self.audio.back().map(|p| p.pts)).unwrap_or(0);
        let cutoff = newest - self.keep;
        // The buffer must always start on a keyframe.
        while self.video.front().is_some_and(|p| !p.key) {
            self.pop_video(1);
        }
        // Drop whole GOPs while the next keyframe is still older than the cutoff.
        while let Some(next_key) = self.video.iter().skip(1).position(|p| p.key).map(|i| i + 1) {
            if self.video[next_key].pts > cutoff {
                break;
            }
            self.pop_video(next_key);
        }
        let audio_cutoff = self.video.front().map_or(cutoff, |p| p.pts);
        while self.audio.front().is_some_and(|p| p.pts < audio_cutoff) {
            let p = self.audio.pop_front().unwrap();
            self.bytes -= p.data.len();
        }
    }

    fn pop_video(&mut self, n: usize) {
        for p in self.video.drain(..n) {
            self.bytes -= p.data.len();
        }
    }

    /// Grab the last `seconds` of footage, starting at the closest keyframe at
    /// or before the requested start.
    pub fn snapshot(&self, seconds: u32) -> Option<Clip> {
        let format = self.format.clone()?;
        let newest = self.video.back()?.pts;
        let start = newest - seconds as i64 * SECOND;
        let first = self
            .video
            .iter()
            .rposition(|p| p.key && p.pts <= start)
            .or_else(|| self.video.iter().position(|p| p.key))?;
        let video: Vec<_> = self.video.iter().skip(first).cloned().collect();
        let t0 = video[0].pts;
        let audio = self.audio.iter().filter(|p| p.pts >= t0 && p.pts <= newest).cloned().collect();
        Some(Clip { format, video, audio })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push `secs` seconds of 10 fps video with a keyframe every second.
    fn fill(ring: &mut Ring, secs: i64) {
        for i in 0..secs * 10 {
            ring.push_video(Packet { pts: i * SECOND / 10, key: i % 10 == 0, data: vec![0; 100] });
        }
    }

    #[test]
    fn counts_buffered_seconds() {
        let mut ring = Ring::new(60);
        assert_eq!(ring.seconds(), 0);
        fill(&mut ring, 12);
        assert_eq!(ring.seconds(), 11); // first to last frame
    }

    #[test]
    fn stops_growing_once_full() {
        let mut ring = Ring::new(10);
        fill(&mut ring, 40);
        // Keeps the replay length plus a little slack for keyframes, no more.
        let secs = ring.seconds();
        assert!((10..=14).contains(&secs), "buffered {secs}s");
    }
}
