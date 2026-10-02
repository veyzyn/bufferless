//! Video pipeline: desktop duplication -> GPU colour conversion/scaling ->
//! hardware encoder -> ring buffer. Runs on its own thread at a fixed frame
//! rate; if the screen hasn't changed we just re-encode the last frame.

use crate::prelude::*;

use alloc::sync::Arc;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::rt::Mutex;

use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE, LUID, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer, TIMER_ALL_ACCESS,
    WaitForSingleObject,
};
use windows::core::{Interface, Result};

use crate::clock::{self, SECOND};
use crate::cursor::CursorOverlay;
use crate::encoder::{EncoderSettings, H264Encoder};
use crate::log;
use crate::ring::{Ring, VideoFormat};

#[derive(Clone)]
pub struct VideoConfig {
    pub monitor: usize,
    pub fps: u32,
    pub bitrate: u32,
    /// Output height, or 0 for native resolution.
    pub height: u32,
    /// Draw the mouse cursor into the video.
    pub cursor: bool,
}

pub struct MonitorInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

struct Monitor {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
}

fn monitors() -> Result<Vec<Monitor>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut out = Vec::new();
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                out.push(Monitor { adapter: adapter.clone(), output: output.cast()? });
                o += 1;
            }
            a += 1;
        }
        Ok(out)
    }
}

/// Monitor list for the settings UI, in the same order `VideoConfig::monitor` indexes.
pub fn list_monitors() -> Vec<MonitorInfo> {
    let Ok(list) = monitors() else { return Vec::new() };
    list.iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let desc = unsafe { m.output.GetDesc().ok()? };
            let r = desc.DesktopCoordinates;
            let primary = r.left == 0 && r.top == 0;
            Some(MonitorInfo {
                name: format!("Display {}{}", i + 1, if primary { " (primary)" } else { "" }),
                width: (r.right - r.left) as u32,
                height: (r.bottom - r.top) as u32,
            })
        })
        .collect()
}

/// NV12 needs even dimensions, and encoders like multiples of 2 at minimum.
fn even(v: u32) -> u32 {
    v & !1
}

pub fn output_size(src_w: u32, src_h: u32, target_h: u32) -> (u32, u32) {
    if target_h == 0 || target_h >= src_h {
        return (even(src_w), even(src_h));
    }
    let w = (src_w as u64 * target_h as u64 / src_h as u64) as u32;
    (even(w), even(target_h))
}

struct Converter {
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input_views: Vec<ID3D11VideoProcessorInputView>,
    output_views: Vec<ID3D11VideoProcessorOutputView>,
}

impl Converter {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        inputs: &[&ID3D11Texture2D],
        (in_w, in_h): (u32, u32),
        outputs: &[ID3D11Texture2D],
        (out_w, out_h): (u32, u32),
        fps: u32,
    ) -> Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice = device.cast()?;
            let video_context: ID3D11VideoContext = context.cast()?;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: rate,
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;

            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut input_views = Vec::new();
            for &t in inputs {
                let mut v = None;
                video_device.CreateVideoProcessorInputView(t, &enumerator, &in_desc, Some(&mut v))?;
                input_views.push(v.unwrap());
            }

            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut output_views = Vec::new();
            for t in outputs {
                let mut v = None;
                video_device.CreateVideoProcessorOutputView(t, &enumerator, &out_desc, Some(&mut v))?;
                output_views.push(v.unwrap());
            }

            let src = RECT { left: 0, top: 0, right: in_w as i32, bottom: in_h as i32 };
            let dst = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            if let Ok(ctx1) = video_context.cast::<ID3D11VideoContext1>() {
                ctx1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                ctx1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }

            Ok(Self { video_context, processor, input_views, output_views })
        }
    }

    fn convert(&self, input: usize, output: usize) -> Result<()> {
        unsafe {
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.input_views[input].clone())),
                ..Default::default()
            };
            let r = self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_views[output],
                0,
                core::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r
        }
    }
}

