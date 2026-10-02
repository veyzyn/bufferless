//! Audio pipeline: WASAPI system loopback + optional microphone, mixed on the
//! shared clock and encoded to AAC-LC. Every WASAPI packet carries a QPC
//! timestamp, which is what keeps audio in sync with video.

use crate::prelude::*;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::rt::Mutex;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::*;
use windows::core::{HSTRING, Result};

use crate::clock::{self, SECOND};
use crate::log;
use crate::mux::{AUDIO_BITRATE, AUDIO_CHANNELS, AUDIO_RATE};
use crate::ring::{Packet, Ring};

const CH: usize = AUDIO_CHANNELS as usize;
const FRAME: usize = 1024; // AAC frame size in samples per channel
/// How far behind real time the mixer runs, so late device packets still make it in.
const MIX_DELAY: i64 = AUDIO_RATE as i64 / 10;
/// Timestamp discrepancy we tolerate before resyncing a source (10ms).
const RESYNC: i64 = AUDIO_RATE as i64 / 100;

#[derive(Clone)]
pub struct AudioConfig {
    pub system: bool,
    pub mic: bool,
    /// Endpoint id, or empty for the default microphone.
    pub mic_device: String,
    pub mic_volume: f32,
}

pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

fn device_id(d: &IMMDevice) -> String {
    unsafe {
        d.GetId()
            .map(|p| {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as _));
                s
            })
            .unwrap_or_default()
    }
}

/// Microphones for the settings UI.
pub fn list_microphones() -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    unsafe {
        let Ok(e) = enumerator() else { return out };
        let Ok(list) = e.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return out };
        for i in 0..list.GetCount().unwrap_or(0) {
            let Ok(d) = list.Item(i) else { continue };
            let name = d
                .OpenPropertyStore(STGM_READ)
                .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "Unknown device".into());
            out.push(DeviceInfo { id: device_id(&d), name });
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    System,
    Mic,
}

struct Source {
    kind: Kind,
    /// Device we opened, to notice when the default device changes.
    device: String,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    gain: f32,
    /// Interleaved stereo samples; `fifo[0]` is frame number `start`.
    fifo: VecDeque<f32>,
    start: i64,
}

impl Source {
    fn open(kind: Kind, cfg: &AudioConfig) -> Result<Self> {
        unsafe {
            let e = enumerator()?;
            let device = match kind {
                Kind::System => e.GetDefaultAudioEndpoint(eRender, eConsole)?,
                Kind::Mic if cfg.mic_device.is_empty() => e.GetDefaultAudioEndpoint(eCapture, eConsole)?,
                Kind::Mic => e.GetDevice(&HSTRING::from(&cfg.mic_device))?,
            };
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            // Ask WASAPI to hand us 48k stereo float whatever the device runs at.
            let format = WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                    nChannels: CH as u16,
                    nSamplesPerSec: AUDIO_RATE,
                    nAvgBytesPerSec: AUDIO_RATE * 4 * CH as u32,
                    nBlockAlign: 4 * CH as u16,
                    wBitsPerSample: 32,
                    cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
                },
                Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
                dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
                SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            };
            let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            if kind == Kind::System {
                flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
            }
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                SECOND / 5,
                0,
                &format as *const _ as *const WAVEFORMATEX,
                None,
            )?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Self {
                kind,
                device: device_id(&device),
                client,
                capture,
                gain: if kind == Kind::Mic { cfg.mic_volume } else { 1.0 },
                fifo: VecDeque::new(),
                start: 0,
            })
        }
    }

    fn end(&self) -> i64 {
        self.start + (self.fifo.len() / CH) as i64
    }

    /// Drain everything WASAPI has for us into the fifo, placed on the timeline.
    fn read(&mut self, origin: i64) -> Result<()> {
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let mut data = core::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let at = if qpc != 0 { qpc as i64 } else { clock::now() };
                let idx = ((at - origin) as i128 * AUDIO_RATE as i128 / SECOND as i128) as i64;

                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let samples: &[f32] = if silent || data.is_null() {
                    &[]
                } else {
                    core::slice::from_raw_parts(data as *const f32, frames as usize * CH)
                };

                let mut skip = 0usize;
                if self.fifo.is_empty() {
                    self.start = idx;
                } else {
                    let gap = idx - self.end();
                    if gap > RESYNC {
                        self.fifo.extend(core::iter::repeat_n(0.0, gap as usize * CH));
                    } else if gap < -RESYNC {
                        // Overlap: drop the part of this packet we already have.
                        skip = ((-gap) as usize).min(frames as usize);
                    }
                    // Otherwise it's jitter: append contiguously.
                }
                for f in skip..frames as usize {
                    for c in 0..CH {
                        let s = samples.get(f * CH + c).copied().unwrap_or(0.0);
                        self.fifo.push_back(s * self.gain);
                    }
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
        Ok(())
    }

    /// Add frames [pos, pos + out.len()/CH) into `out`, consuming them.
    fn mix_into(&mut self, pos: i64, out: &mut [f32]) {
        let n = (out.len() / CH) as i64;
        // Throw away anything that arrived too late to be mixed.
        if self.start < pos {
            let stale = ((pos - self.start) as usize * CH).min(self.fifo.len());
            self.fifo.drain(..stale);
            self.start = pos;
        }
        let offset = self.start - pos;
        if offset >= n {
            return;
        }
        let take = ((n - offset) as usize * CH).min(self.fifo.len());
        for (i, s) in self.fifo.drain(..take).enumerate() {
            out[offset as usize * CH + i] += s;
        }
        self.start += (take / CH) as i64;
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

struct AacEncoder {
    mft: IMFTransform,
    output_size: u32,
}

impl AacEncoder {
    fn new() -> Result<Self> {
        unsafe {
            let mft: IMFTransform = CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CH as u32)?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2 * CH as u32)?;
            input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * 2 * CH as u32)?;

            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CH as u32)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BITRATE / 8)?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw AAC frames

            mft.SetInputType(0, &input, 0)?;
            mft.SetOutputType(0, &output, 0)?;
            let output_size = mft.GetOutputStreamInfo(0)?.cbSize.max(4096);
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self { mft, output_size })
        }
    }

    fn encode(&mut self, pcm: &[i16], pts: i64, on_packet: &mut impl FnMut(Packet)) -> Result<()> {
        unsafe {
            let bytes = pcm.len() * 2;
            let buffer = MFCreateMemoryBuffer(bytes as u32)?;
            let mut ptr = core::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            core::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, ptr, bytes);
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration((pcm.len() / CH) as i64 * SECOND / AUDIO_RATE as i64)?;
            self.mft.ProcessInput(0, &sample, 0)?;

            loop {
                let out_sample = MFCreateSample()?;
                out_sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                let mut out = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(out_sample)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let r = self.mft.ProcessOutput(0, &mut out, &mut status);
                let sample = ManuallyDrop::take(&mut out[0].pSample);
                drop(ManuallyDrop::take(&mut out[0].pEvents));
                match r {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) => return Err(e),
                }
                let Some(sample) = sample else { continue };
                let pts = sample.GetSampleTime()?;
                let buffer = sample.ConvertToContiguousBuffer()?;
                let mut ptr = core::ptr::null_mut();
                let mut len = 0u32;
                buffer.Lock(&mut ptr, None, Some(&mut len))?;
                let data = core::slice::from_raw_parts(ptr, len as usize).to_vec();
                buffer.Unlock()?;
                if !data.is_empty() {
                    on_packet(Packet { pts, key: true, data });
                }
            }
        }
    }
}