fn texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    bind: D3D11_BIND_FLAG,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut t))? };
    Ok(t.unwrap())
}

fn duplicate(output: &IDXGIOutput1, device: &ID3D11Device) -> Result<IDXGIOutputDuplication> {
    unsafe {
        // DuplicateOutput1 is the modern path (needs per-monitor DPI awareness);
        // fall back to the old one if it isn't available.
        if let Ok(o5) = output.cast::<IDXGIOutput5>() {
            if let Ok(d) = o5.DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) {
                return Ok(d);
            }
        }
        output.DuplicateOutput(device)
    }
}

/// Small pool of encoder input surfaces so the encoder can hold on to a few
/// frames while we keep converting new ones.
const POOL: usize = 8;

/// Thread entry point. Restarts the session on errors until `stop` is set.
pub fn run(cfg: VideoConfig, ring: Arc<Mutex<Ring>>, stop: Arc<AtomicBool>, status: Arc<Mutex<String>>) {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        let _ = windows::Win32::System::Threading::SetThreadPriority(
            windows::Win32::System::Threading::GetCurrentThread(),
            windows::Win32::System::Threading::THREAD_PRIORITY_HIGHEST,
        );
    }
    while !stop.load(Ordering::Relaxed) {
        match session(&cfg, &ring, &stop, &status) {
            Ok(()) => break,
            Err(e) => {
                log!("video: session ended: {e}");
                *status.lock() = format!("Video error: {}", e.message());
                // Back off a bit so a persistent failure doesn't spin.
                for _ in 0..20 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    crate::rt::thread::sleep_ms(100);
                }
            }
        }
    }
}

fn session(cfg: &VideoConfig, ring: &Mutex<Ring>, stop: &AtomicBool, status: &Mutex<String>) -> Result<()> {
    let mut list = monitors()?;
    if list.is_empty() {
        return Err(windows::core::Error::new(E_ACCESSDENIED, "no monitors found"));
    }
    let Monitor { adapter, output } = list.swap_remove(cfg.monitor.min(list.len() - 1));
    let adapter_luid: LUID = unsafe { adapter.GetDesc1()?.AdapterLuid };

    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.unwrap();
    let context = context.unwrap();
    unsafe {
        // The encoder MFT uses this device from its own threads.
        let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        // Ask for GPU priority so heavy games don't starve capture. Best effort.
        let _ = device.cast::<IDXGIDevice>()?.SetGPUThreadPriority(7);
    }

    let mut dupl = Some(duplicate(&output, &device)?);
    let ddesc = unsafe { dupl.as_ref().unwrap().GetDesc() };
    let (src_w, src_h) = (ddesc.ModeDesc.Width, ddesc.ModeDesc.Height);
    let (out_w, out_h) = output_size(src_w, src_h, cfg.height);

    let frame = texture(
        &device,
        src_w,
        src_h,
        DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
    )?;
    // The cursor is drawn onto a copy of the frame, never the frame itself:
    // otherwise a cursor moving over an unchanged screen would leave a trail.
    let composed = texture(
        &device,
        src_w,
        src_h,
        DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
    )?;
    let mut cursor = if cfg.cursor {
        CursorOverlay::new(&device, &composed).map_err(|e| log!("video: can't draw the cursor: {e}")).ok()
    } else {
        None
    };
    let pool: Vec<ID3D11Texture2D> = (0..POOL)
        .map(|_| {
            texture(&device, out_w, out_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET | D3D11_BIND_VIDEO_ENCODER)
                .or_else(|_| texture(&device, out_w, out_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET))
        })
        .collect::<Result<_>>()?;
    let converter =
        Converter::new(&device, &context, &[&frame, &composed], (src_w, src_h), &pool, (out_w, out_h), cfg.fps)?;

    let settings = EncoderSettings { width: out_w, height: out_h, fps: cfg.fps, bitrate: cfg.bitrate };
    let mut encoder = H264Encoder::new(&device, adapter_luid, &settings)?;

    ring.lock().set_format(VideoFormat {
        width: out_w,
        height: out_h,
        fps: cfg.fps,
        seq_header: encoder.sequence_header().unwrap_or_default(),
    });
    log!("video: capturing {src_w}x{src_h} -> {out_w}x{out_h} @ {} fps, {} kbps", cfg.fps, cfg.bitrate / 1000);
    *status.lock() = format!("Recording {out_h}p{} with {}", cfg.fps, encoder.name);

    let timer =
        unsafe { CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)? };
    let interval = SECOND as f64 / cfg.fps as f64;
    let start = clock::now();
    let mut tick: i64 = 0;
    let mut next_surface = 0;
    let mut have_frame = false;
    let mut dropped = 0u64;
    let mut last_dupl_attempt = 0i64;

    let mut on_packet = |p| ring.lock().push_video(p);
    let mut on_seq = |s| ring.lock().update_seq_header(s);

    while !stop.load(Ordering::Relaxed) {
        // 1. Grab the newest desktop image if there is one.
        if let Some(d) = dupl.as_ref() {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            match unsafe { d.AcquireNextFrame(0, &mut info, &mut resource) } {
                Ok(()) => {
                    if info.LastPresentTime != 0 {
                        if let Some(tex) = resource.and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
                            unsafe { context.CopyResource(&frame, &tex) };
                            have_frame = true;
                        }
                    }
                    if let Some(c) = cursor.as_mut() {
                        if let Err(e) = c.update(&device, d, &info) {
                            log!("video: cursor update failed: {e}");
                        }
                    }
                    unsafe { d.ReleaseFrame()? };
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                    // Fullscreen switch, UAC prompt, resolution change... keep
                    // encoding the last frame and try to reattach.
                    log!("video: duplication lost ({e}), reattaching");
                    dupl = None;
                    if let Some(c) = cursor.as_mut() {
                        c.reset();
                    }
                }
                Err(e) => return Err(e),
            }
        } else if clock::now() - last_dupl_attempt > SECOND / 4 {
            last_dupl_attempt = clock::now();
            if let Ok(d) = duplicate(&output, &device) {
                let desc = unsafe { d.GetDesc() };
                if (desc.ModeDesc.Width, desc.ModeDesc.Height) != (src_w, src_h) {
                    // Resolution changed: start over with a fresh pipeline.
                    return Err(windows::core::Error::new(E_ACCESSDENIED, "display mode changed"));
                }
                dupl = Some(d);
            }
        }

        // 2. Feed the encoder.
        encoder.poll(&mut on_packet, &mut on_seq)?;
        let pts = start + (tick as f64 * interval) as i64;
        if have_frame {
            if encoder.wants_input() {
                let input = match cursor.as_ref().filter(|c| c.visible()) {
                    Some(c) => {
                        unsafe { context.CopyResource(&composed, &frame) };
                        c.draw(&context);
                        1
                    }
                    None => 0,
                };
                converter.convert(input, next_surface)?;
                encoder.submit(&pool[next_surface], pts, interval as i64)?;
                next_surface = (next_surface + 1) % POOL;
            } else {
                dropped += 1;
                if dropped.is_power_of_two() {
                    log!("video: encoder busy, dropped {dropped} frames so far");
                }
            }
        }
        encoder.poll(&mut on_packet, &mut on_seq)?;

        // 3. Sleep until the next frame is due. If we fell behind (system hitch),
        // skip ahead instead of trying to catch up with a burst of frames.
        tick += 1;
        let now = clock::now();
        let behind = ((now - start) as f64 / interval) as i64;
        if behind > tick + 2 {
            tick = behind + 1;
        }
        let due = start + (tick as f64 * interval) as i64;
        let wait = due - clock::now();
        if wait > 0 {
            unsafe {
                // Negative = relative time in 100ns units.
                SetWaitableTimer(timer, &-wait, 0, None, None, false)?;
                WaitForSingleObject(timer, INFINITE);
            }
        }
    }
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(timer);
    }
    log!("video: loop exited, releasing encoder");
    drop(encoder);
    log!("video: encoder released");
    Ok(())
}