pub fn run(cfg: AudioConfig, ring: Arc<Mutex<Ring>>, stop: Arc<AtomicBool>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    // The audio timeline survives session restarts: after an error we fill the
    // downtime with silence instead of leaving a hole that breaks A/V sync.
    let origin = clock::now();
    let mut pos: i64 = 0;
    while !stop.load(Ordering::Relaxed) {
        match session(&cfg, &ring, &stop, origin, &mut pos) {
            Ok(()) => break,
            Err(e) => {
                log!("audio: session ended: {e}");
                for _ in 0..10 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    crate::rt::thread::sleep_ms(100);
                }
            }
        }
    }
}

fn open_sources(cfg: &AudioConfig) -> Vec<Source> {
    let mut sources = Vec::new();
    for (kind, enabled) in [(Kind::System, cfg.system), (Kind::Mic, cfg.mic)] {
        if !enabled {
            continue;
        }
        match Source::open(kind, cfg) {
            Ok(s) => sources.push(s),
            Err(e) => {
                log!("audio: couldn't open {}: {e}", if kind == Kind::Mic { "microphone" } else { "system audio" })
            }
        }
    }
    sources
}

fn default_device(kind: Kind) -> Option<String> {
    let flow = if kind == Kind::Mic { eCapture } else { eRender };
    unsafe { enumerator().ok()?.GetDefaultAudioEndpoint(flow, eConsole).ok().map(|d| device_id(&d)) }
}

fn session(cfg: &AudioConfig, ring: &Mutex<Ring>, stop: &AtomicBool, origin: i64, pos: &mut i64) -> Result<()> {
    let mut encoder = AacEncoder::new()?;
    let mut sources = open_sources(cfg);
    log!("audio: capturing {} source(s)", sources.len());

    let mut mix = vec![0f32; FRAME * CH];
    let mut pcm = vec![0i16; FRAME * CH];
    let mut on_packet = |p| ring.lock().push_audio(p);
    let mut last_device_check = clock::now();

    while !stop.load(Ordering::Relaxed) {
        for s in sources.iter_mut() {
            if let Err(e) = s.read(origin) {
                if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                    log!("audio: device went away, reopening");
                    s.device.clear(); // forces a reopen below
                } else {
                    return Err(e);
                }
            }
        }

        // Follow default device changes (e.g. switching to headphones).
        let now = clock::now();
        if now - last_device_check > SECOND {
            last_device_check = now;
            let stale = sources.iter().any(|s| {
                let follows_default = s.kind == Kind::System || cfg.mic_device.is_empty();
                s.device.is_empty() || (follows_default && default_device(s.kind).is_some_and(|d| d != s.device))
            });
            if stale {
                log!("audio: default device changed, reopening sources");
                sources = open_sources(cfg);
            }
        }

        let target = (now - origin) as i128 * AUDIO_RATE as i128 / SECOND as i128 - MIX_DELAY as i128;
        while *pos + FRAME as i64 <= target as i64 {
            mix.fill(0.0);
            for s in sources.iter_mut() {
                s.mix_into(*pos, &mut mix);
            }
            for (o, &m) in pcm.iter_mut().zip(&mix) {
                *o = (m.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            }
            let pts = origin + *pos * SECOND / AUDIO_RATE as i64;
            encoder.encode(&pcm, pts, &mut on_packet)?;
            *pos += FRAME as i64;
        }
        crate::rt::thread::sleep_ms(10);
    }
    Ok(())
}
